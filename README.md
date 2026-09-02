# item-finders

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
  detection entirely.
- **crates/item-query** — the read side. CLI (`log`, `ask`) over observations,
  with an OpenAI-compatible VLM client (llama.cpp/Ollama/cloud sidecar) used
  only when `ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL` are set. The Rust core never
  embeds a VLM.

## Quick start

```sh
# pipeline smoke test, no hardware needed
cargo run -p item-ingest -- --demo

# RTSP camera (see "RTSP backend" for env setup)
cargo run --features rtsp -p item-ingest -- --rtsp "rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101" --camera-id living

# webhook receiver (point Frigate event forwarding at POST /frigate/webhook)
cargo run -p item-ingest -- --listen 127.0.0.1:8477

# ask
cargo run -p item-query -- log keys
ITEM_VLM_BASE_URL=http://127.0.0.1:8080/v1 ITEM_VLM_MODEL=qwen2.5vl cargo run -p item-query -- ask "where are my keys"
```

## RTSP backend (`--features rtsp`)

Pulls IP cameras through `ffmpeg-next` (libavformat + swscale, see
`item_ingest::source::rtsp`). This is the only backend that links native C
libraries, so building it needs a dev toolchain: MSVC (cc-rs probe) and
libclang (bindgen) on top of the usual.

The Windows setup used for development vendors everything under `vendor/`
(git-ignored, ~700 MB with build artifacts):

- `vendor/ffmpeg/` — **FFmpeg 7.1** from BtbN's `*-win64-gpl-shared-7.1.zip`
  (include/ + lib/ + bin/ DLLs). Version matters: rust-ffmpeg 9.x supports
  FFmpeg ≤ 7.x; the master-branch build (FFmpeg 8, `avcodec-63.dll`) breaks
  the sys crate's version probe and is rejected by bindgen.
- `vendor/libclang/native/` — `libclang.dll` extracted from the `libclang`
  PyPI wheel (18.1.1). Any LLVM ≥ 9 works; a full LLVM install is overkill.
- `vendor/ffmpeg/bin/*.dll` must sit next to the built exe (copied by hand)
  or be on PATH at *run* time — Windows loader finds them there, not via
  build-time env.

Build + run:

```sh
export FFMPEG_DIR="$PWD/vendor/ffmpeg"
export LIBCLANG_PATH="$PWD/vendor/libclang/native"
cargo run --features rtsp -p item-ingest -- --rtsp rtsp://... --camera-id living
```

Dev-loop tip: a local RTSP test stream is two commands with the vendored
ffmpeg binary — `mediamtx.exe` (also vendored for testing) plus
`ffmpeg -re -stream_loop -1 -i test.mp4 -c copy -f rtsp -rtsp_transport tcp
rtsp://127.0.0.1:8554/cam1`. Verified end to end: decode -> RGB8 -> store.

## Deliberate choices / non-goals (for now)

- No tracker crate: `norfair` has no maintained Rust port, and fixed-camera
  zone aggregation needs only IoU association; see `core::geo`.
- Regions are manual pixel rects per camera; no auto spatial calibration.
- sqlite-vec embeddings not wired yet: keyword lookup carries v1, and vectors
  should be a rebuildable cache.
- ort pinned to 2.0-rc (1.16.x all yanked); the `yolo` module is compile-marked
  as not yet verified against the 2.0 API and is off by default.
- Snapshot storage keeps references (frigate://...), never copies pixels yet.
