use std::path::PathBuf;

fn dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "linen")
}

/// ~/.local/share/linen (or LINEN_DATA_DIR, for test runs that mustn't touch your
/// library; don't move XDG_DATA_HOME for that, it also hides your fonts)
pub fn data_dir() -> PathBuf {
    let d = std::env::var_os("LINEN_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs().map(|d| d.data_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from(".linen"));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// ~/.cache/linen/thumbs
pub fn thumb_dir() -> PathBuf {
    let d = dirs().map(|d| d.cache_dir().join("thumbs")).unwrap_or_else(|| PathBuf::from(".linen-thumbs"));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Where downloaded videos go: ~/Videos/Linen (inside LINEN_DATA_DIR for test runs).
pub fn downloads_dir() -> PathBuf {
    let d = if std::env::var_os("LINEN_DATA_DIR").is_some() {
        data_dir().join("downloads")
    } else {
        directories::UserDirs::new()
            .map(|u| u.video_dir().map(|v| v.to_path_buf()).unwrap_or_else(|| u.home_dir().join("Videos")).join("Linen"))
            .unwrap_or_else(|| data_dir().join("downloads"))
    };
    let _ = std::fs::create_dir_all(&d);
    d
}
