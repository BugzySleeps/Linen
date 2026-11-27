//! Linen's own copy of yt-dlp (what mpv uses to open YouTube links). YouTube
//! changes often and yt-dlp keeps up, so Linen downloads the official release
//! once and updates it about weekly. A yt-dlp already on PATH is used as-is.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// The release that runs anywhere Python does (Linen's stream finder imports it).
const RELEASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp";
/// On Windows, also the stand-alone .exe (for downloads and mpv's own lookups).
#[cfg(windows)]
const RELEASE_EXE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";

fn own_copy() -> PathBuf {
    linen_core::paths::data_dir().join("bin").join(format!("yt-dlp{}", std::env::consts::EXE_SUFFIX))
}

/// The Python release (Linux: the same file as `own_copy`).
fn own_module() -> PathBuf {
    linen_core::paths::data_dir().join("bin/yt-dlp")
}

/// A program on PATH (with ".exe" on Windows).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&file)).find(|p| p.is_file())
}

/// Where Linen.exe is: the Windows build carries Python, Node and libmpv beside it.
fn beside_app(rel: &str) -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join(rel);
    p.is_file().then_some(p)
}

fn on_path() -> Option<PathBuf> {
    // Windows: always Linen's own copies (a yt-dlp.exe on PATH can't be imported)
    if cfg!(windows) {
        return None;
    }
    find_on_path("yt-dlp")
}

/// A JavaScript engine for yt-dlp to solve YouTube's player challenge. Without
/// one, yt-dlp falls back to workarounds that fail now and then.
/// Returns yt-dlp's "--js-runtimes" value, e.g. "node:/usr/bin/node".
pub fn js_runtime() -> Option<String> {
    // the one shipped with Linen (Windows) comes first
    if let Some(p) = beside_app("node/node.exe").or_else(|| beside_app("node.exe")) {
        return Some(format!("node:{}", p.display()));
    }
    // yt-dlp's own order of preference
    for (name, bins) in [("deno", &["deno"][..]), ("node", &["node"][..]), ("bun", &["bun"][..]), ("quickjs", &["qjs", "quickjs"][..])] {
        for b in bins {
            if let Some(p) = find_on_path(b) {
                return Some(format!("{name}:{}", p.display()));
            }
        }
    }
    None
}

/// The Python that runs Linen's stream finder: the one shipped beside Linen.exe on
/// Windows, else python3 (python on Windows) from PATH.
pub fn python() -> PathBuf {
    if let Some(p) = beside_app("python/python.exe") {
        return p;
    }
    if cfg!(windows) { find_on_path("python").unwrap_or_else(|| "python".into()) } else { "python3".into() }
}

/// Where yt-dlp is (or will be once `ensure` has run).
pub fn path() -> PathBuf {
    on_path().unwrap_or_else(own_copy)
}

/// What the stream finder imports yt-dlp from.
pub fn module_path() -> PathBuf {
    on_path().unwrap_or_else(own_module)
}

/// Start a program without a console window popping up (Windows).
pub fn quiet(c: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Open a web address or a folder with the desktop's default app.
pub fn open_external(target: &std::ffi::OsStr) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        quiet(std::process::Command::new("explorer").arg(target)).spawn().map(|_| ())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open").arg(target).spawn().map(|_| ())
    }
}

/// Make sure yt-dlp exists; update Linen's own copy if it's a week old.
/// Blocking: run on a background thread. Returns a line for the log/UI.
pub fn ensure() -> Result<String> {
    if let Some(p) = on_path() {
        return Ok(format!("using {}", p.display()));
    }
    let p = own_copy();
    let module = own_module();
    if !module.is_file() {
        download(RELEASE, &module)?;
    }
    if !p.is_file() {
        #[cfg(windows)]
        download(RELEASE_EXE, &p)?;
        return Ok("yt-dlp downloaded".into());
    }
    let age = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    if age.is_some_and(|a| a > Duration::from_secs(7 * 24 * 3600)) {
        // yt-dlp updates itself in place; touch it so we check again in a week either way
        let out = quiet(&mut std::process::Command::new(&p)).arg("-U").output();
        let _ = filetime_now(&p);
        // Windows: the Python copy is a separate file; fetch it fresh too
        if cfg!(windows) {
            let _ = download(RELEASE, &module);
        }
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

fn download(url: &str, p: &std::path::Path) -> Result<()> {
    let dir = p.parent().context("no data dir")?;
    std::fs::create_dir_all(dir)?;
    let bytes = reqwest::blocking::Client::builder()
        .user_agent("linen")
        .timeout(Duration::from_secs(120))
        .build()?
        .get(url)
        .send()?
        .error_for_status()?
        .bytes()?;
    let part = p.with_extension("part");
    std::fs::write(&part, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&part, p)?;
    Ok(())
}
