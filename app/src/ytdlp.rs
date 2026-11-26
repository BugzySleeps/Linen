//! Linen's own copy of yt-dlp (what mpv uses to open YouTube links). YouTube
//! changes often and yt-dlp keeps up, so Linen downloads the official release
//! once and updates it about weekly. A yt-dlp already on PATH is used as-is.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

const RELEASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp";

fn own_copy() -> PathBuf {
    linen_core::paths::data_dir().join("bin/yt-dlp")
}

fn on_path() -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| PathBuf::from(d).join("yt-dlp"))
        .find(|p| p.is_file())
}

/// A JavaScript engine for yt-dlp to solve YouTube's player challenge. Without
/// one, yt-dlp falls back to workarounds that fail now and then.
/// Returns yt-dlp's "--js-runtimes" value, e.g. "node:/usr/bin/node".
pub fn js_runtime() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    let path = path.to_str()?;
    // yt-dlp's own order of preference
    for (name, bins) in [("deno", &["deno"][..]), ("node", &["node"][..]), ("bun", &["bun"][..]), ("quickjs", &["qjs", "quickjs"][..])] {
        for b in bins {
            if let Some(p) = path.split(':').map(|d| PathBuf::from(d).join(b)).find(|p| p.is_file()) {
                return Some(format!("{name}:{}", p.display()));
            }
        }
    }
    None
}

/// Where yt-dlp is (or will be once `ensure` has run).
pub fn path() -> PathBuf {
    on_path().unwrap_or_else(own_copy)
}

/// Make sure yt-dlp exists; update Linen's own copy if it's a week old.
/// Blocking: run on a background thread. Returns a line for the log/UI.
pub fn ensure() -> Result<String> {
    if let Some(p) = on_path() {
        return Ok(format!("using {}", p.display()));
    }
    let p = own_copy();
    if !p.is_file() {
        download(&p)?;
        return Ok("yt-dlp downloaded".into());
    }
    let age = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    if age.is_some_and(|a| a > Duration::from_secs(7 * 24 * 3600)) {
        // yt-dlp updates itself in place; touch it so we check again in a week either way
        let out = std::process::Command::new(&p).arg("-U").output();
        let _ = filetime_now(&p);
        return Ok(match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).lines().last().unwrap_or("yt-dlp checked").to_string(),
            Err(e) => format!("yt-dlp update failed: {e}"),
        });
    }
    Ok("yt-dlp ready".into())
}

fn filetime_now(p: &std::path::Path) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new().append(true).open(p)?;
    f.set_modified(SystemTime::now())
}

fn download(p: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = p.parent().context("no data dir")?;
    std::fs::create_dir_all(dir)?;
    let bytes = reqwest::blocking::Client::builder()
        .user_agent("linen")
        .timeout(Duration::from_secs(120))
        .build()?
        .get(RELEASE)
        .send()?
        .error_for_status()?
        .bytes()?;
    let part = p.with_extension("part");
    std::fs::write(&part, &bytes)?;
    std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&part, p)?;
    Ok(())
}
