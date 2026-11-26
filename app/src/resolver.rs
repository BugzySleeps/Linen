//! Finding a video's stream addresses ahead of time. Two warm yt-dlp helpers:
//! one only for clicks (so it's never busy with a prefetch), one for prefetching
//! what you're likely to open next (hovered videos, the newest in your feed).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Streams {
    pub video: String,
    pub audio: Option<String>,
    #[allow(dead_code)] // for the Watch page before mpv knows
    pub duration: Option<f64>,
    /// HTTP headers the streams must be fetched with (User-Agent and the rest).
    pub headers: Vec<(String, String)>,
    /// unix time before which YouTube refuses these links
    pub available_at: Option<u64>,
}

impl Streams {
    /// What mpv should open: separate video + audio joined with mpv's EDL
    /// (lengths prefixed, so the URLs' own characters can't break it).
    pub fn mpv_url(&self) -> String {
        Self::join(&self.video, self.audio.as_deref())
    }

    /// The same, fetched through Linen's relay (fast pieces instead of one slow download).
    pub fn mpv_url_via(&self, relay: &crate::proxy::Proxy) -> String {
        match &self.audio {
            Some(a) => {
                let (v, a) = relay.add_pair(&self.video, a, &self.headers);
                Self::join(&v, Some(&a))
            }
            None => Self::join(&relay.add(&self.video, &self.headers), None),
        }
    }

    fn join(video: &str, audio: Option<&str>) -> String {
        match audio {
            Some(a) => format!(
                "edl://!no_clip;!no_chapters;%{}%{};!new_stream;!no_clip;!no_chapters;%{}%{}",
                video.len(),
                video,
                a.len(),
                a
            ),
            None => video.to_string(),
        }
    }
}

pub type Answer = (String, Result<Streams, String>);

/// Background lookups running at once.
pub const PREFETCH_PARALLEL: usize = 2;

#[derive(Clone)]
pub struct Resolver {
    urgent: Sender<String>,
    prefetch: Sender<String>,
    /// the yt-dlp format asked for (Settings > Quality); sent with every lookup
    fmt: Arc<Mutex<String>>,
}

const SCRIPT: &str = include_str!("resolver.py");

fn spawn_helper(script: &std::path::Path, ytdlp: &str, fmt: &str, js: &str, parallel: usize) -> Option<Child> {
    Command::new("python3")
        .arg("-I")
        .arg(script)
        .args([ytdlp, fmt, js, &parallel.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| log::error!("resolver: can't start python3: {e}"))
        .ok()
}

type Pending = Arc<Mutex<HashMap<String, Instant>>>;

/// Answers as the helper finishes them (any order). When it exits, whatever was
/// still asked gets a "stopped" answer, so nothing waits forever.
fn read_answers(name: &'static str, out: ChildStdout, pending: Pending, answer: Sender<Answer>) {
    std::thread::spawn(move || {
        let mut lines = BufReader::new(out);
        let mut line = String::new();
        while lines.read_line(&mut line).unwrap_or(0) > 0 {
            let id = serde_json::from_str::<serde_json::Value>(&line).ok().and_then(|v| v["id"].as_str().map(str::to_string));
            if let Some(id) = id {
                let res = parse(&line);
                let took = pending.lock().unwrap().remove(&id).map_or(0.0, |t| t.elapsed().as_secs_f32());
                log::info!("{name}: {id} in {took:.1} s ({})", if res.is_ok() { "ok" } else { "failed" });
                let _ = answer.send((id, res));
            }
            line.clear();
        }
        for (id, _) in pending.lock().unwrap().drain() {
            let _ = answer.send((id, Err("the stream finder stopped".into())));
        }
    });
}

fn worker(name: &'static str, parallel: usize, rx: Receiver<String>, answer: Sender<Answer>, ytdlp: String, fmt: String, js: String) {
    let script = linen_core::paths::data_dir().join("resolver.py");
    let _ = std::fs::write(&script, SCRIPT);
    std::thread::Builder::new()
        .name(format!("linen-{name}"))
        .spawn(move || {
            let pending: Pending = Default::default();
            let start = |pending: &Pending| -> Option<Child> {
                let mut c = spawn_helper(&script, &ytdlp, &fmt, &js, parallel)?;
                read_answers(name, c.stdout.take()?, pending.clone(), answer.clone());
                Some(c)
            };
            // start right away, so Python and yt-dlp are loaded before the first click
            let mut child = start(&pending);
            for line in rx {
                // "<id>\t<format>"
                let id = line.split('\t').next().unwrap_or_default().to_string();
                // (re)start the helper when needed: after it died
                if child.as_mut().is_none_or(|c| c.try_wait().ok().flatten().is_some()) {
                    child = start(&pending);
                }
                pending.lock().unwrap().insert(id.clone(), Instant::now());
                let sent = child.as_mut().and_then(|c| c.stdin.as_mut()).is_some_and(|i| writeln!(i, "{line}").and_then(|_| i.flush()).is_ok());
                if !sent {
                    pending.lock().unwrap().remove(&id);
                    let _ = answer.send((id, Err("couldn't start the stream finder".into())));
                    child = None;
                }
            }
        })
        .expect("spawn resolver thread");
}

impl Streams {
    /// The same JSON the helper answers with (to keep links in the database).
    pub fn to_json(&self) -> String {
        let headers: serde_json::Map<String, serde_json::Value> =
            self.headers.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))).collect();
        serde_json::json!({
            "ok": true,
            "video": self.video,
            "audio": self.audio,
            "duration": self.duration,
            "available_at": self.available_at,
            "headers": headers,
        })
        .to_string()
    }

    pub fn from_json(data: &str) -> Option<Self> {
        parse(data).ok()
    }

    /// When YouTube stops accepting these links (unix time, from the link itself).
    pub fn expires(&self) -> Option<i64> {
        let q = self.video.split_once('?')?.1;
        q.split('&').find_map(|kv| kv.strip_prefix("expire=")).and_then(|v| v.parse().ok())
    }
}

fn parse(line: &str) -> Result<Streams, String> {
    let v: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if v["ok"].as_bool() != Some(true) {
        return Err(v["error"].as_str().unwrap_or("couldn't get this video").to_string());
    }
    Ok(Streams {
        video: v["video"].as_str().ok_or("no video stream")?.to_string(),
        audio: v["audio"].as_str().map(str::to_string),
        duration: v["duration"].as_f64(),
        available_at: v["available_at"].as_f64().map(|t| t as u64),
        headers: v["headers"]
            .as_object()
            .map(|h| h.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default(),
    })
}

impl Resolver {
    pub fn start(ytdlp: &str, fmt: &str, js: Option<&str>, answer: Sender<Answer>) -> Self {
        let (urgent, urx) = channel();
        let (prefetch, prx) = channel();
        let js = js.unwrap_or("").to_string();
        // one at a time: it's the warmed-up one (a second would start cold)
        worker("resolve", 1, urx, answer.clone(), ytdlp.into(), fmt.into(), js.clone());
        // lookups mostly wait on the network: two at a time in one process (plus
        // one free thread for a click racing the click helper's warm-up)
        worker("prefetch", PREFETCH_PARALLEL + 1, prx, answer, ytdlp.into(), fmt.into(), js);
        Self { urgent, prefetch, fmt: Arc::new(Mutex::new(fmt.into())) }
    }

    /// Lookups from now on ask for this format.
    pub fn set_format(&self, fmt: &str) {
        *self.fmt.lock().unwrap() = fmt.to_string();
    }

    fn line(&self, id: &str) -> String {
        format!("{id}\t{}", self.fmt.lock().unwrap())
    }

    pub fn urgent(&self, id: &str) {
        let _ = self.urgent.send(self.line(id));
    }

    pub fn prefetch(&self, id: &str) {
        let _ = self.prefetch.send(self.line(id));
    }

    /// A click while the click helper is busy: the prefetch helper looks too
    /// (on its spare thread), and the first answer is used.
    pub fn race(&self, id: &str) {
        self.prefetch(id);
    }
}
