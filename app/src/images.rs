//! Thumbnails and channel pictures: downloaded once into ~/.cache/linen/thumbs,
//! decoded small on a few background threads, handed to the UI as pixels.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

pub struct Decoded {
    pub key: String,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone)]
pub struct Loader {
    jobs: Sender<(String, String, u32)>,
    asked: Arc<Mutex<HashSet<String>>>,
}

impl Loader {
    /// `done` is called (on a worker thread) for each picture that's ready.
    pub fn start(done: impl Fn(Decoded) + Send + Sync + 'static) -> Self {
        let (tx, rx): (Sender<(String, String, u32)>, Receiver<_>) = channel();
        let rx = Arc::new(Mutex::new(rx));
        let done = Arc::new(done);
        for i in 0..4 {
            let (rx, done) = (rx.clone(), done.clone());
            std::thread::Builder::new()
                .name(format!("linen-img{i}"))
                .spawn(move || {
                    let Ok(http) = linen_core::http() else { return };
                    loop {
                        let job = rx.lock().unwrap().recv();
                        let Ok((key, url, width)) = job else { return };
                        match fetch(&http, &key, &url, width) {
                            Some(d) => done(d),
                            None => log::debug!("no picture for {key}"),
                        }
                    }
                })
                .expect("spawn image thread");
        }
        Self { jobs: tx, asked: Default::default() }
    }

    /// Allow asking for this picture again (it changed, e.g. another account's).
    pub fn forget(&self, key: &str) {
        self.asked.lock().unwrap().remove(key);
    }

    /// Ask for a picture once; `width` is the decoded width in pixels.
    pub fn want(&self, key: &str, url: &str, width: u32) {
        if url.is_empty() || !self.asked.lock().unwrap().insert(key.to_string()) {
            return;
        }
        let _ = self.jobs.send((key.to_string(), url.to_string(), width));
    }
}

fn fetch(http: &reqwest::blocking::Client, key: &str, url: &str, width: u32) -> Option<Decoded> {
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    let path = linen_core::paths::thumb_dir().join(format!("{:016x}.jpg", h.finish()));
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            let b = http.get(url).send().ok()?.error_for_status().ok()?.bytes().ok()?.to_vec();
            let _ = std::fs::write(&path, &b);
            b
        }
    };
    let img = image::load_from_memory(&bytes).ok()?;
    // YouTube's hqdefault is 4:3 with black bars; crop to 16:9
    let img = if key.starts_with("v:") && img.height() * 4 == img.width() * 3 {
        let h = img.width() * 9 / 16;
        img.crop_imm(0, (img.height() - h) / 2, img.width(), h)
    } else {
        img
    };
    let img = img.thumbnail(width, width).to_rgba8();
    Some(Decoded { key: key.to_string(), w: img.width(), h: img.height(), rgba: img.into_raw() })
}
