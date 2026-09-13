//! The liveness file (docs/resident-ingest.md §5).
//!
//! A file rather than an HTTP endpoint: `item-web` has no HTTP client, and 15s
//! granularity is plenty for "is the ingest daemon alive". The only field that
//! decides liveness is `updated_at` -- `pid` is advisory, because Windows would
//! need `OpenProcess`, Unix pids get reused, and neither is a portable std API.
//!
//! P1 writes the file plainly. The atomic `.tmp` + rename dance with a Windows
//! retry (a reader holding the file open makes `rename` fail there) is P2, per
//! §10.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;

use crate::runner::CameraHealth;

/// Bumped whenever the shape below changes; readers must check it.
pub const SCHEMA: u32 = 1;

/// One camera, as the outside world sees it.
#[derive(Debug, Clone, Serialize)]
pub struct CameraEntry {
    pub id: String,
    pub state: String,
    /// Already redacted: credentials never reach this file.
    pub source: String,
    pub reconnects: u64,
    pub frames: u64,
    pub detections: u64,
    pub recorded: u64,
    pub last_frame_at: Option<String>,
    pub last_frame_age_s: Option<u64>,
    pub last_error: Option<String>,
    pub inference_ms_ewma: f64,
}

/// The whole file, as serialized.
#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub schema: u32,
    pub pid: u32,
    pub started_at: String,
    pub updated_at: String,
    pub seq: u64,
    pub detector_default: String,
    pub cameras: Vec<CameraEntry>,
}

/// Camera threads publish here; the writer task reads.
#[derive(Clone, Default)]
pub struct HealthRegistry {
    inner: Arc<Mutex<BTreeMap<String, CameraHealth>>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&self, camera_id: &str, health: CameraHealth) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(camera_id.to_string(), health);
        }
    }

    pub fn get(&self, camera_id: &str) -> Option<CameraHealth> {
        self.inner.lock().ok()?.get(camera_id).cloned()
    }
}

/// The camera identity the writer needs, kept after the tasks themselves have
/// moved into the supervisor.
#[derive(Debug, Clone)]
pub struct CameraMeta {
    pub id: String,
    pub source: String,
}

/// Writes `health.json` on a cadence and once more on shutdown.
pub struct HealthWriter {
    path: PathBuf,
    started_at: DateTime<Utc>,
    seq: u64,
    detector_default: String,
    cameras: Vec<CameraMeta>,
}

impl HealthWriter {
    pub fn new(
        path: impl Into<PathBuf>,
        detector_default: impl Into<String>,
        cameras: Vec<CameraMeta>,
    ) -> Self {
        Self {
            path: path.into(),
            started_at: Utc::now(),
            seq: 0,
            detector_default: detector_default.into(),
            cameras,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Build the current snapshot without touching the disk.
    pub fn snapshot(&self, registry: &HealthRegistry) -> HealthSnapshot {
        let updated_at = Utc::now();
        HealthSnapshot {
            schema: SCHEMA,
            pid: std::process::id(),
            started_at: self.started_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
            seq: self.seq,
            detector_default: self.detector_default.clone(),
            cameras: self
                .cameras
                .iter()
                .map(|meta| camera_entry(meta, registry.get(&meta.id), updated_at))
                .collect(),
        }
    }

    pub fn write(&mut self, registry: &HealthRegistry) -> std::io::Result<()> {
        self.seq += 1;
        let snapshot = self.snapshot(registry);
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(&snapshot).map_err(std::io::Error::other)?;
        std::fs::write(&self.path, json)
    }
}

fn camera_entry(
    meta: &CameraMeta,
    health: Option<CameraHealth>,
    now: DateTime<Utc>,
) -> CameraEntry {
    let Some(h) = health else {
        // The camera thread has not reported yet: it exists, but has no facts.
        return CameraEntry {
            id: meta.id.clone(),
            state: "starting".to_string(),
            source: meta.source.clone(),
            reconnects: 0,
            frames: 0,
            detections: 0,
            recorded: 0,
            last_frame_at: None,
            last_frame_age_s: None,
            last_error: None,
            inference_ms_ewma: 0.0,
        };
    };

    CameraEntry {
        id: meta.id.clone(),
        state: h.state.as_str().to_string(),
        source: meta.source.clone(),
        reconnects: h.reconnects,
        frames: h.frames,
        detections: h.detections,
        recorded: h.recorded,
        last_frame_at: h
            .last_frame_age
            .map(|age| (now - ChronoDuration::from_std(age).unwrap_or_default()).to_rfc3339()),
        last_frame_age_s: h.last_frame_age.map(|age| age.as_secs()),
        last_error: h.last_error.clone(),
        inference_ms_ewma: (h.inference_ms_ewma * 10.0).round() / 10.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CameraState;
    use std::time::Duration;

    fn health(frames: u64) -> CameraHealth {
        CameraHealth {
            state: CameraState::Running,
            frames,
            detections: 2,
            recorded: 1,
            reconnects: 3,
            last_frame_age: Some(Duration::from_secs(2)),
            last_error: None,
            inference_ms_ewma: 284.14,
        }
    }

    #[test]
    fn a_camera_that_has_not_reported_yet_still_appears() {
        let writer = HealthWriter::new(
            "unused.json",
            "yolo",
            vec![CameraMeta {
                id: "living".into(),
                source: "rtsp://cam.local/stream".into(),
            }],
        );
        let snap = writer.snapshot(&HealthRegistry::new());
        assert_eq!(snap.schema, SCHEMA);
        assert_eq!(snap.cameras.len(), 1);
        assert_eq!(snap.cameras[0].state, "starting");
        assert_eq!(snap.cameras[0].frames, 0);
    }

    #[test]
    fn published_health_reaches_the_snapshot_in_the_documented_shape() {
        let registry = HealthRegistry::new();
        registry.publish("living", health(210_344));
        let writer = HealthWriter::new(
            "unused.json",
            "yolo",
            vec![CameraMeta {
                id: "living".into(),
                source: "rtsp://cam.local/stream".into(),
            }],
        );

        let snap = writer.snapshot(&registry);
        let cam = &snap.cameras[0];
        assert_eq!(cam.state, "running");
        assert_eq!(cam.frames, 210_344);
        assert_eq!(cam.reconnects, 3);
        assert_eq!(cam.last_frame_age_s, Some(2));
        assert!(
            cam.last_frame_at.is_some(),
            "age must also be rendered as a timestamp"
        );
        assert_eq!(cam.inference_ms_ewma, 284.1);
    }

    #[test]
    fn writing_produces_parseable_json_and_bumps_seq() {
        let dir = std::env::temp_dir().join(format!("item-ingest-health-{}", std::process::id()));
        let path = dir.join("health.json");
        let registry = HealthRegistry::new();
        registry.publish("cam", health(7));

        let mut writer = HealthWriter::new(
            &path,
            "yolo",
            vec![CameraMeta {
                id: "cam".into(),
                source: "mock".into(),
            }],
        );
        writer.write(&registry).unwrap();
        writer.write(&registry).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["seq"], 2);
        assert_eq!(value["pid"], std::process::id());
        assert_eq!(value["cameras"][0]["frames"], 7);
        assert_eq!(value["detector_default"], "yolo");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
