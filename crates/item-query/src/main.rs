//! item-query CLI:
//!   item-query log keys            # raw recent sightings
//!   item-query ask "where are my keys"  # optionally through the VLM sidecar
//! VLM is used only when ITEM_VLM_BASE_URL and ITEM_VLM_MODEL are both set;
//! otherwise `ask` prints the log rows it would have sent.
//!
//! This is the read side, so it opens the database **read-only**: a typo in
//! `--db` fails with "no such database" instead of creating an empty one and
//! answering "no sightings" from it, and it can never take the write lock the
//! ingest daemon needs.

use clap::{Parser, Subcommand};

use chrono::Duration;
use item_core::store::Store;
use item_query::build_prompt_with_candidates;
use item_query::containment::{Candidate, find_candidates};
use item_query::vlm::VlmClient;

#[derive(Parser)]
#[command(name = "item-query")]
struct Args {
    #[arg(long, default_value = "data/items.db", global = true)]
    db: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List recent sightings, optionally filtered by label substring.
    Log {
        label: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Ask a natural-language question about item locations.
    Ask {
        question: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// List conservative 2-D disappearance/cover hypotheses.
    Candidates {
        label: String,
        #[arg(long)]
        camera: Option<String>,
        #[arg(long, default_value_t = 120)]
        window_secs: i64,
        #[arg(long, default_value_t = 200)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use anyhow::Context as _;

    let args = Args::parse();
    let store = Store::open_read_only(&args.db).with_context(|| {
        format!(
            "reading {} (has the ingest loop written it yet? --db selects another one)",
            args.db
        )
    })?;

    match args.cmd {
        Cmd::Log { label, limit } => {
            let rows = store.recent(label.as_deref(), limit)?;
            if rows.is_empty() {
                println!("no sightings recorded");
                return Ok(());
            }
            print_rows(&rows);
        }
        Cmd::Candidates {
            label,
            camera,
            window_secs,
            limit,
            json,
        } => {
            let observations = store.recent(Some(&label), limit)?;
            let candidates = query_candidates(
                &store,
                &observations,
                camera.as_deref(),
                Duration::seconds(window_secs.clamp(1, 86_400)),
                limit,
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&candidates)?);
            } else if candidates.is_empty() {
                println!("no supported cover candidates for '{label}'");
            } else {
                print_candidates(&candidates);
            }
        }
        Cmd::Ask { question, label } => {
            // An explicit --label is used verbatim (it is already a filter the
            // user typed); a question is reduced to its content word.
            let word = label.unwrap_or_else(|| item_query::keyword_of(&question));
            let obs = store.recent(Some(&word), 20)?;
            if obs.is_empty() {
                println!("no sightings recorded for '{word}'");
                return Ok(());
            }
            let candidates = query_candidates(&store, &obs, None, Duration::seconds(10), 200)
                .unwrap_or_else(|error| {
                    eprintln!("cover evidence unavailable: {error}");
                    Vec::new()
                });
            let prompt = build_prompt_with_candidates(&question, &obs, &candidates);
            match (
                std::env::var("ITEM_VLM_BASE_URL"),
                std::env::var("ITEM_VLM_MODEL"),
            ) {
                (Ok(base), Ok(model)) => {
                    let client = VlmClient::new(base, model);
                    match client.ask(&prompt).await {
                        Ok(answer) => println!("{answer}"),
                        Err(error) => {
                            eprintln!("VLM unavailable: {error}; using deterministic evidence");
                            print_direct_answer(&obs);
                            print_candidates(&candidates);
                        }
                    }
                }
                _ => {
                    print_direct_answer(&obs);
                    print_candidates(&candidates);
                }
            }
        }
    }
    Ok(())
}

fn query_candidates(
    store: &Store,
    observations: &[item_core::Observation],
    camera: Option<&str>,
    window: Duration,
    limit: i64,
) -> anyhow::Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for observation in observations {
        if camera.is_some_and(|camera| camera != observation.camera_id) {
            continue;
        }
        // Anchor the event window to the recorded target last hit, not now.
        // Include the later missed-gap sweep and closed cover geometry.
        let since = observation.last_seen - window;
        let until = observation.last_seen + window.max(Duration::seconds(31));
        let events = store.scene_events_for_camera(&observation.camera_id, since, until, limit)?;
        for candidate in find_candidates(&events, &observation.label, window) {
            if candidate.target_observation_id == observation.id {
                candidates.push(candidate);
            }
        }
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.target_event_id.cmp(&right.target_event_id))
            .then_with(|| left.cover_event_id.cmp(&right.cover_event_id))
    });
    candidates.dedup_by_key(|candidate| (candidate.target_event_id, candidate.cover_event_id));
    Ok(candidates)
}

fn print_direct_answer(obs: &[item_core::Observation]) {
    if let Some(observation) = obs.first() {
        println!(
            "{} was last directly seen at {}/{} on {} UTC.",
            observation.label,
            observation.camera_id,
            observation.zone,
            observation.last_seen.format("%Y-%m-%d %H:%M:%S"),
        );
    }
}

fn print_candidates(candidates: &[Candidate]) {
    for candidate in candidates {
        println!(
            "Hypothesis: {} -> {} ({}, heuristic score {:.2}): {}",
            candidate.target_label,
            candidate.cover_label,
            candidate.relation,
            candidate.score,
            candidate.explanation
        );
    }
}

fn print_rows(obs: &[item_core::Observation]) {
    for o in obs {
        println!(
            "{:<14} {:>3}x  {:>16} .. {:<16}  {}/{}",
            o.label,
            o.hit_count,
            o.first_seen.format("%m-%d %H:%M"),
            o.last_seen.format("%m-%d %H:%M"),
            o.camera_id,
            o.zone,
        );
    }
}
