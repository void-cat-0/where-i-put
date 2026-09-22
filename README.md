# where-i-put

[![CI](https://github.com/void-cat-0/where-i-put/actions/workflows/ci.yml/badge.svg)](https://github.com/void-cat-0/where-i-put/actions/workflows/ci.yml)

Vision-based item memory: watch cameras (or a Frigate NVR's events), remember
where objects were last seen, answer "where are my keys?".

**Cross-platform by design**: Windows, Linux and macOS are all first-class
targets — not a port to be attempted later. Platform-specific behaviour
(services, signals, atomic file replacement) must be stated explicitly rather
than silently defaulting to one OS; `#[cfg(...)]` is reserved for genuinely
platform-specific code (signal kinds, locking primitives). The only
deliberately single-platform pieces today are the RTSP toolchain fallbacks
(no BtbN prebuilt for macOS, which uses a system/brew FFmpeg instead).

Crates, one-way dependencies (`item-ingest`/`item-query`/`item-web` -> `item-core`;
`item-web` also reuses `item-query`'s prompt+VLM client):

- **crates/core (`item-core`)** — domain model (`Detection`, `Observation`,
  `Region`), box geometry (IoU, greedy NMS), SQLite storage. The unit of truth
  is the *Observation*: "label X was in zone Z of camera C during \[first_seen,
  last_seen]", merged while sightings stay within a 5-min dedup window.
- **crates/item-ingest** — the write side. `FrameSource` (Mock; USB/built-in
  webcams via nokhwa behind `--features camera`; RTSP/IP cameras via
  ffmpeg-next behind `--features rtsp`) -> `Detector` (Null without features;
  real YOLOv8-onnx behind `--features yolo` via ort; open-vocabulary VLM
  grounding behind `--features vlm` — an OpenAI-compatible multimodal sidecar
  answers with labeled boxes, so keys/remotes beyond COCO-80 work) -> NMS ->
  zone mapping -> store. Also an axum webhook server that ingests Frigate
  events directly, skipping local detection entirely, plus an MJPEG web
  preview bridge for RTSP cameras (`--preview`, feature `rtsp`). Runs as a
  resident daemon (`--daemon --config`, see below) that drives every configured
  camera at once, publishes a health snapshot, and keeps its own database and
  snapshot directory bounded.
- **crates/item-query** — the read side. CLI (`log`, `ask`) over observations,
  with an OpenAI-compatible VLM client (llama.cpp/Ollama/cloud sidecar) used
  only when `ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL` are set. The Rust core never
  embeds a VLM.
- **crates/item-web** — the display side. Read-only web UI (binds 127.0.0.1:8478
  by default): one binary serving a zero-build vanilla-JS card grid (snapshot,
  zone/camera chips, hit count, relative time; live-search filter; ask bar that
  answers from the log or via the VLM sidecar when configured; click-to-zoom
  snapshot modal; 10 s auto-refresh) plus a small JSON API (`/api/observations`,
  `/api/observation/{id}/snapshot`, `/api/ask`). Opens the same SQLite WAL file
  the daemon writes, read-only — no write path, no contention, no shared process.

## Quick start

```sh
# pipeline smoke test, no hardware needed
cargo run -p item-ingest -- --demo

# RTSP camera (one-time: `cargo xtask setup` to fetch FFmpeg + libclang)
cargo run --features rtsp -p item-ingest -- --rtsp "rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101" --camera-id living

# Built-in/USB webcam: same closed loop WITHOUT FFmpeg/native setup
cargo run --features "camera,yolo" -p item-ingest -- --webcam 0 --camera-id laptop --detect-fps 2

# live web preview (MJPEG bridge; browsers can't speak RTSP) — open http://<host>:8477/preview
cargo run --features rtsp -p item-ingest -- --preview "rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101"

# object detection on one image (needs a local models/yolov8n.onnx, see below)
cargo run --features yolo -p item-ingest -- --detect path/to/photo.jpg

# open-vocabulary detection on one image via a VLM sidecar (see
# docs/vlm-sidecar.md for standing up llama.cpp + Qwen2.5-VL in minutes)
ITEM_VLM_BASE_URL=http://127.0.0.1:8080/v1 ITEM_VLM_MODEL=qwen2.5-vl-3b-instruct \
cargo run --features vlm -p item-ingest -- \
    --detect path/to/photo.jpg --detector vlm --targets "remote,keys" --out out.png

# THE CLOSED LOOP with the VLM detector: camera -> throttled grounding ->
# zone-mapped observations + snapshots, labels beyond COCO (keys, remote, ...)
ITEM_VLM_BASE_URL=http://127.0.0.1:8080/v1 ITEM_VLM_MODEL=qwen2.5-vl-3b-instruct \
cargo run --features "camera,vlm" -p item-ingest -- --webcam 0 --camera-id desk

# THE CLOSED LOOP: camera -> throttled YOLO -> zone-mapped observations + snapshots
cat > config.toml <<'EOF'
[[camera]]
id = "living"
url = "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102"
[camera.regions]                      # rects in camera pixels
desk = [0.0, 360.0, 1280.0, 720.0]
EOF
cargo run --features "rtsp,yolo" -p item-ingest -- \
    --rtsp "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102" \
    --camera-id living --config config.toml --detect-fps 1
cargo run -p item-query -- log            # what was seen where
cargo run -p item-query -- ask "where is the cup"

# THE WEB UI (read-only; open http://127.0.0.1:8478 while the loop's db exists)
cargo run -p item-web -- --db data/closed-loop.db
# with a VLM sidecar running, `ask` answers in sentences instead of raw log
# rows (works for item-web's ask bar too):
ITEM_VLM_BASE_URL=http://127.0.0.1:8080/v1 ITEM_VLM_MODEL=qwen2.5vl cargo run -p item-web

# webhook receiver (point Frigate event forwarding at POST /frigate/webhook)
cargo run -p item-ingest -- --listen 127.0.0.1:8477
```

## Resident daemon (`--daemon --config <file>`)

One process for a deployment that runs for weeks: the Frigate webhook server, the
optional preview, and one thread per `[[camera]]` row that names a source (`url`, or
`webcam = <index>` for a local camera), all writing through one `Store`. It runs until
it is asked to stop (Ctrl-C, or `SIGTERM` on Unix) and is the only mode that takes the
single-instance lock at `<db dir>/ingest.lock`.

```sh
cat > config.toml <<'EOF'
[daemon]
db = "data/items.db"
snapshots_dir = "data/snapshots"
health_file = "data/health.json"
events_file = "data/events.jsonl"   # appeared/disappeared timeline; "" = off
checkpoint_secs = 300     # WAL checkpoint (TRUNCATE)
retention_days = 90       # 0 = keep observations forever
sweep_secs = 3600         # how often expired observations are pruned
reconcile_secs = 604800   # how often orphan snapshot files are swept

[webhook]
enabled = true
listen = "127.0.0.1:8477"

[[camera]]
id = "living"
url = "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102"
detector = "yolo"
detect_fps = 1.0
[camera.regions]                      # rects in camera pixels
desk = [0.0, 360.0, 1280.0, 720.0]
EOF

cargo run --features "rtsp,yolo" -p item-ingest -- --daemon --config config.toml
```

Relative paths in the config resolve against **the config file's directory**, not the
process cwd: a service manager picks the cwd, and snapshots have to land where
`item-web` looks for them. A row with neither `url` nor `webcam` is fed by Frigate's
webhooks; `enabled = false` keeps a camera out of the daemon without deleting its
regions.

### Is it alive? (`data/health.json`)

Rewritten every 15 s, atomically (`.tmp` + rename), so readers must tolerate the file
being briefly missing or stale:

```json
{"schema": 1, "pid": 12345, "started_at": "…", "updated_at": "2026-09-09T02:37:15Z",
 "seq": 148, "detector_default": "yolo",
 "cameras": [{"id": "living", "state": "running", "frames": 210344, "detections": 21033,
              "recorded": 87, "reconnects": 3, "last_frame_age_s": 1, "last_error": null}]}
```

- **`updated_at` older than 90 s means the process is dead or hung** — the *only*
  liveness signal. `pid` is advisory: Windows would need `OpenProcess`, Unix reuses pids.
- `state` is `starting` / `running` / `reconnecting` / `failed` (gave up, needs a human)
  / `stopped`.
- `last_frame_age_s` staying above ~3x the camera's `detect_fps` means the decode side
  is stuck. Every 5 minutes each camera also logs a `heartbeat` line, so silence never
  looks like success.

### What appeared and disappeared (`data/events.jsonl`)

Each camera also keeps a timeline: an observation becomes an `appeared` line when it
is first seen, and a `disappeared` line once it has gone unseen for `max(30s, 3/detect_fps)`.
Append-only JSONL, one event per line, `jq`/`grep`-ready — and rotated at 64MB keeping
two older generations, so a week of running stays bounded:

```json
{"ts":"2026-09-25T04:14:48Z","camera":"desk","zone":"frame","label":"laptop",
 "obs_id":3,"event":"appeared","hits":1}
{"ts":"2026-09-25T04:15:19Z","camera":"desk","zone":"frame","label":"laptop",
 "obs_id":3,"event":"disappeared","hits":1,"seen_for_s":30.5}
```

`hits` and `seen_for_s` describe the **last sighting** (not the moment the sweep ran),
which is what makes them comparable with the row's own `hit_count` and its
`last_seen - first_seen`. A `daemon_started` marker separates each run, so a
disappearance is never read across a restart boundary. On shutdown every still-open key
is closed first — otherwise the last real disappearance of a run would be lost.

**This file is an observation, not the truth.** It exists to gather real evidence for
the data-model v2 events table (see the roadmap); nothing reads it back as state.
`webhook`-fed cameras produce no events: Frigate sends discrete events with no
continuous frames, so a disappearance can never be observed there.

### Keeping it bounded (`--maintenance`)

Observations older than `retention_days` (by `last_seen`) are pruned, each taking its
snapshot JPEG with it; every `reconcile_secs` a pass deletes snapshot files whose row is
gone. VACUUM is never automatic — run it on demand, with the daemon stopped:

```sh
item-ingest --maintenance --config config.toml          # refuses while a daemon holds the lock
item-ingest --maintenance --force --config config.toml  # only for a lock left by a killed daemon
```

The 90-day default is deliberate: the roadmap's containment inference needs exactly the
disappearance history this deletes. Read the comment on
`config::defaults::RETENTION_DAYS` before lowering it.

**One camera, one source**: a camera is either fed by Frigate or driven locally, never
both. Frigate sends only discrete events (no frames, so no disappearance can be
observed), and both paths on one camera produce two observations of the same object
fighting over the same zone.

### Running it as a service (restart, and surviving a kill)

Ready-to-edit templates for the three platforms live in [`deploy/`](deploy/README.md):
a systemd unit, a launchd plist, and a Windows Task Scheduler script. Each one gives
the daemon a 30 s stop budget, which is what its 10 s camera drain plus final health
write and WAL checkpoint need.

They rely on two behaviours worth knowing about:

- **A killed daemon does not wedge the next start.** The single-instance lock file at
  `<db dir>/ingest.lock` is re-stamped every 15 s while the daemon runs. A start that
  finds a lock silent for 45 s **takes it over** — logging
  `previous run died without shutdown` — instead of refusing forever. A `health.json`
  written in the last 90 s vetoes that takeover, so a live-but-wedged daemon is never
  robbed of its lock; the conservative side of that trade is that a restart right after
  a hard kill waits out those windows (~90 s) before it can take over.
- **The stop signals are the ones a service manager sends.** `SIGTERM`/`SIGINT`/`SIGHUP`
  on Unix; on Windows the console events (`Ctrl-C`, `Ctrl-Break`, console close, logoff,
  shutdown). A clean stop drains camera threads, writes a final health snapshot, closes
  every open event key and checkpoints the WAL before exiting 0.

`--maintenance` keeps the strict form: it never takes over a lock, because a human is
running it and the alternative to refusing is deleting data the daemon may be writing.

## RTSP backend (`--features rtsp`)

Pulls IP cameras through `ffmpeg-next` (libavformat + swscale, see
`item_ingest::source::rtsp`). This is the only backend that links native C
libraries, so building it needs FFmpeg 7.1 + libclang on top of the usual
MSVC toolchain. One command:

```sh
cargo xtask setup        # fetches into target/vendor/, verified by sha256
cargo build --features rtsp
```

No manual env vars: `.cargo/config.toml` points `FFMPEG_DIR`,
`LIBCLANG_PATH`, and the runtime `PATH` at `target/vendor/`, and `xtask
setup` fills exactly those paths (pinned URLs + hashes live in
`crates/xtask/src/main.rs`, module `pins`; each cached dir carries a
`manifest.json` recording url/sha/contents). Because the cache lives under
`target/`, `cargo clean` wipes it — re-run setup to restore.
`cargo xtask status` reports cached/missing/stale.

Linux works the same way; macOS has no BtbN build — use `brew install
ffmpeg` + system LLVM (setup prints hints, and externally-set
`FFMPEG_DIR`/`LIBCLANG_PATH` win over config.toml).

### Building FFmpeg from source: `cargo xtask setup --from-source`

Alternative to the prebuilt zip: downloads the pinned upstream tarball
(FFmpeg 7.1.5, sha256-checked), then `./configure` with a minimal whitelist
(`--disable-everything` + only rtsp/tcp/udp, h264/hevc/mjpeg decode, swscale)
and `make -j` into `target/vendor/ffmpeg/` — same include/lib/bin layout and
FFMPEG_DIR contract as the zip, so everything downstream is identical.
Manifest records flavor `source`; `setup` (no flag) restores the zip.
Why bother: the only first-class route for macOS (BtbN doesn't ship it),
byte-reproducible CI artifacts, and trimming features to our use.

The local build needs a POSIX toolchain — `sh` + `perl` + `make` + `nasm`,
and `cl.exe` on PATH on Windows (run from an MSVC Developer prompt, or use
MSYS2 which provides the rest). Preflight reports exactly what's missing;
minutes-scale, not seconds. The zip path remains the default precisely to
avoid this — use `--from-source` in CI or on Linux/macOS.

Verified in CI: the `rtsp-source` job (ubuntu) runs the full loop —
configure, make (~6 min), cargo test against the produced tree. Windows
and macOS source builds are deliberately not in the matrix yet (Windows
needs a proven cl.exe-in-MSYS2-sh handshake; macOS wants brew-llvm
pathing and has no zip fallback anyway). Layout note: a source install
puts `.so` files under `lib/` with no `bin/` (the zip ships `bin/*.dll`
for Windows); on Linux runtime resolution goes through the baked rpath,
so item-ingest's build.rs DLL staging is a silent no-op there.

Gotchas discovered during CI bring-up, kept in `xtask` so you don't
re-discover them: relative `--prefix` makes `make install` exit 0 into a
path inside the (later-deleted) source tree — `build_from_source`
absolutizes it; `configure` must be run as `sh ./configure` because our
tar unpack may not preserve the exec bit; and `--disable-everything`
disables the LIBRARIES too, so all six are re-enabled explicitly or the
bindgen headers go missing.

Version warning: rust-ffmpeg 9.x supports FFmpeg ≤ 7.x. BtbN's rolling
`latest` tag is FFmpeg 8 (`avcodec-63.dll`) and the sys crate's probe
rejects it — stay on the pinned 7.1 asset.

Dev-loop tip: `vendor/mediamtx.exe` + the vendored ffmpeg binary make a
local RTSP test stream —
`ffmpeg -re -stream_loop -1 -i test.mp4 -c copy -f rtsp -rtsp_transport tcp
rtsp://127.0.0.1:8554/cam1`. Verified end to end: decode -> RGB8 -> store.

### Web preview (`--preview <rtsp-url>`)

Browsers can't consume RTSP, so the daemon bridges it: a background thread
owns an `RtspSource` (with reconnect loop), JPEG-encodes at ~8 fps, and
serves the frames to any browser as MJPEG (`multipart/x-mixed-replace`) —
no JS, no external media server. Routes: `/preview` (page), `/preview.mjpg`
(live), `/preview.jpg` (single frame). Same listener as the webhook; url
credentials stay out of logs. LAN-only for now: anyone who can reach the
port can watch (gate with a proxy/Tailscale before exposing beyond the LAN).

## VLM grounding detector (`--features vlm`)

`--detector vlm` swaps the local YOLO for an open-vocabulary grounding pass
over an OpenAI-compatible multimodal sidecar (llama.cpp server, Ollama,
vLLM, cloud): the frame goes out as a base64 JPEG data URL with an
instruction to return a JSON array of `{"label","bbox_2d"}` (0-1000
normalized, Qwen-VL convention; replies with any coordinate >1000 are
auto-read as pixels), and everything downstream — NMS, zone mapping, dedup,
snapshot burn-in with highlight — is unchanged. Labels are whatever you ask
for via `--targets` (household defaults: remote, keys, scissors, charger, …;
empty string = "list everything visible"), reusing
`ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL` from the ask bar. Caveats: VLMs give no
calibrated confidence (a `score` field is used when present, else 1.0), and
one detection = one HTTP round trip against a local LLM — the loop therefore
defaults to `--detect-fps 0.2` and skips frames (never dies) while the
sidecar is down.

Hardware reality: grounding is an LLM round trip per frame — orders of
magnitude heavier than the YOLO path (~0.3 s/frame on CPU). Measured on a
Core Ultra 9 285H with a 3B model at Q4: 7-9 s/frame on the Arc 140T iGPU
(Vulkan build), 14-26 s/frame on 16 CPU threads. **Run the sidecar on
hardware with real inference headroom (a strong GPU; the Vulkan build also
drives Intel/AMD iGPUs), or point `ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL` at
an external sidecar** — any OpenAI-compatible endpoint on the LAN (vLLM,
llama.cpp server on another box) or a cloud API works with zero local setup,
and the ingest machine itself stays model-free. Runbook with download/start
commands: [docs/vlm-sidecar.md](docs/vlm-sidecar.md).

## Roadmap: location inference from occlusion & containment

The flagship target: answer "where are my keys?" even when they are no longer
visible. Evidence pattern on a single fixed camera: an object disappears from
detections while a covering/container object appears at its last-seen spot.
"Occluded by the box" and "inside the box" are indistinguishable to one
viewpoint and produce the same actionable answer, so one
disappearance/coverage rule covers both — stated probabilistically ("likely
in/under the box, now on the shelf"), never as fact. Building blocks, in order:

1. **Resident ingest** (prerequisite, not optional): the disappearance moment
   and the covering event are temporal facts only continuous observation
   produces — sporadic manual runs never see them. Design draft (process model,
   config, health file, retention, event observation):
   [docs/resident-ingest.md](docs/resident-ingest.md). **P0–P3 landed** (process
   split, daemon skeleton, health/heartbeat/backoff, retention + `--maintenance`);
   P4 (event observation) and P5 (service units) are still open.
2. **Data model v2**: persist per-observation bboxes (the DB currently stores
   zone + a burned-in snapshot only, no coordinates) and an events table
   (appeared / disappeared / covered / moved).
3. **Query-side rule engine** (item-query): match disappearances to coverage
   events; transitive tracking through containers (once contained, the
   container's position IS the item's position — box moves, the answer
   follows); optional open-lid verification by asking the VLM "is X in the
   box?" when it is visible.
4. **WebUI evidence cards**: last-seen snapshot -> covering-event snapshot ->
   inference with explicit confidence.

Identity stays cheap: position-continuity + label association for large
static containers, manual naming in the WebUI over automatic re-ID (no
appearance-embedding trackers). Known hard edges: keys are not in COCO-80
(use `--detector vlm --targets`), small-object VLM grounding is the weak
spot, and transient occluders (people) vs persistent ones must be told apart
— validate the chain with detectable stand-in objects first.

## Deliberate choices / non-goals (for now)

- No tracker crate: `norfair` has no maintained Rust port, and fixed-camera
  zone aggregation needs only IoU association; see `core::geo`.
- Regions are manual pixel rects per camera; no auto spatial calibration.
- sqlite-vec embeddings not wired yet: keyword lookup carries v1, and vectors
  should be a rebuildable cache.
- ort pinned to 2.0-rc (1.16.x all yanked); the `yolo` module is verified
  against that API: detection on bus.jpg matches Ultralytics reference
  output, and `--detect` runs on live camera frames. Models are not vendored
  (`/models` is git-ignored); drop any stock Ultralytics YOLOv8/11 ONNX
  export (opset ≤ 17) at `models/yolov8n.onnx`. Beware hobby exports with
  custom preprocessing nodes (e.g. HzPreprocess) — ort can't run them.
- Snapshots: the closed loop writes one representative JPEG per NEW
  observation (`{snapshot-dir}/{id}.jpg`, attached via `set_sample_snapshot`),
  burned in with that frame's surviving detection boxes (label-colored,
  "label conf" chip via the embedded 8x8 font) and gray dashed zone rects;
  the new row's OWN box is highlighted (thicker stroke + white ring) --
  per-row rendering, so same-frame births get distinct files. Pixels only,
  the DB never stores boxes; the Frigate webhook path keeps storing
  `frigate://` refs instead.
- Detection runs at `--detect-fps` (default 1; 720p CPU inference measured
  ~280ms/frame at quality 60) while decode runs at stream rate; a person
  lingering in one zone yields ONE merged observation with a hit count, not
  per-frame rows. COCO-only is a YOLO limitation: everyday objects like keys
  are exactly what the `--detector vlm` grounding backend (above) is for.
