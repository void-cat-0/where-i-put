//! G4 through the real binary: an on-disk database written by the same store
//! entry points ingest uses, read back by `item-query` with and without a
//! priors file.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, Duration, TimeZone, Utc};
use item_core::ObservationEventInput;
use item_core::store::Store;

struct Sandbox(PathBuf);

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "item-query-{name}-{}-{}",
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

/// One lifecycle row with its immutable geometry, as the daemon writes it.
fn lifecycle(
    store: &Store,
    observation_id: i64,
    label: &str,
    event_type: &str,
    seconds: i64,
    bbox: [f32; 4],
    hits: i64,
) {
    let geometry = store
        .record_geometry(
            observation_id,
            &format!("event_{event_type}_{seconds}"),
            at(seconds),
            bbox,
            Some(1280),
            Some(720),
            "desk",
            "local_detector",
            Some(0.8),
            None,
        )
        .unwrap();
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
            hits,
            seen_for_s: (event_type == "disappeared").then_some(4.0),
            geometry_id: Some(geometry),
        })
        .unwrap();
}

/// Keys on the desk, then a box set down over them that stays put.
fn write_scene(db: &Path) {
    let store = Store::open(db).unwrap();
    let window = std::time::Duration::from_secs(300);
    let keys = store
        .record_sighting("desk", "desk", "keys", at(0), None, window)
        .unwrap()
        .0;
    lifecycle(
        &store,
        keys,
        "keys",
        "appeared",
        -20,
        [600.0, 400.0, 660.0, 440.0],
        1,
    );
    let boxed = store
        .record_sighting("desk", "desk", "box", at(-1), None, window)
        .unwrap()
        .0;
    lifecycle(
        &store,
        boxed,
        "box",
        "appeared",
        -1,
        [560.0, 360.0, 720.0, 480.0],
        1,
    );
    lifecycle(
        &store,
        keys,
        "keys",
        "disappeared",
        0,
        [600.0, 400.0, 660.0, 440.0],
        20,
    );
    lifecycle(
        &store,
        boxed,
        "box",
        "disappeared",
        6,
        [560.0, 360.0, 720.0, 480.0],
        6,
    );
}

fn item_query(db: &Path, priors: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_item-query"));
    command.arg("--db").arg(db);
    if let Some(priors) = priors {
        command.arg("--priors").arg(priors);
    }
    command
        .args(args)
        .env_remove("ITEM_VLM_BASE_URL")
        .env_remove("ITEM_VLM_MODEL");
    command.output().unwrap()
}

fn relations(output: &Output) -> Vec<String> {
    assert!(
        output.status.success(),
        "item-query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    json.as_array()
        .unwrap()
        .iter()
        .map(|candidate| candidate["relation"].as_str().unwrap().to_owned())
        .collect()
}

const CANDIDATES: &[&str] = &["candidates", "keys", "--camera", "desk", "--json"];

#[test]
fn without_priors_a_box_over_the_keys_is_only_possibly_under() {
    let sandbox = Sandbox::new("no-priors");
    let db = sandbox.0.join("items.db");
    write_scene(&db);
    assert_eq!(
        relations(&item_query(&db, None, CANDIDATES)),
        ["possibly_under"]
    );
}

#[test]
fn the_shipped_example_priors_promote_the_open_shoebox() {
    let sandbox = Sandbox::new("example-priors");
    let db = sandbox.0.join("items.db");
    write_scene(&db);
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("priors.example.toml");

    let output = item_query(&db, Some(&example), CANDIDATES);
    assert_eq!(relations(&output), ["possibly_contained_in"]);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let priors_ref = json[0]["priors_ref"].as_str().unwrap();
    assert!(
        priors_ref.contains("priors.example.toml#fnv1a64:"),
        "{priors_ref}"
    );
    assert_eq!(json[0]["rule_version"], "g4-size-1");

    // `ask` without a sidecar prints the direct sighting first, then the
    // qualified hypothesis with its size evidence.
    let ask = item_query(&db, Some(&example), &["ask", "where are my keys?"]);
    assert!(ask.status.success());
    let text = String::from_utf8_lossy(&ask.stdout);
    let direct = text.find("was last directly seen").expect(&text);
    let hypothesis = text.find("possibly_contained_in").expect(&text);
    assert!(direct < hypothesis, "{text}");
    assert!(text.contains("size [supporting]"), "{text}");
}

#[test]
fn a_closed_box_in_the_priors_file_stays_under() {
    let sandbox = Sandbox::new("closed-priors");
    let db = sandbox.0.join("items.db");
    write_scene(&db);
    let example =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("priors.example.toml"))
            .unwrap();
    let closed = sandbox.0.join("closed.toml");
    let edited = example.replace("opening_state = \"open\" ", "opening_state = \"closed\" ");
    assert_ne!(edited, example, "the example's open-box line moved");
    std::fs::write(&closed, edited).unwrap();
    assert_eq!(
        relations(&item_query(&db, Some(&closed), CANDIDATES)),
        ["possibly_under"]
    );
}

#[test]
fn a_bad_priors_file_fails_loudly_before_touching_the_database() {
    let sandbox = Sandbox::new("bad-priors");
    let bad = sandbox.0.join("bad.toml");
    std::fs::write(
        &bad,
        "[[container]]\nlabel = \"box\"\nrole = \"container\"\ninterior_lenght_m = [0.1, 0.2]\n",
    )
    .unwrap();
    // The database does not exist either: the priors error must win.
    let output = item_query(&sandbox.0.join("missing.db"), Some(&bad), CANDIDATES);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("interior_lenght_m"), "{stderr}");
    assert!(stderr.contains("bad.toml"), "{stderr}");

    let missing = item_query(
        &sandbox.0.join("missing.db"),
        Some(&sandbox.0.join("nope.toml")),
        CANDIDATES,
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("reading priors file"));
}
