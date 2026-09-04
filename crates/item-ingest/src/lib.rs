//! item-ingest: turn camera frames / webhook events into observations.
//!
//! Pipeline stages (each behind a trait, so the optional native backends can
//! be swapped independently):
//!   FrameSource  -> Detector -> (NMS) -> Store::record_sighting
//!
//! The Frigate webhook path skips FrameSource+Detector entirely: Frigate has
//! already done decode+detect, we only deduplicate and persist its events.

pub mod annotate;
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
use item_core::{Detection, FrameMeta, Region};

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
/// observation, the frame is annotated (all surviving boxes in label colors,
/// zone rects dashed) and JPEG-encoded to `{dir}/{obs_id}.jpg` -- one
/// representative image per observation, written once. Boxes live in the
/// pixels only; the database stays detection-free.
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
    let survivors: Vec<&Detection> = keep.iter().map(|&i| &owned[i]).collect();
    // Zones come from the DB and only matter for annotated snapshots, so the
    // query stays on the snapshot-providing path.
    let regions: Vec<Region> = match frame_rgb {
        Some(_) => store.regions_for(&meta.camera_id)?,
        None => Vec::new(),
    };

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
            match annotate::annotate(rgb, meta.width, meta.height, &survivors, &regions) {
                Some(img) => match write_snapshot(dir, id, &img) {
                    Ok(rel) => store.set_sample_snapshot(id, &rel)?,
                    Err(e) => tracing::warn!(error = %e, obs = id, "snapshot write failed"),
                },
                None => {
                    tracing::warn!(obs = id, "snapshot skipped: rgb buffer shorter than frame")
                }
            }
        }
        recorded += 1;
    }
    tracing::debug!(camera = %meta.camera_id, recorded, "frame ingested");
    Ok(recorded)
}

/// Encode an (already annotated) RGB image -> JPEG at `{dir}/{id}.jpg`,
/// return the stored path string.
fn write_snapshot(
    dir: &std::path::Path,
    id: i64,
    img: &image::RgbImage,
) -> std::io::Result<String> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{id}.jpg"));
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 75);
    enc.encode(
        img,
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )
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

#[cfg(test)]
mod tests {
    use super::annotate::palette_color;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// Closed-loop smoke: a new observation's snapshot must exist and carry
    /// the burned-in box color at the box's top edge (JPEG q75 bleeds a
    /// little, so compare with tolerance instead of exact equality).
    #[test]
    fn snapshot_written_with_burned_in_boxes() {
        let dir = std::env::temp_dir().join(format!(
            "item-ingest-annot-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Store::in_memory().unwrap();
        store
            .upsert_region("cam", "desk", [4.0, 4.0, 44.0, 44.0])
            .unwrap();
        let meta = FrameMeta {
            camera_id: "cam".into(),
            captured_at: now(),
            width: 64,
            height: 48,
            snapshot_path: None,
        };
        let dets = [Detection {
            label: "bottle".into(),
            confidence: 0.9,
            bbox: [8.0, 8.0, 40.0, 32.0],
        }];
        let rgb = vec![128u8; 64 * 48 * 3];

        let recorded =
            ingest_detections(&store, &meta, &dets, 0.45, 0.3, Some((&rgb, dir.as_path())))
                .unwrap();
        assert_eq!(recorded, 1);

        let obs = store.recent(None, 10).unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].zone, "desk");
        let snap = obs[0].sample_snapshot.clone().expect("snapshot attached");
        let img = image::open(&snap).unwrap().to_rgb8();

        let want = palette_color("bottle");
        let got = *img.get_pixel(24, 8); // middle of the box top edge
        let near =
            |a: image::Rgb<u8>, b: image::Rgb<u8>| (0..3).all(|i| a.0[i].abs_diff(b.0[i]) < 60);
        assert!(
            near(got, want),
            "box edge pixel {got:?} should look like {want:?}"
        );
        assert!(
            got.0.iter().any(|d| d.abs_diff(128) > 60),
            "edge pixel must differ from flat background"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
