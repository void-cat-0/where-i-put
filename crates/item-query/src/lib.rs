//! item-query: read the observation log and answer "where is X".
//!
//! v1 lookup is a plain label substring search over observations; the VLM
//! sidecar (OpenAI-compatible endpoint) formats the answer when configured.

pub mod vlm;

use item_core::Observation;

/// Build the sighting-log prompt fed to the VLM.
pub fn build_prompt(query: &str, obs: &[Observation]) -> String {
    use std::fmt::Write;
    let mut s = String::from("Sighting log (most recent first):\n");
    if obs.is_empty() {
        s.push_str("(empty)\n");
    }
    for o in obs {
        writeln!(
            s,
            "- {} x{}: last seen {} at camera `{}` / zone `{}`",
            o.label,
            o.hit_count,
            o.last_seen.format("%Y-%m-%d %H:%M"),
            o.camera_id,
            o.zone,
        )
        .unwrap();
    }
    write!(
        s,
        "\nQuestion: \"{query}\". Answer in one or two sentences using only the log."
    )
    .unwrap();
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn prompt_lists_observations() {
        let obs = vec![Observation {
            id: 1,
            camera_id: "living".into(),
            zone: "sofa".into(),
            label: "keys".into(),
            first_seen: Utc.with_ymd_and_hms(2026, 9, 2, 9, 0, 0).unwrap(),
            last_seen: Utc.with_ymd_and_hms(2026, 9, 2, 18, 30, 0).unwrap(),
            hit_count: 4,
            sample_snapshot: None,
        }];
        let p = build_prompt("where are my keys", &obs);
        assert!(p.contains("keys"));
        assert!(p.contains("sofa"));
        assert!(p.contains("18:30"));
    }
}
