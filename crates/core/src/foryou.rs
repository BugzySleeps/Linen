//! "For you": a calm, local recommendation list. Built on this computer from
//! what you watched (no account, nothing sent beyond ordinary YouTube lookups):
//!
//! - seeds: your recent watches, weighted by how much of each you watched and
//!   how recently (no history yet: your subscriptions' newest videos);
//! - candidates: what YouTube relates to each seed (a video related to several
//!   seeds counts more), plus older popular videos from the channels you watch
//!   most that you haven't seen;
//! - out: anything watched, hidden, Shorts, live streams, and new videos from
//!   your subscriptions (those are in the Subscriptions tab already);
//! - variety: at most 2 per channel and 6 per seed, 36 in all. No endless list.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::Db;
use crate::db::Pick;
use crate::yt::{VideoHit, Yt};

const TOTAL: usize = 36;
const PER_CHANNEL: usize = 2;
const PER_SEED: usize = 6;
const SEEDS: usize = 6;

struct Seed {
    id: String,
    title: String,
    weight: f64,
}

struct Candidate {
    video: VideoHit,
    score: f64,
    reason: String,
    seed: usize,
}

fn short(title: &str) -> String {
    let t: String = title.chars().take(42).collect();
    if t.len() < title.len() { format!("{}…", t.trim_end()) } else { t }
}

/// Put together a fresh list (takes ~5-15 s: one lookup per seed). Doesn't save it.
pub fn build(db: &Db, yt: &Yt) -> Result<Vec<Pick>> {
    let history = db.history(200)?;
    let (liked, disliked) = db.ratings()?;
    let (hidden_videos, hidden_channels) = db.hidden()?;
    let subs: HashSet<String> = db.channels()?.into_iter().map(|c| c.id).collect();
    let watched: HashSet<&str> = history.iter().map(|h| h.id.as_str()).collect();
    let feed = db.feed(300)?;
    // new subscription videos are already one tab over
    let fresh_from_subs: HashSet<&str> =
        feed.iter().filter(|v| crate::now() - v.published < 21 * 86400).map(|v| v.id.as_str()).collect();

    // seeds: watched enough of it to count (a minute, or a third of a short video)
    let mut seeds: Vec<Seed> = vec![];
    for h in &history {
        let enough = h.furthest >= 60.0 || (h.duration > 0.0 && h.furthest / h.duration >= 0.33) || liked.contains(&h.id);
        if !enough || disliked.contains(&h.id) || seeds.iter().any(|s| s.id == h.id) {
            continue;
        }
        let mut completion = if h.duration > 0.0 { (h.furthest / h.duration).clamp(0.25, 1.0) } else { 0.5 };
        if liked.contains(&h.id) {
            // you said so: as strong as a video watched to the end, and then some
            completion = 1.4;
        }
        let recency = 0.82f64.powi(seeds.len() as i32);
        seeds.push(Seed { id: h.id.clone(), title: h.title.clone(), weight: completion * recency });
        if seeds.len() == SEEDS {
            break;
        }
    }
    let from_history = seeds.len();
    // not much history yet: start from what your channels just posted
    for v in feed.iter().take(12) {
        if seeds.len() >= 4 {
            break;
        }
        if !seeds.iter().any(|s| s.id == v.id) {
            seeds.push(Seed { id: v.id.clone(), title: v.title.clone(), weight: 0.35 });
        }
    }

    // what YouTube relates to each seed (3 lookups at a time)
    let related: Vec<Vec<VideoHit>> = std::thread::scope(|s| {
        let hs: Vec<_> = seeds
            .chunks(3)
            .flat_map(|chunk| {
                let hs: Vec<_> = chunk.iter().map(|seed| s.spawn(|| yt.details(&seed.id))).collect();
                hs.into_iter().map(|h| h.join().ok().and_then(|r| r.map_err(|e| log::warn!("for you: {e:#}")).ok())).collect::<Vec<_>>()
            })
            .collect();
        hs.into_iter().map(|d| d.map(|d| d.related).unwrap_or_default()).collect()
    });

    let mut cands: HashMap<String, Candidate> = HashMap::new();
    for (si, (seed, list)) in seeds.iter().zip(&related).enumerate() {
        let reason = if si < from_history {
            format!("Because you watched “{}”", short(&seed.title))
        } else {
            format!("Like “{}”", short(&seed.title))
        };
        for (rank, v) in list.iter().enumerate() {
            let score = seed.weight / (1.0 + rank as f64 * 0.12);
            match cands.get_mut(&v.id) {
                // related to more than one thing you watched: a stronger pick
                Some(c) => {
                    c.score += score * 0.8;
                    if score > c.score * 0.5 && seed.weight > seeds[c.seed].weight {
                        c.reason = reason.clone();
                        c.seed = si;
                    }
                }
                None => {
                    cands.insert(v.id.clone(), Candidate { video: v.clone(), score, reason: reason.clone(), seed: si });
                }
            }
        }
    }

    // older popular videos from the channels you watch most
    let mut per_channel: HashMap<&str, (usize, &str)> = HashMap::new();
    for h in history.iter().filter(|h| !h.channel_id.is_empty()) {
        let e = per_channel.entry(h.channel_id.as_str()).or_insert((0, h.channel_name.as_str()));
        e.0 += 1;
    }
    let mut top: Vec<(&str, usize, &str)> = per_channel.into_iter().map(|(id, (n, name))| (id, n, name)).collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(subs.contains(b.0).cmp(&subs.contains(a.0))));
    let top: Vec<_> = top.into_iter().filter(|t| !hidden_channels.contains(t.0)).take(2).collect();
    let popular: Vec<Vec<VideoHit>> = std::thread::scope(|s| {
        let hs: Vec<_> = top.iter().map(|t| s.spawn(|| yt.channel_videos(t.0, None))).collect();
        hs.into_iter().map(|h| h.join().ok().and_then(|r| r.ok()).map(|(_, p)| p.videos).unwrap_or_default()).collect()
    });
    for ((_, _, name), mut list) in top.iter().zip(popular) {
        list.retain(|v| crate::now() - v.published > 10 * 86400 && !watched.contains(v.id.as_str()));
        list.sort_by(|a, b| b.views.cmp(&a.views));
        for (rank, v) in list.into_iter().take(4).enumerate() {
            let score = 0.3 / (1.0 + rank as f64 * 0.3);
            cands.entry(v.id.clone()).or_insert(Candidate {
                video: v,
                score,
                reason: format!("Popular on {name}"),
                seed: usize::MAX,
            });
        }
    }

    // out: watched, hidden, Shorts, live, the seeds themselves, new from subscriptions
    let mut list: Vec<Candidate> = cands
        .into_values()
        .filter(|c| {
            let v = &c.video;
            !watched.contains(v.id.as_str())
                && !hidden_videos.contains(&v.id)
                && !hidden_channels.contains(&v.channel_id)
                && !fresh_from_subs.contains(v.id.as_str())
                && !seeds.iter().any(|s| s.id == v.id)
                && !v.live
                && v.duration.is_none_or(|d| d > 60)
                && !v.title.is_empty()
        })
        .collect();
    list.sort_by(|a, b| b.score.total_cmp(&a.score));

    // variety
    let (mut by_channel, mut by_seed): (HashMap<String, usize>, HashMap<usize, usize>) = Default::default();
    // little history yet: let each seed fill more of the list
    let per_seed = (TOTAL / seeds.len().max(1)).max(PER_SEED);
    let mut picks = vec![];
    for c in list {
        let ch = by_channel.entry(c.video.channel_id.clone()).or_default();
        let sd = by_seed.entry(c.seed).or_default();
        if *ch >= PER_CHANNEL || (c.seed != usize::MAX && *sd >= per_seed) {
            continue;
        }
        *ch += 1;
        *sd += 1;
        picks.push(Pick { video: c.video, reason: c.reason });
        if picks.len() == TOTAL {
            break;
        }
    }
    log::info!("for you: {} picks from {} seeds ({} from history)", picks.len(), seeds.len(), from_history);
    Ok(picks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Watched;

    /// Talks to YouTube: cargo test -p linen-core foryou -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_build() {
        let db = Db::open_in_memory().unwrap();
        for (i, (id, title, ch, name)) in [
            ("z-Xl9tGqH14", "Beginner Blender Tutorial (2026)", "UCOKHwx1VCdgnxwbjyb9Iu1g", "Blender Guru"),
            ("pvUy69I_Xuk", "What's New in Blender 5.3! Official Overview", "UCSMOQeBJ2RAnuFungnQOxLg", "Blender"),
        ]
        .iter()
        .enumerate()
        {
            db.watched(&Watched {
                id: id.to_string(),
                title: title.to_string(),
                channel_id: ch.to_string(),
                channel_name: name.to_string(),
                duration: 1600.0,
                furthest: 900.0,
                watched_at: 100 - i as i64,
                ..Default::default()
            })
            .unwrap();
        }
        let t = std::time::Instant::now();
        let picks = build(&db, &Yt::new().unwrap()).unwrap();
        println!("{} picks in {:.1} s", picks.len(), t.elapsed().as_secs_f32());
        for p in &picks {
            println!("  {:<60} {:<22} {}", p.video.title.chars().take(58).collect::<String>(), p.video.channel_name.chars().take(20).collect::<String>(), p.reason);
        }
        assert!(picks.len() >= 15);
        let mut per: HashMap<&str, usize> = HashMap::new();
        for p in &picks {
            *per.entry(&p.video.channel_id).or_default() += 1;
        }
        assert!(per.values().all(|n| *n <= PER_CHANNEL));
        assert!(!picks.iter().any(|p| p.video.id == "z-Xl9tGqH14"));
    }
}
