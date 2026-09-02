//! Frigate webhook consumer: the "borrow the NVR, don't rebuild it" path.
//!
//! Frigate posts `application/json` events for every detected object. We only
//! need enough to deduplicate into observations — no pixels involved, so the
//! snapshot URL is stored as a reference.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use item_core::store::Store;

#[derive(Debug, Deserialize)]
pub struct FrigateEvent {
    pub camera: String,
    #[serde(default)]
    pub label: String,
    /// Bounding box in frame pixels: [x0, y0, x1, y1].
    #[serde(default)]
    pub bbox: Option<[f32; 4]>,
    #[serde(default)]
    pub top_score: Option<f32>,
    /// "YYYY-MM-DDTHH:MM:SS+00:00" — parse defensively, fall back to now.
    #[serde(default)]
    pub frame_time: Option<String>,
    #[serde(default)]
    pub snapshot: Option<FrigateSnapshot>,
}

#[derive(Debug, Deserialize)]
pub struct FrigateSnapshot {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub retainer: bool,
}

/// rusqlite::Connection is Send but not Sync, so the shared handle is
/// Arc<Mutex<Store>>. Neither guard is ever held across an .await.
pub type State = Arc<Mutex<Store>>;

/// POST /frigate/webhook  (point Frigate's `mqtt`->http or use frigate's
/// "rest notice" / a small mosquitto-sub shim to forward events here).
pub async fn webhook(
    axum::extract::State(state): axum::extract::State<State>,
    body: axum::body::Bytes,
) -> axum::http::StatusCode {
    let ev: FrigateEvent = match serde_json::from_slice(&body) {
        Ok(ev) => ev,
        Err(e) => {
            tracing::warn!(error = %e, "bad webhook payload");
            return axum::http::StatusCode::BAD_REQUEST;
        }
    };
    if ev.label.is_empty() {
        return axum::http::StatusCode::OK;
    }
    let seen_at = ev
        .frame_time
        .as_deref()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    let zone = match ev.bbox {
        Some([x0, y0, x1, y1]) => {
            let store = state.lock().expect("store mutex");
            store
                .zone_for_point(&ev.camera, ((x0 + x1) / 2.0, (y0 + y1) / 2.0))
                .unwrap_or_else(|_| "frame".into())
        }
        None => "frame".into(),
    };
    let snapshot = ev
        .snapshot
        .as_ref()
        .and_then(|s| s.id.clone())
        .map(|id| format!("frigate://snapshot/{id}"));

    let store = state.lock().expect("store mutex");
    match store.record_sighting(
        &ev.camera,
        &zone,
        &ev.label,
        seen_at,
        snapshot.as_deref(),
        item_core::store::DEFAULT_DEDUP_WINDOW,
    ) {
        Ok(_) => axum::http::StatusCode::OK,
        Err(e) => {
            tracing::error!(error = %e, "failed to persist frigate event");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

pub fn router(state: State) -> axum::Router {
    axum::Router::new()
        .route("/frigate/webhook", axum::routing::post(webhook))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frigate_payload_decodes() {
        let raw = r#"{
            "camera": "porch",
            "label": "person",
            "bbox": [10.0, 20.0, 110.0, 220.0],
            "top_score": 0.93,
            "frame_time": "2026-09-02T10:15:30+00:00",
            "snapshot": {"id": "abc123", "retainer": false}
        }"#;
        let ev: FrigateEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(ev.camera, "porch");
        assert_eq!(ev.label, "person");
        assert_eq!(ev.top_score, Some(0.93));
        assert_eq!(ev.snapshot.as_ref().unwrap().id.as_deref(), Some("abc123"));
        assert!(
            DateTime::parse_from_rfc3339(ev.frame_time.as_deref().unwrap()).is_ok(),
            "frame_time must be RFC3339-parsable"
        );
    }
}
