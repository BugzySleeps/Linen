//! Your YouTube account in the app: connecting (Firefox's sign-in), keeping
//! subscriptions and watch history in sync both ways, likes, the YouTube Home
//! tab, and the first-start welcome.

use std::collections::HashSet;
use std::path::PathBuf;

use linen_core::db::Watched;
use linen_core::account::{Auth, Identity};
use linen_core::yt::{AccountInfo, ChannelHit, Page, Tracking, VideoHit};
use linen_core::{Channel, Db};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::{Account, App, ChannelItem, IdentityItem, Linen, Msg, Onboard, State, Watch};

/// Channels offered on the welcome screen (calm, broadly liked).
const SUGGESTED: &[(&str, &str)] = &[
    ("UCsXVk37bltHxD1rDPwtNM8Q", "Kurzgesagt – In a Nutshell"),
    ("UCHnyfMqiRRG1u-2MsSQLbXA", "Veritasium"),
    ("UCBa659QWEk1AI4Tg--mrJ2A", "Tom Scott"),
    ("UCsooa4yRKGN_zEE8iknghZA", "TED-Ed"),
    ("UCYO_jab_esuFRV4b17AJtAw", "3Blue1Brown"),
    ("UCY1kMZp36IQSyNx_9h4mpCg", "Mark Rober"),
    ("UC6107grRI4m0o2-emgoDnAA", "SmarterEveryDay"),
    ("UCsaGKqPZnGp_7N80hcHySGQ", "Tasting History with Max Miller"),
    ("UCy0tKL1T7wFoYcxCe0xjN6Q", "Technology Connections"),
    ("UCFhXFikryT4aFcLkLw2LBLA", "NileRed"),
    ("UC9RM-iSvTu1uPJb8X5yp3EQ", "Wendover Productions"),
    ("UCAL3JXZSzSm8AlZyD3nQdBA", "Primitive Technology"),
    ("UCoxcjq-8xIDTYp3uz647V5A", "Numberphile"),
    ("UCEOXxzW2vU0P-0THehuIIeg", "Captain Disillusion"),
    ("UCSMOQeBJ2RAnuFungnQOxLg", "Blender"),
];

pub const HOME: usize = 4;

/// The video being watched, as YouTube's history sees it.
pub struct WatchTrack {
    pub t: Tracking,
    /// watched up to here (last report)
    pub last: f64,
    pub started: bool,
}

impl State {
    // ---- connecting ----

    /// Startup: use the sign-in again if you connected before (the same account
    /// even if Firefox's order of signed-in accounts changed since).
    pub fn reconnect(&mut self, ui: &App) {
        let Some(profile) = self.db.setting("account.profile") else { return };
        let authuser: u32 = self.db.setting("account.authuser").and_then(|a| a.parse().ok()).unwrap_or(0);
        let page_id = self.db.setting("account.page");
        let handle = self.db.setting("account.handle").unwrap_or_default();
        let acc = ui.global::<Account>();
        acc.set_name(self.db.setting("account.name").unwrap_or_default().into());
        acc.set_busy(true);
        let (yt, tx) = (self.yt.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<(Option<Identity>, AccountInfo)> {
                let cookies = linen_core::account::read_firefox(std::path::Path::new(&profile))?;
                if !linen_core::account::signed_in(&cookies) {
                    anyhow::bail!("Firefox is no longer signed in to YouTube. Sign in there, then reconnect here.");
                }
                yt.set_auth(Some(Auth { cookies: cookies.clone(), authuser, page_id: page_id.clone() }));
                let info = yt.account()?;
                if handle.is_empty() || info.handle == handle {
                    return Ok((None, info));
                }
                // Firefox's accounts moved around: find yours again by its handle
                let ids = yt.identities()?;
                let Some(me) = ids.into_iter().find(|i| i.handle == handle) else {
                    anyhow::bail!("{handle} isn't signed in to YouTube in Firefox anymore. Sign in to it there, or pick another account.");
                };
                yt.set_auth(Some(Auth { cookies, authuser: me.authuser, page_id: me.page_id.clone() }));
                Ok((Some(me), yt.account()?))
            })();
            if r.is_err() {
                yt.set_auth(None);
            }
            let _ = tx.send(Msg::Account(r.map(|(id, a)| (None, id, a)).map_err(|e| format!("{e:#}"))));
        });
    }

    /// Find the sign-in and list who it can be; one: use it, several: you pick.
    pub fn connect(&mut self, ui: &App) {
        let acc = ui.global::<Account>();
        acc.set_busy(true);
        acc.set_msg("Looking for your YouTube sign-in in Firefox…".into());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<(PathBuf, Vec<Identity>)> {
                let (profile, cookies) = linen_core::account::find_sign_in()?;
                // a separate client: the one in use keeps its account while you choose
                let list = {
                    let lister = linen_core::yt::Yt::new()?;
                    lister.set_auth(Some(Auth { cookies, ..Default::default() }));
                    lister.identities()?
                };
                if list.is_empty() {
                    anyhow::bail!("Firefox is signed in, but YouTube didn't list any account. Open youtube.com in Firefox once, then try again.");
                }
                Ok((profile, list))
            })();
            let _ = tx.send(Msg::Identities(r.map_err(|e| format!("{e:#}"))));
        });
    }

    pub fn on_identities(&mut self, ui: &App, r: Result<(PathBuf, Vec<Identity>), String>) {
        let acc = ui.global::<Account>();
        acc.set_busy(false);
        match r {
            Ok((profile, list)) => {
                self.identity_profile = Some(profile);
                self.identities = list;
                if self.identities.len() == 1 && !self.yt.signed_in() {
                    return self.choose_identity(ui, 0);
                }
                // developer aid: LINEN_PICK=<n> picks for you (tests)
                if let Some(n) = std::env::var("LINEN_PICK").ok().and_then(|n| n.parse().ok()) {
                    return self.choose_identity(ui, n);
                }
                for (i, id) in self.identities.iter().enumerate() {
                    if let Some(a) = &id.avatar {
                        self.pics.want(&format!("id:{i}:{}", id.handle), a, 96);
                    }
                }
                acc.set_msg("".into());
                acc.set_choosing(true);
                self.show_identities(ui);
            }
            Err(e) => {
                log::warn!("youtube sign-in: {e}");
                acc.set_msg(e.into());
            }
        }
    }

    pub fn show_identities(&self, ui: &App) {
        let items: Vec<IdentityItem> = self
            .identities
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let avatar = self.cache.get(&format!("id:{i}:{}", id.handle)).cloned();
                IdentityItem {
                    name: id.name.clone().into(),
                    handle: id.handle.clone().into(),
                    byline: id.byline.clone().into(),
                    has_avatar: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                }
            })
            .collect();
        ui.global::<Account>().set_choices(ModelRc::new(VecModel::from(items)));
    }

    pub fn choose_identity(&mut self, ui: &App, i: usize) {
        let (Some(me), Some(profile)) = (self.identities.get(i).cloned(), self.identity_profile.clone()) else { return };
        let acc = ui.global::<Account>();
        acc.set_choosing(false);
        acc.set_busy(true);
        self.account = None;
        acc.set_msg(format!("Connecting {}…", if me.handle.is_empty() { &me.name } else { &me.handle }).into());
        // a different account than before: what the old one brought in goes
        if self.db.setting("account.handle").is_some_and(|h| h != me.handle) {
            self.forget_account_data(ui);
        }
        let (yt, tx) = (self.yt.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<AccountInfo> {
                let cookies = linen_core::account::read_firefox(&profile)?;
                yt.set_auth(Some(Auth { cookies, authuser: me.authuser, page_id: me.page_id.clone() }));
                yt.account()
            })();
            if r.is_err() {
                yt.set_auth(None);
            }
            let _ = tx.send(Msg::Account(r.map(|a| (Some(profile), Some(me), a)).map_err(|e| format!("{e:#}"))));
        });
    }

    /// Google's "add another account" page in Firefox (then "Look again" here).
    pub fn open_firefox(&self) {
        const ADD: &str = "https://accounts.google.com/AddSession?continue=https%3A%2F%2Fwww.youtube.com%2F";
        let ok = std::process::Command::new("firefox").arg(ADD).spawn().is_ok();
        if !ok {
            let _ = std::process::Command::new("xdg-open").arg(ADD).spawn();
        }
    }

    pub fn on_account(&mut self, ui: &App, r: Result<(Option<PathBuf>, Option<Identity>, AccountInfo), String>) {
        let acc = ui.global::<Account>();
        acc.set_busy(false);
        match r {
            Ok((profile, identity, info)) => {
                log::info!("youtube account connected");
                if let Some(p) = profile {
                    let _ = self.db.set_setting("account.profile", &p.to_string_lossy());
                }
                if let Some(me) = identity {
                    let _ = self.db.set_setting("account.authuser", &me.authuser.to_string());
                    match &me.page_id {
                        Some(p) => self.db.set_setting("account.page", p),
                        None => self.db.remove_setting("account.page"),
                    }
                    .ok();
                }
                let _ = self.db.set_setting("account.name", &info.name);
                let _ = self.db.set_setting("account.handle", &info.handle);
                acc.set_connected(true);
                acc.set_name(info.name.clone().into());
                acc.set_handle(info.handle.clone().into());
                acc.set_msg("Syncing your subscriptions and history…".into());
                acc.set_has_avatar(false);
                self.cache.remove("acct:me");
                if let Some(a) = &info.avatar {
                    self.pics.forget("acct:me");
                    self.pics.want("acct:me", a, 96);
                }
                self.account = Some(info);
                self.sync_account();
                if ui.global::<Linen>().get_feed_tab() == 2 {
                    self.load_home(ui, false);
                }
            }
            Err(e) => {
                log::warn!("youtube account: {e}");
                acc.set_connected(false);
                acc.set_msg(e.into());
            }
        }
    }

    pub fn disconnect(&mut self, ui: &App) {
        self.yt.set_auth(None);
        self.account = None;
        self.tracking = None;
        for k in ["account.profile", "account.name", "account.handle", "account.authuser", "account.page"] {
            let _ = self.db.remove_setting(k);
        }
        let gone = self.forget_account_data(ui);
        let acc = ui.global::<Account>();
        acc.set_connected(false);
        acc.set_has_avatar(false);
        acc.set_only_here(0);
        acc.set_msg(
            format!("Disconnected. The account's {gone} subscriptions and its history were removed from Linen; channels you followed here stay.").into(),
        );
        let lib = ui.global::<Linen>();
        if lib.get_feed_tab() == 2 {
            lib.set_feed_tab(0);
        }
        self.fill_home(Vec::new(), false);
    }

    /// Remove an account's imported subscriptions and history from the library.
    fn forget_account_data(&mut self, ui: &App) -> usize {
        let gone = self.db.forget_imported().unwrap_or(0);
        self.forget_later_imported(ui);
        self.forget_yt_playlists(ui);
        self.just_imported.clear();
        self.show_feed(ui);
        self.show_channels(ui);
        self.show_foryou(ui);
        self.foryou_dirty = true;
        gone
    }

    pub fn account_pictures(&self, ui: &App) {
        if let Some(a) = self.cache.get("acct:me") {
            let acc = ui.global::<Account>();
            acc.set_avatar(a.clone());
            acc.set_has_avatar(true);
        }
        if ui.global::<Linen>().get_page() == 7 {
            self.show_suggestions(ui);
        }
        if ui.global::<Account>().get_choosing() {
            self.show_identities(ui);
        }
    }

    // ---- keeping things in sync ----

    /// Subscriptions and history from YouTube (after connecting, and every half hour).
    pub fn sync_account(&self) {
        let Some(who) = self.account.as_ref().filter(|_| self.yt.signed_in()).map(|a| a.handle.clone()) else { return };
        let (yt, tx) = (self.yt.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let subs = yt.subscriptions().map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::YtSubs(who.clone(), subs));
            let hist = yt.history(3).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::YtHistory(who.clone(), hist));
            let later = yt.watch_later().map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::YtLater(who.clone(), later));
            let lists = crate::playlists::fetch(&yt).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::YtPlaylists(who, lists));
        });
    }

    pub fn is_current_account(&self, handle: &str) -> bool {
        self.account.as_ref().is_some_and(|a| a.handle == handle) && self.yt.signed_in()
    }

    /// Your YouTube subscriptions are the truth for channels that were on YouTube;
    /// ones followed only in Linen are kept (and offered to add to YouTube).
    pub fn on_yt_subs(&mut self, ui: &App, r: Result<Vec<ChannelHit>, String>) {
        let acc = ui.global::<Account>();
        let list = match r {
            Ok(l) => l,
            Err(e) => {
                log::warn!("youtube subscriptions: {e}");
                acc.set_msg("Couldn't sync subscriptions just now; trying again later.".into());
                return;
            }
        };
        let ids: HashSet<&str> = list.iter().map(|c| c.id.as_str()).collect();
        let was_on_youtube = self.db.on_youtube().unwrap_or_default();
        let (mut added, mut removed) = (0, 0);
        for c in &list {
            if !self.db.is_following(&c.id).unwrap_or(false) {
                let _ = self.db.add_channel(&Channel { id: c.id.clone(), name: c.name.clone(), avatar: c.avatar.clone() });
                let _ = self.db.mark_imported(true, &c.id);
                // their older videos aren't "new" in your feed
                self.just_imported.insert(c.id.clone());
                added += 1;
            }
            let _ = self.db.set_on_youtube(&c.id, true);
        }
        // unsubscribed on another device
        for id in was_on_youtube.iter().filter(|id| !ids.contains(id.as_str())) {
            let _ = self.db.remove_channel(id);
            removed += 1;
        }
        log::info!("youtube subscriptions: {} ({added} new here, {removed} removed)", list.len());
        acc.set_msg(format!("{} subscriptions in sync", list.len()).into());
        acc.set_only_here(self.db.only_here().map(|c| c.len() as i32).unwrap_or(0));
        self.show_channels(ui);
        if added > 0 || removed > 0 {
            self.show_feed(ui);
            if added > 0 {
                self.refresh(ui);
            }
        }
        ui.global::<Onboard>().set_followed(self.channels.len() as i32);
    }

    /// After the first refresh of channels that came from YouTube.
    pub fn after_import(&mut self) {
        for id in self.just_imported.drain() {
            let _ = self.db.mark_all_seen_for(&id);
        }
    }

    /// Videos you watched elsewhere: into the history (with where you stopped).
    pub fn on_yt_history(&mut self, ui: &App, r: Result<Vec<VideoHit>, String>) {
        let list = match r {
            Ok(l) => l,
            Err(e) => return log::warn!("youtube history: {e}"),
        };
        let known: HashSet<String> = self.db.history(5000).unwrap_or_default().into_iter().map(|h| h.id).collect();
        let now = linen_core::now();
        let mut new = 0;
        for (i, v) in list.iter().enumerate().filter(|(_, v)| !v.live) {
            let dur = v.duration.unwrap_or(0) as f64;
            let pct = v.watched_pct.unwrap_or(100) as f64;
            let furthest = dur * pct / 100.0;
            // finished: next time starts over
            let position = if pct >= 95.0 { 0.0 } else { furthest };
            if !known.contains(&v.id) {
                let _ = self.db.watched(&Watched {
                    id: v.id.clone(),
                    title: v.title.clone(),
                    channel_id: v.channel_id.clone(),
                    channel_name: v.channel_name.clone(),
                    duration: dur,
                    position,
                    furthest,
                    // newest first, all a little in the past
                    watched_at: now - 600 - i as i64 * 60,
                });
                let _ = self.db.mark_imported(false, &v.id);
                new += 1;
            } else if position > 0.0 && self.db.position(&v.id).unwrap_or(0.0) < 1.0 {
                let _ = self.db.set_position(&v.id, position);
            }
        }
        log::info!("youtube history: {} videos ({new} new here)", list.len());
        if new > 0 {
            self.foryou_dirty = true;
            self.show_feed(ui);
        }
    }

    /// Follow/unfollow also on YouTube (when connected).
    pub fn follow_on_youtube(&self, id: &str, on: bool) {
        if !self.yt.signed_in() {
            return;
        }
        let (yt, id) = (self.yt.clone(), id.to_string());
        std::thread::spawn(move || match yt.subscribe(&id, on) {
            Ok(()) => {
                if let Ok(db) = Db::open() {
                    // the channel row may still be on its way in; the next sync fixes it if so
                    let _ = db.set_on_youtube(&id, on);
                }
            }
            Err(e) => log::warn!("youtube {}subscribe {id}: {e:#}", if on { "" } else { "un" }),
        });
    }

    /// "Add them to YouTube": channels followed only in Linen, gently (one every 2 s).
    pub fn push_only_here(&mut self, ui: &App) {
        let todo = self.db.only_here().unwrap_or_default();
        if todo.is_empty() || !self.yt.signed_in() {
            return;
        }
        ui.global::<Account>().set_pushing(true);
        let (yt, tx) = (self.yt.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let db = Db::open().ok();
            let mut done = 0;
            for c in todo {
                match yt.subscribe(&c.id, true) {
                    Ok(()) => {
                        done += 1;
                        if let Some(db) = &db {
                            let _ = db.set_on_youtube(&c.id, true);
                        }
                    }
                    Err(e) => log::warn!("youtube subscribe {}: {e:#}", c.id),
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            let _ = tx.send(Msg::Pushed(done));
        });
    }

    pub fn on_pushed(&mut self, ui: &App, n: usize) {
        let acc = ui.global::<Account>();
        acc.set_pushing(false);
        acc.set_only_here(self.db.only_here().map(|c| c.len() as i32).unwrap_or(0));
        ui.global::<Linen>().set_subs_msg(format!("Added {n} channels to your YouTube subscriptions.").into());
    }

    pub fn rate_on_youtube(&self, id: &str, rating: i32) {
        if !self.yt.signed_in() {
            return;
        }
        let (yt, id) = (self.yt.clone(), id.to_string());
        std::thread::spawn(move || {
            if let Err(e) = yt.rate(&id, rating) {
                log::warn!("youtube rating {id}: {e:#}");
            }
        });
    }

    // ---- history + resume on YouTube while you watch here ----

    pub fn fetch_tracking(&self, id: &str) {
        if !self.yt.signed_in() {
            return;
        }
        let (yt, tx, id) = (self.yt.clone(), self.tx.clone(), id.to_string());
        std::thread::spawn(move || match yt.tracking(&id) {
            Ok(t) => {
                let _ = tx.send(Msg::Tracking(id, t));
            }
            Err(e) => log::warn!("youtube tracking {id}: {e:#}"),
        });
    }

    pub fn on_tracking(&mut self, ui: &App, id: String, t: Tracking) {
        if self.watching.as_deref() != Some(id.as_str()) {
            return;
        }
        let start = self.mpv.get_property::<f64>("time-pos").unwrap_or(0.0);
        self.tracking = Some(WatchTrack { t, last: start, started: false });
        if ui.global::<Watch>().get_has_video() {
            self.track_started();
        }
    }

    /// Playback began: it's in your YouTube history now.
    pub fn track_started(&mut self) {
        let Some(tr) = self.tracking.as_mut().filter(|t| !t.started) else { return };
        tr.started = true;
        tr.last = self.mpv.get_property::<f64>("time-pos").unwrap_or(0.0);
        let (yt, t) = (self.yt.clone(), tr.t.clone());
        std::thread::spawn(move || {
            if let Err(e) = yt.ping_playback(&t) {
                log::warn!("youtube history: {e:#}");
            }
        });
    }

    /// Where you are, so YouTube resumes there on your other devices.
    pub fn track_progress(&mut self) {
        let Some(tr) = self.tracking.as_mut().filter(|t| t.started) else { return };
        let Ok(pos) = self.mpv.get_property::<f64>("time-pos") else { return };
        let from = tr.last.min(pos);
        tr.last = pos;
        if (pos - from).abs() < 1.0 && from > 0.0 {
            return;
        }
        let (yt, t) = (self.yt.clone(), tr.t.clone());
        std::thread::spawn(move || {
            if let Err(e) = yt.ping_watchtime(&t, from, pos) {
                log::warn!("youtube watch time: {e:#}");
            }
        });
    }

    // ---- the YouTube Home tab ----

    pub fn load_home(&mut self, ui: &App, more: bool) {
        if !self.yt.signed_in() {
            return;
        }
        let token = if more {
            match self.home_more.take() {
                Some(t) => Some(t),
                None => return,
            }
        } else {
            self.home_req += 1;
            None
        };
        let lib = ui.global::<Linen>();
        lib.set_home_busy(true);
        lib.set_home_msg("".into());
        let (yt, tx, req) = (self.yt.clone(), self.tx.clone(), self.home_req);
        std::thread::spawn(move || {
            let r = yt.home(token.as_deref()).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Home(req, r, more));
        });
    }

    pub fn on_home(&mut self, ui: &App, req: u64, r: Result<Page, String>, append: bool) {
        if req != self.home_req {
            return;
        }
        let lib = ui.global::<Linen>();
        lib.set_home_busy(false);
        match r {
            Ok(page) => {
                self.home_more = page.more.clone();
                lib.set_home_more(self.home_more.is_some());
                let page = Page { playlists: vec![], ..page };
                self.fill_home_page(&page, append);
            }
            Err(e) => {
                log::warn!("youtube home: {e}");
                lib.set_home_msg("Couldn't load your YouTube Home. Try the refresh button.".into());
            }
        }
    }

    // ---- the welcome ----

    pub fn start_onboarding(&mut self, ui: &App) {
        if self.db.flag("onboarded", false) {
            return;
        }
        let ob = ui.global::<Onboard>();
        ob.set_step(0);
        ob.set_followed(self.channels.len() as i32);
        ob.set_suggestions_title("Some calm favourites".into());
        self.suggestions = SUGGESTED
            .iter()
            .map(|(id, name)| ChannelHit { id: id.to_string(), name: name.to_string(), ..Default::default() })
            .collect();
        self.show_suggestions(ui);
        ui.global::<Linen>().set_page(7);
        // pictures and subscriber counts, a few at a time
        let (yt, tx) = (self.yt.clone(), self.tx.clone());
        let ids: Vec<String> = SUGGESTED.iter().map(|s| s.0.to_string()).collect();
        std::thread::spawn(move || {
            let found: Vec<ChannelHit> = std::thread::scope(|s| {
                ids.chunks(5)
                    .flat_map(|chunk| {
                        let hs: Vec<_> = chunk.iter().map(|id| s.spawn(|| yt.channel_videos(id, None))).collect();
                        hs.into_iter().filter_map(|h| h.join().ok()?.ok()?.0).collect::<Vec<_>>()
                    })
                    .map(|i| ChannelHit {
                        subs: i.meta.split(" · ").find(|p| p.contains("subscriber")).unwrap_or_default().to_string(),
                        id: i.id,
                        name: i.name,
                        avatar: i.avatar,
                    })
                    .collect()
            });
            let _ = tx.send(Msg::Suggestions(None, found));
        });
    }

    pub fn onboard_find(&mut self, ui: &App, q: String) {
        let q = q.trim().to_string();
        if q.is_empty() {
            return;
        }
        ui.global::<Onboard>().set_suggestions_title(format!("Looking for “{q}”…").into());
        let (yt, tx) = (self.yt.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let found = yt.search(&q, None).map(|p| p.channels.into_iter().take(6).collect()).unwrap_or_default();
            let _ = tx.send(Msg::Suggestions(Some(q), found));
        });
    }

    pub fn on_suggestions(&mut self, ui: &App, query: Option<String>, found: Vec<ChannelHit>) {
        let ob = ui.global::<Onboard>();
        match query {
            // the starter list, now with pictures (same order)
            None => {
                for f in found {
                    if let Some(s) = self.suggestions.iter_mut().find(|s| s.id == f.id) {
                        *s = f;
                    }
                }
            }
            Some(q) => {
                ob.set_suggestions_title(if found.is_empty() { format!("No channels found for “{q}”.") } else { format!("Channels for “{q}”") }.into());
                self.suggestions = found;
            }
        }
        for c in &self.suggestions {
            if let Some(a) = &c.avatar {
                self.pics.want(&format!("a:{}", c.id), a, 120);
            }
        }
        self.show_suggestions(ui);
    }

    pub fn show_suggestions(&self, ui: &App) {
        let items: Vec<ChannelItem> = self
            .suggestions
            .iter()
            .map(|c| {
                let avatar = self.cache.get(&format!("a:{}", c.id)).cloned();
                ChannelItem {
                    id: c.id.clone().into(),
                    name: c.name.clone().into(),
                    has_avatar: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                    sub: c.subs.clone().into(),
                    following: self.pending_follows.contains(&c.id) || self.db.is_following(&c.id).unwrap_or(false),
                }
            })
            .collect();
        ui.global::<Onboard>().set_suggestions(ModelRc::new(VecModel::from(items)));
        let followed = self.channels.iter().map(|c| &c.id).chain(self.pending_follows.iter()).collect::<HashSet<_>>().len();
        ui.global::<Onboard>().set_followed(followed as i32);
    }

    pub fn finish_onboarding(&mut self, ui: &App) {
        let _ = self.db.set_flag("onboarded", true);
        let lib = ui.global::<Linen>();
        lib.set_feed_tab(0);
        lib.set_page(0);
        self.show_feed(ui);
        self.show_channels(ui);
    }
}
