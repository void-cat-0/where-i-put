//! Frigate webhook consumer: the "borrow the NVR, don't rebuild it" path.
//!
//! Frigate posts `application/json` events for every detected object. We only
//! need enough to deduplicate into observations — no pixels involved, so the
//! snapshot URL is stored as a reference.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use item_core::store::{DEFAULT_DEDUP_WINDOW, Store};
use item_core::{GeometryInput, ObservationEventInput, SightingInput};

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
    let input = SightingInput {
        camera_id: ev.camera.clone(),
        zone: zone.clone(),
        label: ev.label.clone(),
        seen_at,
        snapshot: snapshot.clone(),
        window_seconds: DEFAULT_DEDUP_WINDOW.as_secs(),
        geometry: ev.bbox.map(|bbox| GeometryInput {
            sample_kind: "last".into(),
            captured_at: seen_at,
            bbox,
            frame_width: None,
            frame_height: None,
            zone: zone.clone(),
            source: "frigate".into(),
            confidence: ev.top_score,
            snapshot_ref: snapshot.clone(),
        }),
        depth: None,
    };
    match store.record_sighting_with_geometry(&input) {
        Ok((obs_id, _is_new, hits, geometry_id)) => {
            // A discrete Frigate webhook is an observed fact, not a lifecycle
            // transition: without an end signal it cannot prove disappearance.
            // Freeze the input geometry inside the event transaction so a later
            // webhook can move `last` without changing historical evidence.
            let event = ObservationEventInput {
                observation_id: obs_id,
                camera_id: ev.camera.clone(),
                zone: zone.clone(),
                label: ev.label.clone(),
                event_type: "observed".into(),
                occurred_at: seen_at,
                noticed_at: Utc::now(),
                source: "frigate".into(),
                session_id: None,
                reason: "webhook_observed".into(),
                hits,
                seen_for_s: None,
                geometry_id,
            };
            match store.freeze_and_append_observation_event(&event) {
                Ok(_) => axum::http::StatusCode::OK,
                Err(e) => {
                    tracing::error!(error = %e, obs = obs_id, "frigate event evidence persistence failed");
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                }
            }
        }
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
    use axum::body::Bytes;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn test_state() -> State {
        Arc::new(Mutex::new(Store::in_memory().unwrap()))
    }

    fn payload(frame_time: &str, bbox: [f32; 4]) -> Bytes {
        Bytes::from(
            serde_json::json!({
                "camera": "porch",
                "label": "person",
                "bbox": bbox,
                "top_score": 0.93,
                "frame_time": frame_time,
                "snapshot": {"id": "abc123", "retainer": false}
            })
            .to_string(),
        )
    }

    #[tokio::test]
    async fn webhook_persists_observed_event_and_geometry() {
        let state = test_state();
        let occurred = "2026-09-02T10:15:30+00:00";
        let status = webhook(
            axum::extract::State(Arc::clone(&state)),
            payload(occurred, [10.0, 20.0, 110.0, 220.0]),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);

        let store = state.lock().unwrap();
        let observations = store.recent(None, 10).unwrap();
        assert_eq!(observations.len(), 1);
        let geometries = store.geometry_for_observation(observations[0].id).unwrap();
        assert_eq!(
            geometries.len(),
            3,
            "first, last, and frozen event geometry"
        );
        assert_eq!(geometries[0].sample_kind, "first");
        assert_eq!(geometries[1].sample_kind, "last");
        assert_eq!(geometries[2].bbox, [10.0, 20.0, 110.0, 220.0]);
        let since = DateTime::parse_from_rfc3339("2026-09-02T10:15:29+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let until = DateTime::parse_from_rfc3339("2026-09-02T10:15:31+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let events = store.events_for_camera("porch", since, until, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "observed");
        assert_eq!(events[0].geometry_id, Some(geometries[2].id));
        assert!(events[0].occurred_at < events[0].noticed_at);
    }

    #[tokio::test]
    async fn late_webhook_does_not_roll_back_last_geometry() {
        let state = test_state();
        for (timestamp, bbox) in [
            ("2026-09-02T10:15:30+00:00", [10.0, 20.0, 110.0, 220.0]),
            ("2026-09-02T10:15:40+00:00", [30.0, 40.0, 130.0, 240.0]),
            ("2026-09-02T10:15:35+00:00", [20.0, 30.0, 120.0, 230.0]),
        ] {
            assert_eq!(
                webhook(
                    axum::extract::State(Arc::clone(&state)),
                    payload(timestamp, bbox)
                )
                .await,
                axum::http::StatusCode::OK
            );
        }

        let store = state.lock().unwrap();
        let observation = &store.recent(None, 10).unwrap()[0];
        let geometries = store.geometry_for_observation(observation.id).unwrap();
        let last = geometries.iter().find(|g| g.sample_kind == "last").unwrap();
        assert_eq!(last.bbox, [30.0, 40.0, 130.0, 240.0]);
        let since = DateTime::parse_from_rfc3339("2026-09-02T10:15:29+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let until = DateTime::parse_from_rfc3339("2026-09-02T10:15:41+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let events = store.events_for_camera("porch", since, until, 10).unwrap();
        assert_eq!(events.len(), 3);
        let late_event = events
            .iter()
            .find(|e| {
                e.occurred_at
                    .to_rfc3339()
                    .starts_with("2026-09-02T10:15:35")
            })
            .unwrap();
        let frozen = store
            .geometry_by_id(late_event.geometry_id.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(frozen.bbox, [20.0, 30.0, 120.0, 230.0]);
    }

    #[tokio::test]
    async fn webhook_event_failure_returns_500_and_rolls_back_event_geometry() {
        let path: PathBuf = std::env::temp_dir().join(format!(
            "item-ingest-frigate-{}-{}.sqlite",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Store::open(&path).unwrap();
        let trigger_conn = rusqlite::Connection::open(&path).unwrap();
        trigger_conn
            .execute_batch(
                "CREATE TRIGGER reject_frigate_events BEFORE INSERT ON observation_events
                 BEGIN SELECT RAISE(FAIL, 'test event failure'); END;",
            )
            .unwrap();
        let state = Arc::new(Mutex::new(store));
        let status = webhook(
            axum::extract::State(Arc::clone(&state)),
            payload("2026-09-02T10:15:30+00:00", [10.0, 20.0, 110.0, 220.0]),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let store = state.lock().unwrap();
        let observation = &store.recent(None, 10).unwrap()[0];
        let geometries = store.geometry_for_observation(observation.id).unwrap();
        assert_eq!(geometries.len(), 2, "event geometry copy must roll back");
        assert!(
            store
                .events_for_camera(
                    "porch",
                    DateTime::parse_from_rfc3339("2026-09-02T10:15:29+00:00")
                        .unwrap()
                        .with_timezone(&Utc),
                    DateTime::parse_from_rfc3339("2026-09-02T10:15:31+00:00")
                        .unwrap()
                        .with_timezone(&Utc),
                    10,
                )
                .unwrap()
                .is_empty()
        );
        drop(store);
        drop(trigger_conn);
        std::fs::remove_file(path).ok();
    }

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
