use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{Channel, Video};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS channels (
    id      TEXT PRIMARY KEY,
    name    TEXT NOT NULL,
    avatar  TEXT,
    added   INTEGER NOT NULL,
    checked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS videos (
    id          TEXT PRIMARY KEY,
    channel_id  TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    title       TEXT NOT NULL,
    published   INTEGER NOT NULL,
    thumbnail   TEXT NOT NULL,
    views       INTEGER,
    description TEXT NOT NULL DEFAULT '',
    seen        INTEGER NOT NULL DEFAULT 0,
    position    REAL NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS videos_published ON videos(published DESC);
-- every video you opened, from anywhere (feed, search, channel pages...)
CREATE TABLE IF NOT EXISTS history (
    id           TEXT PRIMARY KEY,
    title        TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    channel_name TEXT NOT NULL,
    duration     REAL NOT NULL DEFAULT 0,
    position     REAL NOT NULL DEFAULT 0,
    -- furthest point reached (how much of it you watched)
    furthest     REAL NOT NULL DEFAULT 0,
    watched_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS history_watched ON history(watched_at DESC);
-- not interested: kind = video | channel
CREATE TABLE IF NOT EXISTS hidden (
    kind TEXT NOT NULL,
    id   TEXT NOT NULL,
    at   INTEGER NOT NULL,
    PRIMARY KEY (kind, id)
);
-- the last For you picks, in order
CREATE TABLE IF NOT EXISTS foryou (
    rank         INTEGER PRIMARY KEY,
    id           TEXT NOT NULL,
    title        TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    channel_name TEXT NOT NULL,
    published    INTEGER NOT NULL,
    views        INTEGER,
    duration     INTEGER,
    reason       TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- your likes (1) and dislikes (-1); local only, For you learns from them
CREATE TABLE IF NOT EXISTS ratings (
    id     TEXT PRIMARY KEY,
    rating INTEGER NOT NULL,
    at     INTEGER NOT NULL
);
-- scheduled streams/premieres (not out yet): what to show, and when it was checked
CREATE TABLE IF NOT EXISTS upcoming (
    id      TEXT PRIMARY KEY,
    label   TEXT NOT NULL,
    checked INTEGER NOT NULL
);
-- DeArrow answers, so each video is asked about rarely
CREATE TABLE IF NOT EXISTS branding (
    id       TEXT PRIMARY KEY,
    title    TEXT,
    thumb_at REAL,
    fetched  INTEGER NOT NULL
);
-- Watch later: saved to watch another time (yt: also on your YouTube Watch later;
-- imported: came from there, so it goes again when the account is switched)
CREATE TABLE IF NOT EXISTS later (
    id           TEXT PRIMARY KEY,
    title        TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    channel_name TEXT NOT NULL,
    published    INTEGER NOT NULL DEFAULT 0,
    views        INTEGER,
    duration     INTEGER,
    added        INTEGER NOT NULL,
    yt           INTEGER NOT NULL DEFAULT 0,
    imported     INTEGER NOT NULL DEFAULT 0
);
-- videos saved to watch without a connection (done: the file is complete)
CREATE TABLE IF NOT EXISTS downloads (
    id           TEXT PRIMARY KEY,
    title        TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    channel_name TEXT NOT NULL,
    published    INTEGER NOT NULL DEFAULT 0,
    views        INTEGER,
    duration     INTEGER,
    path         TEXT NOT NULL,
    bytes        INTEGER NOT NULL DEFAULT 0,
    done         INTEGER NOT NULL DEFAULT 0,
    added        INTEGER NOT NULL
);
-- your playlists: made in Linen (id local:<n>), or on your YouTube account
-- (yt: yours, kept in sync; saved: someone else's you saved there, read only)
CREATE TABLE IF NOT EXISTS playlists (
    id      TEXT PRIMARY KEY,
    name    TEXT NOT NULL,
    yt      INTEGER NOT NULL DEFAULT 0,
    saved   INTEGER NOT NULL DEFAULT 0,
    count   TEXT NOT NULL DEFAULT '',
    first   TEXT,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS playlist_items (
    playlist     TEXT NOT NULL,
    id           TEXT NOT NULL,
    title        TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    channel_name TEXT NOT NULL,
    published    INTEGER NOT NULL DEFAULT 0,
    views        INTEGER,
    duration     INTEGER,
    pos          INTEGER NOT NULL,
    PRIMARY KEY (playlist, id)
);
-- stream links found for videos (good for ~6 h): kept so they work after a restart too
CREATE TABLE IF NOT EXISTS links (
    id      TEXT PRIMARY KEY,
    data    TEXT NOT NULL,
    quality INTEGER NOT NULL,
    found   INTEGER NOT NULL,
    expires INTEGER NOT NULL
);
-- videos watched from the feed before there was a history (cheap, idempotent)
INSERT OR IGNORE INTO history (id, title, channel_id, channel_name, position, furthest, watched_at)
    SELECT v.id, v.title, v.channel_id, c.name, v.position, v.position, v.published
    FROM videos v JOIN channels c ON c.id = v.channel_id WHERE v.position > 0;
";

/// A video you opened.
#[derive(Debug, Clone, Default)]
pub struct Watched {
    pub id: String,
    pub title: String,
    pub channel_id: String,
    pub channel_name: String,
    pub duration: f64,
    pub position: f64,
    pub furthest: f64,
    pub watched_at: i64,
}

/// One "For you" pick and why it's there.
#[derive(Debug, Clone)]
pub struct Pick {
    pub video: crate::yt::VideoHit,
    pub reason: String,
}

/// A downloaded (or downloading) video.
#[derive(Debug, Clone)]
pub struct Download {
    pub video: crate::yt::VideoHit,
    pub path: String,
    pub bytes: u64,
    pub done: bool,
}

/// A playlist of yours (or saved from YouTube).
#[derive(Debug, Clone, Default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    /// yours, on your YouTube account (kept in sync)
    pub yt: bool,
    /// someone else's, saved on your YouTube account (read only, opened from YouTube)
    pub saved: bool,
    /// for saved ones: YouTube's "13 videos"
    pub count: String,
    /// a video in it, for the picture
    pub first: Option<String>,
    pub videos: usize,
}

fn hit_from(r: &rusqlite::Row) -> rusqlite::Result<crate::yt::VideoHit> {
    Ok(crate::yt::VideoHit {
        id: r.get(0)?,
        title: r.get(1)?,
        channel_id: r.get(2)?,
        channel_name: r.get(3)?,
        published: r.get(4)?,
        views: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
        duration: r.get::<_, Option<i64>>(6)?.map(|v| v as u32),
        ..Default::default()
    })
}

/// Columns added after the first release (adding one twice fails harmlessly).
fn upgrade(conn: &Connection) {
    // channels: also subscribed on your YouTube account (1), or only in Linen (0)
    let _ = conn.execute("ALTER TABLE channels ADD COLUMN yt INTEGER NOT NULL DEFAULT 0", []);
    // brought in from a YouTube account by sync (removed again when it's switched or disconnected)
    let _ = conn.execute("ALTER TABLE channels ADD COLUMN imported INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE history ADD COLUMN imported INTEGER NOT NULL DEFAULT 0", []);
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open() -> Result<Self> {
        Self::open_at(&crate::paths::data_dir().join("linen.db"))
    }

    pub fn open_at(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        upgrade(&conn);
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        upgrade(&conn);
        Ok(Self { conn })
    }

    pub fn add_channel(&self, c: &Channel) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT INTO channels (id, name, avatar, added) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET name = ?2, avatar = COALESCE(?3, avatar)",
            params![c.id, c.name, c.avatar, crate::now()],
        )?;
        Ok(n > 0)
    }

    pub fn remove_channel(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM channels WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn channels(&self) -> Result<Vec<Channel>> {
        let mut st = self.conn.prepare("SELECT id, name, avatar FROM channels ORDER BY name COLLATE NOCASE")?;
        let rows = st.query_map([], |r| Ok(Channel { id: r.get(0)?, name: r.get(1)?, avatar: r.get(2)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_avatar(&self, channel: &str, url: &str) -> Result<()> {
        self.conn.execute("UPDATE channels SET avatar = ?2 WHERE id = ?1", params![channel, url])?;
        Ok(())
    }

    /// Store what a feed returned; returns how many videos were new.
    pub fn store_feed(&self, channel: &str, name: Option<&str>, videos: &[Video]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            tx.execute("UPDATE channels SET name = ?2 WHERE id = ?1", params![channel, n])?;
        }
        tx.execute("UPDATE channels SET checked = ?2 WHERE id = ?1", params![channel, crate::now()])?;
        let mut new = 0;
        for v in videos {
            let known: bool = tx
                .query_row("SELECT 1 FROM videos WHERE id = ?1", params![v.id], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
            tx.execute(
                "INSERT INTO videos (id, channel_id, title, published, thumbnail, views, description)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET title = ?3, thumbnail = ?5, views = COALESCE(?6, views), description = ?7",
                params![v.id, channel, v.title, v.published, v.thumbnail, v.views.map(|x| x as i64), v.description],
            )?;
            if !known {
                new += 1;
            }
        }
        tx.commit()?;
        Ok(new)
    }

    /// The feed: newest first.
    pub fn feed(&self, limit: usize) -> Result<Vec<Video>> {
        let mut st = self.conn.prepare(
            "SELECT v.id, v.channel_id, c.name, v.title, v.published, v.thumbnail, v.views, v.description, v.seen,
                    MAX(v.position, COALESCE(h.position, 0)), h.duration
             FROM videos v JOIN channels c ON c.id = v.channel_id
             LEFT JOIN history h ON h.id = v.id
             ORDER BY v.published DESC LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit as i64], |r| {
            Ok(Video {
                id: r.get(0)?,
                channel_id: r.get(1)?,
                channel_name: r.get(2)?,
                title: r.get(3)?,
                published: r.get(4)?,
                thumbnail: r.get(5)?,
                views: r.get::<_, Option<i64>>(6)?.map(|x| x as u64),
                description: r.get(7)?,
                seen: r.get::<_, i64>(8)? != 0,
                position: r.get(9)?,
                duration: r.get::<_, Option<f64>>(10)?.filter(|d| *d > 0.0).map(|d| d as u32),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn video(&self, id: &str) -> Result<Option<Video>> {
        Ok(self.feed(10_000)?.into_iter().find(|v| v.id == id))
    }

    /// Everything currently in the feed has been looked at.
    pub fn mark_all_seen(&self) -> Result<()> {
        self.conn.execute("UPDATE videos SET seen = 1 WHERE seen = 0", [])?;
        Ok(())
    }

    pub fn mark_all_seen_for(&self, channel: &str) -> Result<()> {
        self.conn.execute("UPDATE videos SET seen = 1 WHERE channel_id = ?1", params![channel])?;
        Ok(())
    }

    pub fn set_position(&self, id: &str, secs: f64) -> Result<()> {
        self.conn.execute("UPDATE videos SET position = ?2 WHERE id = ?1", params![id, secs])?;
        self.conn.execute(
            "UPDATE history SET position = ?2, furthest = MAX(furthest, ?2) WHERE id = ?1",
            params![id, secs],
        )?;
        Ok(())
    }

    pub fn set_duration(&self, id: &str, secs: f64) -> Result<()> {
        self.conn.execute("UPDATE history SET duration = ?2 WHERE id = ?1", params![id, secs])?;
        Ok(())
    }

    /// Where you left off in any video (0 = never watched).
    pub fn position(&self, id: &str) -> Result<f64> {
        let h: Option<f64> = self.conn.query_row("SELECT position FROM history WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
        let v: Option<f64> = self.conn.query_row("SELECT position FROM videos WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
        Ok(h.unwrap_or(0.0).max(v.unwrap_or(0.0)))
    }

    /// You opened this video (now).
    pub fn watched(&self, w: &Watched) -> Result<()> {
        self.conn.execute(
            "INSERT INTO history (id, title, channel_id, channel_name, duration, position, furthest, watched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, MAX(?6, ?8), ?7)
             ON CONFLICT(id) DO UPDATE SET watched_at = ?7,
                 title = CASE WHEN ?2 = '' THEN title ELSE ?2 END,
                 channel_id = CASE WHEN ?3 = '' THEN channel_id ELSE ?3 END,
                 channel_name = CASE WHEN ?4 = '' THEN channel_name ELSE ?4 END,
                 duration = CASE WHEN ?5 > 0 THEN ?5 ELSE duration END",
            params![w.id, w.title, w.channel_id, w.channel_name, w.duration, w.position, w.watched_at, w.furthest],
        )?;
        Ok(())
    }

    /// Fill in what the Watch page learned (title, channel) for a history entry.
    pub fn update_watched(&self, id: &str, title: &str, channel_id: &str, channel_name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE history SET
                 title = CASE WHEN ?2 = '' THEN title ELSE ?2 END,
                 channel_id = CASE WHEN ?3 = '' THEN channel_id ELSE ?3 END,
                 channel_name = CASE WHEN ?4 = '' THEN channel_name ELSE ?4 END
             WHERE id = ?1",
            params![id, title, channel_id, channel_name],
        )?;
        Ok(())
    }

    /// Most recently watched first.
    pub fn history(&self, limit: usize) -> Result<Vec<Watched>> {
        let mut st = self.conn.prepare(
            "SELECT id, title, channel_id, channel_name, duration, position, furthest, watched_at
             FROM history ORDER BY watched_at DESC LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit as i64], |r| {
            Ok(Watched {
                id: r.get(0)?,
                title: r.get(1)?,
                channel_id: r.get(2)?,
                channel_name: r.get(3)?,
                duration: r.get(4)?,
                position: r.get(5)?,
                furthest: r.get(6)?,
                watched_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// "Not interested": kind is "video" or "channel".
    pub fn hide(&self, kind: &str, id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO hidden (kind, id, at) VALUES (?1, ?2, ?3)",
            params![kind, id, crate::now()],
        )?;
        Ok(())
    }

    /// (hidden videos, hidden channels)
    pub fn hidden(&self) -> Result<(HashSet<String>, HashSet<String>)> {
        let mut st = self.conn.prepare("SELECT kind, id FROM hidden")?;
        let mut out = (HashSet::new(), HashSet::new());
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (kind, id) = row?;
            if kind == "channel" { out.1.insert(id) } else { out.0.insert(id) };
        }
        Ok(out)
    }

    pub fn save_foryou(&self, picks: &[Pick]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM foryou", [])?;
        for (i, p) in picks.iter().enumerate() {
            let v = &p.video;
            tx.execute(
                "INSERT INTO foryou (rank, id, title, channel_id, channel_name, published, views, duration, reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![i as i64, v.id, v.title, v.channel_id, v.channel_name, v.published, v.views.map(|x| x as i64), v.duration, p.reason],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('foryou_built', ?1)",
            params![crate::now().to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn foryou(&self) -> Result<Vec<Pick>> {
        let mut st = self.conn.prepare(
            "SELECT id, title, channel_id, channel_name, published, views, duration, reason FROM foryou ORDER BY rank",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Pick {
                video: crate::yt::VideoHit {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    channel_id: r.get(2)?,
                    channel_name: r.get(3)?,
                    published: r.get(4)?,
                    views: r.get::<_, Option<i64>>(5)?.map(|x| x as u64),
                    duration: r.get::<_, Option<i64>>(6)?.map(|x| x as u32),
                    live: false,
                    watched_pct: None,
                },
                reason: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_foryou(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM foryou WHERE id = ?1 OR channel_id = ?1", params![id])?;
        Ok(())
    }

    /// When "For you" was last put together (unix time).
    pub fn foryou_built(&self) -> Result<Option<i64>> {
        let v: Option<String> = self.conn.query_row("SELECT value FROM meta WHERE key = 'foryou_built'", [], |r| r.get(0)).optional()?;
        Ok(v.and_then(|v| v.parse().ok()))
    }

    pub fn setting(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    }

    pub fn set_on_youtube(&self, channel: &str, on: bool) -> Result<()> {
        self.conn.execute("UPDATE channels SET yt = ?2 WHERE id = ?1", params![channel, on as i64])?;
        Ok(())
    }

    /// Channels known to be on your YouTube account.
    pub fn on_youtube(&self) -> Result<HashSet<String>> {
        let mut st = self.conn.prepare("SELECT id FROM channels WHERE yt = 1")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Channels you follow only in Linen (not on your YouTube account).
    pub fn only_here(&self) -> Result<Vec<Channel>> {
        Ok(self.channels()?.into_iter().filter(|c| !self.on_youtube().map(|s| s.contains(&c.id)).unwrap_or(false)).collect())
    }

    pub fn mark_imported(&self, table_channel: bool, id: &str) -> Result<()> {
        let sql = if table_channel { "UPDATE channels SET imported = 1 WHERE id = ?1" } else { "UPDATE history SET imported = 1 WHERE id = ?1" };
        self.conn.execute(sql, params![id])?;
        Ok(())
    }

    /// Forget what a YouTube account brought in (its subscriptions and history);
    /// channels you followed in Linen and videos you watched here stay.
    /// Returns how many channels went.
    pub fn forget_imported(&self) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let n = tx.execute("DELETE FROM channels WHERE imported = 1", [])?;
        tx.execute("DELETE FROM history WHERE imported = 1", [])?;
        tx.execute("UPDATE channels SET yt = 0", [])?;
        // For you was built from that history
        tx.execute("DELETE FROM foryou", [])?;
        tx.execute("DELETE FROM meta WHERE key = 'foryou_built'", [])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn clear_on_youtube(&self) -> Result<()> {
        self.conn.execute("UPDATE channels SET yt = 0", [])?;
        Ok(())
    }

    pub fn remove_setting(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Your like (1), dislike (-1) or neither (0).
    pub fn rating(&self, id: &str) -> i32 {
        self.conn.query_row("SELECT rating FROM ratings WHERE id = ?1", params![id], |r| r.get(0)).optional().ok().flatten().unwrap_or(0)
    }

    pub fn set_rating(&self, id: &str, rating: i32) -> Result<()> {
        if rating == 0 {
            self.conn.execute("DELETE FROM ratings WHERE id = ?1", params![id])?;
        } else {
            self.conn.execute(
                "INSERT OR REPLACE INTO ratings (id, rating, at) VALUES (?1, ?2, ?3)",
                params![id, rating, crate::now()],
            )?;
        }
        Ok(())
    }

    /// (liked, disliked) video ids
    pub fn ratings(&self) -> Result<(HashSet<String>, HashSet<String>)> {
        let mut st = self.conn.prepare("SELECT id, rating FROM ratings")?;
        let mut out = (HashSet::new(), HashSet::new());
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?)))? {
            let (id, r) = row?;
            if r > 0 { out.0.insert(id) } else { out.1.insert(id) };
        }
        Ok(out)
    }

    /// Remember a video's stream links (`data`: the resolver's JSON) until they expire.
    pub fn save_links(&self, id: &str, data: &str, quality: i32, expires: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO links (id, data, quality, found, expires) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, data, quality, crate::now(), expires],
        )?;
        Ok(())
    }

    /// Links for this quality still good for at least half an hour: (id, data, found).
    /// Expired ones are thrown away.
    pub fn links(&self, quality: i32) -> Result<Vec<(String, String, i64)>> {
        let now = crate::now();
        self.conn.execute("DELETE FROM links WHERE expires < ?1", params![now])?;
        let mut q = self.conn.prepare("SELECT id, data, found FROM links WHERE quality = ?1 AND expires > ?2")?;
        let rows = q.query_map(params![quality, now + 1800], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn forget_links(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM links WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_upcoming(&self, id: &str, label: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO upcoming (id, label, checked) VALUES (?1, ?2, ?3)",
            params![id, label, crate::now()],
        )?;
        Ok(())
    }

    /// Scheduled videos checked in the last few hours (older ones get looked at again).
    pub fn upcoming(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT id, label FROM upcoming WHERE checked > ?1")?;
        let rows = st.query_map(params![crate::now() - 6 * 3600], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn clear_upcoming(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM upcoming WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// A switch from Settings (stored as "1"/"0").
    pub fn flag(&self, key: &str, default: bool) -> bool {
        self.setting(key).map_or(default, |v| v == "1")
    }

    pub fn set_flag(&self, key: &str, on: bool) -> Result<()> {
        self.set_setting(key, if on { "1" } else { "0" })
    }

    /// A cached DeArrow answer and when it was fetched.
    pub fn branding(&self, id: &str) -> Result<Option<(crate::calm::Branding, i64)>> {
        Ok(self
            .conn
            .query_row("SELECT title, thumb_at, fetched FROM branding WHERE id = ?1", params![id], |r| {
                let title: Option<String> = r.get(0)?;
                Ok((crate::calm::Branding { title: title.map(|t| crate::calm::clean_title(&t)), thumb_at: r.get(1)? }, r.get(2)?))
            })
            .optional()?)
    }

    pub fn set_branding(&self, id: &str, b: &crate::calm::Branding) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO branding (id, title, thumb_at, fetched) VALUES (?1, ?2, ?3, ?4)",
            params![id, b.title, b.thumb_at, crate::now()],
        )?;
        Ok(())
    }

    pub fn is_following(&self, channel: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT 1 FROM channels WHERE id = ?1", params![channel], |_| Ok(())).optional()?.is_some())
    }

    pub fn unseen_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM videos WHERE seen = 0", [], |r| r.get(0))?)
    }

    pub fn channel_name(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT name FROM channels WHERE id = ?1", params![id], |r| r.get(0)).optional()?)
    }

    // ---- Watch later ----

    /// Save a video for later (`yt`: it's on your YouTube Watch later too;
    /// `imported`: it came from there). `added` orders the list (unix time).
    pub fn add_later(&self, v: &crate::yt::VideoHit, yt: bool, imported: bool, added: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO later (id, title, channel_id, channel_name, published, views, duration, added, yt, imported)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET yt = MAX(yt, ?9),
                title = CASE WHEN ?2 = '' THEN title ELSE ?2 END,
                channel_id = CASE WHEN ?3 = '' THEN channel_id ELSE ?3 END,
                channel_name = CASE WHEN ?4 = '' THEN channel_name ELSE ?4 END,
                duration = COALESCE(?7, duration), views = COALESCE(?6, views)",
            params![v.id, v.title, v.channel_id, v.channel_name, v.published, v.views.map(|x| x as i64), v.duration.map(|x| x as i64), added, yt, imported],
        )?;
        Ok(())
    }

    // ---- playlists ----

    /// All of them: yours first (newest made first), then saved ones.
    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        let mut st = self.conn.prepare(
            "SELECT p.id, p.name, p.yt, p.saved, p.count, p.first,
                    (SELECT COUNT(*) FROM playlist_items i WHERE i.playlist = p.id),
                    (SELECT i.id FROM playlist_items i WHERE i.playlist = p.id ORDER BY i.pos LIMIT 1)
             FROM playlists p ORDER BY p.saved, p.created DESC, p.rowid DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Playlist {
                id: r.get(0)?,
                name: r.get(1)?,
                yt: r.get(2)?,
                saved: r.get(3)?,
                count: r.get(4)?,
                first: r.get::<_, Option<String>>(7)?.or(r.get(5)?),
                videos: r.get::<_, i64>(6)? as usize,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn playlist(&self, id: &str) -> Result<Option<Playlist>> {
        Ok(self.playlists()?.into_iter().find(|p| p.id == id))
    }

    /// A new playlist (`id`: YouTube's, or None for one only in Linen). Its id.
    pub fn create_playlist(&self, name: &str, id: Option<&str>) -> Result<String> {
        let id = match id {
            Some(i) => i.to_string(),
            None => {
                let n: i64 = self.conn.query_row("SELECT COALESCE(MAX(rowid), 0) + 1 FROM playlists", [], |r| r.get(0))?;
                format!("local:{n}-{}", crate::now())
            }
        };
        self.conn.execute(
            "INSERT OR IGNORE INTO playlists (id, name, yt, created) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, !id.starts_with("local:"), crate::now()],
        )?;
        Ok(id)
    }

    pub fn rename_playlist(&self, id: &str, name: &str) -> Result<()> {
        self.conn.execute("UPDATE playlists SET name = ?2 WHERE id = ?1", params![id, name])?;
        Ok(())
    }

    pub fn delete_playlist(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM playlist_items WHERE playlist = ?1", params![id])?;
        self.conn.execute("DELETE FROM playlists WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// In playlist order.
    pub fn playlist_videos(&self, id: &str) -> Result<Vec<crate::yt::VideoHit>> {
        let mut st = self.conn.prepare(
            "SELECT id, title, channel_id, channel_name, published, views, duration FROM playlist_items WHERE playlist = ?1 ORDER BY pos",
        )?;
        let rows = st.query_map(params![id], hit_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Add at the end (nothing happens if it's already in).
    pub fn add_to_playlist(&self, playlist: &str, v: &crate::yt::VideoHit) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO playlist_items (playlist, id, title, channel_id, channel_name, published, views, duration, pos)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, (SELECT COALESCE(MAX(pos), 0) + 1 FROM playlist_items WHERE playlist = ?1))",
            params![playlist, v.id, v.title, v.channel_id, v.channel_name, v.published, v.views.map(|x| x as i64), v.duration.map(|x| x as i64)],
        )?;
        Ok(())
    }

    pub fn remove_from_playlist(&self, playlist: &str, video: &str) -> Result<()> {
        self.conn.execute("DELETE FROM playlist_items WHERE playlist = ?1 AND id = ?2", params![playlist, video])?;
        Ok(())
    }

    /// The playlists (yours) a video is in.
    pub fn playlists_with(&self, video: &str) -> Result<HashSet<String>> {
        let mut st = self.conn.prepare("SELECT playlist FROM playlist_items WHERE id = ?1")?;
        let rows = st.query_map(params![video], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// From your YouTube account: your playlists (with their videos) and the ones you
    /// saved. Replaces what came from YouTube before; Linen-only playlists stay.
    pub fn sync_yt_playlists(&self, own: &[(String, String, Vec<crate::yt::VideoHit>)], saved: &[(String, String, String, Option<String>)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let keep: HashSet<&str> = own.iter().map(|o| o.0.as_str()).chain(saved.iter().map(|s| s.0.as_str())).collect();
        let old: Vec<String> = {
            let mut st = tx.prepare("SELECT id FROM playlists WHERE yt = 1 OR saved = 1")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for id in old.iter().filter(|o| !keep.contains(o.as_str())) {
            tx.execute("DELETE FROM playlist_items WHERE playlist = ?1", params![id])?;
            tx.execute("DELETE FROM playlists WHERE id = ?1", params![id])?;
        }
        // newest first on YouTube: keep that order by "made" time
        let now = crate::now();
        for (i, (id, name, videos)) in own.iter().enumerate() {
            tx.execute(
                "INSERT INTO playlists (id, name, yt, saved, created) VALUES (?1, ?2, 1, 0, ?3)
                 ON CONFLICT(id) DO UPDATE SET name = ?2, yt = 1, saved = 0",
                params![id, name, now - 1 - i as i64],
            )?;
            tx.execute("DELETE FROM playlist_items WHERE playlist = ?1", params![id])?;
            for (pos, v) in videos.iter().enumerate() {
                tx.execute(
                    "INSERT OR IGNORE INTO playlist_items (playlist, id, title, channel_id, channel_name, published, views, duration, pos)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![id, v.id, v.title, v.channel_id, v.channel_name, v.published, v.views.map(|x| x as i64), v.duration.map(|x| x as i64), pos as i64],
                )?;
            }
        }
        for (i, (id, name, count, first)) in saved.iter().enumerate() {
            tx.execute(
                "INSERT INTO playlists (id, name, yt, saved, count, first, created) VALUES (?1, ?2, 0, 1, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET name = ?2, saved = 1, count = ?3, first = ?4",
                params![id, name, count, first, now - 1 - i as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Disconnected or switched accounts: what came from YouTube goes.
    pub fn forget_yt_playlists(&self) -> Result<()> {
        self.conn.execute("DELETE FROM playlist_items WHERE playlist IN (SELECT id FROM playlists WHERE yt = 1 OR saved = 1)", [])?;
        self.conn.execute("DELETE FROM playlists WHERE yt = 1 OR saved = 1", [])?;
        Ok(())
    }

    pub fn remove_later(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM later WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Newest saved first.
    pub fn later(&self) -> Result<Vec<crate::yt::VideoHit>> {
        let mut st = self.conn.prepare(
            "SELECT id, title, channel_id, channel_name, published, views, duration FROM later ORDER BY added DESC, rowid DESC",
        )?;
        let rows = st.query_map([], hit_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn later_ids(&self) -> Result<HashSet<String>> {
        let mut st = self.conn.prepare("SELECT id FROM later")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The saved videos that are also on your YouTube Watch later.
    pub fn later_on_youtube(&self) -> Result<HashSet<String>> {
        let mut st = self.conn.prepare("SELECT id FROM later WHERE yt = 1")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Account switched or disconnected: what came from it goes, the rest stays here only.
    pub fn forget_later_imported(&self) -> Result<usize> {
        let n = self.conn.execute("DELETE FROM later WHERE imported = 1", [])?;
        self.conn.execute("UPDATE later SET yt = 0", [])?;
        Ok(n)
    }

    /// Details for saved videos that were saved before we knew them (title etc.).
    /// Returns how many rows learned something.
    pub fn fill_saved(&self, v: &crate::yt::VideoHit) -> Result<usize> {
        let mut n = 0;
        for table in ["later", "downloads"] {
            n += self.conn.execute(
                &format!(
                    "UPDATE {table} SET title = ?2, channel_id = ?3, channel_name = ?4,
                        published = CASE WHEN ?5 > 0 THEN ?5 ELSE published END,
                        views = COALESCE(?6, views), duration = COALESCE(?7, duration)
                     WHERE id = ?1 AND (title = '' OR channel_id = '' OR (duration IS NULL AND ?7 IS NOT NULL))"
                ),
                params![v.id, v.title, v.channel_id, v.channel_name, v.published, v.views.map(|x| x as i64), v.duration.map(|x| x as i64)],
            )?;
        }
        Ok(n)
    }

    // ---- downloads ----

    pub fn add_download(&self, v: &crate::yt::VideoHit, path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO downloads (id, title, channel_id, channel_name, published, views, duration, path, bytes, done, added)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, ?9)",
            params![v.id, v.title, v.channel_id, v.channel_name, v.published, v.views.map(|x| x as i64), v.duration.map(|x| x as i64), path, crate::now()],
        )?;
        Ok(())
    }

    pub fn download_done(&self, id: &str, path: &str, bytes: u64) -> Result<()> {
        self.conn.execute("UPDATE downloads SET done = 1, path = ?2, bytes = ?3 WHERE id = ?1", params![id, path, bytes as i64])?;
        Ok(())
    }

    pub fn remove_download(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM downloads WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Newest first.
    pub fn downloads(&self) -> Result<Vec<Download>> {
        let mut st = self.conn.prepare(
            "SELECT id, title, channel_id, channel_name, published, views, duration, path, bytes, done FROM downloads ORDER BY added DESC, rowid DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Download { video: hit_from(r)?, path: r.get(7)?, bytes: r.get::<_, i64>(8)? as u64, done: r.get::<_, i64>(9)? != 0 })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_and_feed() {
        let db = Db::open_in_memory().unwrap();
        db.add_channel(&Channel { id: "UC1".into(), name: "One".into(), avatar: None }).unwrap();
        let v = |id: &str, t: i64| Video {
            id: id.into(),
            channel_id: "UC1".into(),
            channel_name: String::new(),
            title: format!("video {id}"),
            published: t,
            thumbnail: String::new(),
            views: None,
            description: String::new(),
            seen: false,
            position: 0.0,
            duration: None,
        };
        db.store_feed("UC1", Some("One!"), &[v("a", 10), v("b", 20)]).unwrap();
        let f = db.feed(10).unwrap();
        assert_eq!(f.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(f[0].channel_name, "One!");
        assert_eq!(db.unseen_count().unwrap(), 2);
        db.mark_all_seen().unwrap();
        assert_eq!(db.unseen_count().unwrap(), 0);
        // re-fetching keeps "seen" and the watch position
        db.set_position("a", 42.0).unwrap();
        db.store_feed("UC1", None, &[v("a", 10)]).unwrap();
        let a = db.video("a").unwrap().unwrap();
        assert!(a.seen && a.position == 42.0);
        // history: positions from anywhere, furthest point kept
        db.watched(&Watched { id: "x".into(), title: "X".into(), channel_id: "UC9".into(), channel_name: "Nine".into(), watched_at: 5, ..Default::default() }).unwrap();
        db.set_position("x", 100.0).unwrap();
        db.set_position("x", 20.0).unwrap();
        let h = db.history(10).unwrap();
        assert_eq!((h[0].position, h[0].furthest), (20.0, 100.0));
        assert_eq!(db.position("x").unwrap(), 20.0);
        db.hide("channel", "UC9").unwrap();
        assert!(db.hidden().unwrap().1.contains("UC9"));
        db.remove_channel("UC1").unwrap();
        assert!(db.feed(10).unwrap().is_empty());
    }

    #[test]
    fn playlists() {
        let db = Db::open_in_memory().unwrap();
        let hit = |id: &str| crate::yt::VideoHit { id: id.into(), title: format!("t {id}"), ..Default::default() };
        let a = db.create_playlist("Mine", None).unwrap();
        assert!(a.starts_with("local:"));
        db.add_to_playlist(&a, &hit("v1")).unwrap();
        db.add_to_playlist(&a, &hit("v2")).unwrap();
        db.add_to_playlist(&a, &hit("v1")).unwrap();
        assert_eq!(db.playlist_videos(&a).unwrap().iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["v1", "v2"]);
        assert!(db.playlists_with("v2").unwrap().contains(&a));
        db.sync_yt_playlists(&[("PLx".into(), "On yt".into(), vec![hit("v3")])], &[("PLs".into(), "Theirs".into(), "9 videos".into(), None)]).unwrap();
        let all = db.playlists().unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.last().unwrap().saved);
        assert_eq!(db.playlist("PLx").unwrap().unwrap().first.as_deref(), Some("v3"));
        db.remove_from_playlist(&a, "v1").unwrap();
        assert_eq!(db.playlist(&a).unwrap().unwrap().videos, 1);
        db.sync_yt_playlists(&[], &[]).unwrap();
        assert_eq!(db.playlists().unwrap().len(), 1);
        db.rename_playlist(&a, "New").unwrap();
        assert_eq!(db.playlist(&a).unwrap().unwrap().name, "New");
        db.delete_playlist(&a).unwrap();
        assert!(db.playlists().unwrap().is_empty());
    }

    #[test]
    fn later_and_downloads() {
        let db = Db::open_in_memory().unwrap();
        let hit = |id: &str| crate::yt::VideoHit { id: id.into(), title: format!("T{id}"), channel_id: "UC1".into(), channel_name: "One".into(), duration: Some(60), ..Default::default() };
        db.add_later(&hit("a"), false, false, 1).unwrap();
        db.add_later(&hit("b"), true, true, 2).unwrap();
        // saved again from YouTube: keeps its title, now also on YouTube
        db.add_later(&crate::yt::VideoHit { id: "a".into(), ..Default::default() }, true, false, 3).unwrap();
        let l = db.later().unwrap();
        assert_eq!(l.len(), 2);
        assert!(l.iter().any(|v| v.id == "a" && v.title == "Ta" && v.duration == Some(60)));
        assert_eq!(db.later_on_youtube().unwrap().len(), 2);
        assert_eq!(db.forget_later_imported().unwrap(), 1);
        assert_eq!(db.later_ids().unwrap().into_iter().collect::<Vec<_>>(), ["a"]);
        assert!(db.later_on_youtube().unwrap().is_empty());
        db.add_download(&hit("d"), "/x/d.mp4").unwrap();
        assert!(!db.downloads().unwrap()[0].done);
        db.download_done("d", "/x/d.mkv", 1234).unwrap();
        let d = &db.downloads().unwrap()[0];
        assert!(d.done && d.bytes == 1234 && d.path == "/x/d.mkv" && d.video.title == "Td");
        db.remove_download("d").unwrap();
        assert!(db.downloads().unwrap().is_empty());
    }
}
