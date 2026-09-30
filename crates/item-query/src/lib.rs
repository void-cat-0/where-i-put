//! item-query: read the observation log and answer "where is X".
//!
//! v1 lookup is a plain label substring search over observations; the VLM
//! sidecar (OpenAI-compatible endpoint) formats the answer when configured.

pub mod containment;
pub mod vlm;

use crate::containment::Candidate;
use item_core::Observation;

/// The one content word a plain-language question is about
/// (`"where are my keys?"` -> `"keys"`).
///
/// Both frontends go through this, so the CLI and the web ask bar pick the
/// same keyword for the same question. The rule:
///
/// 1. split on whitespace and keep only alphanumerics from each word --
///    otherwise `"cup?"` is searched for literally, punctuation included, and
///    matches nothing;
/// 2. skip words shorter than two characters and the interrogatives/verbs that
///    appear in every question;
/// 3. fall back to the whole question when nothing is left, which the store
///    then matches as a literal substring.
///
/// Lower-casing is ASCII-only, the same folding SQLite's `LIKE` does.
pub fn keyword_of(question: &str) -> String {
    const STOPWORDS: &[&str] = &[
        "where", "wheres", "what", "whats", "when", "which", "who", "is", "are", "was", "were",
        "did", "do", "does", "the", "my", "me", "you", "your", "it", "its", "see", "saw", "put",
        "left", "tell", "find", "show", "at", "in", "on", "of", "to", "for", "there",
    ];
    question
        .split_whitespace()
        .map(normalize_word)
        .find(|word| word.chars().count() >= 2 && !STOPWORDS.contains(&word.as_str()))
        .unwrap_or_else(|| question.trim().to_lowercase())
}

/// Lower-cased alphanumerics of one word: `"keys?"` -> `"keys"`.
fn normalize_word(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Build the sighting-log prompt fed to the VLM.
pub fn build_prompt(query: &str, obs: &[Observation]) -> String {
    build_prompt_with_candidates(query, obs, &[])
}

/// Keep direct sightings ahead of derived hypotheses. A hypothesis is labelled
/// explicitly so a VLM cannot turn a 2-D relation into a fact.
pub fn build_prompt_with_candidates(
    query: &str,
    obs: &[Observation],
    candidates: &[Candidate],
) -> String {
    use std::fmt::Write;
    let mut s = String::from("Direct sighting log (most recent first):\n");
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
    if !candidates.is_empty() {
        s.push_str("\nDerived cover hypotheses (never direct sightings):\n");
        for candidate in candidates {
            writeln!(
                s,
                "- {} may be {} {}; heuristic score {:.2}; {}",
                candidate.target_label,
                candidate.relation,
                candidate.cover_label,
                candidate.score,
                candidate.explanation,
            )
            .unwrap();
        }
    }
    write!(
        s,
        "\nQuestion: \"{query}\". Answer in one or two sentences. State the direct last-seen location first, then append a clearly qualified hypothesis only if one is listed. Never present a hypothesis as a direct observation."
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

    /// The placeholder text of the web UI's ask bar, and the shape the CLI
    /// takes: punctuation must not travel into the search string.
    #[test]
    fn a_question_reduces_to_its_content_word() {
        assert_eq!(keyword_of("where is the cup?"), "cup");
        assert_eq!(keyword_of("Where are my keys!"), "keys");
        assert_eq!(keyword_of("where did I put the remote"), "remote");
        assert_eq!(keyword_of("  scissors  "), "scissors");
        assert_eq!(
            keyword_of("what's on the sofa?"),
            "sofa",
            "\"what's\" folds to the stopword \"whats\""
        );
        // A label with real punctuation keeps its alphanumerics, and the store
        // matches them literally.
        assert_eq!(keyword_of("50% tint"), "50");
    }

    #[test]
    fn a_question_without_a_content_word_falls_back_to_all_of_it() {
        assert_eq!(keyword_of("where is it?"), "where is it?");
        assert_eq!(keyword_of(""), "");
    }
}
