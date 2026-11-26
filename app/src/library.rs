//! The Library page: Watch later (kept in sync with YouTube's when connected)
//! and downloads (videos saved to ~/Videos/Linen that play without a
//! connection). Also listening without the picture ("audio only").

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use linen_core::Db;
use linen_core::yt::VideoHit;
use slint::ComponentHandle;

use crate::pages::Card;
use crate::{App, Linen, Msg, State, Watch, cmd};

pub const LATER: usize = 5;
pub const DOWNLOADS: usize = 6;

/// News from a download running on its own thread.
pub enum Dl {
    Progress(f32),
    /// (file, size, what yt-dlp says the video is)
    Done(String, u64, Option<VideoHit>),
    Failed(String),
}

pub enum DlState {
    Waiting,
    Running { pct: f32, child: Arc<Mutex<Option<Child>>>, cancel: Arc<AtomicBool> },
    Done { path: String, bytes: u64 },
}

/// "312 MB"
fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b => format!("{} MB", (b + (1 << 19)) >> 20),
    }
}

/// Every file of a video in the downloads folder (finished or partial).
fn files_of(id: &str) -> Vec<std::path::PathBuf> {
    let tag = format!("[{id}]");
    std::fs::read_dir(linen_core::paths::downloads_dir())
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains(&tag))).collect())
        .unwrap_or_default()
}

impl State {
    /// What we know about a video, in the shape the lists store.
    pub fn hit_of(&self, id: &str) -> VideoHit {
        let v = self.known.get(id);
        let mut h = VideoHit { id: id.to_string(), ..Default::default() };
        if let Some(v) = v {
            h.title = v.title.clone();
            h.channel_id = v.channel_id.clone();
            h.channel_name = v.channel_name.clone();
            h.published = v.published;
            h.views = v.views;
            h.duration = v.duration;
        }
        // the Watch page may know more (a video opened from a link)
        if h.duration.is_none() && self.watching.as_deref() == Some(id) && self.duration > 0.0 {
            h.duration = Some(self.duration as u32);
        }
        h
    }

    /// Startup: what's saved.
    pub fn load_library(&mut self, ui: &App) {
        self.later = self.db.later_ids().unwrap_or_default();
        for d in self.db.downloads().unwrap_or_default() {
            let id = d.video.id.clone();
            if d.done && std::path::Path::new(&d.path).is_file() {
                self.downloads.insert(id, DlState::Done { path: d.path, bytes: d.bytes });
            } else {
                // stopped halfway when Linen closed: carry on (yt-dlp resumes its partial file)
                self.downloads.insert(id.clone(), DlState::Waiting);
                self.dl_queue.push_back(id);
            }
        }
        ui.global::<Linen>().set_downloads_folder(linen_core::paths::downloads_dir().to_string_lossy().replace(&home(), "~").into());
        self.show_later(ui);
        self.show_downloads(ui);
        self.pump_downloads();
    }

    // ---- Watch later ----

    pub fn toggle_later(&mut self, ui: &App, id: &str) {
        let on = !self.later.contains(id);
        if on {
            let hit = self.hit_of(id);
            let _ = self.db.add_later(&hit, false, false, linen_core::now());
            self.fill_in_later(&hit);
            self.later.insert(id.to_string());
        } else {
            let _ = self.db.remove_later(id);
            self.later.remove(id);
        }
        if self.yt.signed_in() {
            let (yt, hit, id) = (self.yt.clone(), self.hit_of(id), id.to_string());
            std::thread::spawn(move || match yt.set_watch_later(&id, on) {
                Ok(()) if on => {
                    if let Ok(db) = Db::open() {
                        let _ = db.add_later(&hit, true, false, linen_core::now());
                    }
                }
                Ok(()) => {}
                Err(e) => log::warn!("youtube watch later {id}: {e:#}"),
            });
        }
        self.later_changed(ui);
    }

    /// Saved before we knew what it was (a pasted link): ask YouTube.
    fn fill_in_later(&self, hit: &VideoHit) {
        if !hit.title.is_empty() {
            return;
        }
        let (yt, tx, id) = (self.yt.clone(), self.tx.clone(), hit.id.clone());
        std::thread::spawn(move || {
            if let Ok(d) = yt.details(&id) {
                let h = VideoHit { id, title: d.title, channel_id: d.channel_id, channel_name: d.channel_name, views: d.views, ..Default::default() };
                let _ = tx.send(Msg::Filled(h));
            }
        });
    }

    pub fn on_filled(&mut self, ui: &App, h: VideoHit) {
        if self.db.fill_saved(&h).unwrap_or(0) > 0 {
            if !self.known.get(&h.id).is_some_and(|v| !v.title.is_empty()) {
                self.known.remove(&h.id);
                self.remember_hits(std::slice::from_ref(&h));
            }
            self.show_later(ui);
            self.show_downloads(ui);
        }
    }

    fn later_changed(&mut self, ui: &App) {
        self.show_later(ui);
        self.refresh_cards(ui);
        if let Some(id) = &self.watching {
            ui.global::<Watch>().set_later(self.later.contains(id));
        }
    }

    pub fn show_later(&mut self, ui: &App) {
        let hits = self.db.later().unwrap_or_default();
        self.remember_hits(&hits);
        let cards = hits.iter().map(|h| Card::Video(h.id.clone())).collect();
        self.fill_list(LATER, cards);
        ui.global::<Linen>().set_later_count(hits.len() as i32);
    }

    pub fn on_yt_later(&mut self, ui: &App, r: Result<Vec<VideoHit>, String>) {
        let list = match r {
            Ok(l) => l,
            Err(e) => return log::warn!("youtube watch later: {e}"),
        };
        let ids: std::collections::HashSet<&str> = list.iter().map(|v| v.id.as_str()).collect();
        let was = self.db.later_on_youtube().unwrap_or_default();
        let now = linen_core::now();
        let mut new = 0;
        for (i, v) in list.iter().enumerate() {
            let fresh = !self.later.contains(&v.id);
            // YouTube's order, a moment in the past (things you save here go on top)
            let _ = self.db.add_later(v, true, fresh, now - 60 - i as i64);
            new += fresh as usize;
        }
        let mut gone = 0;
        for id in was.iter().filter(|id| !ids.contains(id.as_str())) {
            let _ = self.db.remove_later(id);
            gone += 1;
        }
        log::info!("youtube watch later: {} ({new} new here, {gone} removed)", list.len());
        self.later = self.db.later_ids().unwrap_or_default();
        self.later_changed(ui);
    }

    /// The account went: what came from its Watch later goes too.
    pub fn forget_later_imported(&mut self, ui: &App) {
        let _ = self.db.forget_later_imported();
        self.later = self.db.later_ids().unwrap_or_default();
        self.later_changed(ui);
    }

    // ---- downloads ----

    /// The Watch page's Download button: start, stop, or (when done) show it in the Library.
    pub fn download_button(&mut self, ui: &App, id: &str) {
        match self.downloads.get(id) {
            None => self.download(ui, id),
            Some(DlState::Done { .. }) => {
                ui.global::<Linen>().set_lib_tab(1);
                self.nav_root(ui, 8);
            }
            Some(_) => self.delete_download(ui, id),
        }
    }

    pub fn download(&mut self, ui: &App, id: &str) {
        if self.downloads.contains_key(id) {
            return;
        }
        let hit = self.hit_of(id);
        let _ = self.db.add_download(&hit, "");
        self.fill_in_later(&hit);
        self.downloads.insert(id.to_string(), DlState::Waiting);
        self.dl_queue.push_back(id.to_string());
        self.pump_downloads();
        self.downloads_changed(ui);
    }

    /// One download at a time (they share your connection with what you watch).
    pub fn pump_downloads(&mut self) {
        if self.downloads.values().any(|d| matches!(d, DlState::Running { .. })) {
            return;
        }
        while let Some(id) = self.dl_queue.pop_front() {
            if !matches!(self.downloads.get(&id), Some(DlState::Waiting)) {
                continue;
            }
            let child = Arc::new(Mutex::new(None));
            let cancel = Arc::new(AtomicBool::new(false));
            self.downloads.insert(id.clone(), DlState::Running { pct: 0.0, child: child.clone(), cancel: cancel.clone() });
            start_download(id, child, cancel, self.tx.clone());
            return;
        }
    }

    pub fn on_download(&mut self, ui: &App, id: String, e: Dl) {
        match e {
            Dl::Progress(p) => {
                if let Some(DlState::Running { pct, .. }) = self.downloads.get_mut(&id) {
                    // the cards redraw on whole percents
                    let changed = (p * 100.0) as i32 != (*pct * 100.0) as i32;
                    *pct = p;
                    if changed {
                        self.downloads_progress(ui, &id);
                    }
                }
                return;
            }
            Dl::Done(path, bytes, info) => {
                log::info!("downloaded {id}: {path} ({})", size(bytes));
                let _ = self.db.download_done(&id, &path, bytes);
                if let Some(h) = info {
                    let _ = self.db.fill_saved(&h);
                    if !self.known.contains_key(&id) {
                        self.remember_hits(std::slice::from_ref(&h));
                    }
                }
                self.downloads.insert(id.clone(), DlState::Done { path, bytes });
            }
            Dl::Failed(e) => {
                // deleted while it ran: already gone
                if matches!(self.downloads.get(&id), Some(DlState::Running { cancel, .. }) if cancel.load(Ordering::Relaxed)) || !self.downloads.contains_key(&id) {
                    self.downloads.remove(&id);
                } else {
                    log::warn!("download {id}: {e}");
                    self.downloads.remove(&id);
                    let _ = self.db.remove_download(&id);
                    ui.global::<Linen>().set_downloads_msg(format!("Couldn't download “{}”: {e}", self.hit_of(&id).title).into());
                }
            }
        }
        self.pump_downloads();
        self.downloads_changed(ui);
    }

    /// Stop it (if it's running) and delete its file.
    pub fn delete_download(&mut self, ui: &App, id: &str) {
        match self.downloads.remove(id) {
            Some(DlState::Running { child, cancel, .. }) => {
                cancel.store(true, Ordering::Relaxed);
                if let Some(c) = child.lock().unwrap().as_mut() {
                    let _ = c.kill();
                }
                // its thread deletes the partial files once yt-dlp has stopped
            }
            _ => {
                for f in files_of(id) {
                    let _ = std::fs::remove_file(f);
                }
            }
        }
        self.dl_queue.retain(|q| q != id);
        let _ = self.db.remove_download(id);
        self.pump_downloads();
        self.downloads_changed(ui);
    }

    /// The file to play instead of streaming, when it's downloaded.
    pub fn offline_file(&self, id: &str) -> Option<String> {
        match self.downloads.get(id) {
            Some(DlState::Done { path, .. }) if std::path::Path::new(path).is_file() => Some(path.clone()),
            _ => None,
        }
    }

    /// For the cards: 0 not downloaded, 1 on its way, 2 downloaded; and a short text.
    pub fn offline_state(&self, id: &str) -> (i32, String) {
        match self.downloads.get(id) {
            None => (0, String::new()),
            Some(DlState::Waiting) => (1, "Waiting".into()),
            Some(DlState::Running { pct, .. }) => (1, format!("{}%", (pct * 100.0) as i32)),
            Some(DlState::Done { bytes, .. }) => (2, size(*bytes)),
        }
    }

    fn downloads_progress(&mut self, ui: &App, id: &str) {
        self.refresh_cards(ui);
        self.show_watch_download(ui);
        let _ = id;
    }

    fn downloads_changed(&mut self, ui: &App) {
        self.show_downloads(ui);
        self.refresh_cards(ui);
        self.show_watch_download(ui);
    }

    pub fn show_watch_download(&self, ui: &App) {
        let w = ui.global::<Watch>();
        let Some(id) = &self.watching else { return };
        let (state, text) = self.offline_state(id);
        w.set_dl_state(state);
        w.set_dl_text(
            match state {
                0 => "Download".to_string(),
                1 if text == "Waiting" => "Waiting to download".to_string(),
                1 => format!("Downloading {text}"),
                _ => "Downloaded".to_string(),
            }
            .into(),
        );
    }

    pub fn show_downloads(&mut self, ui: &App) {
        let list = self.db.downloads().unwrap_or_default();
        let hits: Vec<VideoHit> = list.iter().map(|d| d.video.clone()).collect();
        self.remember_hits(&hits);
        let cards = hits.iter().map(|h| Card::Video(h.id.clone())).collect();
        self.fill_list(DOWNLOADS, cards);
        let total: u64 = self.downloads.values().map(|d| if let DlState::Done { bytes, .. } = d { *bytes } else { 0 }).sum();
        let done = self.downloads.values().filter(|d| matches!(d, DlState::Done { .. })).count();
        let lib = ui.global::<Linen>();
        lib.set_downloads_count(list.len() as i32);
        lib.set_downloads_sub(
            if done == 0 { String::new() } else { format!("{done} video{} · {}", if done == 1 { "" } else { "s" }, size(total)) }.into(),
        );
    }

    pub fn open_downloads_folder(&self) {
        let _ = Command::new("xdg-open").arg(linen_core::paths::downloads_dir()).spawn();
    }

    // ---- audio only ----

    /// Listen without the picture: only the sound is fetched (much less data).
    pub fn set_audio_only(&mut self, ui: &App, on: bool) {
        self.audio_only = on;
        let _ = self.db.set_flag("audio-only", on);
        ui.global::<Watch>().set_audio_only(on);
        let Some(id) = self.watching.clone() else { return };
        let mpv = self.mpv;
        let streamed = self.offline_file(&id).is_none() && !self.fell_back;
        match self.fresh_streams(&id).filter(|s| streamed && s.audio.is_some() && self.pending_open.is_none() && self.play_at.is_none()) {
            // streaming: load again with (or without) the picture, from where you are
            Some(st) => {
                let pos = mpv.get_property::<f64>("time-pos").unwrap_or(0.0);
                let paused = mpv.get_property::<bool>("pause").unwrap_or(false);
                self.load_streams_at(&id, &st, Some(pos));
                let _ = mpv.set_property("cache-secs", 36000.0f64);
                let _ = mpv.set_property("pause", paused);
            }
            // a downloaded file (or one mpv opened itself): just stop showing the picture
            None => {
                let _ = mpv.set_property("vid", if on { "no" } else { "auto" });
            }
        }
    }

    /// Play the downloaded file.
    pub fn play_file(&self, id: &str, path: &str) {
        cmd(self.mpv, "change-list", &["http-header-fields", "clr", ""]);
        let opts = format!("{},vid={}", self.start_option(id), if self.audio_only { "no" } else { "auto" });
        cmd(self.mpv, "loadfile", &[path, "replace", "-1", &opts]);
        let _ = self.mpv.set_property("cache-secs", 36000.0f64);
        let _ = self.mpv.set_property("pause", false);
    }
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Download with yt-dlp on its own thread: the same picture quality Linen plays,
/// with chapters and details kept in the file.
fn start_download(id: String, slot: Arc<Mutex<Option<Child>>>, cancel: Arc<AtomicBool>, tx: std::sync::mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        let send = |e: Dl| {
            let _ = tx.send(Msg::Download(id.clone(), e));
        };
        let dir = linen_core::paths::downloads_dir();
        let mut c = Command::new(crate::ytdlp::path());
        c.args(["--ignore-config", "--no-playlist", "--newline", "--progress", "--no-simulate"])
            .args(["-f", crate::format(), "--merge-output-format", "mkv", "--embed-metadata", "--embed-chapters"])
            .args(["--progress-template", "download:LINENPROG %(info.format_id)s %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s"])
            .args(["--print", "after_move:LINENFILE %(filepath)s"])
            .args(["--print", "after_move:LINENINFO %(title)j\t%(channel)j\t%(channel_id)j\t%(duration)j\t%(timestamp)j\t%(view_count)j"])
            .arg("-o")
            .arg(dir.join("%(title).90B [%(id)s].%(ext)s"));
        if let Some(js) = crate::ytdlp::js_runtime() {
            c.args(["--js-runtimes", &js]);
        }
        c.arg("--").arg(&id).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
        let mut child = match c.spawn() {
            Ok(ch) => ch,
            Err(e) => return send(Dl::Failed(format!("couldn't start yt-dlp: {e}"))),
        };
        let (out, err) = (child.stdout.take(), child.stderr.take());
        *slot.lock().unwrap() = Some(child);
        // errors come on stderr; keep the last one to show
        let last_err = Arc::new(Mutex::new(String::new()));
        let err_reader = err.map(|e| {
            let last_err = last_err.clone();
            std::thread::spawn(move || {
                for l in BufReader::new(e).lines().map_while(Result::ok) {
                    if l.starts_with("ERROR") {
                        *last_err.lock().unwrap() = l.trim_start_matches("ERROR:").trim().to_string();
                    }
                }
            })
        });
        let mut file = None;
        let mut info = None;
        // picture first, then sound: the picture is ~9/10 of the bytes
        let mut parts: VecDeque<String> = VecDeque::new();
        let mut last = -1.0f32;
        if let Some(out) = out {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(p) = l.strip_prefix("LINENINFO ") {
                    info = parse_info(&id, p);
                } else if let Some(p) = l.strip_prefix("LINENFILE ") {
                    file = Some(p.trim().to_string());
                } else if let Some(p) = l.strip_prefix("LINENPROG ") {
                    let f: Vec<&str> = p.split_whitespace().collect();
                    let [fmt, got, total, est] = f[..] else { continue };
                    if !parts.iter().any(|x| x == fmt) {
                        parts.push_back(fmt.to_string());
                    }
                    let n = |s: &str| s.parse::<f64>().ok();
                    let Some(got) = n(got) else { continue };
                    let Some(total) = n(total).or(n(est)).filter(|t| *t > 0.0) else { continue };
                    let frac = (got / total).clamp(0.0, 1.0) as f32;
                    let pct = match parts.len() {
                        1 => frac * 0.9,
                        _ => 0.9 + frac * 0.1,
                    }
                    .min(0.99);
                    if pct - last >= 0.005 {
                        last = pct;
                        send(Dl::Progress(pct));
                    }
                }
            }
        }
        let status = slot.lock().unwrap().take().map(|mut c| c.wait());
        if let Some(r) = err_reader {
            let _ = r.join();
        }
        if cancel.load(Ordering::Relaxed) {
            for f in files_of(&id) {
                let _ = std::fs::remove_file(f);
            }
            return send(Dl::Failed("cancelled".into()));
        }
        let ok = status.is_some_and(|s| s.is_ok_and(|s| s.success()));
        match file.filter(|f| ok && std::path::Path::new(f).is_file()) {
            Some(f) => {
                let bytes = std::fs::metadata(&f).map_or(0, |m| m.len());
                send(Dl::Done(f, bytes, info));
            }
            None => {
                let e = last_err.lock().unwrap().clone();
                send(Dl::Failed(if e.is_empty() { "yt-dlp stopped".into() } else { e }));
            }
        }
    });
}

/// yt-dlp's details of a finished download: JSON values separated by tabs.
fn parse_info(id: &str, line: &str) -> Option<VideoHit> {
    let f: Vec<serde_json::Value> = line.split('\t').map(|x| serde_json::from_str(x).unwrap_or_default()).collect();
    let [title, channel, channel_id, duration, timestamp, views] = &f[..] else { return None };
    Some(VideoHit {
        id: id.to_string(),
        title: title.as_str()?.to_string(),
        channel_name: channel.as_str().unwrap_or_default().to_string(),
        channel_id: channel_id.as_str().unwrap_or_default().to_string(),
        duration: duration.as_f64().map(|d| d as u32),
        published: timestamp.as_i64().unwrap_or(0),
        views: views.as_u64(),
        ..Default::default()
    })
}
