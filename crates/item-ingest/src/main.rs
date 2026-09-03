//! item-ingest daemon: receives Frigate webhooks, and (with features) runs a
//! camera loop -- RTSP pull -> throttled YOLO detect -> zone-mapped
//! observations + representative snapshots. All paths converge on
//! item_ingest::ingest_detections / Store::record_sighting.

use std::net::SocketAddr;
use std::path::Path;
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

    /// Camera/region config (TOML); regions are seeded into the store at
    /// startup on every mode. See item_ingest::config.
    #[arg(long)]
    config: Option<String>,

    /// Address for the Frigate webhook server.
    #[arg(long, default_value = "127.0.0.1:8477")]
    listen: String,

    /// Directory for observation snapshot JPEGs.
    #[arg(long, default_value = "data/snapshots")]
    snapshots_dir: String,

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

    /// How often to run detection on the stream (decode runs at stream rate;
    /// only detected frames can create observations/snapshots).
    #[cfg(feature = "rtsp")]
    #[arg(long, default_value_t = 1.0)]
    detect_fps: f64,

    /// Serve a web MJPEG preview of this RTSP url at GET /preview
    /// (requires `--features rtsp`). Credentials in the url are kept out of logs.
    #[cfg(feature = "rtsp")]
    #[arg(long)]
    preview: Option<String>,

    /// Run the YOLO-onnx detector on one JPEG/PNG image and exit
    /// (requires `--features yolo`). Prints detections + timing.
    #[cfg(feature = "yolo")]
    #[arg(long)]
    detect: Option<String>,

    /// Model path for detection (yolov8n/yolo11n export, dynamic or 640 input).
    #[cfg(feature = "yolo")]
    #[arg(long, default_value = "models/yolov8n.onnx")]
    model: String,

    /// Input tensor size the ONNX graph was exported at.
    #[cfg(feature = "yolo")]
    #[arg(long, default_value_t = 640)]
    input_size: usize,

    /// Confidence floor for detection (both --detect and the camera loop).
    #[cfg(feature = "yolo")]
    #[arg(long, default_value_t = 0.3)]
    conf: f32,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        // stdout is reserved for command output (e.g. --detect results);
        // logs (including the ort bridge, which is noisy) go to stderr.
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    if let Some(dir) = Path::new(&args.db).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let store = Store::open(&args.db).context("opening sqlite store")?;

    if let Some(path) = args.config.as_deref() {
        let cfg = item_ingest::config::Config::load(Path::new(path))?;
        let n = cfg.seed_regions(&store)?;
        tracing::info!(
            cameras = cfg.camera.len(),
            regions = n,
            "config regions seeded"
        );
    }

    if args.demo {
        return demo_pass(&store);
    }

    #[cfg(feature = "yolo")]
    if let Some(img) = args.detect.as_deref() {
        return detect_pass(img, &args.model, args.input_size, args.conf);
    }

    #[cfg(feature = "rtsp")]
    if let Some(url) = args.rtsp.as_deref() {
        return rtsp_pass(&store, &args, url);
    }

    let state: item_ingest::frigate::State = Arc::new(Mutex::new(store));
    let app = item_ingest::frigate::router(state);

    // Shadow (not mut): with `rtsp` off there is nothing to merge.
    #[cfg(feature = "rtsp")]
    let app = match args.preview.clone() {
        Some(url) => {
            // Credentials are part of the url; log only scheme+host.
            tracing::info!(
                target_url = redact_url(&url),
                "web preview enabled at GET /preview"
            );
            app.merge(item_ingest::preview::router(
                item_ingest::preview::spawn_streamer(url),
            ))
        }
        None => app,
    };

    let addr: SocketAddr = args.listen.parse().context("bad --listen")?;
    tracing::info!(%addr, "frigate webhook server listening");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        anyhow::Ok(())
    })
}

/// The detector for the camera loop: real YOLO when built with `yolo`,
/// otherwise Null (pipeline still exercises, but records nothing).
#[cfg(all(feature = "rtsp", feature = "yolo"))]
fn build_detector(args: &Args) -> anyhow::Result<Box<dyn Detector>> {
    use item_ingest::detector::COCO_LABELS;
    use item_ingest::detector::yolo::YoloDetector;

    let det = YoloDetector::new(
        Path::new(&args.model),
        COCO_LABELS.iter().map(|s| s.to_string()).collect(),
        args.input_size,
        args.conf,
    )
    .map_err(|e| anyhow::anyhow!("model load: {e}"))?;
    tracing::info!(model = %args.model, conf = args.conf, "yolo detector enabled");
    Ok(Box::new(det))
}

#[cfg(all(feature = "rtsp", not(feature = "yolo")))]
fn build_detector(_args: &Args) -> anyhow::Result<Box<dyn Detector>> {
    tracing::warn!(
        "built without 'yolo' feature: camera loop runs NullDetector (no observations); \
         rebuild with --features yolo"
    );
    Ok(Box::new(NullDetector))
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
        let n = item_ingest::ingest_detections(store, &frame.meta, &dets, 0.5, 0.25, None)?;
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
    println!(
        "demo observation: {:?}",
        obs.first().map(|o| (&o.zone, &o.label))
    );
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

/// Run YOLO-onnx on a single image file (yolo verification / smoke test):
/// decode -> detect -> NMS report with timing.
#[cfg(feature = "yolo")]
fn detect_pass(
    img_path: &str,
    model_path: &str,
    input_size: usize,
    conf: f32,
) -> anyhow::Result<()> {
    use std::time::Instant;

    use item_ingest::detector::COCO_LABELS;
    use item_ingest::detector::yolo::YoloDetector;

    let img = image::open(img_path)
        .with_context(|| format!("opening {}", img_path))?
        .to_rgb8();
    let (w, h) = img.dimensions();
    let started = Instant::now();
    let det = YoloDetector::new(
        Path::new(model_path),
        COCO_LABELS.iter().map(|s| s.to_string()).collect(),
        input_size,
        conf,
    )
    .map_err(|e| anyhow::anyhow!("model load: {e}"))?;
    let load_ms = started.elapsed();
    let raw = det
        .detect(img.as_raw(), w, h)
        .map_err(|e| anyhow::anyhow!("inference: {e}"))?;
    let infer_ms = started.elapsed();
    let kept: Vec<_> = item_core::geo::nms(&raw, 0.45)
        .into_iter()
        .map(|i| raw[i].clone())
        .collect();
    println!(
        "load {:.0}ms | inference {:.0}ms | {} raw -> {} kept",
        load_ms.as_secs_f64() * 1e3,
        (infer_ms - load_ms).as_secs_f64() * 1e3,
        raw.len(),
        kept.len()
    );
    for d in &kept {
        println!(
            "  {:<14} {:>5.0}%  [{:.0}, {:.0}, {:.0}, {:.0}]",
            d.label,
            d.confidence * 100.0,
            d.bbox[0],
            d.bbox[1],
            d.bbox[2],
            d.bbox[3]
        );
    }
    Ok(())
}

/// The closed loop: RTSP frames are decoded continuously (and cheaply
/// dropped), detection runs at --detect-fps, surviving hits become zone-
/// mapped observations whose first sighting gets a snapshot JPEG. Reconnects
/// forever, like preview does.
#[cfg(feature = "rtsp")]
fn rtsp_pass(store: &Store, args: &Args, url: &str) -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use item_ingest::source::rtsp::RtspSource;

    let detector = build_detector(args)?;
    let snaps = std::path::PathBuf::from(&args.snapshots_dir);
    let interval = Duration::from_secs_f64(1.0 / args.detect_fps.max(0.05));
    let mut last_detect = Instant::now() - interval;
    let mut frames = 0u64;
    let mut detected = 0u64;
    loop {
        tracing::info!(target_url = redact_url(url), "opening rtsp stream");
        let mut source = match RtspSource::new(&args.camera_id, url) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "connect failed, retrying in 2s");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        loop {
            if args.frames > 0 && frames >= args.frames {
                println!(
                    "ingested {frames} rtsp frames ({detected} detections) from {}",
                    args.camera_id
                );
                return Ok(());
            }
            let frame = match source.next_frame() {
                Ok(f) => f,
                Err(SourceError::Eof) => {
                    tracing::info!("stream ended, reconnecting");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "stream error, reconnecting");
                    break;
                }
            };
            frames += 1;
            if last_detect.elapsed() < interval {
                continue; // decode-only frame, throttle detection
            }
            last_detect = Instant::now();
            let dets = detector
                .detect(&frame.rgb, frame.meta.width, frame.meta.height)
                .map_err(|e| anyhow::anyhow!("inference: {e}"))?;
            detected += 1;
            let recorded = item_ingest::ingest_detections(
                store,
                &frame.meta,
                &dets,
                0.45,
                0.0, // detector already floors at conf
                Some((&frame.rgb, snaps.as_path())),
            )?;
            if recorded > 0 {
                tracing::info!(frames, recorded, "detections ingested");
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
