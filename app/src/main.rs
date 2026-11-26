mod account;
mod calm;
mod comments;
mod images;
mod library;
mod media;
mod pages;
mod playlists;
mod proxy;
mod resolver;
mod ytdlp;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use linen_core::{Channel, Db, Video};
use linen_video::{Event, Options, cmd};
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

slint::include_modules!();

/// Things other threads tell the UI thread.
enum Msg {
    Player(Event),
    Ytdlp(Result<String, String>),
    Picture(images::Decoded),
    Refreshed { new: usize, failed: usize },
    Added(Result<String, String>),
    Imported(Result<usize, String>),
    Resolved(resolver::Answer),
    /// a page's contents arrived: (which request, what)
    Browse(u64, pages::Browse),
    Details(String, Result<linen_core::yt::Details, String>),
    ForYou(Result<Vec<linen_core::db::Pick>, String>),
    Branding(String, linen_core::calm::Branding),
    Segments(String, Vec<linen_core::calm::Segment>),
    Votes(String, (u64, u64)),
    /// connected: (profile when new, the identity when chosen/moved, who)
    Account(Result<(Option<std::path::PathBuf>, Option<linen_core::account::Identity>, linen_core::yt::AccountInfo), String>),
    Identities(Result<(std::path::PathBuf, Vec<linen_core::account::Identity>), String>),
    /// (for which account, what): answers for an account since switched away are dropped
    YtSubs(String, Result<Vec<linen_core::yt::ChannelHit>, String>),
    YtHistory(String, Result<Vec<linen_core::yt::VideoHit>, String>),
    Tracking(String, linen_core::yt::Tracking),
    Home(u64, Result<linen_core::yt::Page, String>, bool),
    Suggestions(Option<String>, Vec<linen_core::yt::ChannelHit>),
    Pushed(usize),
    /// the desktop's media keys / player controls
    Media(media::Command),
    YtLater(String, Result<Vec<linen_core::yt::VideoHit>, String>),
    Download(String, library::Dl),
    /// what a saved video is (saved before we knew)
    Filled(linen_core::yt::VideoHit),
    /// comments for this video arrived
    Comments(String, comments::Ask, Result<linen_core::yt::Comments, String>),
    /// your playlists on YouTube (for which account)
    YtPlaylists(String, Result<playlists::YtLists, String>),
    /// a playlist made on YouTube: (name, the video to put in it, its id)
    PlaylistMade(String, Option<String>, Result<String, String>),
}

/// Videos waiting to be looked up ahead of time, and how many lookups per 10 minutes.
const PREFETCH_QUEUE: usize = 24;
const PREFETCH_LIMIT: usize = 60;
/// What the click helper looks up at start, to be warm for your first click ("Me at the zoo").
const WARM_UP: &str = "jNQXAC9IVRw";

/// Stream addresses stay valid for ~6 h; reuse them for a while.
const STREAMS_FRESH: Duration = Duration::from_secs(3 * 3600);

/// H.264 (the only codec older Intel GPUs decode in hardware): 1080p for normal
/// videos, 720p for 60 fps ones (1080p60 needs ~as much as YouTube sends a stream,
/// so it buffered, and this GPU can't show it smoothly in a window anyway).
/// Auto: 1080p only when it's 30 fps (1080p60 needs about the most YouTube sends one
/// stream at, and the T440 can't draw it windowed), else the best up to 720p.
const FORMAT: &str = "bv*[vcodec^=avc1][height=1080][fps<=30]+ba/bv*[vcodec^=avc1][height<=?720]+ba/b[vcodec^=avc1][height<=?720]/bv*[height<=?720]+ba/b";
/// Best: up to 1080p, 60 fps included.
const FORMAT_BEST: &str = "bv*[vcodec^=avc1][height<=?1080]+ba/bv*[height<=?1080]+ba/b";
/// Data saver: up to 480p.
const FORMAT_SAVER: &str = "bv*[vcodec^=avc1][height<=?480]+ba/bv*[height<=?480]+ba/b[height<=?480]/b";
/// Settings > Quality: 0 auto, 1 best, 2 data saver.
static QUALITY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The yt-dlp format for the quality picked in Settings.
pub fn format() -> &'static str {
    match QUALITY.load(std::sync::atomic::Ordering::Relaxed) {
        1 => FORMAT_BEST,
        2 => FORMAT_SAVER,
        _ => FORMAT,
    }
}
const REFRESH_EVERY: Duration = Duration::from_secs(30 * 60);

/// yt-dlp's message for a video that hasn't started yet -> "Begins in 11 days" /
/// "Premieres in 3 hours". None for every other kind of failure.
fn upcoming_from(error: &str) -> Option<String> {
    let msg = error.rsplit(": ").next().unwrap_or(error).trim().trim_end_matches('.');
    if let Some(rest) = msg.strip_prefix("This live event will begin ") {
        return Some(format!("Begins {rest}"));
    }
    if let Some(rest) = msg.strip_prefix("Premieres ") {
        return Some(format!("Premieres {rest}"));
    }
    if msg.contains("live event will begin") || msg.contains("Premiere will begin") || msg.contains("is upcoming") {
        return Some("Starts soon".into());
    }
    None
}

thread_local! {
    /// when the last skip was asked for (to log how long it took)
    static SEEK_AT: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Skip somewhere in the video.
fn seek(mpv: &libmpv2::Mpv, args: &[&str]) {
    SEEK_AT.with(|s| s.set(Some(Instant::now())));
    // tell the relay where to: it fetches the audio for there right away
    if let (Some(relay), Some(value)) = (proxy::RELAY.get(), args.first().and_then(|v| v.parse::<f64>().ok())) {
        let mode = args.get(1).copied().unwrap_or("relative");
        let target = if mode.starts_with("absolute-percent") {
            mpv.get_property::<f64>("duration").ok().map(|d| d * value / 100.0)
        } else if mode.starts_with("absolute") {
            Some(value)
        } else {
            mpv.get_property::<f64>("time-pos").ok().map(|p| p + value)
        };
        if let Some(t) = target {
            relay.expect_time(t.max(0.0));
        }
    }
    cmd(mpv, "seek", args);
}

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn ago(t: i64) -> String {
    let d = (linen_core::now() - t).max(0);
    let (n, unit) = match d {
        0..60 => return "just now".into(),
        60..3600 => (d / 60, "minute"),
        3600..86400 => (d / 3600, "hour"),
        86400..604800 => (d / 86400, "day"),
        604800..2629800 => (d / 604800, "week"),
        2629800..31557600 => (d / 2629800, "month"),
        _ => (d / 31557600, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

fn views(n: Option<u64>) -> String {
    match n {
        None => String::new(),
        Some(n) if n >= 1_000_000_000 => format!("{:.1}B views", n as f64 / 1e9).replace(".0B", "B"),
        Some(n) if n >= 1_000_000 => format!("{:.1}M views", n as f64 / 1e6).replace(".0M", "M"),
        Some(n) if n >= 1_000 => format!("{:.1}K views", n as f64 / 1e3).replace(".0K", "K"),
        Some(1) => "1 view".into(),
        Some(n) => format!("{n} views"),
    }
}

/// The focused monitor's refresh rate from Hyprland; 60 Hz otherwise.
fn display_fps() -> f64 {
    let out = std::process::Command::new("hyprctl").args(["monitors", "-j"]).output().ok();
    out.and_then(|o| {
        let text = String::from_utf8(o.stdout).ok()?;
        // tiny parse: the "refreshRate" of the focused monitor (or the first)
        let monitors: Vec<&str> = text.split("\"name\"").skip(1).collect();
        let pick = monitors.iter().find(|m| m.contains("\"focused\": true")).or(monitors.first())?;
        let i = pick.find("\"refreshRate\":")? + "\"refreshRate\":".len();
        pick[i..].trim_start().split([',', '\n', '}']).next()?.trim().parse::<f64>().ok()
    })
    .filter(|f| (20.0..=500.0).contains(f))
    .unwrap_or(60.0)
}

fn to_image(d: &images::Decoded) -> Image {
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&d.rgba, d.w, d.h))
}

struct State {
    db: Db,
    tx: Sender<Msg>,
    pics: images::Loader,
    cache: HashMap<String, Image>,
    feed: Vec<Video>,
    feed_model: Rc<VecModel<VideoItem>>,
    channels: Vec<Channel>,
    refreshing: bool,
    last_refresh: Option<Instant>,
    /// the video on the Watch page
    watching: Option<String>,
    duration: f64,
    feed_seen_at: Option<Instant>,
    resolver: resolver::Resolver,
    streams: HashMap<String, (resolver::Streams, Instant)>,
    /// the video waiting for its streams to play
    pending_open: Option<String>,
    /// background lookups running now (and since when)
    prefetching: Vec<(String, Instant)>,
    prefetch_queue: std::collections::VecDeque<String>,
    prefetch_times: std::collections::VecDeque<Instant>,
    click_helper_warm: bool,
    /// the click helper's warm-up lookup is still running (a click then also asks the other helper)
    warming: bool,
    mpv: &'static libmpv2::Mpv,
    opened_at: Option<Instant>,
    /// already fell back to letting mpv look the video up itself
    fell_back: bool,
    /// brand-new links YouTube refused: tries with the same links, and when to retry
    link_retries: u32,
    retry_at: Option<Instant>,
    /// how long YouTube's ad wait was in all (for the progress bar)
    ad_wait_total: i32,
    /// fetches video for mpv in fast pieces (proxy.rs)
    relay: Option<std::sync::Arc<proxy::Proxy>>,
    /// the desktop's media controls (MPRIS)
    media: Option<media::Media>,
    /// scheduled videos (not out yet) and when they start
    upcoming: HashMap<String, String>,
    /// the card the mouse rests on, and since when
    hover: Option<(String, Instant)>,
    /// a video loaded (paused) because you're hovering it; true once its first frame is in
    preload: Option<(String, bool)>,
    /// streams that YouTube only allows from a moment from now: (video, unix time)
    play_at: Option<(String, u64)>,
    // ---- browsing (pages.rs) ----
    yt: std::sync::Arc<linen_core::yt::Yt>,
    /// every video shown anywhere (feed, For you, search, channels, playlists)
    known: HashMap<String, Video>,
    reasons: HashMap<String, String>,
    foryou: pages::List,
    results: pages::List,
    ch_list: pages::List,
    pl_list: pages::List,
    /// newest request per page kind (search, channel, playlist); older answers are dropped
    req: [u64; 3],
    query: String,
    results_more: Option<String>,
    found_channels: Vec<linen_core::yt::ChannelHit>,
    ch_id: String,
    ch_tab: i32,
    ch_more: Option<String>,
    ch_info: Option<linen_core::yt::ChannelInfo>,
    pl_id: String,
    pl_more: Option<String>,
    /// pages to go back to
    nav: Vec<i32>,
    foryou_busy: bool,
    foryou_dirty: bool,
    /// links on the Watch page, in the order shown
    watch_links: Vec<linen_core::yt::Link>,
    // ---- calm extras (calm.rs) ----
    prefs: calm::Prefs,
    branding: HashMap<String, linen_core::calm::Branding>,
    branding_asked: std::collections::HashSet<String>,
    branding_jobs: Sender<(String, i64)>,
    segments: Vec<linen_core::calm::Segment>,
    /// segments you un-skipped in this video
    undone: std::collections::HashSet<String>,
    skip_shown_at: Option<Instant>,
    last_skip: Option<(String, Instant)>,
    last_skipped: Option<linen_core::calm::Segment>,
    watch_votes: Option<(u64, u64)>,
    /// views, date, likes for the Watch page
    watch_meta: Vec<String>,
    watch_original: String,
    // ---- YouTube account (account.rs) ----
    account: Option<linen_core::yt::AccountInfo>,
    tracking: Option<account::WatchTrack>,
    home: pages::List,
    home_more: Option<String>,
    home_req: u64,
    just_imported: std::collections::HashSet<String>,
    /// follows on their way (the welcome screen counts them right away)
    pending_follows: std::collections::HashSet<String>,
    suggestions: Vec<linen_core::yt::ChannelHit>,
    /// accounts/channels the Firefox sign-in offers (the picker)
    identities: Vec<linen_core::account::Identity>,
    identity_profile: Option<std::path::PathBuf>,
    // ---- Library (library.rs) ----
    later: std::collections::HashSet<String>,
    later_list: pages::List,
    dl_list: pages::List,
    /// Library > Playlists, the one of yours open, and the video the Save box is for
    my_lists: pages::List,
    my_playlist: Option<String>,
    save_video: String,
    downloads: HashMap<String, library::DlState>,
    dl_queue: std::collections::VecDeque<String>,
    /// listening without the picture
    audio_only: bool,
    /// the video's chapters (for the marks on the progress bar), and the one shown
    chapters: Vec<(String, f64)>,
    chapter_shown: String,
    /// the comments under the video being watched
    thread: comments::Thread,
}

impl State {
    fn video_item(&self, v: &Video) -> VideoItem {
        // scheduled: say when instead of views and length
        if let Some(when) = self.upcoming.get(&v.id) {
            let mut item = self.video_item_plain(v);
            item.meta = format!("Scheduled · {}", when.to_lowercase()).into();
            item.duration = "Upcoming".into();
            return item;
        }
        self.video_item_plain(v)
    }

    fn video_item_plain(&self, v: &Video) -> VideoItem {
        let (thumb, src) = self.shown_thumb(&v.id).unzip();
        let when = if v.published > 0 { ago(v.published) } else { String::new() };
        let meta = [when, views(v.views)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
        let offline = self.offline_state(&v.id);
        let progress = match v.duration {
            Some(d) if d > 0 && v.position > 0.0 => (v.position / d as f64).clamp(0.02, 1.0) as f32,
            _ => 0.0,
        };
        VideoItem {
            id: v.id.clone().into(),
            title: self.shown_title(v).into(),
            channel: v.channel_name.clone().into(),
            channel_id: v.channel_id.clone().into(),
            thumb_src: src.unwrap_or_default().into(),
            meta: meta.into(),
            has_thumb: thumb.is_some(),
            thumb: thumb.unwrap_or_default(),
            new: !v.seen,
            progress,
            duration: v.duration.map(|d| fmt_time(d as f64)).unwrap_or_default().into(),
            reason: self.reasons.get(&v.id).cloned().unwrap_or_default().into(),
            kind: 0,
            later: self.later.contains(&v.id),
            offline: offline.0,
            dl_text: offline.1.into(),
        }
    }

    fn show_feed(&mut self, ui: &App) {
        self.feed = self.db.feed(300).unwrap_or_default();
        for v in &self.feed {
            self.pics.want(&format!("v:{}", v.id), &v.thumbnail, 480);
            self.known.insert(v.id.clone(), v.clone());
        }
        let todo: Vec<(String, i64)> = self.feed.iter().map(|v| (v.id.clone(), v.published)).collect();
        self.want_branding(&todo);
        let items: Vec<VideoItem> = self.feed.iter().map(|v| self.video_item(v)).collect();
        self.feed_model.set_vec(items);
        self.show_feed_sub(ui);
    }

    /// The top of the feed is where clicks happen: look those up ahead of time.
    /// The click helper warms up on a video that always works (the newest in the
    /// feed may be a live stream or a premiere, which fail slowly and warm nothing).
    fn prefetch_newest(&mut self) {
        let ids: Vec<String> = self.feed.iter().take(12).map(|v| v.id.clone()).collect();
        if !self.click_helper_warm {
            self.click_helper_warm = true;
            self.warming = true;
            self.resolver.urgent(WARM_UP);
        }
        self.want_prefetch_page(ids, true);
    }

    fn show_feed_sub(&self, ui: &App) {
        let new = self.feed.iter().filter(|v| !v.seen).count();
        let mut parts = vec![];
        if new > 0 {
            parts.push(format!("{new} new"));
        }
        match (self.refreshing, self.last_refresh) {
            (true, _) => parts.push("checking for new videos…".into()),
            (false, Some(t)) => parts.push(format!("updated {}", ago(linen_core::now() - t.elapsed().as_secs() as i64))),
            _ => {}
        }
        ui.global::<Linen>().set_feed_sub(parts.join(" · ").into());
    }

    fn show_channels(&mut self, ui: &App) {
        self.channels = self.db.channels().unwrap_or_default();
        let latest: HashMap<&str, i64> = self.feed.iter().fold(HashMap::new(), |mut m, v| {
            let e = m.entry(v.channel_id.as_str()).or_insert(0);
            *e = (*e).max(v.published);
            m
        });
        let items: Vec<ChannelItem> = self
            .channels
            .iter()
            .map(|c| {
                if let Some(a) = &c.avatar {
                    self.pics.want(&format!("a:{}", c.id), a, 96);
                }
                let avatar = self.cache.get(&format!("a:{}", c.id)).cloned();
                ChannelItem {
                    following: true,
                    id: c.id.clone().into(),
                    name: if c.name.is_empty() { c.id.clone().into() } else { c.name.clone().into() },
                    has_avatar: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                    sub: latest.get(c.id.as_str()).map(|t| format!("Last video {}", ago(*t))).unwrap_or_default().into(),
                }
            })
            .collect();
        ui.global::<Linen>().set_channels(ModelRc::new(VecModel::from(items)));
    }

    /// Check every channel's feed for new videos (in the background).
    fn refresh(&mut self, ui: &App) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        ui.global::<Linen>().set_refreshing(true);
        self.show_feed_sub(ui);
        let (tx, channels) = (self.tx.clone(), self.db.channels().unwrap_or_default());
        std::thread::spawn(move || {
            let (mut new, mut failed) = (0, 0);
            if let (Ok(db), Ok(http)) = (Db::open(), linen_core::http()) {
                // a few at a time: quick, without hammering YouTube
                for chunk in channels.chunks(6) {
                    let results: Vec<_> = std::thread::scope(|s| {
                        let hs: Vec<_> = chunk.iter().map(|c| (c, s.spawn(|| linen_core::feed::fetch(&http, &c.id)))).collect();
                        hs.into_iter().map(|(c, h)| (c, h.join())).collect()
                    });
                    for (c, r) in results {
                        match r {
                            Ok(Ok(f)) => new += db.store_feed(&c.id, Some(&f.channel_name), &f.videos).unwrap_or(0),
                            _ => {
                                failed += 1;
                                log::warn!("couldn't refresh {}", c.name);
                            }
                        }
                    }
                }
            }
            let _ = tx.send(Msg::Refreshed { new, failed });
        });
    }

    fn add_channel(&mut self, ui: &App, input: String) {
        let input = input.trim().to_string();
        if input.is_empty() {
            return;
        }
        let lib = ui.global::<Linen>();
        lib.set_adding(true);
        lib.set_subs_msg("Looking it up…".into());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = (|| -> Result<String> {
                let http = linen_core::http()?;
                let mut c = linen_core::channel::resolve(&http, &input)?;
                let feed = linen_core::feed::fetch(&http, &c.id)?;
                if c.name.is_empty() {
                    c.name = feed.channel_name.clone();
                }
                if c.avatar.is_none() {
                    c.avatar = linen_core::channel::avatar(&http, &c.id);
                }
                let db = Db::open()?;
                db.add_channel(&c)?;
                db.store_feed(&c.id, Some(&feed.channel_name), &feed.videos)?;
                // a new channel's back catalogue isn't "new"
                db.mark_all_seen_for(&c.id)?;
                Ok(c.name)
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Added(res));
        });
    }

    fn import_takeout(&mut self, ui: &App) {
        let tx = self.tx.clone();
        ui.global::<Linen>().set_subs_msg("Pick your subscriptions.csv (Takeout → YouTube → subscriptions)".into());
        std::thread::spawn(move || {
            let Some(file) = rfd::FileDialog::new().add_filter("Takeout subscriptions", &["csv"]).pick_file() else {
                let _ = tx.send(Msg::Imported(Err("No file picked.".into())));
                return;
            };
            let res = (|| -> Result<usize> {
                let channels = linen_core::takeout::parse_csv(&std::fs::read_to_string(file)?);
                let db = Db::open()?;
                for c in &channels {
                    db.add_channel(c)?;
                }
                // pictures for the imported channels, one by one in the background
                let todo: Vec<String> = channels.iter().map(|c| c.id.clone()).collect();
                std::thread::spawn(move || {
                    let (Ok(http), Ok(db)) = (linen_core::http(), Db::open()) else { return };
                    for id in todo {
                        if let Some(a) = linen_core::channel::avatar(&http, &id) {
                            let _ = db.set_avatar(&id, &a);
                        }
                    }
                });
                Ok(channels.len())
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Imported(res));
        });
    }

    fn fresh_streams(&self, id: &str) -> Option<resolver::Streams> {
        self.streams.get(id).filter(|(_, t)| t.elapsed() < STREAMS_FRESH).map(|(s, _)| s.clone())
    }

    /// Find a video's streams in the background: a hovered card goes first.
    fn want_prefetch(&mut self, id: &str) {
        if self.fresh_streams(id).is_some() || self.is_prefetching(id) || self.pending_open.as_deref() == Some(id) {
            return;
        }
        self.prefetch_queue.retain(|q| q != id);
        self.prefetch_queue.push_front(id.to_string());
        self.prefetch_queue.truncate(PREFETCH_QUEUE);
        self.pump_prefetch();
    }

    /// A page showed these. `now`: it's on screen, so they go first; otherwise
    /// (a list in a tab you're not looking at) after everything else.
    fn want_prefetch_page(&mut self, ids: impl IntoIterator<Item = String>, now: bool) {
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| self.fresh_streams(id).is_none() && !self.is_prefetching(id) && !self.upcoming.contains_key(id))
            .collect();
        self.prefetch_queue.retain(|q| !ids.contains(q));
        if now {
            for id in ids.into_iter().rev() {
                self.prefetch_queue.push_front(id);
            }
        } else {
            self.prefetch_queue.extend(ids);
        }
        self.prefetch_queue.truncate(PREFETCH_QUEUE);
        self.pump_prefetch();
    }

    fn is_prefetching(&self, id: &str) -> bool {
        self.prefetching.iter().any(|p| p.0 == id)
    }

    pub fn pump_prefetch(&mut self) {
        // one that never answered (helper trouble): stop waiting for it
        self.prefetching.retain(|p| p.1.elapsed() < Duration::from_secs(60));
        // a click is waiting on its lookup: don't compete with it
        if self.pending_open.is_some() && self.play_at.is_none() {
            return;
        }
        // gentle with YouTube: at most PREFETCH_LIMIT lookups in 10 minutes
        while self.prefetch_times.front().is_some_and(|t| t.elapsed() > Duration::from_secs(600)) {
            self.prefetch_times.pop_front();
        }
        if self.prefetch_times.len() >= PREFETCH_LIMIT {
            return;
        }
        while self.prefetching.len() < resolver::PREFETCH_PARALLEL && self.prefetch_times.len() < PREFETCH_LIMIT {
            let Some(id) = self.prefetch_queue.pop_front() else { break };
            if self.fresh_streams(&id).is_none() && !self.is_prefetching(&id) {
                self.resolver.prefetch(&id);
                self.prefetch_times.push_back(Instant::now());
                self.prefetching.push((id, Instant::now()));
            }
        }
    }

    fn on_resolved(&mut self, ui: &App, id: String, res: Result<resolver::Streams, String>) {
        if id == WARM_UP {
            self.warming = false;
        }
        self.prefetching.retain(|p| p.0 != id);
        match &res {
            Ok(st) => {
                self.streams.insert(id.clone(), (st.clone(), Instant::now()));
                // kept for after a restart too (a first click then needs no lookup)
                if id != WARM_UP {
                    let expires = st.expires().unwrap_or(linen_core::now() + 3 * 3600);
                    let q = QUALITY.load(std::sync::atomic::Ordering::Relaxed);
                    let _ = self.db.save_links(&id, &st.to_json(), q, expires);
                }
                // it was scheduled and now it's out
                if self.upcoming.remove(&id).is_some() {
                    let _ = self.db.clear_upcoming(&id);
                    self.refresh_cards(ui);
                }
                // connections to its servers, ready for a click
                if let Some(r) = &self.relay {
                    let urls = std::iter::once(st.video.clone()).chain(st.audio.clone()).collect();
                    r.warm(urls, st.headers.clone());
                }
            }
            Err(e) => match upcoming_from(e) {
                // a scheduled stream or premiere: say when, on its card and page
                Some(when) => {
                    log::info!("{id} is scheduled: {when}");
                    let _ = self.db.set_upcoming(&id, &when);
                    self.upcoming.insert(id.clone(), when);
                    self.refresh_cards(ui);
                }
                None => log::warn!("couldn't find streams for {id}: {e}"),
            },
        }
        if self.pending_open.as_deref() == Some(id.as_str()) && self.watching.as_deref() == Some(id.as_str()) {
            self.pending_open = None;
            match res {
                Ok(st) => self.play_when_ready(&id, &st),
                Err(_) if self.upcoming.contains_key(&id) => self.show_upcoming(ui, &id),
                // last resort: let mpv ask yt-dlp itself
                Err(_) => self.play_url(&id),
            }
        }
        self.pump_prefetch();
    }

    /// The desktop's "now playing", from what the Watch page shows.
    fn update_media_info(&mut self, ui: &App) {
        let Some(id) = self.watching.clone() else { return };
        let w = ui.global::<Watch>();
        let (title, channel, dur) = (w.get_title().to_string(), w.get_channel().to_string(), self.duration);
        if let Some(m) = self.media.as_mut() {
            m.now_playing(&id, &title, &channel, dur);
        }
    }

    /// Play/pause/skip from the keyboard's media keys or the desktop's player widgets.
    fn on_media(&mut self, ui: &App, c: media::Command) {
        if self.watching.is_none() {
            return;
        }
        match c {
            media::Command::Toggle => cmd(self.mpv, "cycle", &["pause"]),
            media::Command::Play => {
                let _ = self.mpv.set_property("pause", false);
            }
            media::Command::Pause => {
                let _ = self.mpv.set_property("pause", true);
            }
            media::Command::Stop => self.stop_video(ui, self.mpv),
            media::Command::SeekBy(s) => seek(self.mpv, &[&format!("{s}"), "relative+keyframes"]),
            media::Command::SeekTo(t) => seek(self.mpv, &[&format!("{t}"), "absolute+keyframes"]),
            media::Command::Raise => {}
        }
    }

    /// A scheduled video on the Watch page: when it starts, instead of an error.
    fn show_upcoming(&self, ui: &App, id: &str) {
        let Some(when) = self.upcoming.get(id) else { return };
        let w = ui.global::<Watch>();
        w.set_has_video(false);
        w.set_ad_left(0);
        w.set_status(format!("Not out yet. This is scheduled: {}.", when.to_lowercase()).into());
    }

    fn start_option(&self, id: &str) -> String {
        let pos = self.db.position(id).unwrap_or(0.0);
        if pos > 15.0 { format!("start={}", pos - 3.0) } else { "start=0".into() }
    }

    /// Play now, or as soon as YouTube allows these links.
    fn play_when_ready(&mut self, id: &str, st: &resolver::Streams) {
        match st.available_at {
            Some(t) if t > linen_core::now() as u64 => {
                log::info!("{id}: YouTube allows these links in {} s", t - linen_core::now() as u64);
                self.play_at = Some((id.to_string(), t));
                self.ad_wait_total = (t - linen_core::now() as u64) as i32;
            }
            _ => {
                self.play_at = None;
                self.play_streams(id, st);
            }
        }
    }

    fn play_streams(&self, id: &str, st: &resolver::Streams) {
        self.load_streams(id, st);
        let _ = self.mpv.set_property("cache-secs", 36000.0f64);
        let _ = self.mpv.set_property("pause", false);
    }

    /// Load a video without starting it.
    fn load_streams(&self, id: &str, st: &resolver::Streams) {
        self.load_streams_at(id, st, None);
    }

    /// The same, from `at` seconds (else where you left off). Audio only: just the sound stream.
    fn load_streams_at(&self, id: &str, st: &resolver::Streams, at: Option<f64>) {
        // the same headers yt-dlp used, or YouTube answers 403 for some videos
        cmd(self.mpv, "change-list", &["http-header-fields", "clr", ""]);
        for (k, v) in &st.headers {
            if k.eq_ignore_ascii_case("user-agent") {
                let _ = self.mpv.set_property("user-agent", v.as_str());
            } else {
                cmd(self.mpv, "change-list", &["http-header-fields", "append", &format!("{k}: {v}")]);
            }
        }
        log::debug!(
            "headers for mpv: ua={:?} fields={:?}",
            self.mpv.get_property::<String>("user-agent").ok(),
            self.mpv.get_property::<String>("http-header-fields").ok()
        );
        let _ = self.mpv.set_property("pause", true);
        // through the relay when it's running (YouTube slows one long download down)
        let url = match (&self.relay, st.audio.as_ref().filter(|_| self.audio_only)) {
            (Some(r), Some(a)) => r.add(a, &st.headers),
            (None, Some(a)) => a.clone(),
            (Some(r), None) => st.mpv_url_via(r),
            (None, None) => st.mpv_url(),
        };
        let start = match at {
            Some(t) => format!("start={t:.2}"),
            None => self.start_option(id),
        };
        // a stream with sound and picture together: just don't show the picture
        let opts = format!("{start},vid={}", if self.audio_only && st.audio.is_none() { "no" } else { "auto" });
        cmd(self.mpv, "loadfile", &[&url, "replace", "-1", &opts]);
    }

    /// The mouse has rested on a card: load that video paused, so a click starts it
    /// at once. Only while nothing else is loaded, and only ~10 s of it is fetched.
    fn maybe_preload(&mut self, ui: &App) {
        let Some((id, since)) = self.hover.clone() else { return };
        if since.elapsed() < Duration::from_millis(600)
            || self.watching.is_some()
            || ui.global::<Linen>().get_page() == 2
            || self.preload.as_ref().is_some_and(|p| p.0 == id)
        {
            return;
        }
        let Some(st) = self.fresh_streams(&id) else { return };
        // YouTube's hold (the length of the ad): not before it's over
        if st.available_at.is_some_and(|t| t > linen_core::now() as u64) {
            return;
        }
        log::debug!("preloading {id}");
        let _ = self.mpv.set_property("cache-secs", 10.0f64);
        self.load_streams(&id, &st);
        self.preload = Some((id, false));
        self.hover = None;
    }

    fn play_url(&self, id: &str) {
        // mpv asks yt-dlp itself here, with its own headers
        cmd(self.mpv, "change-list", &["http-header-fields", "clr", ""]);
        let url = format!("https://www.youtube.com/watch?v={id}");
        let opts = format!("{},vid={}", self.start_option(id), if self.audio_only { "no" } else { "auto" });
        cmd(self.mpv, "loadfile", &[&url, "replace", "-1", &opts]);
        let _ = self.mpv.set_property("pause", false);
    }

    fn open_video(&mut self, ui: &App, id: &str, mpv: &'static libmpv2::Mpv) {
        // a video we haven't listed (a link in a description): its details fill in shortly
        let v = self.known.get(id).cloned().unwrap_or_else(|| pages::bare_video(id));
        self.save_position(mpv);
        let w = ui.global::<Watch>();
        self.want_branding(&[(v.id.clone(), v.published)]);
        self.show_watch_title(ui, &v.id, &v.title.clone());
        w.set_channel(v.channel_name.clone().into());
        w.set_channel_id(v.channel_id.clone().into());
        w.set_following(!v.channel_id.is_empty() && self.db.is_following(&v.channel_id).unwrap_or(false));
        w.set_subs("".into());
        let avatar = self.cache.get(&format!("a:{}", v.channel_id)).cloned();
        w.set_has_channel_avatar(avatar.is_some());
        w.set_channel_avatar(avatar.unwrap_or_default());
        let when = if v.published > 0 { ago(v.published) } else { String::new() };
        self.watch_meta = vec![views(v.views), when];
        self.watch_votes = None;
        w.set_meta("".into());
        w.set_rating(self.db.rating(&v.id));
        self.show_watch_meta(ui);
        self.segments.clear();
        self.undone.clear();
        self.last_skipped = None;
        self.skip_shown_at = None;
        w.set_skip_msg("".into());
        w.set_segments(ModelRc::default());
        self.fetch_calm(&v.id);
        let shown = self.shown_title(&v);
        if let Some(m) = self.media.as_mut() {
            m.now_playing(&v.id, &shown, &v.channel_name, v.duration.unwrap_or(0) as f64);
        }
        self.tracking = None;
        self.fetch_tracking(&v.id);
        w.set_description(v.description.clone().into());
        w.set_chapters(ModelRc::default());
        w.set_marks(ModelRc::default());
        w.set_chapter("".into());
        self.chapters.clear();
        self.chapter_shown.clear();
        w.set_later(self.later.contains(&v.id));
        w.set_links(ModelRc::default());
        self.watch_links.clear();
        let _ = self.db.watched(&linen_core::db::Watched {
            id: v.id.clone(),
            title: v.title.clone(),
            channel_id: v.channel_id.clone(),
            channel_name: v.channel_name.clone(),
            duration: v.duration.unwrap_or(0) as f64,
            position: self.db.position(&v.id).unwrap_or(0.0),
            furthest: 0.0,
            watched_at: linen_core::now(),
        });
        self.reset_comments(ui, &v.id);
        self.show_watch_saved(ui);
        self.fetch_details(&v.id);
        w.set_has_video(false);
        w.set_status("Loading…".into());
        w.set_progress(0.0);
        w.set_pos_text("0:00".into());
        let poster = self.cache.get(&format!("v:{}", v.id)).cloned();
        if poster.is_none() {
            self.pics.want(&format!("v:{}", v.id), &v.thumbnail, 480);
        }
        w.set_has_poster(poster.is_some());
        w.set_poster(poster.unwrap_or_default());
        self.go(ui, 2);
        let preloaded = self.preload.take().filter(|p| p.0 == v.id);
        if preloaded.is_none() {
            cmd(mpv, "stop", &[]);
        }
        self.watching = Some(v.id.clone());
        self.show_watch_download(ui);
        self.duration = 0.0;
        self.opened_at = Some(Instant::now());
        self.fell_back = false;
        w.set_ad_left(0);
        self.link_retries = 0;
        self.retry_at = None;
        // streams found ahead of time: play right away; otherwise find them now
        // (on the helper that's kept free for clicks)
        self.play_at = None;
        let offline = self.offline_file(&v.id);
        if let Some(path) = offline {
            // downloaded: from the file (no waiting, works without a connection)
            self.pending_open = None;
            self.play_file(&v.id, &path);
        } else if self.upcoming.contains_key(&v.id) {
            self.pending_open = None;
            self.show_upcoming(ui, &v.id);
        } else if let Some((_, started)) = preloaded {
            // already loaded while you hovered: just start it
            self.pending_open = None;
            let _ = mpv.set_property("cache-secs", 36000.0f64);
            let _ = mpv.set_property("pause", false);
            if started {
                w.set_status("".into());
                w.set_has_video(true);
                self.duration = mpv.get_property::<f64>("duration").unwrap_or(0.0);
                w.set_dur_text(fmt_time(self.duration).into());
                self.show_segments(ui);
                self.opened_at = None;
                log::info!("click to first frame: instant (loaded while hovering)");
            }
        } else if let Some(st) = self.fresh_streams(&v.id) {
            self.pending_open = None;
            self.play_when_ready(&v.id, &st);
        } else {
            self.pending_open = Some(v.id.clone());
            if !self.is_prefetching(&v.id) {
                self.resolver.urgent(&v.id);
                // the click helper is still warming up: whichever helper answers first wins
                if self.warming {
                    self.resolver.race(&v.id);
                }
            }
        }
    }

    fn save_position(&self, mpv: &libmpv2::Mpv) {
        let Some(id) = &self.watching else { return };
        if self.duration > 0.0 {
            let _ = self.db.set_duration(id, self.duration);
        }
        if let Ok(p) = mpv.get_property::<f64>("time-pos") {
            // finished (or nearly): start over next time (but remember it was watched)
            if self.duration > 0.0 && p > self.duration - 20.0 {
                let _ = self.db.set_position(id, self.duration);
                let _ = self.db.set_position(id, 0.0);
            } else {
                let _ = self.db.set_position(id, p);
            }
        }
    }

    /// Done with the video on the Watch page: save where you were and stop it.
    fn stop_video(&mut self, ui: &App, mpv: &libmpv2::Mpv) {
        let Some(id) = self.watching.clone() else { return };
        self.track_progress();
        self.tracking = None;
        self.save_position(mpv);
        if mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 60.0 {
            // you watched something: For you can learn from it
            self.foryou_dirty = true;
        }
        let _ = mpv.set_property("pause", true);
        cmd(mpv, "stop", &[]);
        self.watching = None;
        self.pending_open = None;
        self.play_at = None;
        let w = ui.global::<Watch>();
        w.set_fullscreen(false);
        w.set_has_video(false);
        w.set_mini(false);
        if let Some(m) = self.media.as_mut() {
            m.stopped();
        }
        w.set_skip_msg("".into());
        w.set_ad_left(0);
        self.segments.clear();
        // progress bars on the cards
        self.refresh_progress_of(&id);
    }
}

/// Stream links found in an earlier run that still work, as if just looked up
/// (their age counts toward STREAMS_FRESH).
fn saved_links(db: &Db) -> HashMap<String, (resolver::Streams, Instant)> {
    let q = QUALITY.load(std::sync::atomic::Ordering::Relaxed);
    let now = linen_core::now();
    let out: HashMap<_, _> = db
        .links(q)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(id, data, found)| {
            let st = resolver::Streams::from_json(&data)?;
            let at = Instant::now().checked_sub(Duration::from_secs((now - found).max(0) as u64))?;
            Some((id, (st, at)))
        })
        .collect();
    if !out.is_empty() {
        log::info!("{} videos' links kept from before", out.len());
    }
    out
}

impl State {
    /// Settings > Quality. Videos opened from now on (and downloads) use it.
    fn set_quality(&mut self, ui: &App, q: i32) {
        let q = q.clamp(0, 2);
        QUALITY.store(q, std::sync::atomic::Ordering::Relaxed);
        let _ = self.db.set_setting("quality", &q.to_string());
        ui.global::<Settings>().set_quality(q);
        self.resolver.set_format(format());
        let _ = self.mpv.set_property("ytdl-format", format());
        // links found ahead of time were for the old quality
        self.streams.clear();
        self.streams = saved_links(&self.db);
        log::info!("quality: {}", ["auto", "best", "data saver"][q as usize]);
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let ui = App::new()?;
    slint::set_xdg_app_id("linen")?;

    let quality = Db::open()?.setting("quality").and_then(|q| q.parse().ok()).unwrap_or(0).clamp(0, 2);
    QUALITY.store(quality, std::sync::atomic::Ordering::Relaxed);
    ui.global::<Settings>().set_quality(quality);
    let video = Rc::new(RefCell::new(linen_video::Video::new(&Options {
        display_fps: display_fps(),
        ytdl_path: ytdlp::path().to_string_lossy().into_owned(),
        ytdl_format: format().into(),
        ytdl_raw: match ytdlp::js_runtime() {
            Some(js) => {
                log::info!("yt-dlp will use {js}");
                Some(format!("js-runtimes={js}"))
            }
            None => {
                log::warn!("no JavaScript runtime (deno/node/bun/quickjs): some videos may fail to load");
                None
            }
        },
    })?));
    let mpv = video.borrow().mpv();
    log::info!("display refresh: {:.1} Hz", display_fps());
    // the volume you chose last time (slider 0..1; loudness follows its square)
    let volume: f32 = Db::open()?.setting("volume").and_then(|v| v.parse().ok()).unwrap_or(0.8f32).clamp(0.0, 1.0);
    ui.global::<Watch>().set_volume(volume);
    let _ = mpv.set_property("volume", (volume * volume * 100.0) as f64);

    let (tx, rx) = mpsc::channel::<Msg>();
    {
        let tx = tx.clone();
        linen_video::watch(mpv, move |e| {
            let _ = tx.send(Msg::Player(e));
        });
    }
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Ytdlp(ytdlp::ensure().map_err(|e| format!("{e:#}"))));
        });
    }
    let pics = {
        let tx = tx.clone();
        images::Loader::start(move |d| {
            let _ = tx.send(Msg::Picture(d));
        })
    };

    let js = ytdlp::js_runtime();
    let resolver = {
        let tx = tx.clone();
        let (atx, arx) = mpsc::channel::<resolver::Answer>();
        std::thread::spawn(move || {
            for a in arx {
                if tx.send(Msg::Resolved(a)).is_err() {
                    break;
                }
            }
        });
        resolver::Resolver::start(&ytdlp::path().to_string_lossy(), format(), js.as_deref(), atx)
    };
    let feed_model = Rc::new(VecModel::<VideoItem>::default());
    ui.global::<Linen>().set_feed(ModelRc::from(feed_model.clone()));
    let state = Rc::new(RefCell::new(State {
        db: Db::open()?,
        tx: tx.clone(),
        pics,
        cache: HashMap::new(),
        feed: vec![],
        feed_model,
        channels: vec![],
        refreshing: false,
        last_refresh: None,
        watching: None,
        duration: 0.0,
        feed_seen_at: None,
        resolver,
        streams: saved_links(&Db::open()?),
        pending_open: None,
        prefetching: vec![],
        prefetch_queue: Default::default(),
        prefetch_times: Default::default(),
        click_helper_warm: false,
        warming: false,
        mpv,
        opened_at: None,
        fell_back: false,
        link_retries: 0,
        retry_at: None,
        ad_wait_total: 0,
        upcoming: Db::open()?.upcoming().unwrap_or_default().into_iter().collect(),
        relay: proxy::Proxy::start(),
        media: media::Media::start(tx.clone()),
        hover: None,
        preload: None,
        play_at: None,
        yt: std::sync::Arc::new(linen_core::yt::Yt::new()?),
        known: HashMap::new(),
        reasons: HashMap::new(),
        foryou: pages::List::new(),
        results: pages::List::new(),
        ch_list: pages::List::new(),
        pl_list: pages::List::new(),
        req: [0; 3],
        query: String::new(),
        results_more: None,
        found_channels: vec![],
        ch_id: String::new(),
        ch_tab: 0,
        ch_more: None,
        ch_info: None,
        pl_id: String::new(),
        pl_more: None,
        nav: vec![],
        foryou_busy: false,
        foryou_dirty: false,
        watch_links: vec![],
        prefs: calm::Prefs::load(&Db::open()?),
        branding: HashMap::new(),
        branding_asked: Default::default(),
        branding_jobs: calm::start_branding(tx.clone()),
        segments: vec![],
        undone: Default::default(),
        skip_shown_at: None,
        last_skip: None,
        last_skipped: None,
        watch_votes: None,
        watch_meta: vec![],
        watch_original: String::new(),
        account: None,
        tracking: None,
        home: pages::List::new(),
        home_more: None,
        home_req: 0,
        just_imported: Default::default(),
        pending_follows: Default::default(),
        suggestions: vec![],
        identities: vec![],
        identity_profile: None,
        later: Default::default(),
        later_list: pages::List::new(),
        dl_list: pages::List::new(),
        my_lists: pages::List::new(),
        my_playlist: None,
        save_video: String::new(),
        downloads: HashMap::new(),
        dl_queue: Default::default(),
        audio_only: Db::open()?.flag("audio-only", false),
        chapters: vec![],
        chapter_shown: String::new(),
        thread: Default::default(),
    }));
    {
        let mut s = state.borrow_mut();
        s.show_feed(&ui);
        s.show_channels(&ui);
        // the saved feed is likely where the first click goes: start on it now
        s.prefetch_newest();
        s.refresh(&ui);
        s.attach_lists(&ui);
        s.prefs.show(&ui);
        s.show_foryou(&ui);
        s.reconnect(&ui);
        s.start_onboarding(&ui);
        s.load_library(&ui);
        s.show_playlists(&ui);
        ui.global::<Watch>().set_audio_only(s.audio_only);
    }

    // video frames: mpv draws into an OpenGL texture right before each UI frame
    {
        let video = video.clone();
        let weak = ui.as_weak();
        ui.window().set_rendering_notifier(move |state, _api| {
            let Some(ui) = weak.upgrade() else { return };
            match state {
                slint::RenderingState::BeforeRendering => {
                    let mut v = video.borrow_mut();
                    if !v.ready() {
                        use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
                        let wl = ui.window().window_handle().display_handle().ok().and_then(|d| match d.as_raw() {
                            RawDisplayHandle::Wayland(w) => Some(w.display.as_ptr() as *const std::ffi::c_void),
                            _ => None,
                        });
                        let redraw = ui.as_weak();
                        let on_frame = move || {
                            let w = redraw.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = w.upgrade() {
                                    ui.window().request_redraw();
                                }
                            });
                        };
                        if let Err(e) = unsafe { v.init_gl(wl, on_frame) } {
                            log::error!("video output: {e:#}");
                            return;
                        }
                    }
                    // set up from the start (a hovered video can load before you click it);
                    // frames are only drawn on the Watch page
                    if ui.global::<Linen>().get_page() != 2 && !ui.global::<Watch>().get_mini() {
                        return;
                    }
                    let scale = ui.window().scale_factor();
                    let (w, h) = ((ui.get_video_w() * scale) as u32, (ui.get_video_h() * scale) as u32);
                    let t = Instant::now();
                    let out = unsafe { v.render(w, h) };
                    if std::env::var_os("LINEN_STATS").is_some() {
                        thread_local!(static FT: RefCell<(Instant, u32, f64, f64)> = RefCell::new((Instant::now(), 0, 0.0, 0.0)));
                        FT.with(|f| {
                            let mut f = f.borrow_mut();
                            let ms = t.elapsed().as_secs_f64() * 1000.0;
                            f.1 += 1;
                            f.2 += ms;
                            f.3 = f.3.max(ms);
                            if f.0.elapsed() >= Duration::from_secs(5) {
                                log::info!("frames: {} drawn in 5 s · mpv part avg {:.1} ms, max {:.1} ms · {}x{}", f.1, f.2 / f.1 as f64, f.3, w, h);
                                *f = (Instant::now(), 0, 0.0, 0.0);
                            }
                        });
                    }
                    if let Some((tex, w, h, fresh)) = out {
                        if fresh {
                            let img = unsafe {
                                slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(tex, (w, h).into())
                                    .origin(slint::BorrowedOpenGLTextureOrigin::TopLeft)
                                    .build()
                            };
                            ui.global::<Watch>().set_frame(img);
                        }
                    }
                }
                slint::RenderingState::AfterRendering => video.borrow().report_swap(),
                _ => {}
            }
        })?;
    }

    // callbacks
    macro_rules! on {
        ($global:ident, $cb:ident, |$s:ident, $ui:ident $(, $arg:ident)*| $body:expr) => {{
            let st = state.clone();
            let weak = ui.as_weak();
            ui.global::<$global>().$cb(move |$($arg),*| {
                let Some($ui) = weak.upgrade() else { return };
                #[allow(unused_mut)]
                let mut $s = st.borrow_mut();
                $body;
            });
        }};
    }
    on!(Linen, on_refresh, |s, ui| s.refresh(&ui));
    on!(Linen, on_open_video, |s, ui, id| s.open_video(&ui, &id, mpv));
    on!(Linen, on_hover_video, |s, ui, id| {
        let _ = &ui;
        s.want_prefetch(&id);
        s.hover = Some((id.to_string(), Instant::now()));
    });
    on!(Linen, on_add_channel, |s, ui, input| s.add_channel(&ui, input.to_string()));
    on!(Linen, on_remove_channel, |s, ui, id| s.follow(&ui, id.to_string(), false));
    on!(Linen, on_import_takeout, |s, ui| s.import_takeout(&ui));
    on!(Watch, on_leave, |s, ui| s.back(&ui));
    on!(Linen, on_back, |s, ui| s.back(&ui));
    on!(Linen, on_nav, |s, ui, page| s.nav_root(&ui, page));
    on!(Linen, on_search, |s, ui, q| s.search(&ui, q.to_string()));
    on!(Linen, on_more_results, |s, ui| s.more_results(&ui));
    on!(Linen, on_open_channel, |s, ui, id| s.open_channel(&ui, id.to_string()));
    on!(Linen, on_ch_set_tab, |s, ui, tab| s.ch_set_tab(&ui, tab));
    on!(Linen, on_ch_load_more, |s, ui| s.ch_load_more(&ui));
    on!(Linen, on_open_playlist, |s, ui, id| s.open_playlist(&ui, id.to_string()));
    on!(Linen, on_pl_load_more, |s, ui| s.pl_load_more(&ui));
    on!(Linen, on_follow, |s, ui, id, on| s.follow(&ui, id.to_string(), on));
    on!(Linen, on_rebuild_foryou, |s, ui| s.rebuild_foryou(&ui));
    on!(Linen, on_not_interested, |s, ui, id| s.not_interested(&ui, &id));
    on!(Linen, on_hide_channel, |s, ui, id| s.hide_channel(&ui, &id));
    on!(Watch, on_open_link, |s, ui, i| s.open_link(&ui, i as usize));
    on!(Watch, on_undo_skip, |s, ui| s.undo_skip(&ui));
    on!(Watch, on_expand, |s, ui| s.expand(&ui));
    on!(Watch, on_close_mini, |s, ui| s.stop_video(&ui, mpv));
    on!(Watch, on_rate, |s, ui, r| s.rate(&ui, r));
    on!(Watch, on_toggle_later, |s, ui| {
        if let Some(id) = s.watching.clone() {
            s.toggle_later(&ui, &id);
        }
    });
    on!(Watch, on_download, |s, ui| {
        if let Some(id) = s.watching.clone() {
            s.download_button(&ui, &id);
        }
    });
    on!(Watch, on_set_audio_only, |s, ui, on| s.set_audio_only(&ui, on));
    on!(Watch, on_show_comments, |s, ui| s.open_comments(&ui));
    on!(Watch, on_save, |s, ui| s.open_save(&ui, ""));
    on!(Linen, on_open_save, |s, ui, id| s.open_save(&ui, &id));
    on!(Linen, on_toggle_save, |s, ui, id| s.toggle_save(&ui, &id));
    on!(Linen, on_new_playlist, |s, ui, name| s.new_playlist(&ui, &name));
    on!(Linen, on_rename_playlist, |s, ui, name| s.rename_playlist(&ui, &name));
    on!(Linen, on_delete_playlist, |s, ui| s.delete_playlist(&ui));
    on!(Linen, on_remove_from_playlist, |s, ui, id| s.remove_from_playlist(&ui, &id));
    on!(Watch, on_sort_comments, |s, ui, newest| s.sort_comments(&ui, newest));
    on!(Watch, on_more_comments, |s, ui| s.more_comments(&ui));
    on!(Watch, on_toggle_replies, |s, ui, id| s.toggle_replies(&ui, &id));
    on!(Watch, on_more_replies, |s, ui, id| s.more_replies(&ui, &id));
    on!(Linen, on_toggle_later, |s, ui, id| s.toggle_later(&ui, &id));
    on!(Linen, on_delete_download, |s, ui, id| s.delete_download(&ui, &id));
    on!(Linen, on_open_downloads_folder, |s, ui| {
        let _ = &ui;
        s.open_downloads_folder();
    });
    {
        let st = state.clone();
        ui.global::<Watch>().on_chapter_at(move |f| st.try_borrow().map(|s| s.chapter_name_at(f as f64 * s.duration)).unwrap_or_default().into());
        let st = state.clone();
        ui.global::<Watch>().on_time_at(move |f| st.try_borrow().map(|s| fmt_time(f as f64 * s.duration)).unwrap_or_default().into());
    }
    on!(Account, on_connect, |s, ui| s.connect(&ui));
    on!(Account, on_disconnect, |s, ui| s.disconnect(&ui));
    on!(Account, on_choose, |s, ui, i| s.choose_identity(&ui, i as usize));
    on!(Account, on_open_firefox, |s, ui| {
        let _ = &ui;
        s.open_firefox();
    });
    on!(Account, on_push_only_here, |s, ui| s.push_only_here(&ui));
    on!(Onboard, on_find, |s, ui, q| s.onboard_find(&ui, q.to_string()));
    on!(Onboard, on_finish, |s, ui| s.finish_onboarding(&ui));
    on!(Linen, on_load_home, |s, ui, more| s.load_home(&ui, more));
    on!(Linen, on_tab_shown, |s, ui, tab| {
        let _ = &ui;
        s.tab_shown(tab);
    });
    on!(Settings, on_set, |s, ui, key, on| s.set_setting(&ui, &key, on));
    on!(Settings, on_set_quality, |s, ui, q| s.set_quality(&ui, q));
    // skips land on the nearest keyframe (like YouTube's own player): playback
    // starts as soon as its first bytes arrive, instead of after downloading and
    // decoding up to the exact second
    ui.global::<Watch>().on_seek_to(move |t| seek(mpv, &[&format!("{t}"), "absolute+keyframes"]));
    ui.global::<Watch>().on_toggle(move || cmd(mpv, "cycle", &["pause"]));
    ui.global::<Watch>().on_seek(move |f| seek(mpv, &[&format!("{}", f * 100.0), "absolute-percent+keyframes"]));
    ui.global::<Watch>().on_seek_by(move |secs| seek(mpv, &[&format!("{secs}"), "relative+keyframes"]));
    {
        let st = state.clone();
        ui.global::<Watch>().on_set_volume(move |v| {
            let _ = mpv.set_property("volume", (v * v * 100.0) as f64);
            // remembered for next time (a drag sends many; each write is tiny)
            if let Ok(s) = st.try_borrow() {
                let _ = s.db.set_setting("volume", &format!("{v:.3}"));
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.global::<Watch>().on_cursor_hidden(move |hide| {
            use slint::winit_030::WinitWindowAccessor;
            log::debug!("pointer {}", if hide { "hidden" } else { "shown" });
            if let Some(ui) = weak.upgrade() {
                ui.window().with_winit_window(|w| w.set_cursor_visible(!hide));
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.global::<Watch>().on_set_fullscreen(move |on| {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_fullscreen(on);
            }
        });
    }

    // the UI thread's clock: messages, position, saving, periodic refresh
    let timer = slint::Timer::default();
    {
        let st = state.clone();
        let weak = ui.as_weak();
        let mut ticks = 0u32;
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut s = st.borrow_mut();
            let (lib, w) = (ui.global::<Linen>(), ui.global::<Watch>());
            let mut pictures = false;
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Ytdlp(Ok(line)) => log::info!("{line}"),
                    Msg::Ytdlp(Err(e)) => log::error!("yt-dlp: {e}"),
                    Msg::Picture(d) => {
                        s.cache.insert(d.key.clone(), to_image(&d));
                        pictures = true;
                    }
                    Msg::Resolved((id, res)) => s.on_resolved(&ui, id, res),
                    Msg::Browse(req, b) => s.on_browse(&ui, req, b),
                    Msg::Details(id, d) => s.on_details(&ui, &id, d),
                    Msg::ForYou(r) => s.on_foryou(&ui, r),
                    Msg::Branding(id, b) => {
                        s.on_branding(&ui, id, b);
                        pictures = true;
                    }
                    Msg::Segments(id, segs) => s.on_segments(&ui, &id, segs),
                    Msg::Votes(id, v) => s.on_votes(&ui, &id, v),
                    Msg::Account(r) => s.on_account(&ui, r),
                    Msg::Identities(r) => s.on_identities(&ui, r),
                    Msg::YtSubs(who, r) if s.is_current_account(&who) => s.on_yt_subs(&ui, r),
                    Msg::YtHistory(who, r) if s.is_current_account(&who) => s.on_yt_history(&ui, r),
                    Msg::YtSubs(..) | Msg::YtHistory(..) => log::info!("dropped a sync for a previous account"),
                    Msg::Tracking(id, t) => s.on_tracking(&ui, id, t),
                    Msg::Home(req, r, more) => s.on_home(&ui, req, r, more),
                    Msg::Suggestions(q, found) => s.on_suggestions(&ui, q, found),
                    Msg::Pushed(n) => s.on_pushed(&ui, n),
                    Msg::Media(c) => s.on_media(&ui, c),
                    Msg::YtLater(who, r) if s.is_current_account(&who) => s.on_yt_later(&ui, r),
                    Msg::YtLater(..) => {}
                    Msg::YtPlaylists(who, r) if s.is_current_account(&who) => s.on_yt_playlists(&ui, r),
                    Msg::YtPlaylists(..) => {}
                    Msg::PlaylistMade(name, v, r) => s.on_playlist_made(&ui, &name, v, r),
                    Msg::Download(id, e) => s.on_download(&ui, id, e),
                    Msg::Filled(h) => s.on_filled(&ui, h),
                    Msg::Comments(v, ask, res) => s.on_comments(&ui, &v, ask, res),
                    Msg::Refreshed { new, failed } => {
                        log::info!("refresh: {new} new videos, {failed} channels failed");
                        s.refreshing = false;
                        s.last_refresh = Some(Instant::now());
                        s.after_import();
                        lib.set_refreshing(false);
                        s.show_feed(&ui);
                        s.show_channels(&ui);
                        s.prefetch_newest();
                        // an old (or no) For you list: make a new one now the feed is fresh
                        if s.foryou_stale() {
                            s.rebuild_foryou(&ui);
                        }
                    }
                    Msg::Added(Ok(name)) => {
                        lib.set_adding(false);
                        lib.set_subs_msg(format!("Following {name}.").into());
                        s.show_feed(&ui);
                        s.show_channels(&ui);
                        s.refresh_following(&ui);
                        let mut pending = std::mem::take(&mut s.pending_follows);
                        pending.retain(|id| !s.db.is_following(id).unwrap_or(false));
                        s.pending_follows = pending;
                        if lib.get_page() == 7 {
                            s.show_suggestions(&ui);
                        }
                    }
                    Msg::Added(Err(e)) => {
                        lib.set_adding(false);
                        lib.set_subs_msg(e.clone().into());
                        log::warn!("follow: {e}");
                        s.pending_follows.clear();
                        s.refresh_following(&ui);
                        if lib.get_page() == 7 {
                            s.show_suggestions(&ui);
                        }
                    }
                    Msg::Imported(Ok(n)) => {
                        lib.set_subs_msg(format!("Imported {n} channels. Fetching their videos…").into());
                        s.show_channels(&ui);
                        s.refresh(&ui);
                    }
                    Msg::Imported(Err(e)) => lib.set_subs_msg(e.into()),
                    Msg::Player(Event::Title(_)) => {}
                    Msg::Player(Event::Duration(d)) => {
                        s.duration = d;
                        s.show_chapter_marks(&ui);
                        s.update_media_info(&ui);
                        w.set_dur_text(fmt_time(d).into());
                        s.show_segments(&ui);
                    }
                    Msg::Player(Event::Paused(p)) => {
                        w.set_paused(p);
                        if s.watching.is_some() {
                            let pos = mpv.get_property::<f64>("time-pos").unwrap_or(0.0);
                            if let Some(m) = s.media.as_mut() {
                                m.state(!p, pos);
                            }
                        }
                    }
                    Msg::Player(Event::Buffering(b)) => w.set_buffering(b),
                    Msg::Player(Event::Loading) => w.set_status("Loading…".into()),
                    // a hovered video got its first frame, paused, behind the page
                    Msg::Player(Event::Started) if s.watching.is_none() => {
                        if let Some(p) = s.preload.as_mut() {
                            p.1 = true;
                        }
                    }
                    // the hovered video couldn't load: nothing to show, just forget it
                    Msg::Player(Event::Ended(Some(_))) if s.watching.is_none() => s.preload = None,
                    Msg::Player(Event::Started) => {
                        if let Some(t) = SEEK_AT.with(|s| s.take()) {
                            log::info!("skip took {:.2} s (to {:.0})", t.elapsed().as_secs_f32(), mpv.get_property::<f64>("time-pos").unwrap_or(-1.0));
                        }
                        w.set_status("".into());
                        w.set_has_video(true);
                        s.track_started();
                        if let Some(t) = s.opened_at.take() {
                            log::info!("click to first frame: {:.1} s", t.elapsed().as_secs_f32());
                        }
                        let p = |n: &str| mpv.get_property::<String>(n).unwrap_or_else(|_| "-".into());
                        log::info!("playing: {} {}x{} hwdec={}", p("video-codec"), p("width"), p("height"), p("hwdec-current"));
                    }
                    Msg::Player(Event::Ended(err)) => {
                        // YouTube often refuses links for their first seconds: try the same
                        // ones again shortly (a new lookup would just get new fresh links)
                        if err.is_some() && !s.fell_back && s.pending_open.is_none() && s.link_retries < 3 {
                            if let Some(id) = s.watching.clone().filter(|id| s.fresh_streams(id).is_some()) {
                                s.link_retries += 1;
                                let wait = [1.5, 3.0, 5.0][s.link_retries as usize - 1];
                                log::info!("links for {id} refused; trying again in {wait} s");
                                s.retry_at = Some(Instant::now() + Duration::from_secs_f64(wait));
                                w.set_status("Starting…".into());
                                continue;
                            }
                        }
                        // our stream links failed: forget them, let mpv look it up itself
                        if err.is_some() && !s.fell_back && s.pending_open.is_none() {
                            if let Some(id) = s.watching.clone() {
                                log::info!("streams for {id} failed; asking mpv's yt-dlp instead");
                                s.streams.remove(&id);
                                let _ = s.db.forget_links(&id);
                                s.fell_back = true;
                                s.play_url(&id);
                                continue;
                            }
                        }
                        if let Some(e) = err {
                            w.set_status(e.into());
                            w.set_has_video(false);
                        }
                    }
                }
            }
            if pictures {
                s.pictures_arrived(&ui);
                s.account_pictures(&ui);
                if lib.get_page() == 1 {
                    s.show_channels(&ui);
                }
            }
            // start a video whose links YouTube now allows
            if let Some((id, t)) = s.play_at.clone() {
                if s.watching.as_deref() != Some(id.as_str()) {
                    s.play_at = None;
                } else if linen_core::now() as u64 >= t {
                    s.play_at = None;
                    w.set_status("".into());
                    w.set_ad_left(0);
                    if let Some(st) = s.fresh_streams(&id) {
                        s.play_streams(&id, &st);
                    }
                } else {
                    // YouTube holds the video for as long as its ad would have run
                    let left = (t - linen_core::now() as u64) as i32;
                    w.set_ad_total(s.ad_wait_total.max(left));
                    w.set_ad_left(left.max(1));
                }
            }
            // SponsorBlock: skip marked parts as they start
            if s.watching.is_some() {
                s.check_skip(&ui);
            }
            if ticks % 4 == 0 {
                s.maybe_preload(&ui);
            }
            if s.retry_at.is_some_and(|t| Instant::now() >= t) {
                s.retry_at = None;
                if let Some(id) = s.watching.clone() {
                    if let Some(st) = s.fresh_streams(&id) {
                        s.play_streams(&id, &st);
                    }
                }
            }
            // a small window: fewer words on the Watch page's buttons
            if ticks % 10 == 0 {
                let width = ui.window().size().width as f32 / ui.window().scale_factor();
                if (width < 1240.0) != w.get_narrow() {
                    w.set_narrow(width < 1240.0);
                }
            }
            // carry on with lookups held back by the limit
            if ticks % 40 == 0 {
                s.pump_prefetch();
            }
            ticks += 1;
            // position, 4x a second while playing
            if ticks % 5 == 0 && s.watching.is_some() && !w.get_paused() {
                if let Ok(p) = mpv.get_property::<f64>("time-pos") {
                    w.set_pos_text(fmt_time(p).into());
                    s.show_chapter_at(&ui, p);
                    if s.duration > 0.0 {
                        w.set_progress((p / s.duration).clamp(0.0, 1.0) as f32);
                    }
                }
            }
            // developer aid: LINEN_CACHE=1 logs the buffer every 5 s
            if ticks % 100 == 0 && s.watching.is_some() && std::env::var_os("LINEN_CACHE").is_some() {
                let f = |n: &str| mpv.get_property::<f64>(n).unwrap_or(-1.0);
                let speed = mpv.get_property::<i64>("cache-speed").unwrap_or(-1);
                log::info!(
                    "cache: limit {:.0} s · ahead {:.1} s · speed {:.0} kbit/s · pos {:.0} · waiting {}",
                    f("cache-secs"),
                    f("demuxer-cache-duration"),
                    speed as f64 * 8.0 / 1000.0,
                    f("time-pos"),
                    mpv.get_property::<bool>("paused-for-cache").unwrap_or(false)
                );
            }
            // developer aid: LINEN_STATS=1 logs dropped/late frames every 10 s
            if ticks % 200 == 0 && lib.get_page() == 2 && std::env::var_os("LINEN_STATS").is_some() {
                let g = |n: &str| mpv.get_property::<i64>(n).unwrap_or(-1);
                log::info!(
                    "stats: decoder drops {} · vo drops {} · late frames {} · mistimed {}",
                    g("decoder-frame-drop-count"),
                    g("frame-drop-count"),
                    g("vo-delayed-frame-count"),
                    g("mistimed-frame-count")
                );
            }
            // remember where you are every 5 s
            if ticks % 100 == 0 && s.watching.is_some() {
                s.save_position(mpv);
                // and the desktop's player widgets, where we are
                let (playing, pos) = (!w.get_paused(), mpv.get_property::<f64>("time-pos").unwrap_or(0.0));
                if let Some(m) = s.media.as_mut() {
                    m.state(playing, pos);
                }
            }
            // and tell YouTube every 30 s (resume on your other devices)
            if ticks % 600 == 0 && s.watching.is_some() && !w.get_paused() {
                s.track_progress();
            }
            // the feed's been on screen a few seconds: its new videos are now seen
            if lib.get_page() == 0 && lib.get_feed_tab() == 0 && s.feed.iter().any(|v| !v.seen) {
                let t = *s.feed_seen_at.get_or_insert_with(Instant::now);
                if t.elapsed() > Duration::from_secs(4) {
                    let _ = s.db.mark_all_seen();
                    for v in &mut s.feed {
                        v.seen = true;
                    }
                    s.feed_seen_at = None;
                }
            } else if lib.get_page() != 0 || lib.get_feed_tab() != 0 {
                s.feed_seen_at = None;
            }
            // check for new videos every half hour
            if s.last_refresh.is_some_and(|t| t.elapsed() > REFRESH_EVERY) {
                s.refresh(&ui);
                s.sync_account();
            }
            // you've watched something since For you was made: freshen it (not too often)
            if ticks % 20 == 0 && s.foryou_dirty && !s.foryou_busy && s.foryou_age() > 10 * 60 && s.watching.is_none() {
                s.foryou_dirty = false;
                s.rebuild_foryou(&ui);
            }
        });
    }

    // developer aid: LINEN_PAGE=<n> starts on that page (screenshots)
    if let Some(p) = std::env::var("LINEN_PAGE").ok().and_then(|p| p.parse().ok()) {
        ui.global::<Linen>().set_page(p);
    }
    // developer aids (screenshots): LINEN_TAB=1 (For you), LINEN_SEARCH=<words>, LINEN_CHANNEL=<id>
    if let Some(t) = std::env::var("LINEN_TAB").ok().and_then(|p| p.parse().ok()) {
        ui.global::<Linen>().set_feed_tab(t);
    }
    // LINEN_ONBOARD=<step> shows the welcome at that step; LINEN_CONNECT=1 connects right away
    if let Some(step) = std::env::var("LINEN_ONBOARD").ok().and_then(|p| p.parse().ok()) {
        let _ = state.borrow().db.set_flag("onboarded", false);
        state.borrow_mut().start_onboarding(&ui);
        ui.global::<Onboard>().set_step(step);
    }
    if std::env::var_os("LINEN_CONNECT").is_some() {
        state.borrow_mut().connect(&ui);
    }
    // LINEN_LATER=<id,id> saves those for later; LINEN_DOWNLOAD=<id> downloads it; LINEN_LIBTAB=1 downloads tab
    for id in std::env::var("LINEN_LATER").unwrap_or_default().split(',').filter(|i| !i.is_empty()) {
        let mut s = state.borrow_mut();
        if !s.later.contains(id) {
            s.toggle_later(&ui, id);
        }
    }
    if let Ok(id) = std::env::var("LINEN_DOWNLOAD") {
        state.borrow_mut().download(&ui, &id);
    }
    // LINEN_PLAYLIST=<id>: open that playlist
    if let Ok(p) = std::env::var("LINEN_PLAYLIST") {
        state.borrow_mut().open_playlist(&ui, p);
    }
    // LINEN_SAVE=<id>: the Save box for that video
    if let Ok(v) = std::env::var("LINEN_SAVE") {
        state.borrow_mut().open_save(&ui, &v);
    }
    if let Some(t) = std::env::var("LINEN_LIBTAB").ok().and_then(|p| p.parse().ok()) {
        ui.global::<Linen>().set_lib_tab(t);
    }
    if let Ok(q) = std::env::var("LINEN_SEARCH") {
        state.borrow_mut().search(&ui, q);
    }
    if let Ok(id) = std::env::var("LINEN_CHANNEL") {
        state.borrow_mut().open_channel(&ui, id);
    }
    // developer aid: LINEN_SEEKS="25=120,35=60,45=600" skips (to seconds) at those times
    let _seek_timers: Vec<slint::Timer> = std::env::var("LINEN_SEEKS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| p.split_once('='))
        .filter_map(|(at, to)| Some((at.parse::<u64>().ok()?, to.to_string())))
        .map(|(at, to)| {
            let t = slint::Timer::default();
            t.start(slint::TimerMode::SingleShot, Duration::from_secs(at), move || {
                log::info!("skipping to {to}");
                let mode = std::env::var("LINEN_SEEK_MODE").unwrap_or_else(|_| "absolute+keyframes".into());
                seek(mpv, &[&to, &mode]);
            });
            t
        })
        .collect();
    // developer aid: LINEN_AUDIO_SWITCH=<secs> flips audio only then
    let _audio_timer = std::env::var("LINEN_AUDIO_SWITCH").ok().and_then(|d| d.parse().ok()).map(|at: u64| {
        let (st, weak) = (state.clone(), ui.as_weak());
        let t = slint::Timer::default();
        t.start(slint::TimerMode::SingleShot, Duration::from_secs(at), move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut s = st.borrow_mut();
            log::info!("audio only {} at {:.1}", !s.audio_only, mpv.get_property::<f64>("time-pos").unwrap_or(-1.0));
            let on = !s.audio_only;
            s.set_audio_only(&ui, on);
        });
        t
    });
    // developer aid: LINEN_DELETE=<video id>@<secs> deletes that download then
    let _delete_timer = std::env::var("LINEN_DELETE").ok().and_then(|d| {
        let (id, at) = d.split_once('@')?;
        let (id, at) = (id.to_string(), at.parse::<u64>().ok()?);
        let (st, weak) = (state.clone(), ui.as_weak());
        let t = slint::Timer::default();
        t.start(slint::TimerMode::SingleShot, Duration::from_secs(at), move || {
            let Some(ui) = weak.upgrade() else { return };
            log::info!("deleting download {id}");
            st.borrow_mut().delete_download(&ui, &id);
        });
        Some(t)
    });
    // developer aid: LINEN_HOVER=<video id> rests the mouse on it after LINEN_HOVER_DELAY seconds
    let _hover_timer = std::env::var("LINEN_HOVER").ok().map(|id| {
        let delay = std::env::var("LINEN_HOVER_DELAY").ok().and_then(|d| d.parse().ok()).unwrap_or(0u64);
        let st = state.clone();
        let t = slint::Timer::default();
        t.start(slint::TimerMode::SingleShot, Duration::from_secs(delay), move || {
            let mut s = st.borrow_mut();
            s.want_prefetch(&id);
            s.hover = Some((id.clone(), Instant::now()));
        });
        t
    });
    // developer aid: LINEN_OPEN=<video id> opens it (after LINEN_OPEN_DELAY seconds)
    let _open_timer = std::env::var("LINEN_OPEN").ok().map(|id| {
        let delay = std::env::var("LINEN_OPEN_DELAY").ok().and_then(|d| d.parse().ok()).unwrap_or(0u64);
        let (st, weak) = (state.clone(), ui.as_weak());
        let t = slint::Timer::default();
        t.start(slint::TimerMode::SingleShot, Duration::from_secs(delay), move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut s = st.borrow_mut();
            s.open_video(&ui, &id, mpv);
        });
        t
    });

    ui.run()?;
    state.borrow().save_position(mpv);
    cmd(mpv, "quit", &[]);
    Ok(())
}
