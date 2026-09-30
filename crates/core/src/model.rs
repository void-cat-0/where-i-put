use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single detector hit on one frame, in pixel coordinates of that frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    /// COCO-style class name or model-specific label, e.g. "keys" / "backpack".
    pub label: String,
    pub confidence: f32,
    /// Bounding box [x_min, y_min, x_max, y_max], pixels.
    pub bbox: [f32; 4],
}

impl Detection {
    pub fn center(&self) -> (f32, f32) {
        let [x0, y0, x1, y1] = self.bbox;
        ((x0 + x1) / 2.0, (y0 + y1) / 2.0)
    }
}

/// Context for one decoded frame, so detections can be mapped back to a
/// camera and moment (and snapshot file, if kept).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameMeta {
    pub camera_id: String,
    pub captured_at: DateTime<Utc>,
    pub width: u32,
    pub height: u32,
    /// Optional path to a saved JPEG/PNG snapshot for this frame.
    pub snapshot_path: Option<String>,
}

/// A named physical area in front of a camera, defined by a polygon or rect in
/// that camera's pixel space. v1 keeps this a manual config: automatic spatial
/// calibration is deliberately out of scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    pub id: i64,
    pub camera_id: String,
    pub name: String,
    /// Rectangle in camera pixels: [x_min, y_min, x_max, y_max].
    pub rect: [f32; 4],
}

impl Region {
    pub fn contains(&self, point: (f32, f32)) -> bool {
        let (x, y) = point;
        let [x0, y0, x1, y1] = self.rect;
        x >= x0 && x <= x1 && y >= y0 && y <= y1
    }
}

/// Deduplicated presence record: "label was seen at camera/region during
/// [first_seen, last_seen]". This is the unit the query layer answers from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: i64,
    pub camera_id: String,
    /// Region name if the center fell inside a configured region, else "frame".
    pub zone: String,
    pub label: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub hit_count: i64,
    pub sample_snapshot: Option<String>,
}

/// A short-lived track: detections linked across consecutive frames by IoU.
/// Intentionally minimal; persistence stores Observations, not Trajectories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trajectory {
    pub track_id: i64,
    pub label: String,
    pub detections: Vec<(FrameMeta, Detection)>,
}

/// Values supplied when persisting one bounded geometry sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeometryInput {
    pub sample_kind: String,
    pub captured_at: DateTime<Utc>,
    pub bbox: [f32; 4],
    pub frame_width: Option<u32>,
    pub frame_height: Option<u32>,
    pub zone: String,
    pub source: String,
    pub confidence: Option<f32>,
    pub snapshot_ref: Option<String>,
}

/// Values supplied by a relative-depth response. `response_id` ties all
/// fields to the provider response that produced them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DepthInput {
    pub provider: String,
    pub frame_id: String,
    pub response_id: String,
    pub prompt_version: String,
    pub backend_version: String,
    pub mode: String,
    pub relation_to_camera: String,
    pub relative_score: Option<f32>,
    pub quality: String,
    pub model_ref: Option<String>,
    pub value_m: Option<f32>,
    pub uncertainty_m: Option<f32>,
    pub valid_fraction: Option<f32>,
    pub coordinate_frame: Option<String>,
    pub calibration_ref: Option<String>,
}

/// One sighting and its optional geometry/depth response, persisted atomically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SightingInput {
    pub camera_id: String,
    pub zone: String,
    pub label: String,
    pub seen_at: DateTime<Utc>,
    pub snapshot: Option<String>,
    pub window_seconds: u64,
    pub geometry: Option<GeometryInput>,
    pub depth: Option<DepthInput>,
}

/// A bounded geometry sample for an observation. The ingest path stores only
/// lifecycle samples (`first`, `last`, and event-boundary copies), never every
/// decoded frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationGeometry {
    pub id: i64,
    pub observation_id: i64,
    pub sample_kind: String,
    pub captured_at: DateTime<Utc>,
    pub bbox: [f32; 4],
    pub frame_width: Option<u32>,
    pub frame_height: Option<u32>,
    pub zone: String,
    pub source: String,
    pub confidence: Option<f32>,
    pub snapshot_ref: Option<String>,
}

/// An authoritative observation lifecycle fact. JSONL remains an operational
/// mirror; query-side rules consume these rows instead of reading that file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationEvent {
    pub id: i64,
    pub observation_id: i64,
    pub camera_id: String,
    pub zone: String,
    pub label: String,
    pub event_type: String,
    pub occurred_at: DateTime<Utc>,
    pub noticed_at: DateTime<Utc>,
    pub source: String,
    pub session_id: Option<String>,
    pub reason: String,
    pub hits: i64,
    pub seen_for_s: Option<f64>,
    pub geometry_id: Option<i64>,
}

/// Values supplied when an ingest path appends an immutable lifecycle fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationEventInput {
    pub observation_id: i64,
    pub camera_id: String,
    pub zone: String,
    pub label: String,
    pub event_type: String,
    pub occurred_at: DateTime<Utc>,
    pub noticed_at: DateTime<Utc>,
    pub source: String,
    pub session_id: Option<String>,
    pub reason: String,
    pub hits: i64,
    pub seen_for_s: Option<f64>,
    pub geometry_id: Option<i64>,
}

/// A lifecycle event together with its immutable geometry evidence, suitable
/// for a read-side candidate rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneEvent {
    pub event: ObservationEvent,
    pub geometry: Option<ObservationGeometry>,
    pub depth: Vec<DepthEvidence>,
}

/// Relative depth evidence attached to one geometry sample. Metric fields are
/// intentionally optional: the current sidecar provider only supplies ordinal
/// ordering and must not fabricate metres or calibration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DepthEvidence {
    pub id: i64,
    pub geometry_id: i64,
    pub provider: String,
    pub frame_id: String,
    pub response_id: String,
    pub prompt_version: String,
    pub backend_version: String,
    pub mode: String,
    pub relation_to_camera: String,
    pub relative_score: Option<f32>,
    pub quality: String,
    pub model_ref: Option<String>,
    pub value_m: Option<f32>,
    pub uncertainty_m: Option<f32>,
    pub valid_fraction: Option<f32>,
    pub coordinate_frame: Option<String>,
    pub calibration_ref: Option<String>,
}
