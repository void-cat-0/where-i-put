//! Minimal web preview for an RTSP camera, gated behind feature `rtsp`.
//!
//! Browsers can't speak RTSP, so we do the classic bridge: one background
//! thread owns an `RtspSource` (decode -> RGB8), JPEG-encodes frames
//! (throttled), and broadcasts them; the HTTP side serves
//!   GET /preview      - tiny HTML page with an <img> tag
//!   GET /preview.mjpg - multipart/x-mixed-replace JPEG stream
//! MJPEG needs no JS, works in every browser including phones, and the
//! <img> auto-redisplay is the player. Latency is one frame; quality is a
//! deliberate trade (we re-encode; at 10 fps it's plenty for "what's in the
//! room right now").
//!
//! The streamer thread reconnects forever: dead camera -> log + retry every
//! 2 s, so the page shows the last frame until the stream returns. This is
//! the supervision loop the source module deferred, kept inside the preview
//! path only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::stream::{StreamExt as _, TryStreamExt as _};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::source::{FrameSource, rtsp::RtspSource};

/// Encoder settings for the preview feed. 25 fps matches the camera
/// substream (102) natively — anything lower reads as stutter next to the
/// Hik client. Quality 60 keeps ~25fps at ~720p affordable CPU-wise and
/// ~5-10 Mbps on the wire, which LAN Wi-Fi handles fine.
const TARGET_FPS: u64 = 25;
const JPEG_QUALITY: u8 = 60;

pub type FrameTx = broadcast::Sender<Arc<[u8]>>;

/// Spawn the pull->decode->JPEG thread; returns the broadcast sender the
/// HTTP routes subscribe from. `url` may embed credentials
/// (rtsp://user:pass@host/...), so keep it out of logs.
pub fn spawn_streamer(url: String) -> FrameTx {
    let (tx, _rx) = broadcast::channel::<Arc<[u8]>>(2);
    let feed_tx = tx.clone();
    std::thread::spawn(move || {
        // Throttle by CREDIT, never by wall-clock: time gates quantize badly
        // when source fps ~ target fps (a 93ms gate and a 1.5*src-interval
        // gate both measured exactly half delivery — 25fps in, 12.5fps out —
        // because frame arrivals are quantized to multiples of the source
        // interval and land microseconds either side of a fixed threshold).
        // Instead: estimate source fps via EWMA of arrival gaps, accrue
        // TARGET_FPS/src_fps "credits" per decoded frame, emit when credit >=
        // 1. A source at/below target accrues >= 1 per frame (every frame
        // passes); faster ones average out to the target rate smoothly, with
        // no modulo-phase jumps when the estimate jitters across a boundary.
        let mut prev_arrival = Instant::now();
        let mut src_interval = Duration::from_millis(40); // seed ~25fps
        let mut credit = 1.0f64;
        loop {
            tracing::info!("preview: opening rtsp stream");
            match RtspSource::new("preview", &url) {
                Ok(mut source) => {
                    loop {
                        match source.next_frame() {
                            Ok(frame) => {
                                let now = Instant::now();
                                // EWMA the source interval (1/4 weight).
                                let gap = now
                                    .saturating_duration_since(prev_arrival)
                                    .min(Duration::from_secs(1));
                                src_interval = (src_interval * 3 + gap) / 4;
                                prev_arrival = now;
                                // Accrue target-frames-worth of credit for
                                // this one source frame, emit when >= 1.
                                let src_fps =
                                    1_000_000_000u64 / src_interval.as_nanos().max(1) as u64;
                                credit += TARGET_FPS as f64 / src_fps.max(1) as f64;
                                if credit < 1.0 {
                                    continue; // skipped, decoded only
                                }
                                credit -= 1.0;
                                match encode_jpeg(&frame.rgb, frame.meta.width, frame.meta.height) {
                                    Ok(jpeg) => {
                                        // Err only when nobody is listening.
                                        let _ = feed_tx.send(Arc::from(jpeg));
                                    }
                                    Err(e) => tracing::warn!(error = %e, "preview: jpeg encode"),
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "preview: stream ended, reconnecting");
                                break;
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "preview: connect failed, retrying"),
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    tx
}

fn encode_jpeg(rgb: &[u8], width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let img = image::RgbImage::from_raw(width, height, rgb.to_vec())
        .ok_or_else(|| anyhow::anyhow!("rgb buffer shorter than frame size"))?;
    let mut out = std::io::Cursor::new(Vec::new());
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    enc.encode(&img, width, height, image::ExtendedColorType::Rgb8)?;
    Ok(out.into_inner())
}

/// multipart chunk wrapping one JPEG in the standard MJPEG framing.
fn part(jpeg: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(jpeg.len() + 96);
    buf.extend_from_slice(b"\r\n--frame\r\nContent-Type: image/jpeg\r\nContent-Length: ");
    buf.extend_from_slice(jpeg.len().to_string().as_bytes());
    buf.extend_from_slice(b"\r\n\r\n");
    buf.extend_from_slice(jpeg);
    buf
}

const INDEX_HTML: &str = r#"<!doctype html>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>where-i-put preview</title>
<style>
 body{margin:0;background:#111;color:#ccc;font:14px/1.5 system-ui;display:flex;flex-direction:column;align-items:center;min-height:100vh}
 h1{font-size:16px;font-weight:500;margin:12px}
 img{max-width:100vw;max-height:85vh;border:1px solid #333}
 footer{margin:8px;color:#777}
</style>
<h1>camera preview</h1>
<img src="/preview.mjpg" alt="live preview (stops if the stream drops)">
<footer>MJPEG via where-i-put · <a style="color:#777" href="/preview.jpg">current frame</a></footer>
"#;

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// Single-shot JPEG of the latest frame — handy for curl and for the
/// future snapshot-on-detection feature to reuse.
async fn snapshot(State(tx): State<FrameTx>) -> Response {
    let rx = tx.subscribe();
    match BroadcastStream::new(rx).next().await {
        Some(Ok(jpeg)) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            jpeg.to_vec(),
        )
            .into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "no frame yet").into_response(),
    }
}

async fn mjpeg(State(tx): State<FrameTx>) -> Response {
    let stream = BroadcastStream::new(tx.subscribe()).filter_map(|item| async move {
        item.ok().map(|jpeg| Ok::<_, std::io::Error>(part(&jpeg)))
    });
    let body = Body::from_stream(stream.map_err(std::io::Error::other));
    (
        [
            (
                header::CONTENT_TYPE,
                "multipart/x-mixed-replace; boundary=frame",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

pub fn router(tx: FrameTx) -> Router {
    Router::new()
        .route("/preview", get(index))
        .route("/preview.jpg", get(snapshot))
        .route("/preview.mjpg", get(mjpeg))
        .with_state(tx)
}
