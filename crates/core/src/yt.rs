//! YouTube's own web API ("InnerTube", what youtube.com itself calls): search,
//! channel pages, playlists and a video's details. No account, no API key, and
//! much faster than going through yt-dlp (one kept-open connection, ~1 s a call).
//! yt-dlp is still what finds the playable streams.
//!
//! YouTube changes these responses now and then, so everything here is read
//! defensively: unknown shapes are skipped rather than failing the whole page.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const BASE: &str = "https://www.youtube.com/youtubei/v1";
const CLIENT_VERSION: &str = "2.20251120.01.00";
/// The "Videos" and "Playlists" tabs of a channel page.
const TAB_VIDEOS: &str = "EgZ2aWRlb3PyBgQKAjoA";
const TAB_PLAYLISTS: &str = "EglwbGF5bGlzdHPyBgQKAkIA";

#[derive(Debug, Clone, Default)]
pub struct VideoHit {
    pub id: String,
    pub title: String,
    pub channel_id: String,
    pub channel_name: String,
    /// approximate unix time ("2 weeks ago"), 0 when unknown
    pub published: i64,
    pub views: Option<u64>,
    /// seconds
    pub duration: Option<u32>,
    pub live: bool,
    /// how much of it you watched (signed-in history and lists), percent
    pub watched_pct: Option<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct ChannelHit {
    pub id: String,
    pub name: String,
    pub avatar: Option<String>,
    /// "1.26M subscribers"
    pub subs: String,
}

#[derive(Debug, Clone, Default)]
pub struct PlaylistHit {
    pub id: String,
    pub title: String,
    /// "95 videos"
    pub count: String,
    /// a video in it, for the picture
    pub first_video: Option<String>,
    pub thumb: Option<String>,
}

/// One page of results; `more` fetches the next one.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub videos: Vec<VideoHit>,
    pub channels: Vec<ChannelHit>,
    pub playlists: Vec<PlaylistHit>,
    pub more: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    pub avatar: Option<String>,
    pub banner: Option<String>,
    /// "@BlenderOfficial · 1.26M subscribers · 1.5K videos"
    pub meta: String,
    pub about: String,
}

#[derive(Debug, Clone)]
pub enum Link {
    /// a moment in this video
    Time(f64),
    /// another YouTube video
    Video(String),
    Url(String),
}

#[derive(Debug, Clone, Default)]
pub struct Details {
    pub id: String,
    pub title: String,
    pub channel_id: String,
    pub channel_name: String,
    pub channel_avatar: Option<String>,
    pub subs: String,
    pub views: Option<u64>,
    pub date: String,
    pub likes: String,
    pub description: String,
    /// (label, where it goes)
    pub links: Vec<(String, Link)>,
    pub chapters: Vec<(String, f64)>,
    pub related: Vec<VideoHit>,
    /// "5.4M" (empty when comments are off)
    pub comment_count: String,
    /// where the comments start: top ones, and newest first
    pub comments_top: Option<String>,
    pub comments_newest: Option<String>,
}

/// One comment (or reply) under a video.
#[derive(Debug, Clone, Default)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub channel_id: String,
    pub avatar: Option<String>,
    pub text: String,
    /// "1 day ago"
    pub when: String,
    /// "124" (empty: none)
    pub likes: String,
    /// "6" (empty: none)
    pub replies: String,
    /// where its replies are
    pub replies_token: Option<String>,
    /// written by the video's channel
    pub creator: bool,
    pub pinned: bool,
    /// the video's channel gave it a heart
    pub hearted: bool,
    /// a reply to another comment
    pub reply: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Comments {
    pub list: Vec<Comment>,
    /// the next page
    pub more: Option<String>,
}

pub struct Yt {
    http: reqwest::blocking::Client,
    /// your YouTube sign-in, when connected
    auth: std::sync::RwLock<Option<crate::account::Auth>>,
}

#[derive(Debug, Clone, Default)]
pub struct AccountInfo {
    pub name: String,
    pub handle: String,
    pub avatar: Option<String>,
}

/// Where YouTube wants to hear that a video is being watched (history + resume).
#[derive(Debug, Clone, Default)]
pub struct Tracking {
    pub playback: String,
    pub watchtime: String,
    /// a made-up playback id, the same for all pings of one viewing
    pub cpn: String,
}

impl Yt {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::blocking::Client::builder()
                .user_agent(crate::UA)
                .timeout(std::time::Duration::from_secs(20))
                .pool_idle_timeout(std::time::Duration::from_secs(300))
                .build()?,
            auth: Default::default(),
        })
    }

    /// Sign in (Some) or out (None). Everything after this is done as you.
    pub fn set_auth(&self, auth: Option<crate::account::Auth>) {
        *self.auth.write().unwrap() = auth;
    }

    /// Every account and channel the browser sign-in can act as (YouTube's
    /// "switch account" list, for each Google account signed in).
    pub fn identities(&self) -> Result<Vec<crate::account::Identity>> {
        let base = self.auth.read().unwrap().clone().context("not signed in")?;
        let mut out = vec![];
        for authuser in 0..8 {
            let me = crate::account::Auth { authuser, page_id: None, ..base.clone() };
            *self.auth.write().unwrap() = Some(me);
            let d = self.call(
                "account/accounts_list",
                json!({ "requestType": "ACCOUNTS_LIST_REQUEST_TYPE_CHANNEL_SWITCHER", "callCircumstance": "SWITCHING_USERS_FULL" }),
            );
            let Ok(d) = d else { break };
            let items: Vec<&Value> = find(&d, "accountItemRenderer").collect();
            // past the last signed-in account
            if items.is_empty() {
                break;
            }
            for it in items {
                let page_id = find(&it["serviceEndpoint"], "pageIdToken").find_map(|p| p["pageId"].as_str()).map(str::to_string);
                out.push(crate::account::Identity {
                    authuser,
                    page_id,
                    name: text(&it["accountName"]),
                    handle: text(&it["channelHandle"]),
                    byline: text(&it["accountByline"]),
                    avatar: best_image(&it["accountPhoto"]),
                });
            }
        }
        *self.auth.write().unwrap() = Some(base);
        // the same identity can show up under two slots; keep the first
        let mut seen = std::collections::HashSet::new();
        out.retain(|i| seen.insert((i.handle.clone(), i.page_id.clone(), i.name.clone())));
        Ok(out)
    }

    pub fn signed_in(&self) -> bool {
        self.auth.read().unwrap().is_some()
    }

    fn signed(&self, mut req: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        if let Some(c) = self.auth.read().unwrap().as_ref() {
            for (k, v) in crate::account::headers(c) {
                req = req.header(k, v);
            }
        }
        req
    }

    fn need_auth(&self) -> Result<()> {
        if !self.signed_in() {
            bail!("not connected to a YouTube account");
        }
        Ok(())
    }

    /// Who you're signed in as (fails when the sign-in is no longer valid).
    pub fn account(&self) -> Result<AccountInfo> {
        self.need_auth()?;
        let d = self.call("account/account_menu", json!({}))?;
        let h = find(&d, "activeAccountHeaderRenderer").next().context("YouTube says you're signed out")?;
        Ok(AccountInfo { name: text(&h["accountName"]), handle: text(&h["channelHandle"]), avatar: best_image(&h["accountPhoto"]) })
    }

    /// Every channel you're subscribed to.
    pub fn subscriptions(&self) -> Result<Vec<ChannelHit>> {
        self.need_auth()?;
        let mut out = vec![];
        let mut p = page(&self.call("browse", json!({ "browseId": "FEchannels" }))?, None);
        for _ in 0..50 {
            out.append(&mut p.channels);
            let Some(more) = p.more.take() else { break };
            p = page(&self.call("browse", json!({ "continuation": more }))?, None);
        }
        Ok(out)
    }

    pub fn subscribe(&self, channel: &str, on: bool) -> Result<()> {
        self.need_auth()?;
        let ep = if on { "subscription/subscribe" } else { "subscription/unsubscribe" };
        self.call(ep, json!({ "channelIds": [channel] }))?;
        Ok(())
    }

    /// 1 like, -1 dislike, 0 neither.
    pub fn rate(&self, video: &str, rating: i32) -> Result<()> {
        self.need_auth()?;
        let ep = match rating {
            1 => "like/like",
            -1 => "like/dislike",
            _ => "like/removelike",
        };
        self.call(ep, json!({ "target": { "videoId": video } }))?;
        Ok(())
    }

    /// Your watch history, newest first (how far you got in `watched_pct`).
    pub fn history(&self, pages: usize) -> Result<Vec<VideoHit>> {
        self.need_auth()?;
        let mut out = vec![];
        let mut p = page(&self.call("browse", json!({ "browseId": "FEhistory" }))?, None);
        for _ in 0..pages {
            out.append(&mut p.videos);
            let Some(more) = p.more.take() else { break };
            p = page(&self.call("browse", json!({ "continuation": more }))?, None);
        }
        Ok(out)
    }

    /// Your YouTube Watch later list (all of it, oldest saved last).
    pub fn watch_later(&self) -> Result<Vec<VideoHit>> {
        self.need_auth()?;
        let (_, _, mut p) = self.playlist("WL", None)?;
        let mut out = vec![];
        for _ in 0..20 {
            out.append(&mut p.videos);
            let Some(more) = p.more.take() else { break };
            p = page(&self.call("browse", json!({ "continuation": more }))?, None);
        }
        Ok(out)
    }

    /// Add a video to (or take it off) your YouTube Watch later.
    pub fn set_watch_later(&self, video: &str, on: bool) -> Result<()> {
        self.need_auth()?;
        self.set_in_playlist("WL", video, on)
    }

    /// Your playlists on YouTube (not Watch later or Liked videos), each with
    /// whether it's yours (true) or someone else's you saved.
    pub fn my_playlists(&self) -> Result<Vec<(PlaylistHit, bool)>> {
        self.need_auth()?;
        let mut d = self.call("browse", json!({ "browseId": "FEplaylist_aggregation" }))?;
        let mut out = vec![];
        for _ in 0..10 {
            // yours have Edit / Delete in their menu
            let own: std::collections::HashSet<String> = find(&d, "lockupViewModel")
                .filter(|l| l.to_string().contains("\"Delete\""))
                .filter_map(|l| l["contentId"].as_str().map(str::to_string))
                .collect();
            let mut p = page(&d, None);
            out.extend(p.playlists.drain(..).map(|pl| {
                let mine = own.contains(&pl.id);
                (pl, mine)
            }));
            let Some(more) = p.more.take() else { break };
            d = self.call("browse", json!({ "continuation": more }))?;
        }
        out.retain(|p| p.0.id != "WL" && p.0.id != "LL");
        Ok(out)
    }

    /// All the videos of a playlist (up to ~2000).
    pub fn playlist_all(&self, id: &str) -> Result<Vec<VideoHit>> {
        let (_, _, mut p) = self.playlist(id, None)?;
        let mut out = vec![];
        for _ in 0..20 {
            out.append(&mut p.videos);
            let Some(more) = p.more.take() else { break };
            p = page(&self.call("browse", json!({ "continuation": more }))?, None);
        }
        Ok(out)
    }

    /// A new (private) playlist on your account, with this video in it. Its id.
    pub fn create_playlist(&self, title: &str, video: Option<&str>) -> Result<String> {
        self.need_auth()?;
        let d = self.call("playlist/create", json!({ "title": title, "privacyStatus": "PRIVATE", "videoIds": video.into_iter().collect::<Vec<_>>() }))?;
        d["playlistId"].as_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("YouTube didn't make the playlist"))
    }

    /// Put a video in one of your playlists (or take it out).
    pub fn set_in_playlist(&self, playlist: &str, video: &str, on: bool) -> Result<()> {
        let action = if on {
            json!({ "action": "ACTION_ADD_VIDEO", "addedVideoId": video })
        } else {
            json!({ "action": "ACTION_REMOVE_VIDEO_BY_VIDEO_ID", "removedVideoId": video })
        };
        self.edit_playlist(playlist, action)
    }

    pub fn rename_playlist(&self, playlist: &str, name: &str) -> Result<()> {
        self.edit_playlist(playlist, json!({ "action": "ACTION_SET_PLAYLIST_NAME", "playlistName": name }))
    }

    pub fn delete_playlist(&self, playlist: &str) -> Result<()> {
        self.need_auth()?;
        self.call("playlist/delete", json!({ "playlistId": playlist }))?;
        Ok(())
    }

    fn edit_playlist(&self, playlist: &str, action: Value) -> Result<()> {
        self.need_auth()?;
        let d = self.call("browse/edit_playlist", json!({ "playlistId": playlist, "actions": [action] }))?;
        if d["status"].as_str().is_some_and(|s| s != "STATUS_SUCCEEDED") {
            bail!("YouTube didn't change the playlist: {}", d["status"]);
        }
        Ok(())
    }

    /// Your YouTube Home feed (what youtube.com recommends you).
    pub fn home(&self, more: Option<&str>) -> Result<Page> {
        self.need_auth()?;
        let d = match more {
            Some(c) => self.call("browse", json!({ "continuation": c }))?,
            None => self.call("browse", json!({ "browseId": "FEwhat_to_watch" }))?,
        };
        Ok(page(&d, None))
    }

    /// The addresses for telling YouTube you're watching a video.
    pub fn tracking(&self, video: &str) -> Result<Tracking> {
        self.need_auth()?;
        let d = self.call("player", json!({ "videoId": video }))?;
        let t = &d["playbackTracking"];
        let url = |k: &str| t[k]["baseUrl"].as_str().map(str::to_string);
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()) as u64;
        let mut x = seed ^ 0x9e37_79b9_7f4a_7c15;
        let cpn = (0..16)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ALPHABET[(x & 63) as usize] as char
            })
            .collect();
        Ok(Tracking {
            playback: url("videostatsPlaybackUrl").context("no playback tracking")?,
            watchtime: url("videostatsWatchtimeUrl").context("no watchtime tracking")?,
            cpn,
        })
    }

    /// "Started watching" (puts the video in your history).
    pub fn ping_playback(&self, t: &Tracking) -> Result<()> {
        self.ping(&t.playback, &[("ver", "2".into()), ("cpn", t.cpn.clone()), ("cmt", "0".into()), ("el", "detailpage".into())])
    }

    /// "Watched from `from` to `to` seconds" (YouTube's resume point follows this).
    pub fn ping_watchtime(&self, t: &Tracking, from: f64, to: f64) -> Result<()> {
        let f = |x: f64| format!("{x:.3}");
        self.ping(
            &t.watchtime,
            &[("ver", "2".into()), ("cpn", t.cpn.clone()), ("cmt", f(to)), ("st", f(from)), ("et", f(to)), ("el", "detailpage".into())],
        )
    }

    fn ping(&self, base: &str, params: &[(&str, String)]) -> Result<()> {
        let mut url = reqwest::Url::parse(base)?;
        // ours replace any the base address already has
        let keep: Vec<(String, String)> =
            url.query_pairs().filter(|(k, _)| !params.iter().any(|(p, _)| p == k)).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        url.query_pairs_mut().clear().extend_pairs(keep).extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
        let res = self.signed(self.http.get(url)).send()?;
        if !res.status().is_success() {
            bail!("YouTube tracking: {}", res.status());
        }
        Ok(())
    }

    fn call(&self, endpoint: &str, body: Value) -> Result<Value> {
        let mut body = body;
        body["context"] = json!({ "client": { "clientName": "WEB", "clientVersion": CLIENT_VERSION, "hl": "en", "gl": "US" } });
        let res = self
            .signed(self.http.post(format!("{BASE}/{endpoint}?prettyPrint=false")))
            .header("Content-Type", "application/json")
            .header("X-YouTube-Client-Name", "1")
            .header("X-YouTube-Client-Version", CLIENT_VERSION)
            .body(body.to_string())
            .send()
            .with_context(|| format!("YouTube {endpoint}"))?;
        if !res.status().is_success() {
            bail!("YouTube {endpoint}: {}", res.status());
        }
        Ok(serde_json::from_str(&res.text()?)?)
    }

    pub fn search(&self, query: &str, more: Option<&str>) -> Result<Page> {
        let d = match more {
            Some(c) => self.call("search", json!({ "continuation": c }))?,
            None => self.call("search", json!({ "query": query }))?,
        };
        Ok(page(&d, None))
    }

    /// A channel's newest videos (and who it is, on the first page).
    pub fn channel_videos(&self, id: &str, more: Option<&str>) -> Result<(Option<ChannelInfo>, Page)> {
        let d = match more {
            Some(c) => self.call("browse", json!({ "continuation": c }))?,
            None => self.call("browse", json!({ "browseId": id, "params": TAB_VIDEOS }))?,
        };
        let info = more.is_none().then(|| channel_info(&d, id));
        let owner = info.as_ref().map(|i| (id.to_string(), i.name.clone()));
        Ok((info, page(&d, owner)))
    }

    pub fn channel_playlists(&self, id: &str, more: Option<&str>) -> Result<Page> {
        let d = match more {
            Some(c) => self.call("browse", json!({ "continuation": c }))?,
            None => self.call("browse", json!({ "browseId": id, "params": TAB_PLAYLISTS }))?,
        };
        Ok(page(&d, None))
    }

    /// A playlist: (title, who made it, its videos).
    pub fn playlist(&self, id: &str, more: Option<&str>) -> Result<(String, String, Page)> {
        let d = match more {
            Some(c) => self.call("browse", json!({ "continuation": c }))?,
            None => self.call("browse", json!({ "browseId": format!("VL{id}") }))?,
        };
        let header = find(&d, "pageHeaderViewModel").next();
        let title = header.map(|h| text(&h["title"])).unwrap_or_default();
        let by = header
            .and_then(|h| find(h, "metadataRows").next())
            .and_then(|r| r.as_array())
            .map(|rows| rows.iter().flat_map(|r| parts(r)).filter(|p| !p.is_empty()).take(2).collect::<Vec<_>>().join(" · "))
            .unwrap_or_default();
        Ok((title, by, page(&d, None)))
    }

    /// Everything about a video for the Watch page, plus what YouTube relates to it.
    pub fn details(&self, id: &str) -> Result<Details> {
        let d = self.call("next", json!({ "videoId": id }))?;
        let mut out = Details { id: id.to_string(), ..Default::default() };
        if let Some(p) = find(&d, "videoPrimaryInfoRenderer").next() {
            out.title = text(&p["title"]);
            out.views = parse_count(&text(&p["viewCount"]["videoViewCountRenderer"]["viewCount"]));
            out.date = text(&p["dateText"]);
            // the like button's label ("74K"); the first short number on it
            out.likes = find(&p["videoActions"], "likeButtonViewModel")
                .next()
                .and_then(|l| find(l, "title").find_map(|t| t.as_str().filter(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))))
                .unwrap_or_default()
                .to_string();
        }
        if let Some(s) = find(&d, "videoSecondaryInfoRenderer").next() {
            let o = &s["owner"]["videoOwnerRenderer"];
            out.channel_name = text(&o["title"]);
            out.channel_id = o["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str().unwrap_or_default().to_string();
            out.channel_avatar = best_image(&o["thumbnail"]);
            out.subs = text(&o["subscriberCountText"]);
            let desc = &s["attributedDescription"];
            out.description = desc["content"].as_str().unwrap_or_default().to_string();
            out.links = links(desc, id);
        }
        out.chapters = find(&d, "macroMarkersListItemRenderer")
            .filter_map(|m| {
                let at = m["onTap"]["watchEndpoint"]["startTimeSeconds"].as_f64().or_else(|| parse_duration(&text(&m["timeDescription"])).map(f64::from))?;
                Some((text(&m["title"]), at))
            })
            .collect();
        // chapters are listed twice in the response (panel + player bar)
        out.chapters.dedup_by(|a, b| a.1 == b.1);
        let mut seen = std::collections::HashSet::new();
        out.chapters.retain(|c| seen.insert(c.1 as i64));
        if let Some(sec) = find(&d, "secondaryResults").next() {
            out.related = page(sec, None).videos;
        }
        // the comments panel: count, where they start, and "Newest first"
        if let Some(panel) = d["engagementPanels"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| &p["engagementPanelSectionListRenderer"])
            .find(|p| p["panelIdentifier"] == "engagement-panel-comments-section")
        {
            let header = &panel["header"]["engagementPanelTitleHeaderRenderer"];
            out.comment_count = text(&header["contextualInfo"]);
            out.comments_top = find(&panel["content"], "continuationCommand").next().and_then(|c| c["token"].as_str()).map(str::to_string);
            out.comments_newest = header["menu"]["sortFilterSubMenuRenderer"]["subMenuItems"]
                .get(1)
                .and_then(|i| i["serviceEndpoint"]["continuationCommand"]["token"].as_str())
                .map(str::to_string);
        }
        Ok(out)
    }

    /// A page of comments, or of one comment's replies (read only).
    pub fn comments(&self, token: &str) -> Result<Comments> {
        let d = self.call("next", json!({ "continuation": token }))?;
        Ok(parse_comments(&d))
    }
}

fn parse_comments(d: &Value) -> Comments {
    // the comments' words and authors come separately, keyed
    let mut payloads = std::collections::HashMap::new();
    let mut hearts = std::collections::HashSet::new();
    for m in d["frameworkUpdates"]["entityBatchUpdate"]["mutations"].as_array().into_iter().flatten() {
        let p = &m["payload"];
        if let Some(c) = p.get("commentEntityPayload") {
            if let Some(k) = c["key"].as_str() {
                payloads.insert(k.to_string(), c);
            }
        }
        if let Some(t) = p.get("engagementToolbarStateEntityPayload") {
            if t["heartState"] == "TOOLBAR_HEART_STATE_HEARTED" {
                if let Some(k) = t["key"].as_str() {
                    hearts.insert(k.to_string());
                }
            }
        }
    }
    let mut out = Comments::default();
    let items = d["onResponseReceivedEndpoints"].as_array().into_iter().flatten().flat_map(|e| {
        let a = &e["reloadContinuationItemsCommand"]["continuationItems"];
        let b = &e["appendContinuationItemsAction"]["continuationItems"];
        a.as_array().into_iter().flatten().chain(b.as_array().into_iter().flatten())
    });
    for item in items {
        let (vm, replies_token) = if let Some(t) = item.get("commentThreadRenderer") {
            let token = find(&t["replies"], "continuationCommand").next().and_then(|c| c["token"].as_str()).map(str::to_string);
            (&t["commentViewModel"]["commentViewModel"], token)
        } else if let Some(vm) = item.get("commentViewModel") {
            (vm, None)
        } else {
            if item.get("continuationItemRenderer").is_some() {
                out.more = find(item, "continuationCommand").next().and_then(|c| c["token"].as_str()).map(str::to_string);
            }
            continue;
        };
        let Some(p) = vm["commentKey"].as_str().and_then(|k| payloads.get(k)) else { continue };
        let props = &p["properties"];
        let author = &p["author"];
        let toolbar = &p["toolbar"];
        out.list.push(Comment {
            id: props["commentId"].as_str().unwrap_or_default().to_string(),
            author: author["displayName"].as_str().unwrap_or_default().to_string(),
            channel_id: author["channelId"].as_str().unwrap_or_default().to_string(),
            avatar: author["avatarThumbnailUrl"].as_str().map(str::to_string),
            text: props["content"]["content"].as_str().unwrap_or_default().to_string(),
            when: props["publishedTime"].as_str().unwrap_or_default().to_string(),
            likes: toolbar["likeCountNotliked"].as_str().unwrap_or_default().trim().to_string(),
            replies: toolbar["replyCount"].as_str().unwrap_or_default().trim().to_string(),
            replies_token,
            creator: author["isCreator"].as_bool().unwrap_or(false),
            pinned: vm.get("pinnedText").is_some(),
            hearted: vm["toolbarStateKey"].as_str().is_some_and(|k| hearts.contains(k)),
            reply: props["replyLevel"].as_u64().unwrap_or(0) > 0,
        });
    }
    out
}

#[cfg(test)]
mod comment_tests {
    /// Reads real comments (no account). `cargo test -p linen-core live_comments -- --ignored`
    #[test]
    #[ignore]
    fn live_comments() {
        let yt = super::Yt::new().unwrap();
        let d = yt.details("9bZkp7q19f0").unwrap();
        println!("count {:?}", d.comment_count);
        let c = yt.comments(d.comments_top.as_deref().unwrap()).unwrap();
        assert!(c.list.len() >= 10, "{}", c.list.len());
        let first = &c.list[0];
        println!("{} {} {} likes {} replies: {}", first.author, first.when, first.likes, first.replies, first.text);
        assert!(!first.text.is_empty() && !first.author.is_empty());
        assert!(c.more.is_some());
        let with = c.list.iter().find(|c| c.replies_token.is_some()).unwrap();
        let r = yt.comments(with.replies_token.as_deref().unwrap()).unwrap();
        assert!(!r.list.is_empty() && r.list.iter().all(|c| c.reply));
        let n = yt.comments(d.comments_newest.as_deref().unwrap()).unwrap();
        println!("newest: {} {}", n.list[0].when, n.list[0].text);
        assert!(!n.list.is_empty());
    }
}

// ---- reading the responses ----

/// Every value under `key`, anywhere in the tree (depth-first, in order).
fn find<'a>(v: &'a Value, key: &'a str) -> Box<dyn Iterator<Item = &'a Value> + 'a> {
    match v {
        Value::Object(m) => Box::new(m.iter().flat_map(move |(k, v)| {
            let here: Box<dyn Iterator<Item = &Value>> = if k == key { Box::new(std::iter::once(v)) } else { Box::new(std::iter::empty()) };
            here.chain(find(v, key))
        })),
        Value::Array(a) => Box::new(a.iter().flat_map(move |v| find(v, key))),
        _ => Box::new(std::iter::empty()),
    }
}

/// Text in any of YouTube's three shapes: simpleText, runs, content.
fn text(v: &Value) -> String {
    if let Some(s) = v["simpleText"].as_str() {
        return s.to_string();
    }
    if let Some(r) = v["runs"].as_array() {
        return r.iter().filter_map(|r| r["text"].as_str()).collect();
    }
    if let Some(s) = v["content"].as_str() {
        return s.to_string();
    }
    if let Some(s) = v["dynamicTextViewModel"]["text"]["content"].as_str() {
        return s.to_string();
    }
    v.as_str().unwrap_or_default().to_string()
}

/// The texts of one metadata row of the newer "lockup" layout.
fn parts(row: &Value) -> Vec<String> {
    row["metadataParts"].as_array().map(|p| p.iter().map(|p| text(&p["text"])).collect()).unwrap_or_default()
}

fn best_image(v: &Value) -> Option<String> {
    let list = v["thumbnails"].as_array().or_else(|| v["sources"].as_array())?;
    let url = list.iter().max_by_key(|t| t["width"].as_u64().unwrap_or(0))?["url"].as_str()?;
    Some(if url.starts_with("//") { format!("https:{url}") } else { url.to_string() })
}

/// "2,998,592 views", "2.9M views", "10M", "326K views" -> number
pub fn parse_count(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.starts_with("No ") {
        return Some(0);
    }
    let num: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',').filter(|c| *c != ',').collect();
    let n: f64 = num.parse().ok()?;
    let mult = match s[num.len().min(s.len())..].trim_start().chars().next() {
        _ if s.contains(',') => 1.0,
        Some('K') => 1e3,
        Some('M') => 1e6,
        Some('B') => 1e9,
        _ => 1.0,
    };
    Some((n * mult) as u64)
}

/// "4:19:10" -> seconds
pub fn parse_duration(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() || !s.contains(':') {
        return None;
    }
    s.split(':').try_fold(0u32, |acc, p| Some(acc * 60 + p.trim().parse::<u32>().ok()?))
}

/// "2 weeks ago", "15y ago", "Streamed 3 days ago" -> approximate unix time
pub fn parse_ago(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches("Streamed ").trim_start_matches("Premiered ");
    if !s.ends_with("ago") {
        return None;
    }
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: i64 = digits.parse().ok()?;
    let unit = s[digits.len()..].trim_start().trim_end_matches("ago").trim();
    let secs = match unit.trim_end_matches('s') {
        "second" | "sec" => 1,
        "minute" | "min" | "m" => 60,
        "hour" | "hr" | "h" => 3600,
        "day" | "d" => 86400,
        "week" | "wk" | "w" => 604800,
        "month" | "mo" => 2_629_800,
        "year" | "yr" | "y" => 31_557_600,
        _ => return None,
    };
    Some(crate::now() - n * secs)
}

/// The videos, channels and playlists of a response, in order, without Shorts.
/// `owner` fills in the channel on a channel's own page (its cards don't say).
fn page(d: &Value, owner: Option<(String, String)>) -> Page {
    let mut p = Page::default();
    let mut seen = std::collections::HashSet::new();
    walk(d, &mut p, &mut seen, &owner);
    // the "load more" marker at the end of the list (other tokens are filters etc.)
    p.more = find(d, "continuationItemRenderer")
        .filter_map(|c| find(c, "continuationCommand").find_map(|c| c["token"].as_str()))
        .last()
        .map(str::to_string);
    p
}

fn walk(v: &Value, p: &mut Page, seen: &mut std::collections::HashSet<String>, owner: &Option<(String, String)>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                match k.as_str() {
                    // Shorts: never shown in Linen
                    "reelShelfRenderer" | "shortsLockupViewModel" | "reelItemRenderer" | "gridShelfViewModel" => continue,
                    "videoRenderer" | "compactVideoRenderer" | "gridVideoRenderer" | "playlistVideoRenderer" => {
                        if let Some(h) = old_video(v, owner).filter(|h| seen.insert(h.id.clone())) {
                            p.videos.push(h);
                        }
                        continue;
                    }
                    "lockupViewModel" => {
                        match v["contentType"].as_str() {
                            Some("LOCKUP_CONTENT_TYPE_VIDEO") => {
                                if let Some(h) = lockup_video(v, owner).filter(|h| seen.insert(h.id.clone())) {
                                    p.videos.push(h);
                                }
                            }
                            Some("LOCKUP_CONTENT_TYPE_PLAYLIST") => {
                                if let Some(h) = lockup_playlist(v).filter(|h| seen.insert(h.id.clone())) {
                                    p.playlists.push(h);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    "channelRenderer" => {
                        let id = v["channelId"].as_str().unwrap_or_default().to_string();
                        if !id.is_empty() && seen.insert(id.clone()) {
                            // YouTube puts the handle in "subscriberCountText" and the count in "videoCountText"
                            let a = text(&v["subscriberCountText"]);
                            let b = text(&v["videoCountText"]);
                            let subs = [a, b].into_iter().filter(|s| s.contains("subscriber")).next().unwrap_or_default();
                            p.channels.push(ChannelHit { id, name: text(&v["title"]), avatar: best_image(&v["thumbnail"]), subs });
                        }
                        continue;
                    }
                    _ => walk(v, p, seen, owner),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|v| walk(v, p, seen, owner)),
        _ => {}
    }
}

fn old_video(v: &Value, owner: &Option<(String, String)>) -> Option<VideoHit> {
    let id = v["videoId"].as_str()?.to_string();
    let by = if v["longBylineText"].is_object() { &v["longBylineText"] } else { &v["shortBylineText"] };
    let channel_id = by["runs"][0]["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str().map(str::to_string);
    let len = text(&v["lengthText"]);
    let live = v["badges"].to_string().contains("LIVE") || (len.is_empty() && text(&v["viewCountText"]).contains("watching"));
    let (oid, oname) = owner.clone().unzip();
    let watched_pct = find(&v["thumbnailOverlays"], "percentDurationWatched").find_map(|p| p.as_u64()).map(|p| p.min(100) as u8);
    Some(VideoHit {
        watched_pct,
        title: text(&v["title"]),
        channel_id: channel_id.or(oid).unwrap_or_default(),
        channel_name: Some(text(by)).filter(|s| !s.is_empty()).or(oname).unwrap_or_default(),
        published: parse_ago(&text(&v["publishedTimeText"])).unwrap_or(0),
        views: parse_count(&text(&v["viewCountText"])),
        duration: parse_duration(&len),
        live,
        id,
    })
}

fn lockup_video(v: &Value, owner: &Option<(String, String)>) -> Option<VideoHit> {
    let id = v["contentId"].as_str()?.to_string();
    let meta = &v["metadata"]["lockupMetadataViewModel"];
    let mut h = VideoHit { id, title: text(&meta["title"]), ..Default::default() };
    // the channel: from the avatar's link (not there on the channel's own page)
    h.channel_id = find(&meta["image"], "browseId").next().and_then(|b| b.as_str()).map(str::to_string).unwrap_or_default();
    for row in meta["metadata"]["contentMetadataViewModel"]["metadataRows"].as_array().into_iter().flatten() {
        for part in parts(row) {
            if part.ends_with("ago") {
                h.published = parse_ago(&part).unwrap_or(0);
            } else if part.contains("watching") {
                h.live = true;
            } else if part.contains("view") || (h.views.is_none() && parse_count(&part).is_some() && !h.channel_name.is_empty()) {
                h.views = parse_count(&part);
            } else if h.channel_name.is_empty() && !part.is_empty() {
                h.channel_name = part;
            }
        }
    }
    h.watched_pct = find(&v["contentImage"], "thumbnailOverlayProgressBarViewModel")
        .find_map(|p| p["startPercent"].as_u64())
        .map(|p| p.min(100) as u8);
    for badge in find(&v["contentImage"], "thumbnailBadgeViewModel") {
        let t = badge["text"].as_str().unwrap_or_default();
        if let Some(d) = parse_duration(t) {
            h.duration = Some(d);
        } else if t == "LIVE" || badge["badgeStyle"].as_str().is_some_and(|s| s.contains("LIVE")) {
            h.live = true;
        }
    }
    if let Some((oid, oname)) = owner {
        if h.channel_id.is_empty() {
            h.channel_id = oid.clone();
            // on a channel page the first row is views, not the channel
            if h.channel_name.is_empty() || h.channel_name != *oname {
                h.channel_name = oname.clone();
            }
        }
    }
    Some(h)
}

fn lockup_playlist(v: &Value) -> Option<PlaylistHit> {
    let id = v["contentId"].as_str()?.to_string();
    let meta = &v["metadata"]["lockupMetadataViewModel"];
    let count = find(&v["contentImage"], "thumbnailBadgeViewModel")
        .filter_map(|b| b["text"].as_str())
        .find(|t| t.contains("video") || t.contains("episode") || t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or_default()
        .to_string();
    let first_video = find(v, "watchEndpoint").find_map(|w| w["videoId"].as_str()).map(str::to_string);
    let thumb = find(&v["contentImage"], "image").find_map(best_image);
    Some(PlaylistHit { id, title: text(&meta["title"]), count, first_video, thumb })
}

fn channel_info(d: &Value, id: &str) -> ChannelInfo {
    let mut c = ChannelInfo { id: id.to_string(), ..Default::default() };
    if let Some(h) = find(d, "pageHeaderViewModel").next() {
        c.name = text(&h["title"]);
        c.avatar = find(&h["image"], "image").find_map(best_image);
        c.banner = find(&h["banner"], "image").find_map(best_image);
        c.meta = h["metadata"]["contentMetadataViewModel"]["metadataRows"]
            .as_array()
            .map(|rows| rows.iter().flat_map(|r| parts(r)).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · "))
            .unwrap_or_default();
        c.about = text(&h["description"]["descriptionPreviewViewModel"]["description"]);
    }
    if c.name.is_empty() {
        if let Some(m) = find(d, "channelMetadataRenderer").next() {
            c.name = m["title"].as_str().unwrap_or_default().to_string();
            c.about = m["description"].as_str().unwrap_or_default().to_string();
            c.avatar = best_image(&m["avatar"]);
        }
    }
    c
}

/// Links in a description: web links, other videos, and timestamps in this one.
/// (Positions are counted in UTF-16 units, as in JavaScript.)
fn links(desc: &Value, this_video: &str) -> Vec<(String, Link)> {
    let content: Vec<u16> = desc["content"].as_str().unwrap_or_default().encode_utf16().collect();
    let mut out = vec![];
    for run in desc["commandRuns"].as_array().into_iter().flatten() {
        let (Some(start), Some(len)) = (run["startIndex"].as_u64(), run["length"].as_u64()) else { continue };
        let (start, len) = (start as usize, len as usize);
        let Some(slice) = content.get(start..(start + len).min(content.len())) else { continue };
        let label = String::from_utf16_lossy(slice).trim().to_string();
        let cmd = &run["onTap"]["innertubeCommand"];
        if let Some(w) = cmd.get("watchEndpoint") {
            let vid = w["videoId"].as_str().unwrap_or(this_video);
            match w["startTimeSeconds"].as_f64() {
                Some(t) if vid == this_video => {
                    // a timestamp: label it with the rest of its line ("4:32 Lighting")
                    let line_end = content[start + len..].iter().position(|&c| c == '\n' as u16).map_or(content.len(), |p| start + len + p);
                    let line_start = content[..start].iter().rposition(|&c| c == '\n' as u16).map_or(0, |p| p + 1);
                    let line = String::from_utf16_lossy(&content[line_start..line_end]).trim().to_string();
                    out.push((if line.is_empty() { label } else { line }, Link::Time(t)));
                }
                None if vid == this_video => out.push((label, Link::Time(0.0))),
                _ => out.push((label, Link::Video(vid.to_string()))),
            }
        } else if let Some(u) = cmd["urlEndpoint"]["url"].as_str() {
            // YouTube wraps outside links in its redirect page; take the real address
            let real = u
                .split(['?', '&'])
                .find_map(|kv| kv.strip_prefix("q="))
                .map(percent_decode)
                .unwrap_or_else(|| u.to_string());
            out.push((label, Link::Url(real)));
        } else if let Some(b) = cmd["browseEndpoint"]["browseId"].as_str() {
            if b.starts_with("UC") {
                out.push((label, Link::Url(format!("https://www.youtube.com/channel/{b}"))));
            }
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 0 && i + 2 <= b.len() - 1 {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(parse_count("2,998,592 views"), Some(2_998_592));
        assert_eq!(parse_count("2.9M views"), Some(2_900_000));
        assert_eq!(parse_count("326K views"), Some(326_000));
        assert_eq!(parse_count("10M"), Some(10_000_000));
        assert_eq!(parse_count("No views"), Some(0));
        assert_eq!(parse_duration("4:19:10"), Some(15550));
        assert_eq!(parse_duration("9:57"), Some(597));
        assert_eq!(parse_duration("LIVE"), None);
        let now = crate::now();
        assert!((parse_ago("2 weeks ago").unwrap() - (now - 2 * 604800)).abs() < 5);
        assert!((parse_ago("15y ago").unwrap() - (now - 15 * 31_557_600)).abs() < 5);
        assert!(parse_ago("Streamed 3 days ago").is_some());
        assert_eq!(percent_decode("https%3A%2F%2Fexample.com%2Fa+b"), "https://example.com/a b");
    }

    /// Talks to YouTube: cargo test -p linen-core -- --ignored
    /// Uses your Firefox sign-in, READ-ONLY (prints only counts):
    /// cargo test -p linen-core live_account -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_account() {
        let (_, cookies) = crate::account::find_sign_in().unwrap();
        let yt = Yt::new().unwrap();
        yt.set_auth(Some(crate::account::Auth { cookies, ..Default::default() }));
        let ids = yt.identities().unwrap();
        println!("identities: {} (slots {:?})", ids.len(), ids.iter().map(|i| i.authuser).collect::<Vec<_>>());
        assert!(!ids.is_empty());
        // each choice really acts as that account
        let base = yt.auth.read().unwrap().clone().unwrap();
        for i in &ids {
            yt.set_auth(Some(crate::account::Auth { authuser: i.authuser, page_id: i.page_id.clone(), ..base.clone() }));
            let who = yt.account().unwrap();
            assert_eq!(who.handle, i.handle, "slot {}", i.authuser);
        }
        println!("each identity answers as itself");
        yt.set_auth(Some(base));
        let a = yt.account().unwrap();
        assert!(!a.name.is_empty());
        let subs = yt.subscriptions().unwrap();
        println!("subscriptions: {}", subs.len());
        assert!(!subs.is_empty() && subs.iter().all(|c| c.id.starts_with("UC")));
        let h = yt.history(1).unwrap();
        println!("history: {} videos, {} with progress", h.len(), h.iter().filter(|v| v.watched_pct.is_some()).count());
        assert!(!h.is_empty());
        let home = yt.home(None).unwrap();
        println!("home: {} videos, more: {}", home.videos.len(), home.more.is_some());
        assert!(home.videos.len() >= 10);
        let wl = yt.watch_later().unwrap();
        println!("watch later: {} videos", wl.len());
        assert!(wl.iter().all(|v| v.id.len() == 11));
        let t = yt.tracking("aqz-KE-bpKQ").unwrap();
        assert!(t.playback.starts_with("https://") && t.watchtime.starts_with("https://") && t.cpn.len() == 16);
    }

    /// Read-only: your Watch later on the account Linen is connected to.
    #[test]
    #[ignore]
    fn live_watch_later() {
        let (_, cookies) = crate::account::find_sign_in().unwrap();
        let yt = Yt::new().unwrap();
        let authuser = std::env::var("LINEN_TEST_AUTHUSER").ok().and_then(|a| a.parse().ok()).unwrap_or(0);
        let page_id = std::env::var("LINEN_TEST_PAGE_ID").ok();
        yt.set_auth(Some(crate::account::Auth { cookies, authuser, page_id }));
        println!("as {}", yt.account().unwrap().handle);
        let wl = yt.watch_later().unwrap();
        println!("watch later: {} videos", wl.len());
        for v in wl.iter().take(5) {
            println!("  {} {:?} {} · {}", v.id, v.duration, v.title, v.channel_name);
        }
        assert!(wl.iter().all(|v| v.id.len() == 11 && !v.title.is_empty()));
    }

    /// Read-only: the playlists on the account Linen is connected to.
    #[test]
    #[ignore]
    fn live_my_playlists() {
        let (_, cookies) = crate::account::find_sign_in().unwrap();
        let yt = Yt::new().unwrap();
        let authuser = std::env::var("LINEN_TEST_AUTHUSER").ok().and_then(|a| a.parse().ok()).unwrap_or(0);
        let page_id = std::env::var("LINEN_TEST_PAGE_ID").ok();
        yt.set_auth(Some(crate::account::Auth { cookies, authuser, page_id }));
        println!("as {}", yt.account().unwrap().handle);
        if std::env::var("LINEN_TEST_DUMP").is_ok() {
            let d = yt.call("browse", serde_json::json!({ "browseId": "FEplaylist_aggregation" })).unwrap();
            for l in find(&d, "lockupViewModel") {
                println!("RAW {}", serde_json::to_string(&l["metadata"]).unwrap());
            }
        }
        let lists = yt.my_playlists().unwrap();
        println!("{} playlists", lists.len());
        for (p, own) in &lists {
            println!("  {} {:?} {:?} own={own}", p.id, p.title, p.count);
        }
        if let Some((p, _)) = lists.iter().find(|l| l.1) {
            let v = yt.playlist_all(&p.id).unwrap();
            println!("{}: {} videos, first {:?}", p.title, v.len(), v.first().map(|v| &v.title));
        }
    }

    #[test]
    #[ignore]
    fn live_related() {
        let d = Yt::new().unwrap().details("z-Xl9tGqH14").unwrap();
        for v in &d.related {
            println!("{:<12} ch={:<26} {:<20} dur={:?} views={:?} pub={} live={} {}", v.id, v.channel_id, v.channel_name.chars().take(18).collect::<String>(), v.duration, v.views, v.published, v.live, v.title.chars().take(40).collect::<String>());
        }
    }

    #[test]
    #[ignore]
    fn live_api() {
        let yt = Yt::new().unwrap();
        let s = yt.search("blender donut tutorial", None).unwrap();
        assert!(s.videos.len() >= 5, "search: {} videos", s.videos.len());
        assert!(s.videos.iter().all(|v| !v.title.is_empty()));
        assert!(s.videos.iter().filter(|v| !v.channel_id.is_empty() && v.views.is_some()).count() >= 5);
        assert!(s.more.is_some());
        let s2 = yt.search("", s.more.as_deref()).unwrap();
        assert!(!s2.videos.is_empty(), "search page 2");
        let (info, vids) = yt.channel_videos("UCSMOQeBJ2RAnuFungnQOxLg", None).unwrap();
        let info = info.unwrap();
        assert_eq!(info.name, "Blender");
        assert!(info.avatar.is_some() && info.meta.contains("subscribers"), "{info:?}");
        assert!(vids.videos.len() >= 20 && vids.videos.iter().all(|v| v.channel_id == info.id));
        assert!(vids.videos.iter().filter(|v| v.published > 0 && v.duration.is_some()).count() >= 20);
        let pl = yt.channel_playlists("UCSMOQeBJ2RAnuFungnQOxLg", None).unwrap();
        assert!(!pl.playlists.is_empty());
        let (title, _, items) = yt.playlist(&pl.playlists[0].id, None).unwrap();
        assert!(!title.is_empty() && !items.videos.is_empty(), "playlist {title}");
        let d = yt.details("z-Xl9tGqH14").unwrap();
        assert_eq!(d.channel_id, "UCOKHwx1VCdgnxwbjyb9Iu1g");
        assert!(d.chapters.len() >= 5, "{:?}", d.chapters);
        assert!(d.links.iter().any(|l| matches!(l.1, Link::Url(ref u) if u.contains("blenderguru"))), "{:?}", d.links);
        assert!(d.related.len() >= 5);
        assert!(!d.likes.is_empty() && d.views.is_some() && !d.date.is_empty(), "{} {:?} {}", d.likes, d.views, d.date);
    }
}
