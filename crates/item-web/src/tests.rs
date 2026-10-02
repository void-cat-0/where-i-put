//! Router-level tests: the JSON the page renders from, over a real on-disk
//! database written through the same store calls ingest uses.

use super::*;
use axum::body::Body;
use axum::http::Request;
use chrono::{DateTime, TimeZone, Utc};
use item_core::{GeometryInput, ObservationEventInput};
use tower::ServiceExt as _;

struct Sandbox(PathBuf);

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "item-web-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).unwrap() + Duration::seconds(seconds)
}

#[allow(clippy::too_many_arguments)]
fn lifecycle(
    store: &Store,
    observation_id: i64,
    label: &str,
    event_type: &str,
    seconds: i64,
    bbox: [f32; 4],
    snapshot: Option<&str>,
    relation_to_camera: Option<&str>,
) -> i64 {
    let geometry = store
        .record_geometry_input(
            observation_id,
            &GeometryInput {
                sample_kind: format!("event_{event_type}_{seconds}"),
                captured_at: at(seconds),
                bbox,
                frame_width: Some(1280),
                frame_height: Some(720),
                zone: "desk".into(),
                source: "local_detector".into(),
                confidence: Some(0.8),
                snapshot_ref: snapshot.map(str::to_owned),
            },
        )
        .unwrap();
    if let Some(relation) = relation_to_camera {
        store
            .record_depth(
                geometry,
                "test-depth",
                "relative",
                relation,
                Some(0.7),
                "good",
                Some("qwen-test"),
            )
            .unwrap();
    }
    store
        .append_observation_event(&ObservationEventInput {
            observation_id,
            camera_id: "desk".into(),
            zone: "desk".into(),
            label: label.into(),
            event_type: event_type.into(),
            occurred_at: at(seconds),
            noticed_at: at(seconds + 1),
            source: "local_detector".into(),
            session_id: Some("run-1".into()),
            reason: if event_type == "appeared" {
                "appeared".into()
            } else {
                "missed_gap".into()
            },
            hits: if event_type == "appeared" { 1 } else { 6 },
            seen_for_s: (event_type == "disappeared").then_some(4.0),
            geometry_id: Some(geometry),
        })
        .unwrap();
    geometry
}

/// Keys on the desk; a box (label supplied) set down over them that stays.
fn write_scene(db: &std::path::Path, cover_label: &str, keys_snapshot: Option<&str>) {
    let store = Store::open(db).unwrap();
    let window = std::time::Duration::from_secs(300);
    let keys = store
        .record_sighting("desk", "desk", "keys", at(0), None, window)
        .unwrap()
        .0;
    let boxed = store
        .record_sighting("desk", "desk", cover_label, at(-1), None, window)
        .unwrap()
        .0;
    let keys_box = [600.0, 400.0, 660.0, 440.0];
    let cover_box = [560.0, 360.0, 720.0, 480.0];
    lifecycle(&store, keys, "keys", "appeared", -20, keys_box, None, None);
    lifecycle(
        &store,
        boxed,
        cover_label,
        "appeared",
        -1,
        cover_box,
        None,
        None,
    );
    lifecycle(
        &store,
        keys,
        "keys",
        "disappeared",
        0,
        keys_box,
        keys_snapshot,
        Some("farther"),
    );
    lifecycle(
        &store,
        boxed,
        cover_label,
        "disappeared",
        6,
        cover_box,
        None,
        None,
    );
}

fn app(db: &std::path::Path, root: PathBuf, priors: Priors) -> State_ {
    Arc::new(App {
        store: Mutex::new(Store::open_read_only(db).unwrap()),
        root,
        priors,
        vlm: std::sync::OnceLock::new(),
    })
}

async fn get_json(app: &State_, uri: &str) -> serde_json::Value {
    let response = router(app.clone())
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn example_priors() -> Priors {
    Priors::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../item-query/priors.example.toml"),
    )
    .unwrap()
}

#[tokio::test]
async fn ask_returns_the_direct_sighting_and_an_auditable_card() {
    let sandbox = Sandbox::new("card");
    let db = sandbox.0.join("items.db");
    write_scene(&db, "box", None);
    let app = app(&db, sandbox.0.clone(), Priors::builtin().clone());

    let body = get_json(&app, "/api/ask?q=where%20are%20my%20keys%3F").await;
    assert_eq!(body["mode"], "log");
    assert_eq!(body["keyword"], "keys");
    assert_eq!(body["matched"][0]["label"], "keys");
    assert!(body["note"].is_null());

    let cards = body["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    let card = &cards[0];
    assert_eq!(card["direct"]["label"], "keys");
    assert_eq!(card["direct"]["zone"], "desk");
    assert_eq!(card["hypothesis"]["relation"], "possibly_under");
    assert_eq!(card["hypothesis"]["phrase"], "under");
    assert_eq!(card["hypothesis"]["cover_label"], "box");
    assert_eq!(card["hypothesis"]["cover_offset_s"], -1.0);
    assert_eq!(card["geometry"]["target"]["bbox"][0], 600.0);
    assert_eq!(card["geometry"]["cover"]["frame_width"], 1280);
    assert_eq!(card["depth"][0]["role"], "target");
    assert_eq!(card["depth"][0]["model_ref"], "qwen-test");
    assert_eq!(card["provenance"]["rule_version"], "g4-size-1");
    assert!(
        card["caveat"]
            .as_str()
            .unwrap()
            .contains("not an observation")
    );
    let kinds: Vec<&str> = card["hypothesis"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    for kind in [
        "temporal_proximity",
        "projected_overlap",
        "persistence",
        "depth",
        "size",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
}

#[tokio::test]
async fn a_priors_file_on_item_web_promotes_the_same_way_as_the_cli() {
    let sandbox = Sandbox::new("priors");
    let db = sandbox.0.join("items.db");
    write_scene(&db, "box", None);
    let app = app(&db, sandbox.0.clone(), example_priors());
    let body = get_json(&app, "/api/ask?q=keys").await;
    let card = &body["cards"][0];
    assert_eq!(card["hypothesis"]["relation"], "possibly_contained_in");
    assert_eq!(card["hypothesis"]["phrase"], "inside");
    assert!(
        card["provenance"]["priors_ref"]
            .as_str()
            .unwrap()
            .contains("priors.example.toml")
    );
}

#[tokio::test]
async fn evidence_snapshots_are_served_only_when_the_file_exists() {
    let sandbox = Sandbox::new("snap");
    let db = sandbox.0.join("items.db");
    std::fs::write(sandbox.0.join("keys.jpg"), b"\xff\xd8jpeg").unwrap();
    write_scene(&db, "box", Some("keys.jpg"));
    let app = app(&db, sandbox.0.clone(), Priors::builtin().clone());

    let body = get_json(&app, "/api/ask?q=keys").await;
    let target = &body["cards"][0]["geometry"]["target"];
    assert_eq!(target["has_snapshot"], true);
    assert_eq!(body["cards"][0]["geometry"]["cover"]["has_snapshot"], false);

    let id = target["geometry_id"].as_i64().unwrap();
    let response = router(app.clone())
        .oneshot(
            Request::get(format!("/api/geometry/{id}/snapshot"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");

    let cover = body["cards"][0]["geometry"]["cover"]["geometry_id"]
        .as_i64()
        .unwrap();
    for uri in [
        format!("/api/geometry/{cover}/snapshot"),
        "/api/geometry/999999/snapshot".to_owned(),
    ] {
        let response = router(app.clone())
            .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn a_pre_evidence_database_still_answers_with_a_note() {
    let sandbox = Sandbox::new("v0");
    let db = sandbox.0.join("old.db");
    {
        // The shape of a database from before schema versioning.
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE observations (
                 id INTEGER PRIMARY KEY, camera_id TEXT NOT NULL, zone TEXT NOT NULL,
                 label TEXT NOT NULL, first_seen TEXT NOT NULL, last_seen TEXT NOT NULL,
                 hit_count INTEGER NOT NULL DEFAULT 1, sample_snapshot TEXT);
             INSERT INTO observations (camera_id, zone, label, first_seen, last_seen)
                 VALUES ('desk', 'desk', 'keys', '2026-09-13T13:32:00+00:00', '2026-09-13T13:35:00+00:00');",
        )
        .unwrap();
    }
    let app = app(&db, sandbox.0.clone(), Priors::builtin().clone());
    let body = get_json(&app, "/api/ask?q=keys").await;
    assert_eq!(body["matched"][0]["label"], "keys");
    assert_eq!(body["cards"].as_array().unwrap().len(), 0);
    assert!(body["note"].as_str().unwrap().contains("schema v0"));
}

#[tokio::test]
async fn hostile_labels_reach_the_page_as_data_and_the_page_escapes_card_fields() {
    let sandbox = Sandbox::new("xss");
    let db = sandbox.0.join("items.db");
    let hostile = "<img src=x onerror=alert(1)>";
    write_scene(&db, hostile, None);
    let app = app(&db, sandbox.0.clone(), Priors::builtin().clone());
    let body = get_json(&app, "/api/ask?q=keys").await;
    assert_eq!(body["cards"][0]["hypothesis"]["cover_label"], hostile);

    // The page builds cards with innerHTML; every model/webhook-supplied
    // field must pass through esc() on the way in.
    for field in [
        "esc(h.cover_label)",
        "esc(d.label)",
        "esc(d.zone)",
        "esc(d.camera)",
        "esc(e.detail)",
        "esc(h.explanation)",
        "esc(p.priors_ref)",
    ] {
        assert!(
            INDEX_HTML.contains(field),
            "index.html no longer escapes {field}"
        );
    }
}
