//! The calm extras, from community services (free, no account):
//! - SponsorBlock: segments to skip (sponsor reads, "like and subscribe", ...)
//! - DeArrow: community titles and thumbnails instead of clickbait
//! - Return YouTube Dislike: the dislike count YouTube hides
//!
//! SponsorBlock and DeArrow are asked by the first 4 characters of the video id's
//! SHA-256, never the id itself: the answer covers many videos and we pick ours, so
//! the services can't tell which one you're watching.

use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};

const SPONSORBLOCK: &str = "https://sponsor.ajay.app/api";
const DEARROW_THUMBS: &str = "https://dearrow-thumb.ajay.app/api/v1/getThumbnail";
const RYD: &str = "https://returnyoutubedislikeapi.com/votes";

/// SponsorBlock categories Linen knows: (key, name, what it is, skipped by default)
pub const CATEGORIES: &[(&str, &str, &str, bool)] = &[
    ("sponsor", "Sponsors", "Paid promotion and sponsor reads", true),
    ("selfpromo", "Self-promotion", "Merch, Patreon, other channels of theirs", true),
    ("interaction", "Interaction reminders", "\"Like and subscribe\" and the like", true),
    ("intro", "Intros", "Intro animations and pauses with no content", false),
    ("outro", "Endings", "End cards and credits", false),
    ("preview", "Previews and recaps", "Clips of what's coming up or what happened", false),
    ("filler", "Tangents", "Off-topic jokes and asides (can cut a lot)", false),
    ("music_offtopic", "Non-music parts", "Talking in music videos", false),
];

fn prefix(video_id: &str) -> String {
    let h = Sha256::digest(video_id.as_bytes());
    format!("{:02x}{:02x}", h[0], h[1])
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub category: String,
    pub start: f64,
    pub end: f64,
    pub uuid: String,
}

/// The skippable parts of a video, all known categories (filter at play time).
pub fn segments(http: &reqwest::blocking::Client, video_id: &str) -> Result<Vec<Segment>> {
    let cats = CATEGORIES.iter().map(|c| format!("\"{}\"", c.0)).collect::<Vec<_>>().join(",");
    let res = http
        .get(format!("{SPONSORBLOCK}/skipSegments/{}", prefix(video_id)))
        .query(&[("categories", format!("[{cats}]")), ("actionTypes", "[\"skip\"]".to_string())])
        .send()?;
    // 404: no video under this prefix has segments
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(vec![]);
    }
    let all: Value = serde_json::from_str(&res.error_for_status()?.text()?)?;
    let mut out = vec![];
    for v in all.as_array().into_iter().flatten().filter(|v| v["videoID"] == video_id) {
        for s in v["segments"].as_array().into_iter().flatten() {
            let (Some(start), Some(end)) = (s["segment"][0].as_f64(), s["segment"][1].as_f64()) else { continue };
            if end - start < 0.5 {
                continue;
            }
            out.push(Segment {
                category: s["category"].as_str().unwrap_or_default().to_string(),
                start,
                end,
                uuid: s["UUID"].as_str().unwrap_or_default().to_string(),
            });
        }
    }
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(out)
}

/// DeArrow's answer for one video: a better title, and/or a better frame for the picture.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Branding {
    pub title: Option<String>,
    /// seconds into the video
    pub thumb_at: Option<f64>,
}

/// Several videos' DeArrow submissions in one go (one request per hash prefix).
pub fn branding(http: &reqwest::blocking::Client, video_id: &str) -> Result<Branding> {
    let res = http.get(format!("{SPONSORBLOCK}/branding/{}", prefix(video_id))).send()?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Branding::default());
    }
    let all: Value = serde_json::from_str(&res.error_for_status()?.text()?)?;
    Ok(read_branding(&all[video_id]))
}

fn read_branding(v: &Value) -> Branding {
    // trusted: locked by a moderator, or not voted down; the best comes first
    let trusted = |x: &&Value| x["locked"].as_bool() == Some(true) || x["votes"].as_i64().unwrap_or(-1) >= 0;
    let title = v["titles"]
        .as_array()
        .and_then(|t| t.iter().find(trusted))
        .filter(|t| t["original"].as_bool() != Some(true))
        .and_then(|t| t["title"].as_str())
        .map(clean_title)
        .filter(|t| !t.is_empty());
    let thumb_at = v["thumbnails"]
        .as_array()
        .and_then(|t| t.iter().find(trusted))
        .filter(|t| t["original"].as_bool() != Some(true))
        .and_then(|t| t["timestamp"].as_f64());
    Branding { title, thumb_at }
}

/// DeArrow marks words to keep as typed with a ">" right before them ("at >Seaton");
/// the marks aren't part of the title. A spaced ">" ("A > B") is real text.
pub fn clean_title(t: &str) -> String {
    // a ">" leading the whole title is always DeArrow's mark
    let t = t.trim_start().strip_prefix('>').unwrap_or(t);
    let mut out = String::with_capacity(t.len());
    let mut prev_space = true;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        let next_word = chars.peek().is_some_and(|n| !n.is_whitespace());
        if c == '>' && prev_space && next_word {
            continue;
        }
        prev_space = c.is_whitespace();
        out.push(c);
    }
    out.trim().to_string()
}

/// The picture DeArrow made from that moment of the video.
pub fn thumb_url(video_id: &str, at: f64) -> String {
    format!("{DEARROW_THUMBS}?videoID={video_id}&time={at}")
}

/// (likes, dislikes): YouTube hides dislikes; this is the community's estimate.
pub fn votes(http: &reqwest::blocking::Client, video_id: &str) -> Result<(u64, u64)> {
    let v: Value = serde_json::from_str(&http.get(RYD).query(&[("videoId", video_id)]).send()?.error_for_status()?.text()?)?;
    Ok((v["likes"].as_u64().unwrap_or(0), v["dislikes"].as_u64().unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_and_reading() {
        assert_eq!(prefix("dQw4w9WgXcQ"), "5f6b");
        let v: Value = serde_json::from_str(
            r#"{"titles":[{"title":"> Never gonna give you up","original":false,"votes":1,"locked":false},
                          {"title":"x","original":true,"votes":5,"locked":false}],
               "thumbnails":[{"timestamp":null,"original":true,"votes":9,"locked":false}]}"#,
        )
        .unwrap();
        let b = read_branding(&v);
        assert_eq!(b.title.as_deref(), Some("Never gonna give you up"));
        assert_eq!(clean_title("Tram at >Seaton >Tramway | >England >E35"), "Tram at Seaton Tramway | England E35");
        assert_eq!(clean_title("Rust > C"), "Rust > C");
        assert_eq!(b.thumb_at, None);
        // voted down and not locked: ignored
        let v: Value = serde_json::from_str(r#"{"titles":[{"title":"bad","original":false,"votes":-2,"locked":false}],"thumbnails":[]}"#).unwrap();
        assert_eq!(read_branding(&v), Branding::default());
    }

    /// Talks to the services: cargo test -p linen-core calm -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live() {
        let http = crate::http().unwrap();
        let b = branding(&http, "dQw4w9WgXcQ").unwrap();
        println!("{b:?}");
        assert!(b.title.is_some());
        let (likes, dislikes) = votes(&http, "dQw4w9WgXcQ").unwrap();
        assert!(likes > 1_000_000 && dislikes > 1000);
        // a video with a sponsor read at ~0:31
        let s = segments(&http, "89uRpoPmR5U").unwrap();
        println!("{s:?}");
        assert!(s.iter().any(|s| s.category == "sponsor" && s.start > 25.0));
    }
}
