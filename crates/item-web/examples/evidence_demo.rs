//! A self-contained scene for looking at the G5 evidence cards in a browser:
//! keys on a desk, then a shoebox set down over them. Writes the database and a
//! synthetic snapshot through the same store calls ingest uses.
//!
//!   cargo run -p item-web --example evidence_demo -- target/evidence-demo
//!   cargo run -p item-web -- --db target/evidence-demo/items.db \
//!       --priors crates/item-query/priors.example.toml
//!
//! Run item-web from the repository root: snapshot refs are stored relative to it.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use image::{Rgb, RgbImage};
use item_core::store::Store;
use item_core::{GeometryInput, ObservationEventInput};

const W: u32 = 1280;
const H: u32 = 720;
const KEYS: [f32; 4] = [600.0, 400.0, 660.0, 440.0];
const SHOEBOX: [f32; 4] = [540.0, 350.0, 740.0, 490.0];

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "target/evidence-demo".into()),
    );
    std::fs::create_dir_all(&dir)?;
    let db = dir.join("items.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }

    let before = dir.join("keys-before.jpg");
    let after = dir.join("box-after.jpg");
    scene(false).save(&before)?;
    scene(true).save(&after)?;

    let store = Store::open(&db)?;
    let now = Utc::now();
    let t = |s: i64| now - Duration::seconds(120) + Duration::seconds(s);
    let window = std::time::Duration::from_secs(300);
    let keys = store
        .record_sighting("desk", "desk", "keys", t(0), None, window)?
        .0;
    store.set_sample_snapshot(keys, &before.to_string_lossy())?;
    let boxed = store
        .record_sighting("desk", "desk", "box", t(-1), None, window)?
        .0;
    store.set_sample_snapshot(boxed, &after.to_string_lossy())?;

    let event = |obs: i64, label: &str, kind: &str, s: i64, bbox, snap: &PathBuf| {
        let geometry = store.record_geometry_input(
            obs,
            &GeometryInput {
                sample_kind: format!("event_{kind}_{s}"),
                captured_at: t(s),
                bbox,
                frame_width: Some(W),
                frame_height: Some(H),
                zone: "desk".into(),
                source: "local_detector".into(),
                confidence: Some(0.82),
                snapshot_ref: Some(snap.to_string_lossy().into_owned()),
            },
        )?;
        store.append_observation_event(&ObservationEventInput {
            observation_id: obs,
            camera_id: "desk".into(),
            zone: "desk".into(),
            label: label.into(),
            event_type: kind.into(),
            occurred_at: t(s),
            noticed_at: t(s + 1),
            source: "local_detector".into(),
            session_id: Some("demo".into()),
            reason: if kind == "appeared" {
                "appeared"
            } else {
                "missed_gap"
            }
            .into(),
            hits: if kind == "appeared" { 1 } else { 8 },
            seen_for_s: (kind == "disappeared").then_some(6.0),
            geometry_id: Some(geometry),
        })
    };
    event(keys, "keys", "appeared", -30, KEYS, &before)?;
    event(boxed, "box", "appeared", -1, SHOEBOX, &after)?;
    event(keys, "keys", "disappeared", 0, KEYS, &before)?;
    event(boxed, "box", "disappeared", 7, SHOEBOX, &after)?;
    store.checkpoint()?;
    println!("wrote {}", db.display());
    Ok(())
}

/// A flat desk, the keys, and (after) an open shoebox over them.
fn scene(with_box: bool) -> RgbImage {
    let mut img = RgbImage::from_fn(W, H, |_, y| {
        if y < 300 {
            Rgb([214, 220, 226])
        } else {
            Rgb([176, 134, 92])
        }
    });
    fill(&mut img, [0.0, 296.0, W as f32, 304.0], Rgb([120, 88, 58]));
    fill(&mut img, KEYS, Rgb([196, 196, 204]));
    fill(&mut img, [612.0, 412.0, 628.0, 428.0], Rgb([150, 150, 160]));
    if with_box {
        fill(&mut img, SHOEBOX, Rgb([70, 96, 140]));
        fill(&mut img, [552.0, 362.0, 728.0, 380.0], Rgb([40, 58, 90]));
    }
    img
}

fn fill(img: &mut RgbImage, [x0, y0, x1, y1]: [f32; 4], color: Rgb<u8>) {
    for y in y0 as u32..(y1 as u32).min(H) {
        for x in x0 as u32..(x1 as u32).min(W) {
            img.put_pixel(x, y, color);
        }
    }
}
