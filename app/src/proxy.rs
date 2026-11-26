//! A tiny relay on 127.0.0.1 between mpv and YouTube's video servers.
//!
//! YouTube slows one long open-ended download to barely the video's own bitrate
//! (~1.5 Mbit/s: any hiccup and it buffers), but serves short ranges at full
//! speed. mpv can only make the long request, so it asks this relay instead, and
//! the relay fetches the video in small pieces, three at a time (~5x faster), much
//! like yt-dlp downloads. Only this computer can connect to it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Pieces grow: small first (mpv gets an answer at once, and opening a video
/// makes several short reads), then big ones for speed.
/// Kept small: YouTube slows big ranges down too (4 MB pieces crawled at times).
const PIECES: [u64; 3] = [256 << 10, 512 << 10, 1 << 20];
const AT_ONCE: usize = 3;

fn piece(n: usize) -> u64 {
    PIECES[n.min(PIECES.len() - 1)]
}

#[derive(Clone)]
struct Upstream {
    url: String,
    headers: Vec<(String, String)>,
    /// total size, when known
    len: Option<u64>,
    /// for a video: its audio stream (mpv asks for the audio only after the video;
    /// the relay starts on the audio it'll want right away)
    partner: Option<u64>,
}

/// Audio fetched ahead on a guess: where it starts and ends.
struct Guess {
    from: u64,
    to: u64,
    /// the time it was for
    when: f64,
    piece: Arc<Piece>,
    at: std::time::Instant,
}

type Guesses = Arc<Mutex<HashMap<u64, Guess>>>;

/// Where Linen just asked mpv to skip to (seconds), and when.
type Hint = Arc<Mutex<Option<(f64, std::time::Instant)>>>;

/// The running relay (for the skip hints).
pub static RELAY: std::sync::OnceLock<Arc<Proxy>> = std::sync::OnceLock::new();

pub struct Proxy {
    port: u16,
    hint: Hint,
    http: reqwest::blocking::Client,
    streams: Arc<Mutex<HashMap<u64, Upstream>>>,
    next: AtomicU64,
}

impl Proxy {
    pub fn start() -> Option<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| log::warn!("relay: {e}")).ok()?;
        let port = listener.local_addr().ok()?.port();
        let streams: Arc<Mutex<HashMap<u64, Upstream>>> = Default::default();
        let guesses: Guesses = Default::default();
        let hint: Hint = Default::default();
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(8)
            // keep warmed-up connections around for a while
            .pool_idle_timeout(std::time::Duration::from_secs(240))
            .build()
            .ok()?;
        {
            let streams = streams.clone();
            let http = http.clone();
            let hint = hint.clone();
            std::thread::Builder::new()
                .name("linen-relay".into())
                .spawn(move || {
                    for conn in listener.incoming().flatten() {
                        let (streams, http, guesses, hint) = (streams.clone(), http.clone(), guesses.clone(), hint.clone());
                        std::thread::spawn(move || {
                            if let Err(e) = serve(conn, &streams, &http, &guesses, &hint) {
                                log::debug!("relay: {e:#}");
                            }
                        });
                    }
                })
                .ok()?;
        }
        log::info!("relay on 127.0.0.1:{port}");
        let me = Arc::new(Self { port, hint, http, streams, next: AtomicU64::new(1) });
        let _ = RELAY.set(me.clone());
        Some(me)
    }

    /// mpv is about to skip to this time: its next video request is for there.
    pub fn expect_time(&self, secs: f64) {
        *self.hint.lock().unwrap() = Some((secs, std::time::Instant::now()));
    }

    /// Open (and keep open) connections to a video's servers before it's clicked:
    /// setting one up takes about a second on a slow line.
    pub fn warm(&self, urls: Vec<String>, headers: Vec<(String, String)>) {
        let http = self.http.clone();
        std::thread::spawn(move || {
            for url in urls {
                let up = Upstream { url, headers: headers.clone(), len: None, partner: None };
                let _ = fetch(&http, &up, 0, 0);
            }
        });
    }

    /// Local addresses for a video and its audio (linked, so the audio can be
    /// fetched ahead when the video is asked for).
    pub fn add_pair(&self, video: &str, audio: &str, headers: &[(String, String)]) -> (String, String) {
        let a = self.add(audio, headers);
        let a_id = a.rsplit('/').next().and_then(|n| n.parse().ok());
        (self.add_linked(video, headers, a_id), a)
    }

    /// A local address mpv can open for this YouTube stream.
    pub fn add(&self, url: &str, headers: &[(String, String)]) -> String {
        self.add_linked(url, headers, None)
    }

    fn add_linked(&self, url: &str, headers: &[(String, String)], partner: Option<u64>) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        // YouTube puts the size in the address ("clen=")
        let len = url.split(['?', '&']).find_map(|kv| kv.strip_prefix("clen=")).and_then(|v| v.parse().ok());
        let mut streams = self.streams.lock().unwrap();
        // keep the last few (a video, its audio, the one before)
        if streams.len() > 16 {
            let oldest = *streams.keys().min().unwrap();
            streams.remove(&oldest);
        }
        streams.insert(n, Upstream { url: url.to_string(), headers: headers.to_vec(), len, partner });
        format!("http://127.0.0.1:{}/{n}", self.port)
    }
}

fn fetch(http: &reqwest::blocking::Client, up: &Upstream, from: u64, to: u64) -> anyhow::Result<(u16, Option<u64>, Vec<u8>)> {
    let mut last = None;
    let t = std::time::Instant::now();
    for attempt in 0..3 {
        if attempt > 0 {
            log::debug!("piece {from}-{to}: retry {attempt} after {last:?}");
        }
        let mut req = http.get(&up.url).header("Range", format!("bytes={from}-{to}"));
        for (k, v) in &up.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        match req.send() {
            Ok(res) => {
                let status = res.status().as_u16();
                let total = res
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.rsplit('/').next())
                    .and_then(|v| v.parse().ok());
                if status >= 400 {
                    return Ok((status, total, vec![]));
                }
                match res.bytes() {
                    Ok(b) => {
                        log::debug!("piece {from}-{to}: {status}, {} KB in {} ms", b.len() / 1024, t.elapsed().as_millis());
                        return Ok((status, total, b.to_vec()));
                    }
                    Err(e) => last = Some(anyhow::anyhow!(e)),
                }
            }
            Err(e) => last = Some(anyhow::anyhow!(e)),
        }
        std::thread::sleep(std::time::Duration::from_millis(300 * (attempt + 1)));
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("fetch failed")))
}

/// How much audio is fetched around the guessed spot (little before it: a piece
/// downloads from its start, so the right bytes should come first).
const GUESS_BEFORE: u64 = 256 << 10;
const GUESS_SIZE: u64 = 1280 << 10;

/// Per audio stream: where its bytes really were for the times skipped to so far.
/// The plain time/length estimate is a few % off (index at the start, the rate
/// drifting with the content); a line through these points does much better.
static AUDIO_SEEN: std::sync::LazyLock<Mutex<HashMap<u64, Vec<(f64, f64)>>>> = std::sync::LazyLock::new(Default::default);

/// Where in an audio file `when` (seconds) probably is.
fn audio_byte(seen: &[(f64, f64)], when: f64, alen: f64, dur: f64) -> f64 {
    match seen {
        [] => when / dur.max(1.0) * alen,
        [(t, b)] => b / t.max(1.0) * when,
        _ => {
            // least squares: byte = a + rate * time
            let n = seen.len() as f64;
            let (st, sb) = seen.iter().fold((0.0, 0.0), |(st, sb), (t, b)| (st + t, sb + b));
            let (mt, mb) = (st / n, sb / n);
            let (num, den) = seen.iter().fold((0.0, 0.0), |(num, den), (t, b)| (num + (t - mt) * (b - mb), den + (t - mt) * (t - mt)));
            if den < 1.0 {
                return mb / mt.max(1.0) * when;
            }
            let rate = num / den;
            mb + rate * (when - mt)
        }
    }
}

fn serve(
    conn: TcpStream,
    streams: &Mutex<HashMap<u64, Upstream>>,
    http: &reqwest::blocking::Client,
    guesses: &Guesses,
    hint: &Hint,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut out = conn;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let mut range: Option<(u64, Option<u64>)> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("range:").map(|v| v.trim().to_string()) {
            if let Some(spec) = v.strip_prefix("bytes=") {
                let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
                range = a.parse().ok().map(|a| (a, b.parse().ok()));
            }
        }
    }
    let id: u64 = path.trim_start_matches('/').parse().unwrap_or(0);
    let Some(mut up) = streams.lock().unwrap().get(&id).cloned() else {
        out.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    };
    let start = range.map_or(0, |r| r.0);
    // a video: start on its audio now, at the same point in time. Audio runs at a
    // near-steady rate, so time / length gives its spot in the file (the video's
    // own bytes don't: its bitrate varies too much).
    if let Some(a_id) = up.partner {
        let audio = streams.lock().unwrap().get(&a_id).cloned();
        let when = if start == 0 {
            Some(0.0)
        } else {
            hint.lock().unwrap().take().filter(|h| h.1.elapsed() < std::time::Duration::from_secs(5)).map(|h| h.0)
        };
        let dur = audio.as_ref().and_then(|a| a.url.split(['?', '&']).find_map(|kv| kv.strip_prefix("dur=")).and_then(|d| d.parse::<f64>().ok()));
        if let (Some(audio), Some(when), Some(dur)) = (audio.filter(|a| a.len.is_some()), when, dur) {
            let alen = audio.len.unwrap_or(0);
            let seen = AUDIO_SEEN.lock().unwrap().get(&a_id).cloned().unwrap_or_default();
            let guess = audio_byte(&seen, when, alen as f64, dur).clamp(0.0, alen as f64) as u64;
            let from = guess.saturating_sub(GUESS_BEFORE);
            let to = (from + GUESS_SIZE - 1).min(alen.saturating_sub(1));
            log::debug!("relay {id}: video from byte {start} at {when:.0} s -> audio guess {guess}/{alen} (window {from}-{to})");
            let piece = Piece::start(http.clone(), Arc::new(audio), from, to);
            guesses.lock().unwrap().insert(a_id, Guess { from, to, piece, when, at: std::time::Instant::now() });
        }
    }
    // audio that was guessed right: no wait for YouTube at all
    let guess = guesses.lock().unwrap().remove(&id);
    // remember where that time really was, for the next skip in this video
    if let Some(g) = guess.as_ref().filter(|g| g.when > 5.0 && start > 0) {
        let mut all = AUDIO_SEEN.lock().unwrap();
        let seen = all.entry(id).or_default();
        seen.push((g.when, start as f64));
        if seen.len() > 12 {
            seen.remove(0);
        }
    }
    if let (Some(g), Some(len)) = (guess, up.len) {
        // useful only if it has (nearly) reached that spot already; otherwise a fresh
        // request straight to it is quicker than downloading up to it
        let have = g.from + g.piece.state.lock().unwrap().0.len() as u64;
        let close = start <= have + (128 << 10);
        if close && g.from <= start && start <= g.to && g.at.elapsed() < std::time::Duration::from_secs(20) && method != "HEAD" {
            log::debug!("relay {id}: from byte {start}: guessed right ({} KB in)", (start - g.from) / 1024);
            let end = range.and_then(|r| r.1).map_or(len - 1, |e| e.min(len - 1));
            let head = if range.is_some() {
                format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{len}\r\n")
            } else {
                "HTTP/1.1 200 OK\r\n".to_string()
            };
            out.write_all(
                format!("{head}Content-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", end - start + 1)
                    .as_bytes(),
            )?;
            let skip = (start - g.from) as usize;
            if !g.piece.send_to(&mut out, skip, (end - start + 1) as usize)? {
                return Ok(());
            }
            return send_rest(&mut out, http, Arc::new(up), g.to.min(end) + 1, end, 1);
        }
    }
    // the first piece also tells us the size (and whether YouTube says yes at all)
    let t = std::time::Instant::now();
    let first_end = start + piece(0) - 1;
    let first_end = up.len.map_or(first_end, |l| first_end.min(l.saturating_sub(1)));
    let mut req = http.get(&up.url).header("Range", format!("bytes={start}-{first_end}"));
    for (k, v) in &up.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let first = req.send()?;
    let status = first.status().as_u16();
    let total = first
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.parse().ok());
    log::debug!("relay {id}: {method} from byte {start} (to {:?}): answer in {} ms", range.and_then(|r| r.1), t.elapsed().as_millis());
    if status >= 400 {
        // passed on, so Linen's "try again shortly" logic sees it
        out.write_all(format!("HTTP/1.1 {status} Upstream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes())?;
        return Ok(());
    }
    if up.len.is_none() {
        up.len = total;
    }
    let Some(len) = up.len else {
        anyhow::bail!("unknown size");
    };
    let end = range.and_then(|r| r.1).map_or(len - 1, |e| e.min(len - 1));
    let head = if range.is_some() {
        format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{len}\r\n")
    } else {
        "HTTP/1.1 200 OK\r\n".to_string()
    };
    out.write_all(
        format!("{head}Content-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", end - start + 1)
            .as_bytes(),
    )?;
    if method == "HEAD" {
        return Ok(());
    }
    // Every piece is passed on as its bytes arrive, while the next ones download
    // alongside. (Waiting for whole pieces left mpv stuck mid-read when you
    // skipped, before it could even ask for the new spot.)
    let up = Arc::new(up);
    let first_piece = Piece::new();
    {
        let p = first_piece.clone();
        let want = (first_end - start + 1).min(end - start + 1);
        std::thread::spawn(move || p.fill(std::io::Read::take(first, want)));
    }
    let mut pieces: std::collections::VecDeque<Arc<Piece>> = [first_piece].into();
    let next = first_end.min(end) + 1;
    pump(&mut out, http, &up, &mut pieces, next, end, 1)
}

/// Everything from `next` on, in pieces.
fn send_rest(out: &mut TcpStream, http: &reqwest::blocking::Client, up: Arc<Upstream>, next: u64, end: u64, n: usize) -> anyhow::Result<()> {
    let mut pieces = Default::default();
    pump(out, http, &up, &mut pieces, next, end, n)
}

fn pump(
    out: &mut TcpStream,
    http: &reqwest::blocking::Client,
    up: &Arc<Upstream>,
    pieces: &mut std::collections::VecDeque<Arc<Piece>>,
    mut next: u64,
    end: u64,
    mut n: usize,
) -> anyhow::Result<()> {
    loop {
        while pieces.len() < AT_ONCE && next <= end {
            let (a, b) = (next, (next + piece(n) - 1).min(end));
            n += 1;
            pieces.push_back(Piece::start(http.clone(), up.clone(), a, b));
            next = b + 1;
        }
        let Some(current) = pieces.front().cloned() else { return Ok(()) };
        // mpv went away (skipped elsewhere, closed): stop; the downloads see it and stop too
        if !current.send_to(out, 0, usize::MAX)? {
            return Ok(());
        }
        pieces.pop_front();
    }
}

/// One range of the video, downloading into a buffer that the sender drains as it grows.
struct Piece {
    state: Mutex<(Vec<u8>, bool, Option<String>)>, // bytes so far, finished, error
    more: std::sync::Condvar,
}

impl Piece {
    fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new((Vec::new(), false, None)), more: std::sync::Condvar::new() })
    }

    fn start(http: reqwest::blocking::Client, up: Arc<Upstream>, from: u64, to: u64) -> Arc<Self> {
        let p = Self::new();
        let me = p.clone();
        std::thread::spawn(move || {
            let mut err = None;
            for attempt in 0..3u64 {
                let mut req = http.get(&up.url).header("Range", format!("bytes={from}-{to}"));
                for (k, v) in &up.headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                match req.send() {
                    Ok(res) if res.status().as_u16() < 400 => return me.fill(res),
                    Ok(res) => err = Some(format!("YouTube answered {} at byte {from}", res.status())),
                    Err(e) => err = Some(e.to_string()),
                }
                // nobody waits for it any more
                if Arc::strong_count(&me) == 1 {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(300 * (attempt + 1)));
            }
            me.finish(err);
        });
        p
    }

    fn fill(self: Arc<Self>, mut body: impl std::io::Read) {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            // the sender gave up on this connection: stop downloading
            if Arc::strong_count(&self) == 1 {
                return;
            }
            match body.read(&mut buf) {
                Ok(0) => return self.finish(None),
                Ok(n) => {
                    self.state.lock().unwrap().0.extend_from_slice(&buf[..n]);
                    self.more.notify_all();
                }
                Err(e) => return self.finish(Some(e.to_string())),
            }
        }
    }

    fn finish(&self, err: Option<String>) {
        let mut s = self.state.lock().unwrap();
        s.1 = true;
        s.2 = err;
        self.more.notify_all();
    }

    /// Pass it on as it comes, from byte `skip` of the piece, at most `limit` bytes;
    /// false if mpv hung up.
    fn send_to(&self, out: &mut TcpStream, skip: usize, limit: usize) -> anyhow::Result<bool> {
        let mut sent = skip;
        let stop = skip.saturating_add(limit);
        loop {
            if sent >= stop {
                return Ok(true);
            }
            let chunk = {
                let mut s = self.state.lock().unwrap();
                while s.0.len() <= sent && !s.1 {
                    s = self.more.wait_timeout(s, std::time::Duration::from_millis(250)).unwrap().0;
                }
                if s.0.len() <= sent {
                    if let Some(e) = &s.2 {
                        anyhow::bail!("{e}");
                    }
                    return Ok(true);
                }
                s.0[sent..s.0.len().min(stop)].to_vec()
            };
            if out.write_all(&chunk).is_err() {
                return Ok(false);
            }
            sent += chunk.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_guesses() {
        // real points from a 3.7 h stream (time, byte)
        let pts = [(600.0, 8967948.0), (900.0, 13499881.0), (300.0, 4594705.0), (1200.0, 17916535.0)];
        let g = audio_byte(&pts, 2000.0, 200189454.0, 13393.0);
        assert!((g - 29083871.0).abs() < (256 << 10) as f64 + 600_000.0, "{g}");
        assert_eq!(audio_byte(&[], 0.0, 100.0, 10.0), 0.0);
        assert!((audio_byte(&[(100.0, 1000.0)], 200.0, 0.0, 0.0) - 2000.0).abs() < 1.0);
    }
    use std::io::Read;

    /// LINEN_TEST_STREAM=<json with "video" and "headers"> cargo test -p linen-player relay -- --ignored --nocapture
    #[test]
    #[ignore]
    fn relay_throughput() {
        let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).try_init();
        let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(std::env::var("LINEN_TEST_STREAM").unwrap()).unwrap()).unwrap();
        let headers: Vec<(String, String)> =
            j["headers"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())).collect();
        let relay = Proxy::start().unwrap();
        let url = relay.add(j["video"].as_str().unwrap(), &headers);
        let addr = url.trim_start_matches("http://");
        let (host, path) = addr.split_once('/').unwrap();
        let mut c = TcpStream::connect(host).unwrap();
        write!(c, "GET /{path} HTTP/1.1\r\nHost: {host}\r\nRange: bytes=0-\r\n\r\n").unwrap();
        let t = std::time::Instant::now();
        let (mut got, mut buf, mut last) = (0usize, vec![0u8; 65536], 0u64);
        while t.elapsed().as_secs() < 15 {
            let n = c.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got += n;
            let s = t.elapsed().as_secs();
            if s != last {
                println!("{s:2} s: {:.1} MB so far", got as f64 / 1e6);
                last = s;
            }
        }
        println!("{:.1} MB in {:.1} s = {:.1} Mbit/s", got as f64 / 1e6, t.elapsed().as_secs_f64(), got as f64 * 8.0 / 1e6 / t.elapsed().as_secs_f64());
    }
}
