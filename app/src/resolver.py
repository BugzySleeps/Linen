# Linen's stream resolver: keeps yt-dlp loaded (Python + yt-dlp take ~2 s to
# start) and answers one request per line on stdin with one JSON line.
#   in:  <video id>  or  <video id>\t<yt-dlp format>  (the quality picked in Settings)
#   out: {"id", "ok", "video", "audio", "duration", "headers"} or {"id", "ok": false, "error"}
# Several lookups can run at once (argv[4]); answers come back as they finish.
import json
import os
import re
import sys
import threading
from concurrent.futures import ThreadPoolExecutor

ytdlp_path, fmt, js = sys.argv[1], sys.argv[2], sys.argv[3]
parallel = int(sys.argv[4]) if len(sys.argv) > 4 else 1
if ytdlp_path.endswith("yt-dlp") and not ytdlp_path.endswith(".py"):
    sys.path.insert(0, ytdlp_path)  # the release file is a zip: importable as-is
import yt_dlp  # noqa: E402


# ---- a kept-running Node for YouTube's JavaScript challenge ----
# yt-dlp starts Node for every video and has it process YouTube's 2.5 MB player
# script from scratch (~1.2 s). Here one Node stays running and remembers the
# processed player (it only changes every few days): ~60 ms per video instead.
# If yt-dlp's internals ever change, this quietly isn't used.
SOLVER = r"""
const vm = require('vm');
const crypto = require('crypto');
const solvers = new Map();   // processed player -> its n/sig functions
const processed = new Map(); // raw player -> processed player
const hash = (s) => crypto.createHash('sha1').update(s).digest('hex');
function functionsFor(pre) {
  const key = hash(pre);
  let t = solvers.get(key);
  if (!t) {
    t = { n: null, sig: null };
    Function('_result', pre)(t);
    solvers.set(key, t);
    if (solvers.size > 3) solvers.delete(solvers.keys().next().value);
  }
  return t;
}
function solve(pre, requests) {
  const t = functionsFor(pre);
  return {
    type: 'result',
    responses: requests.map((r) => {
      const f = t[r.type];
      if (!f) return { type: 'error', error: `Failed to extract ${r.type} function` };
      try { return { type: 'result', data: Object.fromEntries(r.challenges.map((c) => [c, f(c)])) }; }
      catch (e) { return { type: 'error', error: String(e) }; }
    }),
  };
}
const rl = require('readline').createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on('line', (line) => {
  let out;
  try {
    const msg = JSON.parse(line);
    if (msg.init) {
      vm.runInThisContext(msg.init);
      out = { type: 'ready' };
    } else if (msg.type === 'preprocessed') {
      out = solve(msg.preprocessed_player, msg.requests);
    } else {
      const key = hash(msg.player);
      let pre = processed.get(key);
      if (!pre) {
        const first = globalThis.jsc({ ...msg, output_preprocessed: true });
        if (first.type === 'result' && first.preprocessed_player) {
          pre = first.preprocessed_player;
          processed.set(key, pre);
          if (processed.size > 2) processed.delete(processed.keys().next().value);
        } else {
          out = first;
        }
      }
      if (pre) {
        out = solve(pre, msg.requests);
        if (msg.output_preprocessed) out.preprocessed_player = pre;
      }
    }
  } catch (e) {
    out = { type: 'error', error: String((e && e.stack) || e) };
  }
  process.stdout.write(JSON.stringify(out) + '\n');
});
"""


def warm_solver():
    import subprocess

    from yt_dlp.extractor.youtube.jsc._builtin import ejs, node

    # keep the processed player in yt-dlp's cache too: a new helper then skips
    # processing it (~1.2 s on the first video after starting)
    ejs.EJSBaseJCP._ENABLE_PREPROCESSED_PLAYER_CACHE = True
    # (~4 MB each, a new player every few days: keep the 4 newest)
    try:
        cache = os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "yt-dlp", "challenge-solver")
        old = sorted((os.path.join(cache, n) for n in os.listdir(cache) if n.startswith("player,")), key=os.path.getmtime)
        for o in old[:-4]:
            os.remove(o)
    except OSError:
        pass
    orig_construct = ejs.EJSBaseJCP._construct_stdin
    orig_run = node.NodeJCP._run_js_runtime
    state = {"proc": None}
    # one Node for all lookup threads, one question at a time (~60 ms each)
    lock = threading.Lock()
    fallback = threading.local()

    def construct(self, player, preprocessed, requests):
        # what the normal path would send, in case the kept-running one fails
        fallback.stdin = orig_construct(self, player, preprocessed, requests)
        reqs = [{"type": r.type.value, "challenges": r.input.challenges} for r in requests]
        if preprocessed:
            data = {"type": "preprocessed", "preprocessed_player": player, "requests": reqs}
        else:
            data = {"type": "player", "player": player, "requests": reqs, "output_preprocessed": True}
        return "\x00WARM" + json.dumps(data)

    def run(self, stdin):
        if not stdin.startswith("\x00WARM"):
            return orig_run(self, stdin)
        with lock:
            line = ask(self, stdin)
        return line if line else orig_run(self, fallback.stdin)

    def ask(self, stdin):
        for _ in range(2):
            try:
                p = state["proc"]
                if p is None or p.poll() is not None:
                    # the same sandbox yt-dlp uses: no files, no network
                    p = subprocess.Popen(
                        [self.runtime_info.path, "--permission", "-e", SOLVER],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
                    )
                    init = self._lib_script.code + "\nObject.assign(globalThis, lib);\n" + self._core_script.code
                    p.stdin.write(json.dumps({"init": init}) + "\n")
                    p.stdin.flush()
                    if json.loads(p.stdout.readline() or "{}").get("type") != "ready":
                        raise RuntimeError("solver didn't start")
                    state["proc"] = p
                p.stdin.write(stdin[5:] + "\n")
                p.stdin.flush()
                line = p.stdout.readline()
                if line:
                    return line
            except Exception:  # noqa: BLE001
                pass
            state["proc"] = None
        return None

    ejs.EJSBaseJCP._construct_stdin = construct
    node.NodeJCP._run_js_runtime = run



# ---- YouTube's player script, kept on disk ----
# Every new helper downloaded the 2.5 MB player (~2 s) before its first answer,
# though it only changes every few days. Kept beside this script; the 4 newest stay.
PLAYERS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "players")


def cache_players():
    from yt_dlp.extractor.youtube._video import YoutubeIE

    orig = YoutubeIE._load_player

    def load(self, video_id, player_url, fatal=True):
        key = self._player_js_cache_key(player_url)
        if key in self._code_cache:
            return self._code_cache[key]
        path = os.path.join(PLAYERS, re.sub(r"[^A-Za-z0-9_-]", "_", key) + ".js")
        try:
            with open(path, encoding="utf-8") as f:
                code = f.read()
            if len(code) > 100_000:
                self._code_cache[key] = code
                return code
        except OSError:
            pass
        code = orig(self, video_id, player_url, fatal)
        if code:
            try:
                os.makedirs(PLAYERS, exist_ok=True)
                with open(path + ".part", "w", encoding="utf-8") as f:
                    f.write(code)
                os.replace(path + ".part", path)
                old = sorted((os.path.join(PLAYERS, n) for n in os.listdir(PLAYERS)), key=os.path.getmtime)
                for o in old[:-4]:
                    os.remove(o)
            except OSError:
                pass
        return code

    YoutubeIE._load_player = load


try:
    cache_players()
except Exception:  # noqa: BLE001
    pass

opts = {
    "quiet": True,
    "no_warnings": True,
    "noplaylist": True,
    "format": fmt,
    # the HLS list is never used (we take the separate H.264 + audio streams)
    "extractor_args": {"youtube": {"skip": ["hls"]}},
}
if js:
    name, _, path = js.partition(":")
    opts["js_runtimes"] = {name: {"path": path or None}}
    if name == "node":
        try:
            warm_solver()
        except Exception:  # noqa: BLE001
            pass
local = threading.local()
write = threading.Lock()


def lookup(vid, want):
    # a yt-dlp per thread and quality (they keep per-lookup state)
    ydls = getattr(local, "ydls", None)
    if ydls is None:
        ydls = local.ydls = {}
    ydl = ydls.get(want)
    if ydl is None:
        ydl = ydls[want] = yt_dlp.YoutubeDL(dict(opts, format=want))
    try:
        info = ydl.extract_info(f"https://www.youtube.com/watch?v={vid}", download=False)
        parts = info.get("requested_formats") or [info]
        video = next((p for p in parts if p.get("vcodec") not in (None, "none")), parts[0])
        audio = next((p for p in parts if p is not video and p.get("acodec") not in (None, "none")), None)
        out = {
            "id": vid,
            "ok": True,
            "video": video["url"],
            "audio": audio["url"] if audio else None,
            "duration": info.get("duration"),
            # YouTube makes some links usable only a few seconds after they're
            # handed out (403 before that); unix time
            "available_at": max((p.get("available_at") or 0) for p in parts) or None,
            # YouTube refuses (403) stream requests without the same headers yt-dlp used
            "headers": video.get("http_headers") or {},
        }
    except Exception as e:  # noqa: BLE001
        out = {"id": vid, "ok": False, "error": str(e).splitlines()[0][:300]}
    with write:
        sys.stdout.write(json.dumps(out) + "\n")
        sys.stdout.flush()


pool = ThreadPoolExecutor(max_workers=parallel)
for line in sys.stdin:
    vid, _, want = line.strip().partition("\t")
    if vid:
        pool.submit(lookup, vid, want or fmt)
