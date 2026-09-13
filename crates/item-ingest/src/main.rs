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
use item_ingest::config::CliOverrides;
#[cfg(any(
    feature = "yolo",
    feature = "vlm",
    feature = "rtsp",
    feature = "camera"
))]
use item_ingest::config::RuntimeConfig;
use item_ingest::detector::{Detector, NullDetector};
#[cfg(any(
    feature = "yolo",
    feature = "vlm",
    feature = "rtsp",
    feature = "camera"
))]
use item_ingest::runtime::DetectorSpec;
#[cfg(any(feature = "rtsp", feature = "camera"))]
use item_ingest::runtime::{CameraTask, SourceSpec};
use item_ingest::source::{FrameSource, MockSource, SourceError};
#[cfg(any(feature = "rtsp", feature = "camera"))]
use item_ingest::{SharedStore, supervisor};

#[derive(Parser)]
#[command(name = "item-ingest")]
struct Args {
    /// SQLite database path (default: data/items.db).
    #[arg(long)]
    db: Option<String>,

    /// Camera/region config (TOML); regions are seeded into the store at
    /// startup on every mode. See item_ingest::config.
    #[arg(long)]
    config: Option<String>,
    /// Run as a resident daemon: webhook + preview + every enabled camera in
    /// --config, until a stop signal. Requires --config.
    #[arg(long)]
    daemon: bool,

    /// Address for the Frigate webhook server (default: 127.0.0.1:8477).
    #[arg(long)]
    listen: Option<String>,

    /// Directory for observation snapshot JPEGs (default: data/snapshots).
    #[arg(long)]
    snapshots_dir: Option<String>,

    /// Run a mock camera pass (blank frames through the pipeline) and exit.
    #[arg(long)]
    demo: bool,

    /// RTSP url to poll (requires `--features rtsp`), e.g.
    /// rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101
    #[cfg(feature = "rtsp")]
    #[arg(long)]
    rtsp: Option<String>,

    /// Camera id to attribute --rtsp/--webcam frames to (default: rtsp-0).
    #[cfg(any(feature = "rtsp", feature = "camera"))]
    #[arg(long)]
    camera_id: Option<String>,

    /// Max frames to ingest from --rtsp/--webcam before exiting
    /// (0 = run forever; RTSP also stops at stream EOF).
    #[cfg(any(feature = "rtsp", feature = "camera"))]
    #[arg(long, default_value_t = 0)]
    frames: u64,

    /// How often to run detection on the stream (decode/capture runs at
    /// stream rate; only detected frames can create observations/snapshots).
    /// Defaults to 1.0 for yolo/null and 0.2 for vlm (one HTTP round trip
    /// per detection against a local LLM is slow).
    #[cfg(any(feature = "rtsp", feature = "camera"))]
    #[arg(long)]
    detect_fps: Option<f64>,

    /// Built-in/USB webcam index for the same ingest loop (requires
    /// `--features camera`; combine with `--detector yolo|vlm` for real
    /// detections). 0 is the integrated camera on most machines.
    #[cfg(feature = "camera")]
    #[arg(long)]
    webcam: Option<u32>,

    /// Serve a web MJPEG preview of this RTSP url at GET /preview
    /// (requires `--features rtsp`). Credentials in the url are kept out of logs.
    #[cfg(feature = "rtsp")]
    #[arg(long)]
    preview: Option<String>,

    /// Which detection backend to run: `yolo` (local onnx, requires
    /// `--features yolo`) or `vlm` (open-vocabulary grounding via an
    /// OpenAI-compatible sidecar, requires `--features vlm`). Default: yolo.
    #[cfg(any(feature = "yolo", feature = "vlm"))]
    #[arg(long)]
    detector: Option<String>,

    /// Run the selected detector on one JPEG/PNG image and exit
    /// (requires `--features yolo` or `vlm`). Prints detections + timing.
    #[cfg(any(feature = "yolo", feature = "vlm"))]
    #[arg(long)]
    detect: Option<String>,

    /// With --detect: save an annotated copy (boxes + label chips, first box
    /// highlighted like an observation snapshot) to this path. Format follows
    /// the file extension.
    #[cfg(any(feature = "yolo", feature = "vlm"))]
    #[arg(long)]
    out: Option<String>,

    /// Model path for YOLO detection (default: models/yolov8n.onnx).
    #[cfg(feature = "yolo")]
    #[arg(long)]
    model: Option<String>,

    /// Input tensor size the ONNX graph was exported at (default: 640).
    #[cfg(feature = "yolo")]
    #[arg(long)]
    input_size: Option<usize>,

    /// Confidence floor, both --detect and the camera loop (default: 0.3).
    #[cfg(feature = "yolo")]
    #[arg(long)]
    conf: Option<f32>,

    /// Base URL of the OpenAI-compatible VLM sidecar, including the /v1
    /// prefix (e.g. http://127.0.0.1:8080/v1). Falls back to
    /// ITEM_VLM_BASE_URL -- the same variable item-web's ask bar uses.
    #[cfg(feature = "vlm")]
    #[arg(long)]
    vlm_base_url: Option<String>,

    /// Model name the sidecar serves. Falls back to ITEM_VLM_MODEL.
    #[cfg(feature = "vlm")]
    #[arg(long)]
    vlm_model: Option<String>,

    /// Per-request timeout for the VLM sidecar, in seconds (default: 60) --
    /// a local LLM grounding one frame can take tens of seconds.
    #[cfg(feature = "vlm")]
    #[arg(long)]
    vlm_timeout: Option<u64>,

    /// Coordinate convention to ask of the sidecar: norm1000 (Qwen-VL
    /// convention) or pixel (default: norm1000). Any coordinate > 1000 in a
    /// reply is always interpreted as pixels.
    #[cfg(feature = "vlm")]
    #[arg(long)]
    vlm_coords: Option<String>,

    /// Comma-separated object vocabulary to ground; empty = open-ended
    /// "list everything visible" mode (default: the built-in vocabulary).
    #[cfg(feature = "vlm")]
    #[arg(long)]
    targets: Option<String>,
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

    // Four-layer merge: built-in defaults -> config.toml -> environment -> CLI.
    let file_config = match args.config.as_deref() {
        Some(path) => Some(item_ingest::config::Config::load(Path::new(path))?),
        None => None,
    };
    let settings = item_ingest::config::RuntimeConfig::resolve(
        file_config.as_ref(),
        &cli_overrides(&args),
        &item_ingest::config::EnvOverrides::from_env(),
    );
    // Log the resolved shape, never the values that carry credentials.
    tracing::debug!(
        db = %settings.db,
        listen = %settings.listen,
        camera_id = %settings.camera_id,
        detector = %settings.detector,
        detect_fps = settings.detect_fps(),
        "resolved runtime settings"
    );

    // Resident mode owns the store, so it has to run before anything here opens
    // the database: the single-instance lock is taken first (§1).
    if args.daemon {
        let file = file_config
            .as_ref()
            .context("--daemon requires --config <file>")?;
        let mut settings = settings;
        // Relative paths resolve against the config file's directory, not the
        // cwd a service manager happens to pick (§3).
        settings.absolutize(&config_dir(&args));
        return item_ingest::daemon::run(settings, file);
    }

    if let Some(dir) = Path::new(&settings.db).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let store = Store::open(&settings.db).context("opening sqlite store")?;

    if let Some(file) = file_config.as_ref() {
        let n = file.seed_regions(&store)?;
        tracing::info!(
            cameras = file.camera.len(),
            regions = n,
            "config regions seeded"
        );
    }

    if args.demo {
        return demo_pass(&store);
    }

    #[cfg(any(feature = "yolo", feature = "vlm"))]
    if let Some(img) = args.detect.as_deref() {
        return detect_pass(&settings, img, args.out.as_deref());
    }

    #[cfg(feature = "rtsp")]
    if let Some(url) = args.rtsp.as_deref() {
        return rtsp_pass(store, &settings, url, args.frames);
    }

    #[cfg(feature = "camera")]
    if let Some(index) = args.webcam {
        return webcam_pass(store, &settings, index, args.frames);
    }

    let state: item_ingest::frigate::State = Arc::new(Mutex::new(store));
    let app = item_ingest::frigate::router(state);

    // Shadow (not mut): with `rtsp` off there is nothing to merge.
    #[cfg(feature = "rtsp")]
    let app = match settings.preview_url.clone() {
        Some(url) => {
            // Credentials are part of the url; log only scheme+host.
            tracing::info!(
                target_url = item_ingest::runtime::redact_url(&url),
                "web preview enabled at GET /preview"
            );
            app.merge(item_ingest::preview::router(
                item_ingest::preview::spawn_streamer(url),
            ))
        }
        None => app,
    };

    let addr: SocketAddr = settings.listen.parse().context("bad --listen")?;
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

/// The directory that relative paths in config.toml resolve against: the
/// config file's own directory, or the cwd when there is no config file.
fn config_dir(args: &Args) -> std::path::PathBuf {
    args.config
        .as_deref()
        .map(Path::new)
        .and_then(Path::parent)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf()))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// The CLI layer of the merge: only the values the user actually passed.
/// `None` means "not given", which is what lets config.toml beat a default.
fn cli_overrides(args: &Args) -> CliOverrides {
    #[allow(unused_mut)]
    let mut o = CliOverrides {
        db: args.db.clone(),
        snapshots_dir: args.snapshots_dir.clone(),
        listen: args.listen.clone(),
        ..Default::default()
    };
    #[cfg(any(feature = "rtsp", feature = "camera"))]
    {
        o.camera_id = args.camera_id.clone();
        o.detect_fps = args.detect_fps;
    }
    #[cfg(any(feature = "yolo", feature = "vlm"))]
    {
        o.detector = args.detector.clone();
    }
    #[cfg(feature = "yolo")]
    {
        o.model = args.model.clone();
        o.input_size = args.input_size;
        o.conf = args.conf;
    }
    #[cfg(feature = "vlm")]
    {
        o.vlm_base_url = args.vlm_base_url.clone();
        o.vlm_model = args.vlm_model.clone();
        o.vlm_timeout = args.vlm_timeout;
        o.vlm_coords = args.vlm_coords.clone();
        o.targets = args.targets.clone();
    }
    #[cfg(feature = "rtsp")]
    {
        o.preview_url = args.preview.clone();
    }
    o
}

/// The detector this invocation asks for, as data: `yolo` (local onnx;
/// default) or `vlm` (open-vocabulary grounding via the OpenAI-compatible
/// sidecar). Turning the spec into a live detector is `runtime`'s job, so a
/// missing 'yolo' feature degrades to NullDetector while a missing 'vlm'
/// feature hard-errors -- a silently null VLM would be indistinguishable from
/// a broken sidecar.
#[cfg(any(feature = "yolo", feature = "vlm"))]
fn detector_spec(settings: &RuntimeConfig) -> anyhow::Result<DetectorSpec> {
    Ok(match settings.detector.as_str() {
        "yolo" => yolo_spec(settings),
        "vlm" => vlm_spec(settings)?,
        other => anyhow::bail!("unknown --detector '{other}' (yolo|vlm)"),
    })
}

#[cfg(feature = "vlm")]
fn vlm_spec(settings: &RuntimeConfig) -> anyhow::Result<DetectorSpec> {
    Ok(DetectorSpec::Vlm {
        base_url: settings.vlm_base_url.clone().unwrap_or_default(),
        model: settings.vlm_model.clone().unwrap_or_default(),
        targets: item_ingest::detector::vlm::parse_targets(&settings.targets),
        timeout: std::time::Duration::from_secs(settings.vlm_timeout),
        coords: settings.vlm_coords.clone(),
    })
}

/// `--detector vlm` with the feature off: a hard error, because a silently
/// null VLM would be indistinguishable from a broken sidecar.
#[cfg(all(any(feature = "yolo", feature = "vlm"), not(feature = "vlm")))]
fn vlm_spec(_settings: &RuntimeConfig) -> anyhow::Result<DetectorSpec> {
    anyhow::bail!("detector 'vlm' requires the 'vlm' feature: rebuild with --features vlm")
}

#[cfg(feature = "yolo")]
fn yolo_spec(settings: &RuntimeConfig) -> DetectorSpec {
    DetectorSpec::Yolo {
        model: Path::new(&settings.model).to_path_buf(),
        labels: item_ingest::detector::COCO_LABELS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        input_size: settings.input_size,
        conf: settings.conf,
    }
}

/// `--detector yolo` with the feature off: say so once, then run a null
/// detector so the plumbing still exercises (no observations).
#[cfg(all(any(feature = "yolo", feature = "vlm"), not(feature = "yolo")))]
fn yolo_spec(_settings: &RuntimeConfig) -> DetectorSpec {
    tracing::warn!(
        "--detector yolo but built without 'yolo' feature: camera loop runs \
         NullDetector (no observations); rebuild with --features yolo"
    );
    DetectorSpec::Null
}

/// No detector features at all: the camera loop still exercises the plumbing.
#[cfg(all(
    not(any(feature = "yolo", feature = "vlm")),
    any(feature = "rtsp", feature = "camera")
))]
fn detector_spec(_settings: &RuntimeConfig) -> anyhow::Result<DetectorSpec> {
    tracing::warn!(
        "built without 'yolo'/'vlm' features: camera loop runs NullDetector \
         (no observations); rebuild with --features yolo or --features vlm"
    );
    Ok(DetectorSpec::Null)
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

/// Run the selected detector on a single image file (--detect):
/// decode -> detect -> NMS report with timing. `--out` saves an annotated
/// copy (all boxes with label chips; the first one highlighted thick with
/// its white ring, like a freshly born observation snapshot).
#[cfg(any(feature = "yolo", feature = "vlm"))]
fn detect_pass(settings: &RuntimeConfig, img_path: &str, out: Option<&str>) -> anyhow::Result<()> {
    use std::time::Instant;

    let img = image::open(img_path)
        .with_context(|| format!("opening {}", img_path))?
        .to_rgb8();
    let (w, h) = img.dimensions();
    let started = Instant::now();
    let det = item_ingest::runtime::build_detector(&detector_spec(settings)?)?;
    let load_ms = started.elapsed();
    let raw = det
        .detect(img.as_raw(), w, h)
        .map_err(|e| anyhow::anyhow!("inference: {e}"))?;
    let infer_ms = started.elapsed();
    let kept: Vec<item_core::Detection> = item_core::geo::nms(&raw, 0.45)
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
    if let Some(out) = out {
        let refs: Vec<&item_core::Detection> = kept.iter().collect();
        let highlight = (!refs.is_empty()).then_some(0);
        match item_ingest::annotate::annotate(img.as_raw(), w, h, &refs, &[], highlight) {
            Some(annotated) => {
                annotated
                    .save(out)
                    .with_context(|| format!("saving {out}"))?;
                println!("annotated copy saved to {out}");
            }
            None => tracing::warn!("annotation failed (buffer mismatch); no --out written"),
        }
    }
    Ok(())
}

/// Shared tail for the single-camera CLI paths (`--rtsp` / `--webcam`).
///
/// Each one builds a [`CameraTask`] and hands it to the [`supervisor`], so the
/// loop, the frame budget and the retry policy live in one place. `--frames N`
/// still reports the same summary line it always did.
#[cfg(any(feature = "rtsp", feature = "camera"))]
fn run_single_camera(store: Store, task: CameraTask) {
    let kind = task.source.as_ref().map(|s| s.kind()).unwrap_or("camera");
    let camera_id = task.camera_id.clone();
    let shared: SharedStore = Arc::new(Mutex::new(store));
    let outcome = supervisor::run_one(&shared, task);
    if outcome.finished {
        println!(
            "ingested {} {kind} frames ({} detections) from {camera_id}",
            outcome.frames, outcome.detections
        );
    }
}

/// The camera task described by the resolved settings plus a local source.
#[cfg(any(feature = "rtsp", feature = "camera"))]
fn camera_task(
    settings: &RuntimeConfig,
    source: SourceSpec,
    max_frames: u64,
) -> anyhow::Result<CameraTask> {
    Ok(CameraTask {
        camera_id: settings.camera_id.clone(),
        source: Some(source),
        detector: detector_spec(settings)?,
        detect_fps: settings.detect_fps(),
        snapshot_dir: Path::new(&settings.snapshots_dir).to_path_buf(),
        max_frames,
        enabled: true,
    })
}

/// Closed loop over an RTSP stream (reconnecting forever).
#[cfg(feature = "rtsp")]
fn rtsp_pass(
    store: Store,
    settings: &RuntimeConfig,
    url: &str,
    max_frames: u64,
) -> anyhow::Result<()> {
    let source = SourceSpec::Rtsp {
        url: url.to_string(),
    };
    let task = camera_task(settings, source, max_frames)?;
    run_single_camera(store, task);
    Ok(())
}

/// Closed loop over a local webcam (nokhwa; no FFmpeg needed). A dead or
/// busy camera surfaces as a source error, so the loop keeps retrying.
#[cfg(feature = "camera")]
fn webcam_pass(
    store: Store,
    settings: &RuntimeConfig,
    index: u32,
    max_frames: u64,
) -> anyhow::Result<()> {
    let task = camera_task(settings, SourceSpec::Webcam { index }, max_frames)?;
    run_single_camera(store, task);
    Ok(())
}
