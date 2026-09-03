//! item-query CLI:
//!   item-query log keys            # raw recent sightings
//!   item-query ask "where are my keys"  # optionally through the VLM sidecar
//! VLM is used only when ITEM_VLM_BASE_URL (and optionally ITEM_VLM_MODEL)
//! are set; otherwise `ask` prints the log rows it would have sent.

use clap::{Parser, Subcommand};

use item_core::store::Store;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let store = Store::open(&args.db)?;

    match args.cmd {
        Cmd::Log { label, limit } => {
            print_rows(&store.recent(label.as_deref(), limit)?);
        }
        Cmd::Ask { question, label } => {
            let needle = label.as_deref().unwrap_or(question.as_str());
            // crude v1 keyword extraction: first content word, skipping
            // interrogatives/pronouns ("where are my keys" -> "keys")
            let word = needle
                .split_whitespace()
                .map(str::to_lowercase)
                .find(|w| {
                    w.chars().filter(|c| c.is_alphabetic()).count() >= 2
                        && !matches!(
                            w.as_str(),
                            "where"
                                | "is"
                                | "are"
                                | "my"
                                | "the"
                                | "did"
                                | "do"
                                | "i"
                                | "put"
                                | "see"
                                | "was"
                                | "at"
                                | "in"
                                | "on"
                        )
                })
                .unwrap_or_else(|| needle.to_lowercase());
            let obs = store.recent(Some(&word), 20)?;
            if obs.is_empty() {
                println!("no sightings recorded for '{word}'");
                return Ok(());
            }
            let prompt = item_query::build_prompt(&question, &obs);
            match (
                std::env::var("ITEM_VLM_BASE_URL"),
                std::env::var("ITEM_VLM_MODEL"),
            ) {
                (Ok(base), Ok(model)) => {
                    let client = VlmClient::new(base, model);
                    println!("{}", client.ask(&prompt).await?);
                }
                _ => {
                    println!(
                        "(set ITEM_VLM_BASE_URL / ITEM_VLM_MODEL to answer via VLM; raw log below)"
                    );
                    print_rows(&obs);
                }
            }
        }
    }
    Ok(())
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
