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
  preview bridge for RTSP cameras (`--preview`, feature `rtsp`).
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
   [docs/resident-ingest.md](docs/resident-ingest.md).
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
