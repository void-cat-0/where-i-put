//! item-ingest: turn camera frames / webhook events into observations.
//!
//! Pipeline stages (each behind a trait, so the optional native backends can
//! be swapped independently):
//!   FrameSource  -> Detector -> (NMS) -> Store::record_sighting
//!
//! The Frigate webhook path skips FrameSource+Detector entirely: Frigate has
//! already done decode+detect, we only deduplicate and persist its events.

pub mod detector;
pub mod frigate;
#[cfg(feature = "rtsp")]
pub mod preview;
pub mod source;

use chrono::Utc;
use thiserror::Error;

use item_core::geo::nms;
use item_core::store::Store;
use item_core::{Detection, FrameMeta};

#[derive(Debug, Error)]
pub enum IngestError {
    #[error(transparent)]
    Store(#[from] item_core::store::StoreError),
}

pub type Result<T> = std::result::Result<T, IngestError>;

/// Persist one frame's detections: NMS them, map centers to zones, record.
pub fn ingest_detections(
    store: &Store,
    meta: &FrameMeta,
    dets: &[Detection],
    nms_iou_threshold: f32,
    min_confidence: f32,
) -> Result<usize> {
    let owned: Vec<Detection> =
        dets.iter().filter(|d| d.confidence >= min_confidence).cloned().collect();
    let keep = nms(&owned, nms_iou_threshold);

    let mut recorded = 0;
    for idx in keep {
        let det = &owned[idx];
        let zone = store.zone_for_point(&meta.camera_id, det.center())?;
        store.record_sighting(
            &meta.camera_id,
            &zone,
            &det.label,
            meta.captured_at,
            meta.snapshot_path.as_deref(),
            item_core::store::DEFAULT_DEDUP_WINDOW,
        )?;
        recorded += 1;
    }
    tracing::debug!(camera = %meta.camera_id, recorded, "frame ingested");
    Ok(recorded)
}

/// Convenience: shared handle so the webhook server and a camera loop can both
/// write. See `frigate::State` (rusqlite::Connection is not Sync).
pub type SharedStore = std::sync::Arc<std::sync::Mutex<Store>>;

pub fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}
