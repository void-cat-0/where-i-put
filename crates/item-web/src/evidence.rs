//! G5 evidence cards: a cover hypothesis together with everything a person
//! needs to audit it -- the direct last sighting, both boxes, the snapshots
//! they were cut from, depth provenance, and the rule/priors versions.
//!
//! A card never turns a hypothesis into an observation: `direct` is always the
//! last thing actually seen, and `hypothesis` carries its own relation, score
//! and caveat.

use item_core::store::Store;
use item_core::{DepthEvidence, Observation, ObservationGeometry};
use item_query::containment::{Candidate, EvidenceContribution};
use serde::Serialize;

/// Stated on every card, so no frontend can drop it.
pub const CAVEAT: &str = "A derived hypothesis, not an observation. The score only ranks hypotheses; it is not a probability.";

#[derive(Debug, Serialize)]
pub struct Card {
    pub direct: Direct,
    pub hypothesis: Hypothesis,
    pub geometry: Geometry,
    pub depth: Vec<DepthJson>,
    pub provenance: Provenance,
    pub caveat: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Direct {
    pub observation_id: i64,
    pub label: String,
    pub camera: String,
    pub zone: String,
    pub last_seen: String,
    pub hits: i64,
}

#[derive(Debug, Serialize)]
pub struct Hypothesis {
    pub relation: &'static str,
    /// "under", "inside", "behind": the plain-words reading of `relation`.
    pub phrase: &'static str,
    pub cover_label: String,
    pub cover_observation_id: i64,
    pub score: f32,
    pub explanation: String,
    pub same_label_ambiguous: bool,
    pub target_last_hit: String,
    pub cover_appeared_at: String,
    /// Cover appearance minus the target's last hit, in seconds (negative:
    /// the cover was already there).
    pub cover_offset_s: f64,
    pub evidence: Vec<EvidenceContribution>,
}

#[derive(Debug, Serialize)]
pub struct Geometry {
    pub target: Option<Sample>,
    pub cover: Option<Sample>,
}

#[derive(Debug, Serialize)]
pub struct Sample {
    pub geometry_id: i64,
    pub sample_kind: String,
    pub captured_at: String,
    pub bbox: [f32; 4],
    pub frame_width: Option<u32>,
    pub frame_height: Option<u32>,
    pub source: String,
    pub confidence: Option<f32>,
    /// Served by `/api/geometry/{id}/snapshot`; only true when the stored
    /// reference resolves to a file on this machine.
    pub has_snapshot: bool,
}

#[derive(Debug, Serialize)]
pub struct DepthJson {
    /// "target" or "cover".
    pub role: &'static str,
    pub provider: String,
    pub mode: String,
    pub relation_to_camera: String,
    pub quality: String,
    pub relative_score: Option<f32>,
    pub model_ref: Option<String>,
    pub frame_id: String,
    pub response_id: String,
    pub prompt_version: String,
    pub backend_version: String,
}

#[derive(Debug, Serialize)]
pub struct Provenance {
    pub rule_version: &'static str,
    pub priors_ref: String,
    pub target_event_id: i64,
    pub cover_event_id: i64,
}

pub fn phrase(relation: &str) -> &'static str {
    match relation {
        "possibly_contained_in" => "inside",
        "possibly_under" => "under",
        _ => "hidden behind",
    }
}

/// Build the card for one candidate. `direct` is the target's own observation
/// row; `snapshot_exists` decides whether a stored snapshot ref is servable.
pub fn card(
    store: &Store,
    direct: &Observation,
    candidate: &Candidate,
    snapshot_exists: impl Fn(&str) -> bool,
) -> item_core::store::Result<Card> {
    let sample =
        |id: Option<i64>| -> item_core::store::Result<Option<(Sample, Vec<DepthEvidence>)>> {
            let Some(id) = id else { return Ok(None) };
            let Some(geometry) = store.geometry_by_id(id)? else {
                return Ok(None);
            };
            let depth = store.depth_for_geometry(id)?;
            Ok(Some((to_sample(geometry, &snapshot_exists), depth)))
        };
    let target = sample(candidate.target_geometry_id)?;
    let cover = sample(candidate.cover_geometry_id)?;

    let mut depth = Vec::new();
    for (role, side) in [("target", &target), ("cover", &cover)] {
        if let Some((_, rows)) = side {
            depth.extend(rows.iter().map(|row| to_depth(role, row)));
        }
    }

    let offset = candidate
        .cover_appeared_at
        .signed_duration_since(candidate.target_last_hit)
        .num_milliseconds() as f64
        / 1000.0;
    Ok(Card {
        direct: Direct {
            observation_id: direct.id,
            label: direct.label.clone(),
            camera: direct.camera_id.clone(),
            zone: direct.zone.clone(),
            last_seen: direct.last_seen.to_rfc3339(),
            hits: direct.hit_count,
        },
        hypothesis: Hypothesis {
            relation: candidate.relation,
            phrase: phrase(candidate.relation),
            cover_label: candidate.cover_label.clone(),
            cover_observation_id: candidate.cover_observation_id,
            score: candidate.score,
            explanation: candidate.explanation.clone(),
            same_label_ambiguous: candidate.same_label_ambiguous,
            target_last_hit: candidate.target_last_hit.to_rfc3339(),
            cover_appeared_at: candidate.cover_appeared_at.to_rfc3339(),
            cover_offset_s: offset,
            evidence: candidate.evidence.clone(),
        },
        geometry: Geometry {
            target: target.map(|(sample, _)| sample),
            cover: cover.map(|(sample, _)| sample),
        },
        depth,
        provenance: Provenance {
            rule_version: candidate.rule_version,
            priors_ref: candidate.priors_ref.clone(),
            target_event_id: candidate.target_event_id,
            cover_event_id: candidate.cover_event_id,
        },
        caveat: CAVEAT,
    })
}

fn to_sample(geometry: ObservationGeometry, snapshot_exists: &impl Fn(&str) -> bool) -> Sample {
    Sample {
        has_snapshot: geometry
            .snapshot_ref
            .as_deref()
            .is_some_and(snapshot_exists),
        geometry_id: geometry.id,
        sample_kind: geometry.sample_kind,
        captured_at: geometry.captured_at.to_rfc3339(),
        bbox: geometry.bbox,
        frame_width: geometry.frame_width,
        frame_height: geometry.frame_height,
        source: geometry.source,
        confidence: geometry.confidence,
    }
}

fn to_depth(role: &'static str, row: &DepthEvidence) -> DepthJson {
    DepthJson {
        role,
        provider: row.provider.clone(),
        mode: row.mode.clone(),
        relation_to_camera: row.relation_to_camera.clone(),
        quality: row.quality.clone(),
        relative_score: row.relative_score,
        model_ref: row.model_ref.clone(),
        frame_id: row.frame_id.clone(),
        response_id: row.response_id.clone(),
        prompt_version: row.prompt_version.clone(),
        backend_version: row.backend_version.clone(),
    }
}
