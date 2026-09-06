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

    /// Camera id to attribute --rtsp/--webcam frames to.
    #[cfg(any(feature = "rtsp", feature = "camera"))]
    #[arg(long, default_value = "rtsp-0")]
    camera_id: String,

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
    /// OpenAI-compatible sidecar, requires `--features vlm`).
    #[cfg(any(feature = "yolo", feature = "vlm"))]
    #[arg(long, default_value = "yolo")]
    detector: String,

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

    /// Per-request timeout for the VLM sidecar (a local LLM grounding one
    /// frame can take tens of seconds).
    #[cfg(feature = "vlm")]
    #[arg(long, default_value_t = 60)]
    vlm_timeout: u64,

    /// Coordinate convention to ask of the sidecar: norm1000 (Qwen-VL
    /// convention) or pixel. Replies with any coordinate > 1000 are always
    /// interpreted as pixels.
    #[cfg(feature = "vlm")]
    #[arg(long, default_value = "norm1000")]
    vlm_coords: String,

    /// Comma-separated object vocabulary to ground; empty = open-ended
    /// "list everything visible" mode.
    #[cfg(feature = "vlm")]
    #[arg(long, default_value = item_ingest::detector::vlm::DEFAULT_TARGETS)]
    targets: String,
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

    #[cfg(any(feature = "yolo", feature = "vlm"))]
    if let Some(img) = args.detect.as_deref() {
        return detect_pass(&args, img);
    }

    #[cfg(feature = "rtsp")]
    if let Some(url) = args.rtsp.as_deref() {
        return rtsp_pass(&store, &args, url);
    }

    #[cfg(feature = "camera")]
    if let Some(index) = args.webcam {
        return webcam_pass(&store, &args, index);
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

/// The detector for the camera loop and --detect, per `--detector`:
/// `yolo` (local onnx; default) or `vlm` (open-vocabulary grounding via the
/// OpenAI-compatible sidecar). A missing 'yolo' feature falls back to
/// NullDetector so the pipeline still exercises; a missing 'vlm' feature
/// hard-errors -- a silently null VLM would be indistinguishable from a
/// broken sidecar.
#[cfg(any(feature = "yolo", feature = "vlm"))]
fn build_detector(args: &Args) -> anyhow::Result<Box<dyn Detector>> {
    match args.detector.as_str() {
        "yolo" => {
            #[cfg(feature = "yolo")]
            let det: Box<dyn Detector> = {
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
                Box::new(det)
            };
            #[cfg(not(feature = "yolo"))]
            let det: Box<dyn Detector> = {
                tracing::warn!(
                    "--detector yolo but built without 'yolo' feature: camera loop runs \
                     NullDetector (no observations); rebuild with --features yolo"
                );
                Box::new(NullDetector)
            };
            Ok(det)
        }
        "vlm" => {
            #[cfg(feature = "vlm")]
            let det: Box<dyn Detector> = {
                use item_ingest::detector::vlm::{CoordMode, VlmGroundDetector, parse_targets};

                let base = args
                    .vlm_base_url
                    .clone()
                    .or_else(|| std::env::var("ITEM_VLM_BASE_URL").ok())
                    .unwrap_or_default();
                let model = args
                    .vlm_model
                    .clone()
                    .or_else(|| std::env::var("ITEM_VLM_MODEL").ok())
                    .unwrap_or_default();
                let coords: CoordMode = args
                    .vlm_coords
                    .parse()
                    .map_err(|e: String| anyhow::anyhow!("{e}"))?;
                let targets = parse_targets(&args.targets);
                let det = VlmGroundDetector::new(
                    &base,
                    &model,
                    targets.clone(),
                    std::time::Duration::from_secs(args.vlm_timeout),
                    coords,
                )?;
                tracing::info!(
                    base = %base,
                    model = %model,
                    ?targets,
                    timeout_s = args.vlm_timeout,
                    "vlm grounding detector enabled"
                );
                Box::new(det)
            };
            #[cfg(not(feature = "vlm"))]
            let det: Box<dyn Detector> = anyhow::bail!(
                "--detector vlm requires the 'vlm' feature: rebuild with --features vlm"
            );
            Ok(det)
        }
        other => anyhow::bail!("unknown --detector '{other}' (yolo|vlm)"),
    }
}

/// No detector features at all: the camera loop still exercises the plumbing.
#[cfg(all(
    not(any(feature = "yolo", feature = "vlm")),
    any(feature = "rtsp", feature = "camera")
))]
fn build_detector(_args: &Args) -> anyhow::Result<Box<dyn Detector>> {
    tracing::warn!(
        "built without 'yolo'/'vlm' features: camera loop runs NullDetector \
         (no observations); rebuild with --features yolo or --features vlm"
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

/// Run the selected detector on a single image file (--detect):
/// decode -> detect -> NMS report with timing. `--out` saves an annotated
/// copy (all boxes with label chips; the first one highlighted thick with
/// its white ring, like a freshly born observation snapshot).
#[cfg(any(feature = "yolo", feature = "vlm"))]
fn detect_pass(args: &Args, img_path: &str) -> anyhow::Result<()> {
    use std::time::Instant;

    let img = image::open(img_path)
        .with_context(|| format!("opening {}", img_path))?
        .to_rgb8();
    let (w, h) = img.dimensions();
    let started = Instant::now();
    let det = build_detector(args)?;
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
    if let Some(out) = args.out.as_deref() {
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

/// Detection rate for the camera loop: explicit --detect-fps wins; otherwise
/// 0.2 for the VLM sidecar (one HTTP round trip per detection against a local
/// LLM is slow) and 1.0 for everything else.
#[cfg(any(feature = "rtsp", feature = "camera"))]
fn effective_detect_fps(args: &Args) -> f64 {
    match args.detect_fps {
        Some(fps) => fps,
        None => {
            #[cfg(any(feature = "yolo", feature = "vlm"))]
            let vlm = args.detector == "vlm";
            #[cfg(not(any(feature = "yolo", feature = "vlm")))]
            let vlm = false;
            if vlm { 0.2 } else { 1.0 }
        }
    }
}

/// The shared camera loop behind --rtsp/--webcam: frames are pulled at the
/// source's rate (and cheaply dropped), detection runs at --detect-fps,
/// surviving hits become zone-mapped observations whose first sighting gets
/// an annotated snapshot JPEG. On EOF/source error, restarts the source
/// forever after 2s -- like preview does. Detector errors (e.g. the VLM
/// sidecar being down) only skip the frame, they do not kill the loop.
///
/// Threading: `make_source` and every `next_frame` call happen on THIS
/// thread, which nokhwa's COM-affine camera requires (see source::nokhwa).
#[cfg(any(feature = "rtsp", feature = "camera"))]
fn camera_pump(
    store: &Store,
    args: &Args,
    detector: &dyn Detector,
    kind: &str,
    source_desc: &str,
    make_source: &mut dyn FnMut() -> anyhow::Result<Box<dyn FrameSource>>,
) -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    let snaps = Path::new(&args.snapshots_dir);
    let interval = Duration::from_secs_f64(1.0 / effective_detect_fps(args).max(0.05));
    let mut last_detect = Instant::now() - interval;
    let mut frames = 0u64;
    let mut detected = 0u64;
    loop {
        tracing::info!(source = %source_desc, "opening {kind} source");
        let mut source = match make_source() {
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
                    "ingested {frames} {kind} frames ({detected} detections) from {}",
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
                    tracing::warn!(error = %e, "source error, reconnecting");
                    break;
                }
            };
            frames += 1;
            if last_detect.elapsed() < interval {
                continue; // decode-only frame, throttle detection
            }
            last_detect = Instant::now();
            let dets = match detector.detect(&frame.rgb, frame.meta.width, frame.meta.height) {
                Ok(d) => d,
                Err(e) => {
                    // A downed VLM sidecar (or a transient ort error) must not
                    // kill the loop; drop this frame and back off.
                    tracing::warn!(error = %e, "detector failed, skipping frame");
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            detected += 1;
            let recorded = item_ingest::ingest_detections(
                store,
                &frame.meta,
                &dets,
                0.45,
                0.0, // detector already floors at conf
                Some((&frame.rgb, snaps)),
            )?;
            if recorded > 0 {
                tracing::info!(frames, recorded, "detections ingested");
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Closed loop over an RTSP stream (reconnecting forever).
#[cfg(feature = "rtsp")]
fn rtsp_pass(store: &Store, args: &Args, url: &str) -> anyhow::Result<()> {
    use item_ingest::source::rtsp::RtspSource;

    let detector = build_detector(args)?;
    let mut open = || -> anyhow::Result<Box<dyn FrameSource>> {
        Ok(Box::new(RtspSource::new(&args.camera_id, url)?))
    };
    camera_pump(
        store,
        args,
        detector.as_ref(),
        "rtsp",
        &redact_url(url),
        &mut open,
    )
}

/// Closed loop over a local webcam (nokhwa; no FFmpeg needed). A dead or
/// busy camera surfaces as a source error, so the loop keeps retrying.
#[cfg(feature = "camera")]
fn webcam_pass(store: &Store, args: &Args, index: u32) -> anyhow::Result<()> {
    use item_ingest::source::nokhwa::NokhwaSource;

    let detector = build_detector(args)?;
    let mut open = || -> anyhow::Result<Box<dyn FrameSource>> {
        Ok(Box::new(NokhwaSource::new(args.camera_id.clone(), index)))
    };
    camera_pump(
        store,
        args,
        detector.as_ref(),
        "webcam",
        &format!("#{index}"),
        &mut open,
    )
}
