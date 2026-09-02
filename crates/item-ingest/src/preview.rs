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

use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use futures_util::stream::{StreamExt as _, TryStreamExt as _};

use crate::source::{FrameSource, rtsp::RtspSource};

/// Encoder settings for the preview feed. 8 fps is enough for a glance at a
/// room and keeps CPU/bandwidth modest over home Wi-Fi.
const TARGET_FPS: u64 = 8;
const JPEG_QUALITY: u8 = 70;

pub type FrameTx = broadcast::Sender<Arc<[u8]>>;

/// Spawn the pull->decode->JPEG thread; returns the broadcast sender the
/// HTTP routes subscribe from. `url` may embed credentials
/// (rtsp://user:pass@host/...), so keep it out of logs.
pub fn spawn_streamer(url: String) -> FrameTx {
    let (tx, _rx) = broadcast::channel::<Arc<[u8]>>(2);
    let feed_tx = tx.clone();
    std::thread::spawn(move || {
        let min_interval = Duration::from_millis(1000 / TARGET_FPS);
        loop {
            tracing::info!("preview: opening rtsp stream");
            match RtspSource::new("preview", &url) {
                Ok(mut source) => {
                    let mut last_emit = Instant::now() - min_interval;
                    loop {
                        match source.next_frame() {
                            Ok(frame) => {
                                if last_emit.elapsed() < min_interval {
                                    continue; // drop extra frames, decode only
                                }
                                match encode_jpeg(&frame.rgb, frame.meta.width, frame.meta.height) {
                                    Ok(jpeg) => {
                                        // Err only when nobody is listening.
                                        let _ = feed_tx.send(Arc::from(jpeg));
                                        last_emit = Instant::now();
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
    let mut enc =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
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
            [(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-store")],
            jpeg.to_vec(),
        )
            .into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "no frame yet").into_response(),
    }
}

async fn mjpeg(State(tx): State<FrameTx>) -> Response {
    let stream = BroadcastStream::new(tx.subscribe())
        .filter_map(|item| async move { item.ok().map(|jpeg| Ok::<_, std::io::Error>(part(&jpeg))) });
    let body = Body::from_stream(stream.map_err(std::io::Error::other));
    (
        [(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=frame",
        ), (header::CACHE_CONTROL, "no-store")],
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
