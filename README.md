# where-i-put

[![CI](https://github.com/void-cat-0/where-i-put/actions/workflows/ci.yml/badge.svg)](https://github.com/void-cat-0/where-i-put/actions/workflows/ci.yml)

Vision-based item memory: watch cameras (or a Frigate NVR's events), remember
where objects were last seen, answer "where are my keys?".

Three crates, one-way dependencies (`item-query`/`item-ingest` -> `item-core`):

- **crates/core (`item-core`)** — domain model (`Detection`, `Observation`,
  `Region`), box geometry (IoU, greedy NMS), SQLite storage. The unit of truth
  is the *Observation*: "label X was in zone Z of camera C during \[first_seen,
  last_seen]", merged while sightings stay within a 5-min dedup window.
- **crates/item-ingest** — the write side. `FrameSource` (Mock now, nokhwa
  for USB webcams behind `--features camera`, ffmpeg-next for RTSP/IP cameras
  behind `--features rtsp`) -> `Detector` (Null now, YOLO-onnx
  behind `--features yolo` via ort) -> NMS -> zone mapping -> store. Also an
  axum webhook server that ingests Frigate events directly, skipping local
  detection entirely, plus an MJPEG web preview bridge for RTSP cameras
  (`--preview`, feature `rtsp`).
- **crates/item-query** — the read side. CLI (`log`, `ask`) over observations,
  with an OpenAI-compatible VLM client (llama.cpp/Ollama/cloud sidecar) used
  only when `ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL` are set. The Rust core never
  embeds a VLM.

## Quick start

```sh
# pipeline smoke test, no hardware needed
cargo run -p item-ingest -- --demo

# RTSP camera (one-time: `cargo xtask setup` to fetch FFmpeg + libclang)
cargo run --features rtsp -p item-ingest -- --rtsp "rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101" --camera-id living

# live web preview (MJPEG bridge; browsers can't speak RTSP) — open http://<host>:8477/preview
cargo run --features rtsp -p item-ingest -- --preview "rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101"

# object detection on one image (needs a local models/yolov8n.onnx, see below)
cargo run --features yolo -p item-ingest -- --detect path/to/photo.jpg

# webhook receiver (point Frigate event forwarding at POST /frigate/webhook)
cargo run -p item-ingest -- --listen 127.0.0.1:8477

# ask
cargo run -p item-query -- log keys
ITEM_VLM_BASE_URL=http://127.0.0.1:8080/v1 ITEM_VLM_MODEL=qwen2.5vl cargo run -p item-query -- ask "where are my keys"
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
- Snapshot storage keeps references (frigate://...), never copies pixels yet.
