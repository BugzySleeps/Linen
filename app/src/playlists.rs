//! Your playlists: made in Linen, or on your YouTube account (kept in sync when
//! connected; new ones are made there too). Saved playlists of other people
//! show up too, read only.

use linen_core::yt::{PlaylistHit, VideoHit};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::pages::Card;
use crate::{App, Linen, Msg, SaveItem, State, Watch};

/// Which list (pages.rs) holds the playlists in Library.
pub const MINE: usize = 7;

/// From YouTube: yours (id, name, videos), and saved ones (id, name, "13 videos", a video in it).
pub type YtLists = (Vec<(String, String, Vec<VideoHit>)>, Vec<(String, String, String, Option<String>)>);

/// Everything to fetch for the sync (runs on a worker thread).
pub fn fetch(yt: &linen_core::yt::Yt) -> anyhow::Result<YtLists> {
    let mut own = vec![];
    let mut saved = vec![];
    for (p, mine) in yt.my_playlists()? {
        if mine {
            let videos = yt.playlist_all(&p.id)?;
            own.push((p.id, p.title, videos));
        } else {
            saved.push((p.id, p.title, p.count, p.first_video));
        }
    }
    Ok((own, saved))
}

impl State {
    /// The Playlists tab in Library.
    pub fn show_playlists(&mut self, ui: &App) {
        let lists = self.db.playlists().unwrap_or_default();
        let cards = lists
            .iter()
            .map(|p| {
                let count = if p.saved && !p.count.is_empty() {
                    p.count.clone()
                } else if p.videos == 1 {
                    "1 video".into()
                } else {
                    format!("{} videos", p.videos)
                };
                Card::Playlist(PlaylistHit { id: p.id.clone(), title: p.name.clone(), count, first_video: p.first.clone(), thumb: None })
            })
            .collect();
        self.fill_list(MINE, cards);
        ui.global::<Linen>().set_playlists_count(lists.len() as i32);
        self.show_watch_saved(ui);
        if ui.global::<Linen>().get_save_open() {
            self.show_save_dialog(ui);
        }
    }

    /// Yours (made in Linen or on your account) open here; anything else from YouTube.
    pub fn is_my_playlist(&self, id: &str) -> bool {
        self.db.playlist(id).ok().flatten().is_some_and(|p| !p.saved)
    }

    pub fn open_my_playlist(&mut self, ui: &App, id: &str) {
        let Some(p) = self.db.playlist(id).ok().flatten() else { return };
        let videos = self.db.playlist_videos(id).unwrap_or_default();
        self.remember_hits(&videos);
        self.my_playlist = Some(id.to_string());
        let lib = ui.global::<Linen>();
        lib.set_pl_title(p.name.clone().into());
        lib.set_pl_by(
            format!(
                "{} · {}",
                if videos.len() == 1 { "1 video".to_string() } else { format!("{} videos", videos.len()) },
                if p.yt { "on your YouTube account" } else { "in Linen only" }
            )
            .into(),
        );
        lib.set_pl_msg("".into());
        lib.set_pl_more(false);
        lib.set_pl_loading(false);
        lib.set_pl_editable(true);
        self.fill_list(crate::pages::PLAYLIST, videos.iter().map(|v| Card::Video(v.id.clone())).collect());
        self.go(ui, 5);
    }

    /// A new playlist. When the Save box is open, the video goes in it straight away.
    pub fn new_playlist(&mut self, ui: &App, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let video = ui.global::<Linen>().get_save_open().then(|| self.save_video.clone()).filter(|v| !v.is_empty());
        if self.yt.signed_in() {
            // on your account too: its id comes from YouTube
            let (yt, tx, name, video) = (self.yt.clone(), self.tx.clone(), name.to_string(), video);
            ui.global::<Linen>().set_playlists_msg("Making the playlist on YouTube…".into());
            std::thread::spawn(move || {
                let res = yt.create_playlist(&name, video.as_deref()).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::PlaylistMade(name, video, res));
            });
        } else {
            let id = self.db.create_playlist(name, None).unwrap_or_default();
            if let Some(v) = video {
                let _ = self.db.add_to_playlist(&id, &self.hit_of(&v));
            }
            self.show_playlists(ui);
        }
    }

    pub fn on_playlist_made(&mut self, ui: &App, name: &str, video: Option<String>, res: Result<String, String>) {
        let lib = ui.global::<Linen>();
        let id = match res {
            Ok(id) => {
                lib.set_playlists_msg("".into());
                self.db.create_playlist(name, Some(&id)).unwrap_or(id)
            }
            Err(e) => {
                // still made, here
                log::warn!("youtube new playlist: {e}");
                lib.set_playlists_msg(format!("Couldn't make “{name}” on YouTube, so it's in Linen only for now.").into());
                self.db.create_playlist(name, None).unwrap_or_default()
            }
        };
        if let Some(v) = video {
            let _ = self.db.add_to_playlist(&id, &self.hit_of(&v));
        }
        self.show_playlists(ui);
    }

    /// The Save box (Watch page, or a card's hover button).
    pub fn open_save(&mut self, ui: &App, video: &str) {
        let video = if video.is_empty() { self.watching.clone().unwrap_or_default() } else { video.to_string() };
        if video.is_empty() {
            return;
        }
        self.save_video = video;
        ui.global::<Linen>().set_save_open(true);
        self.show_save_dialog(ui);
    }

    fn show_save_dialog(&self, ui: &App) {
        let has = self.db.playlists_with(&self.save_video).unwrap_or_default();
        let items: Vec<SaveItem> = self
            .db
            .playlists()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !p.saved)
            .map(|p| SaveItem {
                on: has.contains(&p.id),
                sub: if p.yt { "On YouTube".into() } else { "In Linen".into() },
                id: p.id.into(),
                name: p.name.into(),
            })
            .collect();
        ui.global::<Linen>().set_save_lists(ModelRc::new(VecModel::from(items)));
    }

    /// In the Save box: put the video in this playlist, or take it out.
    pub fn toggle_save(&mut self, ui: &App, playlist: &str) {
        let video = self.save_video.clone();
        let on = !self.db.playlists_with(&video).unwrap_or_default().contains(playlist);
        self.set_in_playlist(playlist, &video, on);
        self.show_save_dialog(ui);
        self.show_playlists(ui);
        self.refresh_open_playlist(ui, playlist);
    }

    pub fn remove_from_playlist(&mut self, ui: &App, video: &str) {
        let Some(pl) = self.my_playlist.clone() else { return };
        self.set_in_playlist(&pl, video, false);
        self.show_playlists(ui);
        self.refresh_open_playlist(ui, &pl);
    }

    fn set_in_playlist(&mut self, playlist: &str, video: &str, on: bool) {
        if on {
            let _ = self.db.add_to_playlist(playlist, &self.hit_of(video));
        } else {
            let _ = self.db.remove_from_playlist(playlist, video);
        }
        if self.on_youtube(playlist) {
            let (yt, pl, v) = (self.yt.clone(), playlist.to_string(), video.to_string());
            std::thread::spawn(move || {
                if let Err(e) = yt.set_in_playlist(&pl, &v, on) {
                    log::warn!("youtube playlist {pl} ({v}): {e:#}");
                }
            });
        }
    }

    fn on_youtube(&self, playlist: &str) -> bool {
        self.yt.signed_in() && self.db.playlist(playlist).ok().flatten().is_some_and(|p| p.yt)
    }

    /// The playlist page showing this one: show its new contents.
    fn refresh_open_playlist(&mut self, ui: &App, playlist: &str) {
        if self.my_playlist.as_deref() == Some(playlist) && ui.global::<Linen>().get_page() == 5 {
            let videos = self.db.playlist_videos(playlist).unwrap_or_default();
            self.fill_list(crate::pages::PLAYLIST, videos.iter().map(|v| Card::Video(v.id.clone())).collect());
            let lib = ui.global::<Linen>();
            let by = lib.get_pl_by().to_string();
            let rest = by.split_once(" · ").map(|r| r.1.to_string()).unwrap_or_default();
            let n = if videos.len() == 1 { "1 video".to_string() } else { format!("{} videos", videos.len()) };
            lib.set_pl_by(format!("{n} · {rest}").into());
        }
    }

    pub fn rename_playlist(&mut self, ui: &App, name: &str) {
        let (Some(pl), name) = (self.my_playlist.clone(), name.trim()) else { return };
        if name.is_empty() {
            return;
        }
        let _ = self.db.rename_playlist(&pl, name);
        ui.global::<Linen>().set_pl_title(name.into());
        if self.on_youtube(&pl) {
            let (yt, name) = (self.yt.clone(), name.to_string());
            std::thread::spawn(move || {
                if let Err(e) = yt.rename_playlist(&pl, &name) {
                    log::warn!("youtube rename playlist: {e:#}");
                }
            });
        }
        self.show_playlists(ui);
    }

    pub fn delete_playlist(&mut self, ui: &App) {
        let Some(pl) = self.my_playlist.take() else { return };
        if self.on_youtube(&pl) {
            let (yt, pl) = (self.yt.clone(), pl.clone());
            std::thread::spawn(move || {
                if let Err(e) = yt.delete_playlist(&pl) {
                    log::warn!("youtube delete playlist: {e:#}");
                }
            });
        }
        let _ = self.db.delete_playlist(&pl);
        self.show_playlists(ui);
        ui.global::<Linen>().set_lib_tab(2);
        self.go(ui, 8);
    }

    /// The sync's answer: YouTube is the truth for the playlists that are there.
    pub fn on_yt_playlists(&mut self, ui: &App, r: Result<YtLists, String>) {
        match r {
            Ok((own, saved)) => {
                let _ = self.db.sync_yt_playlists(&own, &saved);
                for (_, _, videos) in &own {
                    self.remember_hits(videos);
                }
                self.show_playlists(ui);
            }
            Err(e) => log::warn!("youtube playlists: {e}"),
        }
    }

    pub fn forget_yt_playlists(&mut self, ui: &App) {
        let _ = self.db.forget_yt_playlists();
        self.show_playlists(ui);
    }

    /// The Watch page's Save button reads "Saved" when it's in any playlist.
    pub fn show_watch_saved(&self, ui: &App) {
        if let Some(id) = &self.watching {
            ui.global::<Watch>().set_in_playlist(!self.db.playlists_with(id).unwrap_or_default().is_empty());
        }
    }
}
