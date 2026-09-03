//! item-ingest: turn camera frames / webhook events into observations.
//!
//! Pipeline stages (each behind a trait, so the optional native backends can
//! be swapped independently):
//!   FrameSource  -> Detector -> (NMS) -> Store::record_sighting
//!
//! The Frigate webhook path skips FrameSource+Detector entirely: Frigate has
//! already done decode+detect, we only deduplicate and persist its events.

pub mod config;
pub mod detector;
pub mod frigate;
#[cfg(feature = "rtsp")]
pub mod preview;
pub mod source;

use chrono::Utc;
use std::io::Write;
use thiserror::Error;

use item_core::geo::nms;
use item_core::store::Store;
use item_core::{Detection, FrameMeta};

#[derive(Debug, Error)]
pub enum IngestError {
    #[error(transparent)]
    Store(#[from] item_core::store::StoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, IngestError>;

/// Persist one frame's detections: NMS them, map centers to zones, record.
/// When `frame_rgb` + `snapshots_dir` are provided and a sighting opens a NEW
/// observation, the frame is JPEG-encoded to `{dir}/{obs_id}.jpg` and
/// attached -- one representative image per observation, written once.
pub fn ingest_detections(
    store: &Store,
    meta: &FrameMeta,
    dets: &[Detection],
    nms_iou_threshold: f32,
    min_confidence: f32,
    frame_rgb: Option<(&[u8], &std::path::Path)>,
) -> Result<usize> {
    let owned: Vec<Detection> = dets
        .iter()
        .filter(|d| d.confidence >= min_confidence)
        .cloned()
        .collect();
    let keep = nms(&owned, nms_iou_threshold);

    let mut recorded = 0;
    for idx in keep {
        let det = &owned[idx];
        let zone = store.zone_for_point(&meta.camera_id, det.center())?;
        let (id, is_new) = store.record_sighting(
            &meta.camera_id,
            &zone,
            &det.label,
            meta.captured_at,
            meta.snapshot_path.as_deref(),
            item_core::store::DEFAULT_DEDUP_WINDOW,
        )?;
        // A failed snapshot must not break ingestion; also only the FIRST
        // sighting of an observation gets an image.
        if is_new && let Some((rgb, dir)) = frame_rgb {
            match write_snapshot(dir, id, rgb, meta.width, meta.height) {
                Ok(rel) => store.set_sample_snapshot(id, &rel)?,
                Err(e) => tracing::warn!(error = %e, obs = id, "snapshot write failed"),
            }
        }
        recorded += 1;
    }
    tracing::debug!(camera = %meta.camera_id, recorded, "frame ingested");
    Ok(recorded)
}

/// Encode RGB8 -> JPEG at `{dir}/{id}.jpg`, return the stored path string.
fn write_snapshot(
    dir: &std::path::Path,
    id: i64,
    rgb: &[u8],
    w: u32,
    h: u32,
) -> std::io::Result<String> {
    std::fs::create_dir_all(dir)?;
    let src = image::RgbImage::from_raw(w, h, rgb.to_vec())
        .ok_or_else(|| std::io::Error::other("rgb buffer shorter than frame"))?;
    let path = dir.join(format!("{id}.jpg"));
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 75);
    enc.encode(&src, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    out.flush()?;
    Ok(path.display().to_string())
}

/// Convenience: shared handle so the webhook server and a camera loop can both
/// write. See `frigate::State` (rusqlite::Connection is not Sync).
pub type SharedStore = std::sync::Arc<std::sync::Mutex<Store>>;

pub fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}
