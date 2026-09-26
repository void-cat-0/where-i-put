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
        Cmd::Ask { question, label } => {
            // An explicit --label is used verbatim (it is already a filter the
            // user typed); a question is reduced to its content word.
            let word = label.unwrap_or_else(|| item_query::keyword_of(&question));
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
