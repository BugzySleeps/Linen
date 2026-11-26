//! Linen's data: subscriptions and their videos, kept in a local database and
//! refreshed from YouTube's public RSS feeds (no account, no API key).

pub mod account;
pub mod calm;
pub mod channel;
pub mod db;
pub mod feed;
pub mod foryou;
pub mod paths;
pub mod takeout;
pub mod yt;

pub use db::Db;

/// A channel you follow.
#[derive(Debug, Clone)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub avatar: Option<String>,
}

/// A video from a followed channel.
#[derive(Debug, Clone)]
pub struct Video {
    pub id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub title: String,
    /// unix seconds
    pub published: i64,
    pub thumbnail: String,
    pub views: Option<u64>,
    pub description: String,
    /// Seen in the feed before (so new ones can be marked).
    pub seen: bool,
    /// Where you stopped watching, in seconds (0 = not started).
    pub position: f64,
    /// Length in seconds, when known.
    pub duration: Option<u32>,
}

/// A browser-like User-Agent (YouTube serves odd pages to unknown clients).
pub const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

pub fn http() -> anyhow::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(20))
        .build()?)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
