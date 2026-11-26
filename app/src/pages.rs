//! Browsing: search, channel pages, playlists, "For you", the Watch page's
//! details, and moving between pages. YouTube lookups run on threads and come
//! back as `Msg`s; answers to requests that were since replaced are dropped.

use std::rc::Rc;

use linen_core::Video;
use linen_core::yt::{ChannelHit, Link, Page, PlaylistHit, VideoHit};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{App, ChannelItem, ChapterItem, Linen, LinkItem, Msg, State, VideoItem, Watch, fmt_time, views};

const SEARCH: usize = 0;
const CHANNEL: usize = 1;
pub const PLAYLIST: usize = 2;
/// how many lists there are (search, channel, playlist, For you, Home, Watch later, downloads)
const LISTS: usize = 8;

/// What's in a grid: videos (by id, details in `State::known`) and playlists.
#[derive(Clone)]
pub enum Card {
    Video(String),
    Playlist(PlaylistHit),
}

pub struct List {
    cards: Vec<Card>,
    model: Rc<VecModel<VideoItem>>,
}

impl List {
    pub fn new() -> Self {
        Self { cards: vec![], model: Rc::new(VecModel::default()) }
    }
}

pub enum Browse {
    Search { page: Page, append: bool },
    Channel { info: Option<linen_core::yt::ChannelInfo>, page: Page, append: bool, tab: i32 },
    Playlist { title: String, by: String, page: Page, append: bool },
    Failed { kind: usize, error: String },
}

/// A video we know only by id (its details arrive with the Watch page).
pub fn bare_video(id: &str) -> Video {
    Video {
        id: id.to_string(),
        channel_id: String::new(),
        channel_name: String::new(),
        title: String::new(),
        published: 0,
        thumbnail: format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"),
        views: None,
        description: String::new(),
        seen: true,
        position: 0.0,
        duration: None,
    }
}

/// A YouTube video id from a link (watch?v=, youtu.be/, /shorts/, /live/).
fn video_id_from(url: &str) -> Option<String> {
    let rest = url.split_once("youtube.com/").map(|x| x.1).or_else(|| url.split_once("youtu.be/").map(|x| x.1))?;
    let id = if url.contains("youtu.be/") {
        rest
    } else if let Some(q) = rest.strip_prefix("watch?").or_else(|| rest.split_once("watch?").map(|x| x.1)) {
        q.split('&').find_map(|kv| kv.strip_prefix("v="))?
    } else {
        rest.strip_prefix("shorts/").or_else(|| rest.strip_prefix("live/"))?
    };
    let id: String = id.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    (id.len() == 11).then_some(id)
}

fn channel_id_from(url: &str) -> Option<String> {
    let id = url.split_once("youtube.com/channel/")?.1;
    let id: String = id.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    id.starts_with("UC").then_some(id)
}

impl State {
    /// A feed tab came into view: its top videos go first in the lookups.
    pub fn tab_shown(&mut self, tab: i32) {
        let ids: Vec<String> = match tab {
            0 => self.feed.iter().take(12).map(|v| v.id.clone()).collect(),
            _ => {
                let list = if tab == 1 { &self.foryou } else { &self.home };
                list.cards.iter().filter_map(|c| if let Card::Video(id) = c { Some(id.clone()) } else { None }).take(8).collect()
            }
        };
        self.want_prefetch_page(ids, true);
    }

    pub fn attach_lists(&self, ui: &App) {
        let lib = ui.global::<Linen>();
        lib.set_home(ModelRc::from(self.home.model.clone()));
        lib.set_foryou(ModelRc::from(self.foryou.model.clone()));
        lib.set_results(ModelRc::from(self.results.model.clone()));
        lib.set_ch_items(ModelRc::from(self.ch_list.model.clone()));
        lib.set_pl_items(ModelRc::from(self.pl_list.model.clone()));
        lib.set_later(ModelRc::from(self.later_list.model.clone()));
        lib.set_downloads(ModelRc::from(self.dl_list.model.clone()));
        lib.set_my_playlists(ModelRc::from(self.my_lists.model.clone()));
    }

    // ---- moving between pages ----

    /// Go to a page, remembering where you came from.
    pub fn go(&mut self, ui: &App, page: i32) {
        let lib = ui.global::<Linen>();
        let cur = lib.get_page();
        if cur != page {
            if cur == 2 {
                // off to the channel page etc. from a video: it keeps playing, small
                self.minimize(ui);
            }
            if page == 2 {
                ui.global::<Watch>().set_mini(false);
            }
            self.nav.retain(|p| *p != page);
            self.nav.push(cur);
            lib.set_page(page);
        }
        lib.set_can_back(!self.nav.is_empty());
    }

    /// Back from the video: it goes on in the mini player (× there stops it).
    pub fn back(&mut self, ui: &App) {
        let lib = ui.global::<Linen>();
        let from_video = lib.get_page() == 2;
        if from_video {
            self.minimize(ui);
        }
        let mut to = 0;
        while let Some(p) = self.nav.pop() {
            // not back onto the video we just left (it's in the corner now), and
            // the Watch page only if a video is still there
            if p == 2 && (from_video || self.watching.is_none()) {
                continue;
            }
            to = p;
            break;
        }
        if to == 2 {
            ui.global::<Watch>().set_mini(false);
        }
        lib.set_page(to);
        lib.set_can_back(!self.nav.is_empty());
    }

    /// The sidebar: start over at Feed or Subscriptions (a video carries on small).
    pub fn nav_root(&mut self, ui: &App, page: i32) {
        let lib = ui.global::<Linen>();
        if lib.get_page() == 2 {
            self.minimize(ui);
        }
        self.nav.clear();
        lib.set_page(page);
        lib.set_can_back(false);
    }

    /// The video leaves the Watch page for the corner (if there is one).
    pub fn minimize(&mut self, ui: &App) {
        let w = ui.global::<Watch>();
        w.set_fullscreen(false);
        if self.watching.is_some() {
            self.save_position(self.mpv);
            w.set_mini(true);
        }
    }

    /// The mini player, big again.
    pub fn expand(&mut self, ui: &App) {
        if self.watching.is_some() {
            self.go(ui, 2);
        }
    }

    // ---- grids ----

    fn list(&self, which: usize) -> &List {
        match which {
            SEARCH => &self.results,
            CHANNEL => &self.ch_list,
            PLAYLIST => &self.pl_list,
            crate::account::HOME => &self.home,
            crate::library::LATER => &self.later_list,
            crate::library::DOWNLOADS => &self.dl_list,
            crate::playlists::MINE => &self.my_lists,
            _ => &self.foryou,
        }
    }

    fn list_mut(&mut self, which: usize) -> &mut List {
        match which {
            SEARCH => &mut self.results,
            CHANNEL => &mut self.ch_list,
            PLAYLIST => &mut self.pl_list,
            crate::account::HOME => &mut self.home,
            crate::library::LATER => &mut self.later_list,
            crate::library::DOWNLOADS => &mut self.dl_list,
            crate::playlists::MINE => &mut self.my_lists,
            _ => &mut self.foryou,
        }
    }

    /// Show these in a list (Library).
    pub fn fill_list(&mut self, which: usize, cards: Vec<Card>) {
        self.fill(which, cards, false);
    }

    pub fn remember_hits(&mut self, hits: &[VideoHit]) {
        self.remember(hits);
    }

    fn hit_video(&self, h: &VideoHit) -> Video {
        let mut v = bare_video(&h.id);
        v.title = h.title.clone();
        v.channel_id = h.channel_id.clone();
        v.channel_name = h.channel_name.clone();
        v.published = h.published;
        v.views = h.views;
        v.duration = h.duration;
        v.position = self.db.position(&h.id).unwrap_or(0.0);
        v
    }

    fn remember(&mut self, hits: &[VideoHit]) {
        for h in hits {
            let v = self.hit_video(h);
            match self.known.get_mut(&h.id) {
                // known from the feed: keep its description and date, add what's new
                Some(k) => {
                    k.duration = k.duration.or(v.duration);
                    k.views = v.views.or(k.views);
                    if k.channel_id.is_empty() {
                        k.channel_id = v.channel_id;
                        k.channel_name = v.channel_name;
                    }
                    if k.published == 0 {
                        k.published = v.published;
                    }
                }
                None => {
                    self.known.insert(h.id.clone(), v);
                }
            }
        }
    }

    fn card_item(&self, c: &Card) -> VideoItem {
        match c {
            Card::Video(id) => self.video_item(self.known.get(id).unwrap_or(&bare_video(id))),
            Card::Playlist(p) => {
                let thumb = self.cache.get(&format!("v:pl:{}", p.id)).cloned();
                VideoItem {
                    id: p.id.clone().into(),
                    title: p.title.clone().into(),
                    meta: "Playlist".into(),
                    has_thumb: thumb.is_some(),
                    thumb: thumb.unwrap_or_default(),
                    duration: p.count.clone().into(),
                    kind: 1,
                    ..Default::default()
                }
            }
        }
    }

    fn want_pictures(&self, cards: &[Card]) {
        for c in cards {
            match c {
                Card::Video(id) => {
                    let url = self.known.get(id).map(|v| v.thumbnail.clone()).unwrap_or_else(|| bare_video(id).thumbnail);
                    self.pics.want(&format!("v:{id}"), &url, 480);
                }
                Card::Playlist(p) => {
                    let url = p.thumb.clone().or_else(|| p.first_video.as_ref().map(|v| bare_video(v).thumbnail)).unwrap_or_default();
                    self.pics.want(&format!("v:pl:{}", p.id), &url, 480);
                }
            }
        }
    }

    fn page_cards(&mut self, page: &Page) -> Vec<Card> {
        self.remember(&page.videos);
        page.videos
            .iter()
            .filter(|v| !v.live)
            .map(|v| Card::Video(v.id.clone()))
            .chain(page.playlists.iter().cloned().map(Card::Playlist))
            .collect()
    }

    pub fn fill_home(&mut self, cards: Vec<Card>, append: bool) {
        self.fill(crate::account::HOME, cards, append);
    }

    pub fn fill_home_page(&mut self, page: &Page, append: bool) {
        let cards = self.page_cards(page);
        self.fill(crate::account::HOME, cards, append);
    }

    fn fill(&mut self, which: usize, cards: Vec<Card>, append: bool) {
        self.want_pictures(&cards);
        let todo: Vec<(String, i64)> = cards
            .iter()
            .filter_map(|c| if let Card::Video(id) = c { Some((id.clone(), self.known.get(id).map_or(0, |v| v.published))) } else { None })
            .collect();
        self.want_branding(&todo);
        let items: Vec<VideoItem> = cards.iter().map(|c| self.card_item(c)).collect();
        let list = self.list_mut(which);
        // the top of a freshly shown page: likely clicks, look them up ahead of time
        let ahead: Vec<String> = if append {
            vec![]
        } else {
            let n = if which == CHANNEL || which == PLAYLIST { 6 } else { 8 };
            cards.iter().filter_map(|c| if let Card::Video(id) = c { Some(id.clone()) } else { None }).take(n).collect()
        };
        if append {
            list.cards.extend(cards);
            for i in items {
                list.model.push(i);
            }
        } else {
            list.cards = cards;
            list.model.set_vec(items);
        }
        // downloads play from the file; nothing to look up
        if !ahead.is_empty() && which != crate::library::DOWNLOADS {
            // For you and the Library sit in tabs: the others are pages you're opening right now
            self.want_prefetch_page(ahead, which < 3 || which == crate::account::HOME);
        }
    }

    /// Redraw the cards whose picture or title changed (cheap: same rows).
    pub fn refresh_cards(&mut self, _ui: &App) {
        for i in 0..self.feed_model.row_count().min(self.feed.len()) {
            let item = self.video_item(&self.feed[i]);
            if self.feed_model.row_data(i).is_none_or(|r| crate::calm::differs(&r, &item)) {
                self.feed_model.set_row_data(i, item);
            }
        }
        for which in 0..LISTS {
            let list = self.list(which);
            for i in 0..list.cards.len().min(list.model.row_count()) {
                let item = self.card_item(&list.cards[i]);
                if list.model.row_data(i).is_none_or(|r| crate::calm::differs(&r, &item)) {
                    list.model.set_row_data(i, item);
                }
            }
        }
    }

    /// Pictures came in: update the cards (and pages) that were waiting for them.
    pub fn pictures_arrived(&mut self, ui: &App) {
        self.refresh_cards(ui);
        let lib = ui.global::<Linen>();
        if !self.ch_id.is_empty() {
            if let Some(a) = self.cache.get(&format!("a:{}", self.ch_id)) {
                lib.set_ch_avatar(a.clone());
                lib.set_ch_has_avatar(true);
            }
            if let Some(b) = self.cache.get(&format!("b:{}", self.ch_id)) {
                lib.set_ch_banner(b.clone());
                lib.set_ch_has_banner(true);
            }
        }
        if !self.found_channels.is_empty() {
            self.show_found_channels(ui);
        }
        let w = ui.global::<Watch>();
        // the Watch page's picture (shown while loading, and when listening only)
        if let Some(id) = self.watching.as_ref().filter(|_| !w.get_has_poster()) {
            if let Some(p) = self.cache.get(&format!("v:{id}")) {
                w.set_poster(p.clone());
                w.set_has_poster(true);
            }
        }
        if !w.get_has_channel_avatar() {
            if let Some(a) = self.cache.get(&format!("a:{}", w.get_channel_id())) {
                w.set_channel_avatar(a.clone());
                w.set_has_channel_avatar(true);
            }
        }
        if !self.thread.rows.is_empty() {
            self.show_comments(ui);
        }
    }

    /// After watching: the card's progress bar.
    pub fn refresh_progress_of(&mut self, id: &str) {
        let pos = self.db.position(id).unwrap_or(0.0);
        if let Some(v) = self.known.get_mut(id) {
            v.position = pos;
            if v.duration.is_none() && self.duration > 0.0 {
                v.duration = Some(self.duration as u32);
            }
        }
        for i in 0..self.feed.len() {
            if self.feed[i].id == id {
                self.feed[i].position = pos;
                if self.feed[i].duration.is_none() && self.duration > 0.0 {
                    self.feed[i].duration = Some(self.duration as u32);
                }
                let item = self.video_item(&self.feed[i].clone());
                self.feed_model.set_row_data(i, item);
            }
        }
        for which in 0..LISTS {
            let list = self.list(which);
            for i in 0..list.cards.len() {
                if matches!(&list.cards[i], Card::Video(v) if v == id) {
                    list.model.set_row_data(i, self.card_item(&list.cards[i]));
                }
            }
        }
    }

    // ---- search ----

    pub fn search(&mut self, ui: &App, q: String) {
        let q = q.trim().to_string();
        if q.is_empty() {
            return;
        }
        // a pasted video or channel link: go straight there
        if let Some(id) = video_id_from(&q) {
            return self.open_video(ui, &id, self.mpv);
        }
        if let Some(id) = channel_id_from(&q) {
            return self.open_channel(ui, id);
        }
        self.req[SEARCH] += 1;
        self.query = q.clone();
        self.results_more = None;
        self.found_channels.clear();
        self.fill(SEARCH, vec![], false);
        let lib = ui.global::<Linen>();
        lib.set_query(q.clone().into());
        lib.set_searching(true);
        lib.set_results_msg("".into());
        lib.set_results_more(false);
        lib.set_found_channels(ModelRc::default());
        self.go(ui, 3);
        let (yt, tx, req) = (self.yt.clone(), self.tx.clone(), self.req[SEARCH]);
        std::thread::spawn(move || {
            let b = match yt.search(&q, None) {
                Ok(page) => Browse::Search { page, append: false },
                Err(e) => Browse::Failed { kind: SEARCH, error: format!("{e:#}") },
            };
            let _ = tx.send(Msg::Browse(req, b));
        });
    }

    pub fn more_results(&mut self, ui: &App) {
        let Some(more) = self.results_more.take() else { return };
        ui.global::<Linen>().set_searching(true);
        let (yt, tx, req) = (self.yt.clone(), self.tx.clone(), self.req[SEARCH]);
        std::thread::spawn(move || {
            let b = match yt.search("", Some(&more)) {
                Ok(page) => Browse::Search { page, append: true },
                Err(e) => Browse::Failed { kind: SEARCH, error: format!("{e:#}") },
            };
            let _ = tx.send(Msg::Browse(req, b));
        });
    }

    fn show_found_channels(&self, ui: &App) {
        let items: Vec<ChannelItem> = self
            .found_channels
            .iter()
            .map(|c| {
                let avatar = self.cache.get(&format!("a:{}", c.id)).cloned();
                ChannelItem {
                    id: c.id.clone().into(),
                    name: c.name.clone().into(),
                    has_avatar: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                    sub: c.subs.clone().into(),
                    following: self.db.is_following(&c.id).unwrap_or(false),
                }
            })
            .collect();
        ui.global::<Linen>().set_found_channels(ModelRc::new(VecModel::from(items)));
    }

    // ---- channel pages ----

    pub fn open_channel(&mut self, ui: &App, id: String) {
        if id.is_empty() {
            return;
        }
        self.req[CHANNEL] += 1;
        self.ch_id = id.clone();
        self.ch_tab = 0;
        self.ch_more = None;
        self.ch_info = None;
        self.fill(CHANNEL, vec![], false);
        let lib = ui.global::<Linen>();
        // what we already know, until the page arrives
        let known_name = self.channels.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_default();
        lib.set_ch_id(id.clone().into());
        lib.set_ch_name(known_name.into());
        lib.set_ch_meta("".into());
        lib.set_ch_about("".into());
        lib.set_ch_msg("".into());
        lib.set_ch_tab(0);
        lib.set_ch_more(false);
        lib.set_ch_loading(true);
        lib.set_ch_following(self.db.is_following(&id).unwrap_or(false));
        let avatar = self.cache.get(&format!("a:{id}")).cloned();
        lib.set_ch_has_avatar(avatar.is_some());
        lib.set_ch_avatar(avatar.unwrap_or_default());
        lib.set_ch_has_banner(false);
        self.go(ui, 4);
        self.load_channel(None);
    }

    fn load_channel(&self, more: Option<String>) {
        let (yt, tx, req, id, tab) = (self.yt.clone(), self.tx.clone(), self.req[CHANNEL], self.ch_id.clone(), self.ch_tab);
        std::thread::spawn(move || {
            let append = more.is_some();
            let b = if tab == 0 {
                yt.channel_videos(&id, more.as_deref()).map(|(info, page)| Browse::Channel { info, page, append, tab })
            } else {
                yt.channel_playlists(&id, more.as_deref()).map(|page| Browse::Channel { info: None, page, append, tab })
            };
            let b = b.unwrap_or_else(|e| Browse::Failed { kind: CHANNEL, error: format!("{e:#}") });
            let _ = tx.send(Msg::Browse(req, b));
        });
    }

    pub fn ch_set_tab(&mut self, ui: &App, tab: i32) {
        if tab == self.ch_tab {
            return;
        }
        self.req[CHANNEL] += 1;
        self.ch_tab = tab;
        self.ch_more = None;
        self.fill(CHANNEL, vec![], false);
        let lib = ui.global::<Linen>();
        lib.set_ch_tab(tab);
        lib.set_ch_loading(true);
        lib.set_ch_more(false);
        lib.set_ch_msg("".into());
        self.load_channel(None);
    }

    pub fn ch_load_more(&mut self, ui: &App) {
        let Some(more) = self.ch_more.take() else { return };
        ui.global::<Linen>().set_ch_loading(true);
        self.load_channel(Some(more));
    }

    // ---- playlists ----

    pub fn open_playlist(&mut self, ui: &App, id: String) {
        // yours: from here, and editable
        if self.is_my_playlist(&id) {
            return self.open_my_playlist(ui, &id);
        }
        self.my_playlist = None;
        ui.global::<Linen>().set_pl_editable(false);
        self.req[PLAYLIST] += 1;
        self.pl_id = id.clone();
        self.pl_more = None;
        self.fill(PLAYLIST, vec![], false);
        let lib = ui.global::<Linen>();
        lib.set_pl_title("".into());
        lib.set_pl_by("".into());
        lib.set_pl_msg("".into());
        lib.set_pl_more(false);
        lib.set_pl_loading(true);
        self.go(ui, 5);
        self.load_playlist(None);
    }

    fn load_playlist(&self, more: Option<String>) {
        let (yt, tx, req, id) = (self.yt.clone(), self.tx.clone(), self.req[PLAYLIST], self.pl_id.clone());
        std::thread::spawn(move || {
            let append = more.is_some();
            let b = match yt.playlist(&id, more.as_deref()) {
                Ok((title, by, page)) => Browse::Playlist { title, by, page, append },
                Err(e) => Browse::Failed { kind: PLAYLIST, error: format!("{e:#}") },
            };
            let _ = tx.send(Msg::Browse(req, b));
        });
    }

    pub fn pl_load_more(&mut self, ui: &App) {
        let Some(more) = self.pl_more.take() else { return };
        ui.global::<Linen>().set_pl_loading(true);
        self.load_playlist(Some(more));
    }

    // ---- answers ----

    pub fn on_browse(&mut self, ui: &App, req: u64, b: Browse) {
        let lib = ui.global::<Linen>();
        let kind = match &b {
            Browse::Search { .. } => SEARCH,
            Browse::Channel { .. } => CHANNEL,
            Browse::Playlist { .. } => PLAYLIST,
            Browse::Failed { kind, .. } => *kind,
        };
        if req != self.req[kind] {
            return; // replaced by a newer request
        }
        match b {
            Browse::Search { page, append } => {
                if !append {
                    self.found_channels = page.channels.iter().take(3).cloned().collect::<Vec<ChannelHit>>();
                    for c in &self.found_channels {
                        if let Some(a) = &c.avatar {
                            self.pics.want(&format!("a:{}", c.id), a, 120);
                        }
                    }
                    self.show_found_channels(ui);
                }
                let cards = self.page_cards(&page);
                self.fill(SEARCH, cards, append);
                self.results_more = page.more;
                lib.set_searching(false);
                lib.set_results_more(self.results_more.is_some());
                let n = self.results.cards.len();
                lib.set_results_msg(if n == 0 && self.found_channels.is_empty() {
                    format!("Nothing found for “{}”.", self.query).into()
                } else {
                    "Search results".into()
                });
            }
            Browse::Channel { info, page, append, tab } => {
                if tab != self.ch_tab {
                    return;
                }
                if let Some(info) = info {
                    lib.set_ch_name(info.name.clone().into());
                    lib.set_ch_meta(info.meta.clone().into());
                    lib.set_ch_about(info.about.lines().next().unwrap_or_default().into());
                    if let Some(a) = &info.avatar {
                        self.pics.want(&format!("a:{}", info.id), a, 192);
                        // a followed channel without a picture yet
                        if self.channels.iter().any(|c| c.id == info.id && c.avatar.is_none()) {
                            let _ = self.db.set_avatar(&info.id, a);
                        }
                    }
                    if let Some(b) = &info.banner {
                        self.pics.want(&format!("b:{}", info.id), b, 1600);
                    }
                    self.ch_info = Some(info);
                }
                let cards = self.page_cards(&page);
                self.fill(CHANNEL, cards, append);
                self.ch_more = page.more;
                lib.set_ch_loading(false);
                lib.set_ch_more(self.ch_more.is_some());
            }
            Browse::Playlist { title, by, page, append } => {
                if !append {
                    lib.set_pl_title(title.into());
                    lib.set_pl_by(by.into());
                }
                let cards = self.page_cards(&page);
                self.fill(PLAYLIST, cards, append);
                self.pl_more = page.more;
                lib.set_pl_loading(false);
                lib.set_pl_more(self.pl_more.is_some());
            }
            Browse::Failed { kind, error } => {
                log::warn!("browse: {error}");
                let msg = "Couldn't reach YouTube. Check your connection and try again.";
                match kind {
                    SEARCH => {
                        lib.set_searching(false);
                        lib.set_results_msg(msg.into());
                    }
                    CHANNEL => {
                        lib.set_ch_loading(false);
                        lib.set_ch_msg(msg.into());
                    }
                    _ => {
                        lib.set_pl_loading(false);
                        lib.set_pl_msg(msg.into());
                    }
                }
            }
        }
    }

    // ---- following ----

    pub fn follow(&mut self, ui: &App, id: String, on: bool) {
        if id.is_empty() {
            return;
        }
        if on {
            // shows as followed right away; undone if the lookup fails
            self.add_channel(ui, id.clone());
            self.pending_follows.insert(id.clone());
            self.follow_on_youtube(&id, true);
            self.set_following_flags(ui, &id, true);
        } else {
            let _ = self.db.remove_channel(&id);
            self.pending_follows.remove(&id);
            self.follow_on_youtube(&id, false);
            self.show_feed(ui);
            self.show_channels(ui);
            self.set_following_flags(ui, &id, false);
        }
    }

    fn set_following_flags(&self, ui: &App, id: &str, on: bool) {
        let lib = ui.global::<Linen>();
        if self.ch_id == id {
            lib.set_ch_following(on);
        }
        let w = ui.global::<Watch>();
        if w.get_channel_id() == id {
            w.set_following(on);
        }
        let found = lib.get_found_channels();
        for i in 0..found.row_count() {
            if let Some(mut c) = found.row_data(i).filter(|c| c.id == id) {
                c.following = on;
                found.set_row_data(i, c);
            }
        }
        if lib.get_page() == 7 {
            self.show_suggestions(ui);
        }
    }

    /// The database is the truth (after a follow finished or failed).
    pub fn refresh_following(&self, ui: &App) {
        let lib = ui.global::<Linen>();
        if !self.ch_id.is_empty() {
            lib.set_ch_following(self.db.is_following(&self.ch_id).unwrap_or(false));
        }
        let w = ui.global::<Watch>();
        let wid = w.get_channel_id();
        if !wid.is_empty() {
            w.set_following(self.db.is_following(&wid).unwrap_or(false));
        }
        if !self.found_channels.is_empty() {
            self.show_found_channels(ui);
        }
    }

    // ---- For you ----

    pub fn foryou_age(&self) -> i64 {
        self.db.foryou_built().ok().flatten().map_or(i64::MAX, |t| linen_core::now() - t)
    }

    pub fn foryou_stale(&self) -> bool {
        self.foryou.cards.is_empty() || self.foryou_age() > 6 * 3600
    }

    pub fn rebuild_foryou(&mut self, ui: &App) {
        if self.foryou_busy {
            return;
        }
        self.foryou_busy = true;
        self.show_foryou_sub(ui);
        let (yt, tx) = (self.yt.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<Vec<linen_core::db::Pick>> {
                let db = linen_core::Db::open()?;
                let picks = linen_core::foryou::build(&db, &yt)?;
                // nothing came back (offline?): keep the old list
                if !picks.is_empty() {
                    db.save_foryou(&picks)?;
                }
                Ok(picks)
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::ForYou(r));
        });
    }

    pub fn on_foryou(&mut self, ui: &App, r: Result<Vec<linen_core::db::Pick>, String>) {
        self.foryou_busy = false;
        if let Err(e) = &r {
            log::warn!("for you: {e}");
        }
        self.show_foryou(ui);
    }

    pub fn show_foryou(&mut self, ui: &App) {
        let picks = self.db.foryou().unwrap_or_default();
        let hits: Vec<VideoHit> = picks.iter().map(|p| p.video.clone()).collect();
        self.remember(&hits);
        for p in &picks {
            self.reasons.insert(p.video.id.clone(), p.reason.clone());
        }
        let cards = picks.iter().map(|p| Card::Video(p.video.id.clone())).collect();
        self.fill(3, cards, false);
        self.show_foryou_sub(ui);
    }

    fn show_foryou_sub(&self, ui: &App) {
        let lib = ui.global::<Linen>();
        lib.set_foryou_busy(self.foryou_busy);
        let sub = if self.foryou_busy {
            "Picking videos from what you watch…".to_string()
        } else {
            match self.db.foryou_built().ok().flatten() {
                Some(t) if !self.foryou.cards.is_empty() => format!("Picked from what you watch · {}", crate::ago(t)),
                _ => String::new(),
            }
        };
        lib.set_foryou_sub(sub.into());
    }

    fn drop_foryou_where(&mut self, keep: impl Fn(&Video) -> bool) {
        let mut i = 0;
        while i < self.foryou.cards.len() {
            let gone = matches!(&self.foryou.cards[i], Card::Video(id) if self.known.get(id).is_some_and(|v| !keep(v)));
            if gone {
                self.foryou.cards.remove(i);
                self.foryou.model.remove(i);
            } else {
                i += 1;
            }
        }
    }

    pub fn not_interested(&mut self, _ui: &App, id: &str) {
        let _ = self.db.hide("video", id);
        let _ = self.db.remove_foryou(id);
        self.drop_foryou_where(|v| v.id != id);
    }

    pub fn hide_channel(&mut self, _ui: &App, channel: &str) {
        if channel.is_empty() {
            return;
        }
        let _ = self.db.hide("channel", channel);
        let _ = self.db.remove_foryou(channel);
        self.drop_foryou_where(|v| v.channel_id != channel);
    }

    // ---- the Watch page's details ----

    pub fn fetch_details(&self, id: &str) {
        let (yt, tx, id) = (self.yt.clone(), self.tx.clone(), id.to_string());
        std::thread::spawn(move || {
            let d = yt.details(&id).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Details(id, d));
        });
    }

    pub fn on_details(&mut self, ui: &App, id: &str, d: Result<linen_core::yt::Details, String>) {
        if self.watching.as_deref() != Some(id) && self.pending_open.as_deref() != Some(id) {
            return;
        }
        let d = match d {
            Ok(d) => d,
            Err(e) => return log::warn!("details for {id}: {e}"),
        };
        let w = ui.global::<Watch>();
        if !d.title.is_empty() {
            self.show_watch_title(ui, id, &d.title);
        }
        if !d.channel_id.is_empty() {
            w.set_channel(d.channel_name.clone().into());
            w.set_channel_id(d.channel_id.clone().into());
            w.set_following(self.db.is_following(&d.channel_id).unwrap_or(false));
            if let Some(a) = &d.channel_avatar {
                self.pics.want(&format!("a:{}", d.channel_id), a, 96);
            }
            if let Some(a) = self.cache.get(&format!("a:{}", d.channel_id)) {
                w.set_channel_avatar(a.clone());
                w.set_has_channel_avatar(true);
            }
        }
        w.set_subs(d.subs.clone().into());
        let likes = if d.likes.is_empty() { String::new() } else { format!("{} likes", d.likes) };
        self.watch_meta = vec![views(d.views), d.date.clone(), likes];
        self.show_watch_meta(ui);
        if !d.description.is_empty() {
            w.set_description(d.description.clone().into());
        }
        // chapters, or else the timestamps in the description
        let chapters: Vec<(String, f64)> = if d.chapters.is_empty() {
            d.links.iter().filter_map(|(label, l)| if let Link::Time(t) = l { Some((label.clone(), *t)) } else { None }).collect()
        } else {
            d.chapters.clone()
        };
        let real = !d.chapters.is_empty();
        let chapters: Vec<(String, f64)> = chapters
            .iter()
            .map(|(label, at)| {
                let time = fmt_time(*at);
                // a timestamp line starts with its time; don't show it twice
                let label = label.strip_prefix(time.as_str()).unwrap_or(label).trim_start_matches([' ', '-', '–', '—', ':', '|']).trim();
                (label.to_string(), *at)
            })
            .collect();
        let items: Vec<ChapterItem> =
            chapters.iter().map(|(label, at)| ChapterItem { label: label.into(), time: fmt_time(*at).into(), at: *at as f32 }).collect();
        w.set_chapters(ModelRc::new(VecModel::from(items)));
        // marks on the progress bar: real chapters, or description times that read
        // like chapters (from the start, in order)
        let like_chapters = chapters.len() >= 2 && chapters[0].1 < 1.0 && chapters.windows(2).all(|p| p[0].1 < p[1].1);
        self.chapters = if real || like_chapters { chapters } else { vec![] };
        self.chapter_shown.clear();
        self.show_chapter_marks(ui);
        self.comments_found(ui, id, &d.comment_count, d.comments_top.clone(), d.comments_newest.clone());
        self.watch_links = d.links.iter().filter(|(_, l)| !matches!(l, Link::Time(_))).map(|(_, l)| l.clone()).collect();
        let links: Vec<LinkItem> = d
            .links
            .iter()
            .filter(|(_, l)| !matches!(l, Link::Time(_)))
            .map(|(label, l)| LinkItem {
                label: match l {
                    Link::Url(u) => u.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.").trim_end_matches('/').to_string(),
                    _ => label.clone(),
                }
                .into(),
                kind: if matches!(l, Link::Video(_)) { 1 } else { 0 },
            })
            .collect();
        w.set_links(ModelRc::new(VecModel::from(links)));
        self.update_media_info(ui);
        // history learns the title and channel (a link to a video we hadn't listed)
        let _ = self.db.update_watched(id, &d.title, &d.channel_id, &d.channel_name);
        let v = self.known.entry(id.to_string()).or_insert_with(|| bare_video(id));
        if v.title.is_empty() {
            v.title = d.title.clone();
        }
        if v.channel_id.is_empty() {
            v.channel_id = d.channel_id.clone();
            v.channel_name = d.channel_name.clone();
        }
        v.views = d.views.or(v.views);
        // saved (Watch later, downloads) before we knew what it was
        if !d.title.is_empty() {
            let mut hit = linen_core::yt::VideoHit { id: id.to_string(), title: d.title.clone(), channel_id: d.channel_id.clone(), channel_name: d.channel_name.clone(), views: d.views, ..Default::default() };
            hit.duration = v.duration.or((self.duration > 0.0).then_some(self.duration as u32));
            if self.db.fill_saved(&hit).unwrap_or(0) > 0 {
                self.show_later(ui);
                self.show_downloads(ui);
            }
        }
    }

    /// Where the chapters start, as marks on the progress bar (once the length is known).
    pub fn show_chapter_marks(&self, ui: &App) {
        let w = ui.global::<Watch>();
        let marks: Vec<f32> = if self.duration > 0.0 {
            self.chapters.iter().skip(1).map(|c| (c.1 / self.duration) as f32).filter(|f| *f > 0.0 && *f < 1.0).collect()
        } else {
            vec![]
        };
        w.set_marks(ModelRc::new(VecModel::from(marks)));
    }

    pub fn chapter_name_at(&self, t: f64) -> String {
        self.chapters.iter().rev().find(|c| c.1 <= t + 0.5).map(|c| c.0.clone()).unwrap_or_default()
    }

    /// The chapter you're in, next to the time.
    pub fn show_chapter_at(&mut self, ui: &App, t: f64) {
        let name = self.chapter_name_at(t);
        if name != self.chapter_shown {
            ui.global::<Watch>().set_chapter(name.clone().into());
            self.chapter_shown = name;
        }
    }

    pub fn open_link(&mut self, ui: &App, i: usize) {
        let Some(link) = self.watch_links.get(i).cloned() else { return };
        match link {
            Link::Time(t) => crate::cmd(self.mpv, "seek", &[&format!("{t}"), "absolute"]),
            Link::Video(id) => self.open_video(ui, &id, self.mpv),
            Link::Url(u) => {
                if let Some(id) = video_id_from(&u) {
                    self.open_video(ui, &id, self.mpv);
                } else if let Some(id) = channel_id_from(&u) {
                    self.open_channel(ui, id);
                } else {
                    // everything else opens in your browser (paused here meanwhile)
                    let _ = self.mpv.set_property("pause", true);
                    if let Err(e) = std::process::Command::new("xdg-open").arg(&u).spawn() {
                        log::warn!("couldn't open {u}: {e}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_to_ids() {
        assert_eq!(video_id_from("https://www.youtube.com/watch?v=aqz-KE-bpKQ&t=10").as_deref(), Some("aqz-KE-bpKQ"));
        assert_eq!(video_id_from("https://youtu.be/aqz-KE-bpKQ?si=x").as_deref(), Some("aqz-KE-bpKQ"));
        assert_eq!(video_id_from("youtube.com/shorts/aqz-KE-bpKQ").as_deref(), Some("aqz-KE-bpKQ"));
        assert_eq!(video_id_from("https://www.youtube.com/@BlenderOfficial"), None);
        assert_eq!(video_id_from("blender donut"), None);
        assert_eq!(channel_id_from("https://www.youtube.com/channel/UCSMOQeBJ2RAnuFungnQOxLg/videos").as_deref(), Some("UCSMOQeBJ2RAnuFungnQOxLg"));
    }
}
