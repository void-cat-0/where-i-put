//! Conservative 2-D disappearance/cover candidate rules (G2).
//!
//! These are deterministic hypotheses, not calibrated probabilities and never
//! physical containment claims. Missing depth or size evidence stays unknown.

use chrono::{DateTime, Duration, Utc};
use item_core::{DepthEvidence, ObservationGeometry, SceneEvent};
use serde::Serialize;

pub const RULE_VERSION: &str = "g2-2d-2";
pub const MIN_COVER_HITS: i64 = 2;
pub const MIN_COVER_PERSISTENCE: Duration = Duration::seconds(2);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvidenceContribution {
    pub kind: &'static str,
    pub state: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Candidate {
    pub target_event_id: i64,
    pub cover_event_id: i64,
    pub target_observation_id: i64,
    pub cover_observation_id: i64,
    pub target_geometry_id: Option<i64>,
    pub cover_geometry_id: Option<i64>,
    pub target_label: String,
    pub cover_label: String,
    pub relation: &'static str,
    /// Deterministic heuristic score. It is not a probability or calibrated
    /// confidence and should only be used to order candidates.
    pub score: f32,
    pub same_label_ambiguous: bool,
    pub rule_version: &'static str,
    pub explanation: String,
    pub evidence: Vec<EvidenceContribution>,
}

/// Find conservative 2-D hypotheses from authoritative lifecycle events.
///
/// The slice may contain a bounded scene window. A candidate requires a target
/// `missed_gap` departure and a persistent, geometrically related cover in the
/// same camera, zone, and ingest session. The temporal window is measured from
/// the target's last hit (`occurred_at`), never from the delayed sweep time
/// (`noticed_at`).
pub fn find_candidates(
    events: &[SceneEvent],
    target_label: &str,
    window: Duration,
) -> Vec<Candidate> {
    let window = window.max(Duration::zero());
    let mut out = Vec::new();

    for target in events.iter().filter(|event| is_target(event, target_label)) {
        let Some(target_geo) = valid_geometry(target.geometry.as_ref()) else {
            continue;
        };
        let Some(session_id) = target.event.session_id.as_deref() else {
            continue;
        };
        if target_reappeared(events, target, session_id) {
            continue;
        }

        for cover_appearance in events.iter().filter(|event| {
            is_cover_appearance(event, target, session_id)
                && within_last_hit_window(event.event.occurred_at, target.event.occurred_at, window)
        }) {
            let Some(cover) = cover_lifecycle(events, cover_appearance, target, window) else {
                continue;
            };
            let Some(cover_geo) = valid_geometry(cover.geometry.as_ref()) else {
                continue;
            };
            let Some(overlap) = geometry_overlap(target_geo, cover_geo) else {
                continue;
            };
            if overlap.coverage < 0.15 && overlap.iou < 0.05 {
                continue;
            }

            let depth = depth_compatibility(target, cover, target_geo, cover_geo);
            if depth.state == "conflicting" {
                continue;
            }

            let same_label_ambiguous = target.event.label.eq_ignore_ascii_case(&cover.event.label);
            let person = is_person_label(&cover.event.label);
            let cover_prior = is_cover_label(&cover.event.label);
            let relation = if cover_prior && !person {
                "possibly_under"
            } else {
                "possibly_occluded_by"
            };
            let persistence = persistence_evidence(cover_appearance, cover);
            let temporal_distance = target
                .event
                .occurred_at
                .signed_duration_since(cover_appearance.event.occurred_at)
                .num_milliseconds()
                .unsigned_abs() as f32
                / window.num_milliseconds().max(1) as f32;
            let temporal_score = (1.0 - temporal_distance.min(1.0)).max(0.0);
            let overlap_score = (overlap.coverage.max(overlap.iou)).clamp(0.0, 1.0);
            let persistence_score = (persistence.seconds / 10.0).clamp(0.0, 1.0);
            let semantic_score = if cover_prior { 1.0 } else { 0.0 };
            let score = (temporal_score * 0.25
                + overlap_score * 0.35
                + persistence_score * 0.25
                + semantic_score * 0.15)
                .clamp(0.0, 1.0);

            let mut evidence = vec![
                EvidenceContribution {
                    kind: "temporal_proximity",
                    state: "supporting",
                    detail: format!(
                        "cover lifecycle is {} from the target's last hit",
                        format_delta(
                            cover_appearance
                                .event
                                .occurred_at
                                .signed_duration_since(target.event.occurred_at)
                        )
                    ),
                },
                EvidenceContribution {
                    kind: "projected_overlap",
                    state: "supporting",
                    detail: format!(
                        "target coverage {:.0}%, IoU {:.0}%",
                        overlap.coverage * 100.0,
                        overlap.iou * 100.0
                    ),
                },
                EvidenceContribution {
                    kind: "persistence",
                    state: "supporting",
                    detail: format!(
                        "cover remained observable for {} with at least {} hits",
                        format_delta(Duration::milliseconds(
                            (persistence.seconds * 1000.0).round() as i64
                        )),
                        persistence.hits
                    ),
                },
                EvidenceContribution {
                    kind: "depth",
                    state: depth.state,
                    detail: depth.detail,
                },
            ];
            if same_label_ambiguous {
                evidence.push(EvidenceContribution {
                    kind: "identity",
                    state: "unknown",
                    detail: "cover and target have the same label; identity is ambiguous".into(),
                });
            }
            if !cover_prior {
                evidence.push(EvidenceContribution {
                    kind: "semantic_affordance",
                    state: "unknown",
                    detail: "cover label has no container/cover prior; relation remains occlusion"
                        .into(),
                });
            }

            out.push(Candidate {
                target_event_id: target.event.id,
                cover_event_id: cover_appearance.event.id,
                target_observation_id: target.event.observation_id,
                cover_observation_id: cover.event.observation_id,
                target_geometry_id: target.geometry.as_ref().map(|geometry| geometry.id),
                cover_geometry_id: cover.geometry.as_ref().map(|geometry| geometry.id),
                target_label: target.event.label.clone(),
                cover_label: cover.event.label.clone(),
                relation,
                score,
                same_label_ambiguous,
                rule_version: RULE_VERSION,
                explanation: format!(
                    "{} was last directly seen near {}; {} then covered {:.0}% of that image area and persisted. This is a 2-D visibility hypothesis, not proof of containment.",
                    target.event.label,
                    target_geo.zone,
                    cover.event.label,
                    overlap.coverage * 100.0
                ),
                evidence,
            });
        }
    }

    out.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.target_event_id.cmp(&right.target_event_id))
            .then_with(|| left.cover_event_id.cmp(&right.cover_event_id))
    });
    out
}

fn is_target(event: &SceneEvent, target_label: &str) -> bool {
    event.event.label.eq_ignore_ascii_case(target_label)
        && event.event.event_type == "disappeared"
        && event.event.reason == "missed_gap"
        && !excluded_event(event)
}

fn is_cover_appearance(event: &SceneEvent, target: &SceneEvent, session_id: &str) -> bool {
    event.event.event_type == "appeared"
        && event.event.camera_id == target.event.camera_id
        && event.event.zone == target.event.zone
        && event.event.session_id.as_deref() == Some(session_id)
        && event.event.observation_id != target.event.observation_id
        && !excluded_event(event)
}

fn excluded_event(event: &SceneEvent) -> bool {
    excluded_source(&event.event.source) || excluded_source(&event.event.reason)
}

fn excluded_source(source: &str) -> bool {
    matches!(
        source
            .to_ascii_lowercase()
            .replace([' ', '-'], "_")
            .as_str(),
        "lost" | "shutdown" | "rowchange" | "row_changed"
    )
}

fn target_reappeared(events: &[SceneEvent], target: &SceneEvent, session_id: &str) -> bool {
    events.iter().any(|event| {
        event.event.camera_id == target.event.camera_id
            && event.event.session_id.as_deref() == Some(session_id)
            && event.event.label.eq_ignore_ascii_case(&target.event.label)
            && event.event.occurred_at > target.event.occurred_at
            && event.event.event_type == "appeared"
            && !excluded_event(event)
    })
}

/// Select the lifecycle record that proves a cover persisted and the geometry
/// nearest the target's last hit. A later `disappeared` record is useful: it is
/// the immutable last/closed geometry for a pre-existing cover.
fn cover_lifecycle<'a>(
    events: &'a [SceneEvent],
    appearance: &'a SceneEvent,
    target: &SceneEvent,
    window: Duration,
) -> Option<&'a SceneEvent> {
    let mut related: Vec<&SceneEvent> = events
        .iter()
        .filter(|event| {
            event.event.observation_id == appearance.event.observation_id
                && event.event.camera_id == appearance.event.camera_id
                && event.event.zone == appearance.event.zone
                && event.event.session_id == appearance.event.session_id
                && !excluded_event(event)
                && valid_geometry(event.geometry.as_ref()).is_some()
        })
        .collect();
    related.sort_by_key(|event| event.event.occurred_at);

    let last = related.last().copied()?;
    let duration = last
        .event
        .occurred_at
        .signed_duration_since(appearance.event.occurred_at);
    let has_declared_persistence = related.iter().any(|event| {
        event.event.seen_for_s.unwrap_or_default() >= MIN_COVER_PERSISTENCE.num_seconds() as f64
            || event.event.hits >= MIN_COVER_HITS
    });
    let has_lifecycle_persistence = duration >= MIN_COVER_PERSISTENCE;
    let preexisting_nearby = appearance.event.occurred_at < target.event.occurred_at
        && target
            .event
            .occurred_at
            .signed_duration_since(appearance.event.occurred_at)
            <= window;
    let future_close_nearby = last.event.occurred_at >= target.event.occurred_at
        && last.event.occurred_at <= target.event.occurred_at + window;
    if !(has_declared_persistence || has_lifecycle_persistence) {
        return None;
    }
    if !preexisting_nearby && !future_close_nearby {
        return None;
    }
    if related.iter().any(|event| {
        event.event.event_type == "disappeared"
            && event.event.occurred_at <= target.event.occurred_at
            && event.event.occurred_at > appearance.event.occurred_at
    }) {
        return None;
    }

    // Prefer geometry at or just before the target, otherwise the appearance
    // geometry. This permits a closed/last sample without comparing frames.
    related
        .into_iter()
        .filter(|event| event.event.occurred_at <= target.event.occurred_at)
        .max_by_key(|event| event.event.occurred_at)
        .or_else(|| {
            events
                .iter()
                .find(|event| event.event.id == appearance.event.id)
        })
}

#[derive(Debug, Clone, Copy)]
struct Persistence {
    seconds: f32,
    hits: i64,
}

fn persistence_evidence(appearance: &SceneEvent, cover: &SceneEvent) -> Persistence {
    let seconds = cover
        .event
        .occurred_at
        .signed_duration_since(appearance.event.occurred_at)
        .num_milliseconds()
        .max(0) as f32
        / 1000.0;
    let declared = cover
        .event
        .seen_for_s
        .map(|seconds| seconds as f32)
        .unwrap_or(seconds)
        .max(seconds);
    Persistence {
        seconds: declared,
        hits: cover.event.hits.max(appearance.event.hits),
    }
}

#[derive(Debug, Clone, Copy)]
struct Overlap {
    coverage: f32,
    iou: f32,
}

fn valid_geometry(geometry: Option<&ObservationGeometry>) -> Option<&ObservationGeometry> {
    let geometry = geometry?;
    let width = geometry.frame_width?;
    let height = geometry.frame_height?;
    if width == 0 || height == 0 || geometry.bbox.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let [x0, y0, x1, y1] = geometry.bbox;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(geometry)
}

fn geometry_overlap(a: &ObservationGeometry, b: &ObservationGeometry) -> Option<Overlap> {
    if a.frame_width != b.frame_width || a.frame_height != b.frame_height {
        return None;
    }
    let ix0 = a.bbox[0].max(b.bbox[0]);
    let iy0 = a.bbox[1].max(b.bbox[1]);
    let ix1 = a.bbox[2].min(b.bbox[2]);
    let iy1 = a.bbox[3].min(b.bbox[3]);
    let intersection = (ix1 - ix0).max(0.0) * (iy1 - iy0).max(0.0);
    let area_a = (a.bbox[2] - a.bbox[0]) * (a.bbox[3] - a.bbox[1]);
    let area_b = (b.bbox[2] - b.bbox[0]) * (b.bbox[3] - b.bbox[1]);
    if area_a <= 0.0 || area_b <= 0.0 || !area_a.is_finite() || !area_b.is_finite() {
        return None;
    }
    let union = area_a + area_b - intersection;
    Some(Overlap {
        coverage: intersection / area_a,
        iou: if union > 0.0 {
            intersection / union
        } else {
            0.0
        },
    })
}

#[derive(Debug)]
struct DepthCompatibility {
    state: &'static str,
    detail: String,
}

fn depth_compatibility(
    target: &SceneEvent,
    cover: &SceneEvent,
    target_geo: &ObservationGeometry,
    cover_geo: &ObservationGeometry,
) -> DepthCompatibility {
    if target_geo.captured_at != cover_geo.captured_at {
        return DepthCompatibility {
            state: "unknown",
            detail: "depth not compared: target and cover geometry came from different frames"
                .into(),
        };
    }
    let (Some(target_depth), Some(cover_depth)) =
        (usable_depth(&target.depth), usable_depth(&cover.depth))
    else {
        return DepthCompatibility {
            state: "unknown",
            detail: "no comparable same-frame relative depth evidence".into(),
        };
    };
    if target_depth.response_id.is_empty() || target_depth.response_id != cover_depth.response_id {
        return DepthCompatibility {
            state: "unknown",
            detail: "depth not compared: samples came from different provider responses".into(),
        };
    }
    let target_farther = target_depth.relation_to_camera == "farther";
    let cover_nearer = cover_depth.relation_to_camera == "nearer";
    let target_nearer = target_depth.relation_to_camera == "nearer";
    let cover_farther = cover_depth.relation_to_camera == "farther";
    if target_nearer && cover_farther {
        DepthCompatibility {
            state: "conflicting",
            detail: "same-frame depth places the target in front of the cover".into(),
        }
    } else if target_farther && cover_nearer {
        DepthCompatibility {
            state: "supporting",
            detail: "same-frame depth places the cover in front of the target".into(),
        }
    } else {
        DepthCompatibility {
            state: "unknown",
            detail: "same-frame depth has no decisive ordering".into(),
        }
    }
}

fn usable_depth(depth: &[DepthEvidence]) -> Option<&DepthEvidence> {
    depth.iter().find(|evidence| {
        evidence.quality != "invalid"
            && matches!(
                evidence.relation_to_camera.as_str(),
                "nearer" | "farther" | "same_plane" | "overlapping_depth"
            )
    })
}

fn is_person_label(label: &str) -> bool {
    matches!(
        label.to_ascii_lowercase().as_str(),
        "person" | "human" | "hand" | "people"
    )
}

fn is_cover_label(label: &str) -> bool {
    matches!(
        label.to_ascii_lowercase().as_str(),
        "box" | "book" | "bag" | "drawer" | "cabinet" | "basket" | "tray" | "cloth" | "lid"
    )
}

fn within_last_hit_window(at: DateTime<Utc>, last_hit: DateTime<Utc>, window: Duration) -> bool {
    let delta = at.signed_duration_since(last_hit);
    delta >= -window && delta <= window
}

fn format_delta(delta: Duration) -> String {
    let seconds = delta.num_milliseconds() as f64 / 1000.0;
    format!("{seconds:.1}s")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use item_core::{ObservationEvent, ObservationEventInput, ObservationGeometry};

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap() + Duration::seconds(seconds)
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        id: i64,
        observation_id: i64,
        label: &str,
        kind: &str,
        seconds: i64,
        reason: &str,
        session: Option<&str>,
        bbox: [f32; 4],
        hits: i64,
        seen_for_s: Option<f64>,
    ) -> SceneEvent {
        let timestamp = at(seconds);
        SceneEvent {
            event: ObservationEvent {
                id,
                observation_id,
                camera_id: "cam".into(),
                zone: "desk".into(),
                label: label.into(),
                event_type: kind.into(),
                occurred_at: timestamp,
                noticed_at: timestamp + Duration::seconds(30),
                source: "local_detector".into(),
                session_id: session.map(str::to_owned),
                reason: reason.into(),
                hits,
                seen_for_s,
                geometry_id: Some(id),
            },
            geometry: Some(ObservationGeometry {
                id,
                observation_id,
                sample_kind: kind.into(),
                captured_at: timestamp,
                bbox,
                frame_width: Some(100),
                frame_height: Some(100),
                zone: "desk".into(),
                source: "local_detector".into(),
                confidence: Some(0.9),
                snapshot_ref: None,
            }),
            depth: vec![],
        }
    }

    #[test]
    fn accepts_persistent_cover_using_last_geometry_without_containment_claim() {
        let events = vec![
            event(
                1,
                1,
                "keys",
                "disappeared",
                0,
                "missed_gap",
                Some("s"),
                [10.0, 10.0, 40.0, 40.0],
                3,
                Some(3.0),
            ),
            event(
                2,
                2,
                "box",
                "appeared",
                -1,
                "appeared",
                Some("s"),
                [5.0, 5.0, 50.0, 50.0],
                1,
                None,
            ),
            event(
                3,
                2,
                "box",
                "disappeared",
                3,
                "missed_gap",
                Some("s"),
                [4.0, 4.0, 55.0, 55.0],
                3,
                Some(4.0),
            ),
        ];
        let candidates = find_candidates(&events, "keys", Duration::seconds(10));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].relation, "possibly_under");
        assert!(!candidates[0].explanation.contains("inside"));
        assert!(candidates[0].cover_geometry_id.is_some());
    }

    #[test]
    fn rejects_wrong_session_reappearance_bad_geometry_and_shutdown() {
        let mut reappeared = event(
            2,
            2,
            "keys",
            "appeared",
            2,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            1,
            None,
        );
        reappeared.event.camera_id = "cam".into();
        let mut cover = event(
            3,
            3,
            "box",
            "appeared",
            1,
            "appeared",
            Some("other"),
            [10.0, 10.0, 40.0, 40.0],
            3,
            Some(3.0),
        );
        cover.geometry.as_mut().unwrap().frame_width = None;
        let target = event(
            1,
            1,
            "keys",
            "disappeared",
            0,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        assert!(
            find_candidates(&[target.clone(), cover], "keys", Duration::seconds(10)).is_empty()
        );
        assert!(find_candidates(&[target, reappeared], "keys", Duration::seconds(10)).is_empty());

        let target = event(
            4,
            4,
            "keys",
            "disappeared",
            0,
            "shutdown",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        let cover = event(
            5,
            5,
            "box",
            "appeared",
            1,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            3,
            Some(3.0),
        );
        assert!(find_candidates(&[target, cover], "keys", Duration::seconds(10)).is_empty());
    }

    #[test]
    fn overlap_alone_does_not_pass_persistence_or_make_under_relation() {
        let target = event(
            1,
            1,
            "keys",
            "disappeared",
            0,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        let cover = event(
            2,
            2,
            "chair",
            "appeared",
            1,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            1,
            None,
        );
        assert!(find_candidates(&[target, cover], "keys", Duration::seconds(10)).is_empty());
    }

    #[test]
    fn person_can_only_be_an_occluder() {
        let target = event(
            1,
            1,
            "keys",
            "disappeared",
            0,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        let person = event(
            2,
            2,
            "person",
            "appeared",
            1,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            1,
            Some(3.0),
        );
        let closed = event(
            3,
            2,
            "person",
            "disappeared",
            4,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            3,
            Some(3.0),
        );
        let candidates = find_candidates(&[target, person, closed], "keys", Duration::seconds(10));
        assert_eq!(candidates[0].relation, "possibly_occluded_by");
    }

    #[test]
    fn same_frame_depth_conflict_rejects_but_different_frames_are_unknown() {
        let mut target = event(
            1,
            1,
            "keys",
            "disappeared",
            0,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        let mut cover = event(
            2,
            2,
            "box",
            "appeared",
            1,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            1,
            Some(3.0),
        );
        target.depth.push(DepthEvidence {
            id: 1,
            geometry_id: 1,
            provider: "test".into(),
            frame_id: "frame-1".into(),
            prompt_version: "prompt-1".into(),
            backend_version: "backend-1".into(),
            mode: "relative".into(),
            relation_to_camera: "nearer".into(),
            relative_score: None,
            quality: "good".into(),
            response_id: "test-response".into(),
            model_ref: None,
            value_m: None,
            uncertainty_m: None,
            valid_fraction: None,
            coordinate_frame: None,
            calibration_ref: None,
        });
        cover.depth.push(DepthEvidence {
            id: 2,
            geometry_id: 2,
            provider: "test".into(),
            frame_id: "frame-1".into(),
            prompt_version: "prompt-1".into(),
            backend_version: "backend-1".into(),
            mode: "relative".into(),
            relation_to_camera: "farther".into(),
            relative_score: None,
            quality: "good".into(),
            response_id: "test-response".into(),
            model_ref: None,
            value_m: None,
            uncertainty_m: None,
            valid_fraction: None,
            coordinate_frame: None,
            calibration_ref: None,
        });
        cover.geometry.as_mut().unwrap().captured_at = at(0);
        let candidates = find_candidates(
            &[target.clone(), cover.clone()],
            "keys",
            Duration::seconds(10),
        );
        assert!(candidates.is_empty());
        cover.geometry.as_mut().unwrap().captured_at = at(2);
        let candidates = find_candidates(&[target, cover], "keys", Duration::seconds(10));
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0]
                .evidence
                .iter()
                .find(|evidence| evidence.kind == "depth")
                .map(|evidence| evidence.state),
            Some("unknown")
        );
    }

    #[test]
    fn scene_events_from_read_only_store_are_usable() {
        let store = item_core::store::Store::in_memory().unwrap();
        let target_id = store
            .record_sighting(
                "cam",
                "desk",
                "keys",
                at(0),
                None,
                std::time::Duration::from_secs(300),
            )
            .unwrap()
            .0;
        let target_geo = store
            .record_geometry(
                target_id,
                "target_last",
                at(0),
                [10.0, 10.0, 40.0, 40.0],
                Some(100),
                Some(100),
                "desk",
                "local_detector",
                Some(0.9),
                None,
            )
            .unwrap();
        store
            .append_observation_event(&ObservationEventInput {
                observation_id: target_id,
                camera_id: "cam".into(),
                zone: "desk".into(),
                label: "keys".into(),
                event_type: "disappeared".into(),
                occurred_at: at(0),
                noticed_at: at(30),
                source: "local_detector".into(),
                session_id: Some("s".into()),
                reason: "missed_gap".into(),
                hits: 2,
                seen_for_s: Some(2.0),
                geometry_id: Some(target_geo),
            })
            .unwrap();
        let cover_id = store
            .record_sighting(
                "cam",
                "desk",
                "box",
                at(-1),
                None,
                std::time::Duration::from_secs(300),
            )
            .unwrap()
            .0;
        let cover_geo = store
            .record_geometry(
                cover_id,
                "cover_first",
                at(-1),
                [5.0, 5.0, 50.0, 50.0],
                Some(100),
                Some(100),
                "desk",
                "local_detector",
                Some(0.9),
                None,
            )
            .unwrap();
        store
            .append_observation_event(&ObservationEventInput {
                observation_id: cover_id,
                camera_id: "cam".into(),
                zone: "desk".into(),
                label: "box".into(),
                event_type: "appeared".into(),
                occurred_at: at(-1),
                noticed_at: at(-1),
                source: "local_detector".into(),
                session_id: Some("s".into()),
                reason: "appeared".into(),
                hits: 1,
                seen_for_s: None,
                geometry_id: Some(cover_geo),
            })
            .unwrap();
        let closed_geo = store
            .record_geometry(
                cover_id,
                "cover_closed",
                at(3),
                [4.0, 4.0, 55.0, 55.0],
                Some(100),
                Some(100),
                "desk",
                "local_detector",
                Some(0.9),
                None,
            )
            .unwrap();
        store
            .append_observation_event(&ObservationEventInput {
                observation_id: cover_id,
                camera_id: "cam".into(),
                zone: "desk".into(),
                label: "box".into(),
                event_type: "disappeared".into(),
                occurred_at: at(3),
                noticed_at: at(4),
                source: "local_detector".into(),
                session_id: Some("s".into()),
                reason: "missed_gap".into(),
                hits: 3,
                seen_for_s: Some(4.0),
                geometry_id: Some(closed_geo),
            })
            .unwrap();
        let events = store
            .scene_events_for_camera("cam", at(-10), at(10), 100)
            .unwrap();
        let candidates = find_candidates(&events, "keys", Duration::seconds(10));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].cover_observation_id, cover_id);
    }
}
