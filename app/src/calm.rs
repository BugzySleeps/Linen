//! The calm extras in the app: SponsorBlock skipping, DeArrow titles and
//! pictures, dislike counts, and the Settings page that switches them.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linen_core::calm::{self, Branding, CATEGORIES, Segment};
use linen_core::{Db, Video};
use slint::{Color, ComponentHandle, ModelRc, VecModel};

use crate::{App, CatItem, Msg, SegItem, Settings, State, VideoItem, Watch};

/// What the Settings page switched on.
pub struct Prefs {
    pub sponsorblock: bool,
    pub skip: HashSet<String>,
    pub titles: bool,
    pub thumbs: bool,
    pub dislikes: bool,
}

impl Prefs {
    pub fn load(db: &Db) -> Self {
        Self {
            sponsorblock: db.flag("sb", true),
            skip: CATEGORIES.iter().filter(|c| db.flag(&format!("sb.{}", c.0), c.3)).map(|c| c.0.to_string()).collect(),
            titles: db.flag("dearrow.titles", true),
            thumbs: db.flag("dearrow.thumbs", true),
            dislikes: db.flag("ryd", true),
        }
    }

    pub fn show(&self, ui: &App) {
        let s = ui.global::<Settings>();
        s.set_sponsorblock(self.sponsorblock);
        s.set_dearrow_titles(self.titles);
        s.set_dearrow_thumbs(self.thumbs);
        s.set_dislikes(self.dislikes);
        let cats: Vec<CatItem> = CATEGORIES
            .iter()
            .map(|c| CatItem { key: c.0.into(), name: c.1.into(), desc: c.2.into(), on: self.skip.contains(c.0) })
            .collect();
        s.set_categories(ModelRc::new(VecModel::from(cats)));
    }
}

/// SponsorBlock's usual colours, so marks look familiar.
fn color(category: &str) -> Color {
    let rgb = match category {
        "sponsor" => 0x00d400,
        "selfpromo" => 0xffff00,
        "interaction" => 0xcc00ff,
        "intro" => 0x00ffff,
        "outro" => 0x0202ed,
        "preview" => 0x008fd6,
        "filler" => 0x7300ff,
        "music_offtopic" => 0xff9900,
        _ => 0xaaaaaa,
    };
    Color::from_argb_encoded(0xdd000000 | rgb)
}

fn category_name(category: &str) -> String {
    match category {
        "sponsor" => "sponsor".into(),
        "selfpromo" => "self-promotion".into(),
        "interaction" => "interaction reminder".into(),
        "intro" => "intro".into(),
        "outro" => "ending".into(),
        "preview" => "preview".into(),
        "filler" => "tangent".into(),
        "music_offtopic" => "non-music part".into(),
        other => other.into(),
    }
}

/// "1.2K", "3.4M"
pub fn compact(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}K", n as f64 / 1e3).replace(".0K", "K"),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
        _ => format!("{:.1}B", n as f64 / 1e9).replace(".0B", "B"),
    }
}

/// DeArrow lookups on a few background threads, answers cached in the database
/// (a day for new videos, a month for older ones: titles settle quickly).
pub fn start_branding(tx: Sender<Msg>) -> Sender<(String, i64)> {
    let (jobs, rx): (Sender<(String, i64)>, Receiver<_>) = channel();
    let rx = Arc::new(Mutex::new(rx));
    for i in 0..3 {
        let (rx, tx) = (rx.clone(), tx.clone());
        std::thread::Builder::new()
            .name(format!("linen-dearrow{i}"))
            .spawn(move || {
                let (Ok(db), Ok(http)) = (Db::open(), linen_core::http()) else { return };
                loop {
                    let job = rx.lock().unwrap().recv();
                    let Ok((id, published)) = job else { return };
                    let old = published > 0 && linen_core::now() - published > 14 * 86400;
                    let keep = if old { 30 * 86400 } else { 86400 };
                    let b = match db.branding(&id).ok().flatten() {
                        Some((b, fetched)) if linen_core::now() - fetched < keep => b,
                        cached => match calm::branding(&http, &id) {
                            Ok(b) => {
                                let _ = db.set_branding(&id, &b);
                                b
                            }
                            Err(e) => {
                                log::debug!("dearrow {id}: {e:#}");
                                cached.map(|c| c.0).unwrap_or_default()
                            }
                        },
                    };
                    if tx.send(Msg::Branding(id, b)).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn dearrow thread");
    }
    jobs
}

/// Does a card need redrawing (picture arrived, title or details changed, ...)?
pub fn differs(a: &VideoItem, b: &VideoItem) -> bool {
    a.has_thumb != b.has_thumb || a.thumb_src != b.thumb_src || a.title != b.title || a.meta != b.meta || a.duration != b.duration
        || a.later != b.later
        || a.offline != b.offline
        || a.dl_text != b.dl_text
}

impl State {
    // ---- DeArrow ----

    pub fn want_branding(&mut self, videos: &[(String, i64)]) {
        if !self.prefs.titles && !self.prefs.thumbs {
            return;
        }
        for (id, published) in videos {
            if self.branding_asked.insert(id.clone()) {
                let _ = self.branding_jobs.send((id.clone(), *published));
            }
        }
    }

    pub fn on_branding(&mut self, ui: &App, id: String, b: Branding) {
        if let (true, Some(at)) = (self.prefs.thumbs, b.thumb_at) {
            self.pics.want(&format!("vd:{id}"), &calm::thumb_url(&id, at), 480);
        }
        self.branding.insert(id.clone(), b);
        if self.watching.as_deref() == Some(id.as_str()) {
            let original = self.watch_original.clone();
            self.show_watch_title(ui, &id, &original);
        }
    }

    /// The title to show: DeArrow's when there is one (and it's switched on).
    pub fn shown_title(&self, v: &Video) -> String {
        if self.prefs.titles {
            if let Some(t) = self.branding.get(&v.id).and_then(|b| b.title.clone()) {
                return t;
            }
        }
        v.title.clone()
    }

    /// The picture to show: (image, which one).
    pub fn shown_thumb(&self, id: &str) -> Option<(slint::Image, &'static str)> {
        if self.prefs.thumbs {
            if let Some(img) = self.cache.get(&format!("vd:{id}")) {
                return Some((img.clone(), "dearrow"));
            }
        }
        self.cache.get(&format!("v:{id}")).map(|i| (i.clone(), "original"))
    }

    /// The Watch page's title, with the original underneath when DeArrow changed it.
    pub fn show_watch_title(&mut self, ui: &App, id: &str, original: &str) {
        self.watch_original = original.to_string();
        let w = ui.global::<Watch>();
        let better = if self.prefs.titles { self.branding.get(id).and_then(|b| b.title.clone()) } else { None };
        match better {
            Some(t) if !original.is_empty() && t != original => {
                w.set_title(t.into());
                w.set_original_title(original.into());
            }
            Some(t) if original.is_empty() => {
                w.set_title(t.into());
                w.set_original_title("".into());
            }
            _ => {
                w.set_title(original.into());
                w.set_original_title("".into());
            }
        }
    }

    // ---- SponsorBlock + dislikes for the video being watched ----

    pub fn fetch_calm(&self, id: &str) {
        let (tx, id, sb, ryd) = (self.tx.clone(), id.to_string(), self.prefs.sponsorblock, self.prefs.dislikes);
        std::thread::spawn(move || {
            let Ok(http) = linen_core::http() else { return };
            // always fetched (switching it on mid-video then just works)
            match calm::segments(&http, &id) {
                Ok(s) => {
                    let _ = tx.send(Msg::Segments(id.clone(), s));
                }
                Err(e) if sb => log::debug!("sponsorblock {id}: {e:#}"),
                Err(_) => {}
            }
            if ryd {
                match calm::votes(&http, &id) {
                    Ok(v) => {
                        let _ = tx.send(Msg::Votes(id, v));
                    }
                    Err(e) => log::debug!("dislikes {id}: {e:#}"),
                }
            }
        });
    }

    pub fn on_segments(&mut self, ui: &App, id: &str, segs: Vec<Segment>) {
        if self.watching.as_deref() != Some(id) {
            return;
        }
        if !segs.is_empty() {
            log::info!("sponsorblock: {} segments for {id}", segs.len());
        }
        self.segments = segs;
        self.show_segments(ui);
    }

    /// Marks on the progress bar (needs the video's length).
    pub fn show_segments(&self, ui: &App) {
        let items: Vec<SegItem> = if self.prefs.sponsorblock && self.duration > 0.0 {
            self.segments
                .iter()
                .filter(|s| self.prefs.skip.contains(&s.category))
                .map(|s| SegItem {
                    from: (s.start / self.duration).clamp(0.0, 1.0) as f32,
                    to: (s.end / self.duration).clamp(0.0, 1.0) as f32,
                    color: color(&s.category),
                })
                .collect()
        } else {
            vec![]
        };
        ui.global::<Watch>().set_segments(ModelRc::new(VecModel::from(items)));
    }

    /// Called often while playing: skip a marked part as soon as it starts.
    pub fn check_skip(&mut self, ui: &App) {
        let w = ui.global::<Watch>();
        if self.skip_shown_at.is_some_and(|t| t.elapsed() > Duration::from_secs(5)) {
            self.skip_shown_at = None;
            w.set_skip_msg("".into());
        }
        if !self.prefs.sponsorblock || self.segments.is_empty() || w.get_paused() || !w.get_has_video() {
            return;
        }
        let Ok(pos) = self.mpv.get_property::<f64>("time-pos") else { return };
        // just entered it (seeking into the middle of one doesn't skip it)
        let hit = self.segments.iter().find(|s| {
            self.prefs.skip.contains(&s.category)
                && !self.undone.contains(&s.uuid)
                && pos >= s.start
                && pos < s.end - 0.4
                && pos - s.start < 1.5
                && self.last_skip.as_ref().is_none_or(|(u, t)| u != &s.uuid || t.elapsed() > Duration::from_secs(2))
        });
        if let Some(s) = hit.cloned() {
            log::info!("skipped {} {:.0}-{:.0}", s.category, s.start, s.end);
            crate::cmd(self.mpv, "seek", &[&format!("{}", s.end), "absolute+exact"]);
            w.set_skip_msg(format!("Skipped {}", category_name(&s.category)).into());
            self.skip_shown_at = Some(Instant::now());
            self.last_skip = Some((s.uuid.clone(), Instant::now()));
            self.last_skipped = Some(s);
        }
    }

    pub fn undo_skip(&mut self, ui: &App) {
        let Some(s) = self.last_skipped.take() else { return };
        self.undone.insert(s.uuid.clone());
        crate::cmd(self.mpv, "seek", &[&format!("{}", s.start), "absolute+exact"]);
        self.skip_shown_at = None;
        ui.global::<Watch>().set_skip_msg("".into());
    }

    pub fn on_votes(&mut self, ui: &App, id: &str, (likes, dislikes): (u64, u64)) {
        if self.watching.as_deref() != Some(id) {
            return;
        }
        self.watch_votes = Some((likes, dislikes));
        self.show_watch_meta(ui);
    }

    /// "3M views · Jan 14, 2026" under the title; the counts go on the like buttons.
    /// `watch_meta` is [views, date, likes].
    pub fn show_watch_meta(&self, ui: &App) {
        let w = ui.global::<Watch>();
        let parts: Vec<String> = self.watch_meta.iter().take(2).filter(|s| !s.is_empty()).cloned().collect();
        if !parts.is_empty() {
            w.set_meta(parts.join(" · ").into());
        }
        let yt_likes = self.watch_meta.get(2).map(|l| l.trim_end_matches(" likes").to_string()).unwrap_or_default();
        let likes = match (yt_likes.is_empty(), self.watch_votes) {
            (false, _) => yt_likes,
            (true, Some((l, _))) if l > 0 => compact(l),
            _ => String::new(),
        };
        w.set_likes(likes.into());
        let dislikes = match (self.prefs.dislikes, self.watch_votes) {
            (true, Some((_, d))) => compact(d),
            _ => String::new(),
        };
        w.set_dislikes(dislikes.into());
    }

    /// Your like/dislike (local; YouTube isn't told: there's no account).
    pub fn rate(&mut self, ui: &App, rating: i32) {
        let Some(id) = self.watching.clone() else { return };
        let _ = self.db.set_rating(&id, rating);
        ui.global::<Watch>().set_rating(rating);
        // connected: the real like/dislike on YouTube too
        self.rate_on_youtube(&id, rating);
        // For you takes it into account next time it's made
        self.foryou_dirty = true;
    }

    // ---- the Settings page ----

    pub fn set_setting(&mut self, ui: &App, key: &str, on: bool) {
        let _ = self.db.set_flag(key, on);
        self.prefs = Prefs::load(&self.db);
        self.prefs.show(ui);
        if key.starts_with("dearrow") && on {
            // fetch what's missing for everything listed so far
            let todo: Vec<(String, i64)> = self.known.values().map(|v| (v.id.clone(), v.published)).collect();
            self.branding_asked.clear();
            self.want_branding(&todo);
            for (id, b) in self.branding.clone() {
                if let (true, Some(at)) = (self.prefs.thumbs, b.thumb_at) {
                    self.pics.want(&format!("vd:{id}"), &calm::thumb_url(&id, at), 480);
                }
            }
        }
        self.show_segments(ui);
        self.show_watch_meta(ui);
        if let Some(id) = self.watching.clone() {
            let original = self.watch_original.clone();
            self.show_watch_title(ui, &id, &original);
        }
        self.refresh_cards(ui);
    }
}
