//! Your YouTube account, through the sign-in your browser already has.
//!
//! Linen reads the youtube.com cookies from Firefox each time it starts (it never
//! keeps its own copy), and signs its requests the way youtube.com does. This is
//! not an official Google feature: the welcome screen says so plainly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};

pub type Cookies = HashMap<String, String>;

/// A browser sign-in pointed at one identity: which Google account (several can be
/// signed in at once) and, for a brand channel inside it, which page.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    pub cookies: Cookies,
    /// Google's account slot in the browser (0 = the first signed in)
    pub authuser: u32,
    /// a brand channel of that account
    pub page_id: Option<String>,
}

/// One account or channel you can use, as YouTube's account switcher lists it.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub authuser: u32,
    pub page_id: Option<String>,
    pub name: String,
    pub handle: String,
    /// "11 subscribers" / the account's email-ish line
    pub byline: String,
    pub avatar: Option<String>,
}

/// Firefox-family profile folders that have a cookie store, newest first.
pub fn firefox_profiles() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let roots = [
        home.join(".config/mozilla/firefox"),
        home.join(".mozilla/firefox"),
        home.join(".librewolf"),
        home.join(".config/librewolf/librewolf"),
        home.join(".floorp"),
        home.join(".zen"),
    ];
    let mut out: Vec<(std::time::SystemTime, PathBuf)> = vec![];
    for root in roots {
        for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let db = entry.path().join("cookies.sqlite");
            if let Ok(m) = std::fs::metadata(&db).and_then(|m| m.modified()) {
                out.push((m, entry.path()));
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|x| x.1).collect()
}

/// youtube.com cookies from a profile (copied first: Firefox keeps the file locked).
pub fn read_firefox(profile: &Path) -> Result<Cookies> {
    let tmp = std::env::temp_dir().join(format!("linen-cookies-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let copy = tmp.join("cookies.sqlite");
    std::fs::copy(profile.join("cookies.sqlite"), &copy).context("couldn't read the browser's cookies")?;
    // recent changes may still be in the write-ahead log
    let wal = profile.join("cookies.sqlite-wal");
    if wal.exists() {
        let _ = std::fs::copy(&wal, tmp.join("cookies.sqlite-wal"));
    }
    let result = (|| -> Result<Cookies> {
        let conn = rusqlite::Connection::open(&copy)?;
        let mut st = conn.prepare(
            "SELECT name, value FROM moz_cookies WHERE host = '.youtube.com' OR host = 'www.youtube.com' OR host = 'youtube.com'
             ORDER BY lastAccessed",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

pub fn signed_in(c: &Cookies) -> bool {
    c.contains_key("LOGIN_INFO") && (c.contains_key("SAPISID") || c.contains_key("__Secure-3PAPISID"))
}

/// The first browser profile signed in to YouTube.
pub fn find_sign_in() -> Result<(PathBuf, Cookies)> {
    let profiles = firefox_profiles();
    if profiles.is_empty() {
        bail!("Couldn't find Firefox on this computer.");
    }
    for p in profiles {
        if let Ok(c) = read_firefox(&p) {
            if signed_in(&c) {
                return Ok((p, c));
            }
        }
    }
    bail!("Firefox isn't signed in to YouTube. Sign in at youtube.com in Firefox, then try again.")
}

/// The headers that make a request count as you: cookies plus the session hash
/// youtube.com sends (SHA-1 of "time SAPISID origin", in three variants).
pub fn headers(a: &Auth) -> Vec<(&'static str, String)> {
    const ORIGIN: &str = "https://www.youtube.com";
    let c = &a.cookies;
    let ts = crate::now().to_string();
    let sapisid = c.get("SAPISID").or_else(|| c.get("__Secure-3PAPISID"));
    let mut auth = vec![];
    for (scheme, sid) in [("SAPISIDHASH", sapisid), ("SAPISID1PHASH", c.get("__Secure-1PAPISID")), ("SAPISID3PHASH", c.get("__Secure-3PAPISID"))] {
        if let Some(sid) = sid {
            let hash = Sha1::digest(format!("{ts} {sid} {ORIGIN}").as_bytes());
            let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
            auth.push(format!("{scheme} {ts}_{hex}"));
        }
    }
    let cookie = c.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
    let mut h = vec![
        ("Cookie", cookie),
        ("Authorization", auth.join(" ")),
        ("X-Origin", ORIGIN.into()),
        ("Origin", ORIGIN.into()),
        ("X-Goog-AuthUser", a.authuser.to_string()),
        ("X-Youtube-Bootstrap-Logged-In", "true".into()),
    ];
    if let Some(page) = &a.page_id {
        h.push(("X-Goog-PageId", page.clone()));
    }
    h
}
