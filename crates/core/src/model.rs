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
