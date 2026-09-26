//! item-web: read-only web UI over the observation database.
//!
//! Serves a single-page app (inlined HTML/JS, zero build step) plus a small
//! JSON API. Opens the same SQLite file the ingest daemon writes, read-only
//! (WAL allows concurrent readers), so web and daemon never contend. VLM
//! answering is reused from item-query when ITEM_VLM_BASE_URL/MODEL are set.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use clap::Parser;

use item_core::Observation;
use item_core::store::Store;

const INDEX_HTML: &str = include_str!("../web/index.html");

#[derive(Parser)]
#[command(name = "item-web")]
struct Args {
    /// SQLite database to read (written by item-ingest).
    #[arg(long, default_value = "data/items.db")]
    db: String,

    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1:8478")]
    listen: String,
}

type State_ = Arc<App>;

struct App {
    store: Mutex<Store>,
    /// Base directory the daemon wrote snapshots relative to (paths in the
    /// db may be relative); usually the ingest process's cwd == our cwd.
    root: PathBuf,
    /// The sidecar client, built once from the environment (`None` when
    /// ITEM_VLM_BASE_URL/MODEL are unset). Held rather than rebuilt per
    /// request, so every ask reuses one connection pool.
    vlm: std::sync::OnceLock<Option<item_query::vlm::VlmClient>>,
}

impl App {
    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().expect("store mutex")
    }

    /// The VLM client, if the environment describes one.
    fn vlm(&self) -> Option<&item_query::vlm::VlmClient> {
        self.vlm
            .get_or_init(|| {
                let base = std::env::var("ITEM_VLM_BASE_URL").ok()?;
                let model = std::env::var("ITEM_VLM_MODEL").ok()?;
                Some(item_query::vlm::VlmClient::new(base, model))
            })
            .as_ref()
    }

    /// Resolve a db-stored snapshot path to an on-disk candidate: absolute
    /// as-is, relative against our cwd (== the daemon's, when co-located).
    /// `frigate://` (or any URI) refs have no local file.
    fn resolve_snapshot(&self, stored: &str) -> Option<PathBuf> {
        if stored.contains("://") {
            return None;
        }
        let p = PathBuf::from(stored);
        Some(if p.is_absolute() {
            p
        } else {
            self.root.join(p)
        })
    }
}

#[derive(serde::Deserialize)]
struct ListParams {
    label: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    100
}

#[derive(serde::Serialize)]
struct ObsJson {
    id: i64,
    camera: String,
    zone: String,
    label: String,
    first_seen: String,
    last_seen: String,
    hits: i64,
    /// true when a snapshot file exists and /api/snapshot/{id} will serve it
    has_snapshot: bool,
}

impl ObsJson {
    fn new(o: &Observation, exists: impl Fn(&str) -> bool) -> Self {
        // A stored path counts as available only if it resolves to a real
        // file (the daemon may run elsewhere; frigate:// refs never do).
        let has_snapshot = o.sample_snapshot.as_deref().is_some_and(&exists);
        Self {
            id: o.id,
            camera: o.camera_id.clone(),
            zone: o.zone.clone(),
            label: o.label.clone(),
            first_seen: o.first_seen.to_rfc3339(),
            last_seen: o.last_seen.to_rfc3339(),
            hits: o.hit_count,
            has_snapshot,
        }
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn list(
    State(app): State<State_>,
    Query(q): Query<ListParams>,
) -> Result<axum::Json<Vec<ObsJson>>, StatusCode> {
    let store = app.store();
    let obs = store
        .recent(q.label.as_deref(), q.limit.clamp(1, 500))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    drop(store);
    Ok(axum::Json(
        obs.iter()
            .map(|o| ObsJson::new(o, |s| app.resolve_snapshot(s).is_some_and(|p| p.is_file())))
            .collect(),
    ))
}

async fn snapshot(State(app): State<State_>, AxPath(id): AxPath<i64>) -> Response {
    let stored = {
        let store = app.store();
        match store.snapshot_path(id) {
            Ok(Some(s)) => s,
            _ => return (StatusCode::NOT_FOUND, "no snapshot").into_response(),
        }
    };
    let path = match app.resolve_snapshot(&stored) {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "ref, not file").into_response(),
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "snapshot file missing").into_response(),
    }
}

#[derive(serde::Deserialize)]
struct AskParams {
    q: String,
}

/// NL answering: same path as item-query's CLI (substring keyword -> log ->
/// optional VLM formatting). Returns {answer} or {fallback log} shapes.
async fn ask(State(app): State<State_>, Query(p): Query<AskParams>) -> Response {
    // The same keyword rule the CLI uses, so both answer from the same rows.
    let word = item_query::keyword_of(&p.q);
    let obs = {
        let store = app.store();
        match store.recent(Some(&word), 20) {
            Ok(obs) => obs,
            // A failed read is not "nothing matched": say so, like `list`.
            Err(e) => {
                tracing::warn!(error = %e, "ask: reading observations failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };
    let json_obs: Vec<ObsJson> = obs
        .iter()
        .map(|o| ObsJson::new(o, |s| app.resolve_snapshot(s).is_some_and(|p| p.is_file())))
        .collect();
    // One client (and one connection pool) for the process, built on first
    // ask; `None` means the env vars describing the sidecar are not set.
    let client = app.vlm();
    match client {
        Some(client) => {
            let prompt = item_query::build_prompt(&p.q, &obs);
            match client.ask(&prompt).await {
                Ok(answer) => axum::Json(serde_json::json!({ "mode": "vlm", "answer": answer }))
                    .into_response(),
                Err(e) => axum::Json(serde_json::json!({
                    "mode": "log", "error": e.to_string(), "matched": json_obs,
                }))
                .into_response(),
            }
        }
        None => {
            axum::Json(serde_json::json!({ "mode": "log", "matched": json_obs })).into_response()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    // Strictly read-only: the ingest daemon owns writes, and a fallback to a
    // read-write open would create + migrate a database, or take the write
    // lock, exactly when the daemon is busy with it. A database that is not
    // there yet is the one case we tolerate: serve the empty UI (first run)
    // from an in-memory store instead of creating a file behind the daemon.
    let store = match Store::open_read_only(&args.db) {
        Ok(store) => store,
        Err(_) if !std::path::Path::new(&args.db).exists() => {
            tracing::warn!(
                db = %args.db,
                "no database yet; serving an empty UI (run item-ingest to fill it)"
            );
            Store::in_memory()?
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("opening {} read-only", args.db)));
        }
    };

    let app = Arc::new(App {
        store: Mutex::new(store),
        root: std::env::current_dir()?,
        vlm: std::sync::OnceLock::new(),
    });
    let router = axum::Router::new()
        .route("/", get(index))
        .route("/api/observations", get(list))
        .route("/api/observation/{id}/snapshot", get(snapshot))
        .route("/api/ask", get(ask))
        .with_state(app);

    let addr: std::net::SocketAddr = args.listen.parse().context("bad --listen")?;
    tracing::info!(%addr, db = %args.db, "item-web listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}
