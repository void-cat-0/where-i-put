//! item-ingest daemon: receives Frigate webhooks and (with features) polls a
//! local camera through MockSource/YoloDetector. Both paths converge on
//! item_ingest::ingest_detections / Store::record_sighting.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Parser;

use item_core::store::Store;
use item_ingest::detector::{Detector, NullDetector};
use item_ingest::source::{FrameSource, MockSource, SourceError};

#[derive(Parser)]
#[command(name = "item-ingest")]
struct Args {
    /// SQLite database path.
    #[arg(long, default_value = "data/items.db")]
    db: String,

    /// Address for the Frigate webhook server.
    #[arg(long, default_value = "127.0.0.1:8477")]
    listen: String,

    /// Run a mock camera pass (blank frames through the pipeline) and exit.
    #[arg(long)]
    demo: bool,

    /// RTSP url to poll (requires `--features rtsp`), e.g.
    /// rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101
    #[cfg(feature = "rtsp")]
    #[arg(long)]
    rtsp: Option<String>,

    /// Camera id to attribute --rtsp frames to.
    #[cfg(feature = "rtsp")]
    #[arg(long, default_value = "rtsp-0")]
    camera_id: String,

    /// Max frames to ingest from --rtsp before exiting (0 = run until EOF).
    #[cfg(feature = "rtsp")]
    #[arg(long, default_value_t = 0)]
    frames: u64,

    /// Serve a web MJPEG preview of this RTSP url at GET /preview
    /// (requires `--features rtsp`). Credentials in the url are kept out of logs.
    #[cfg(feature = "rtsp")]
    #[arg(long)]
    preview: Option<String>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    if let Some(dir) = std::path::Path::new(&args.db).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let store = Store::open(&args.db).context("opening sqlite store")?;

    if args.demo {
        return demo_pass(&store);
    }

    #[cfg(feature = "rtsp")]
    if let Some(url) = args.rtsp.as_deref() {
        return rtsp_pass(&store, &args.camera_id, url, args.frames);
    }

    let state: item_ingest::frigate::State = Arc::new(Mutex::new(store));
    let app = item_ingest::frigate::router(state);

    // Shadow (not mut): with `rtsp` off there is nothing to merge.
    #[cfg(feature = "rtsp")]
    let app = match args.preview.clone() {
        Some(url) => {
            // Credentials are part of the url; log only scheme+host.
            tracing::info!(target_url = redact_url(&url), "web preview enabled at GET /preview");
            app.merge(item_ingest::preview::router(item_ingest::preview::spawn_streamer(url)))
        }
        None => app,
    };

    let addr: SocketAddr = args.listen.parse().context("bad --listen")?;
    tracing::info!(%addr, "frigate webhook server listening");

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        anyhow::Ok(())
    })
}

/// End-to-end smoke of the local pipeline without hardware or Frigate:
/// blank frames -> NullDetector -> (zero sightings) -> store stays writable.
fn demo_pass(store: &Store) -> anyhow::Result<()> {
    let mut source: Box<dyn FrameSource> = Box::new(MockSource::new("demo-cam", 3, (640, 480)));
    let detector = NullDetector;
    loop {
        let frame = match source.next_frame() {
            Ok(f) => f,
            Err(SourceError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let dets = detector.detect(&frame.rgb, frame.meta.width, frame.meta.height)?;
        let n = item_ingest::ingest_detections(store, &frame.meta, &dets, 0.5, 0.25)?;
        tracing::info!(recorded = n, "demo frame processed");
    }
    // Prove the zone mapping and dedup round-trip.
    store.upsert_region("demo-cam", "desk", [100.0, 100.0, 300.0, 300.0])?;
    store.record_sighting(
        "demo-cam",
        &store.zone_for_point("demo-cam", (150.0, 150.0))?,
        "keys",
        chrono::Utc::now(),
        None,
        item_core::store::DEFAULT_DEDUP_WINDOW,
    )?;
    let obs = store.recent(Some("keys"), 5)?;
    println!("demo observation: {:?}", obs.first().map(|o| (&o.zone, &o.label)));
    Ok(())
}

/// Strip credentials from an RTSP url for safe logging
/// (rtsp://user:pass@host/path -> rtsp://host/path).
#[cfg(feature = "rtsp")]
fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.rsplit_once('@') {
            Some((_, host_path)) => format!("{scheme}://{host_path}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// Pull N frames from an RTSP camera through the NullDetector plumbing and log
/// them; proves decode -> Frame -> (empty detections) -> store. The real
/// detector swaps in once the `yolo` feature is verified against ort 2.0.
#[cfg(feature = "rtsp")]
fn rtsp_pass(store: &Store, camera_id: &str, url: &str, max_frames: u64) -> anyhow::Result<()> {
    use item_ingest::source::rtsp::RtspSource;

    let mut source = RtspSource::new(camera_id, url)
        .map_err(|e| anyhow::anyhow!("rtsp connect failed: {e}"))?;
    let detector = NullDetector;
    let mut n = 0u64;
    loop {
        if max_frames > 0 && n >= max_frames {
            break;
        }
        let frame = match source.next_frame() {
            Ok(f) => f,
            Err(SourceError::Eof) => {
                tracing::info!("rtsp stream ended");
                break;
            }
            Err(e) => return Err(anyhow::anyhow!("rtsp read failed after {n} frames: {e}")),
        };
        let dets = detector.detect(&frame.rgb, frame.meta.width, frame.meta.height)?;
        item_ingest::ingest_detections(store, &frame.meta, &dets, 0.5, 0.25)?;
        n += 1;
        if n % 30 == 1 {
            tracing::info!(
                frames = n,
                w = frame.meta.width,
                h = frame.meta.height,
                bytes = frame.rgb.len(),
                "rtsp frames flowing"
            );
        }
    }
    println!("ingested {n} rtsp frames from {camera_id}");
    Ok(())
}
