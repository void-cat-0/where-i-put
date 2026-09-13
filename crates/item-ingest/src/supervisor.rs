//! `Supervisor`: one OS thread per camera, plus the graceful-stop flag
//! (docs/resident-ingest.md §2/§3).
//!
//! It does exactly three things: start threads, watch the stop flag, and reopen
//! a camera's source after a backoff. A panicking or erroring camera never
//! escapes into the process: `step()` classifies its errors and returns, and a
//! genuinely fatal condition (e.g. a model that cannot be loaded) marks that
//! camera `failed` and stops retrying instead of spamming the log.
//!
//! Scope note (P0): the backoff is the historical fixed 2s. Exponential backoff
//! with a 60s ceiling is P2 and the event checkpoints that hook into the same
//! `step()` boundary are P4 (docs/resident-ingest.md §10). The store lock is
//! taken coarsely, once per `step()`; moving annotation + JPEG encoding out of
//! it (§3) needs `ingest_detections` to take the shared handle, which lands
//! with the daemon in P1.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::SharedStore;
use crate::runner::{CameraRunner, StepOutcome};
use crate::runtime::{self, CameraTask};
use crate::source::SourceError;

/// Fixed wait before reopening a source or after a detector failure.
pub const BACKOFF: Duration = Duration::from_secs(2);

/// How often the backoff sleep re-checks the stop flag.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Where one camera ended up, so the caller can log or report it.
#[derive(Debug, Clone)]
pub struct CameraOutcome {
    pub camera_id: String,
    pub frames: u64,
    pub detections: u64,
    pub recorded: u64,
    pub reconnects: u64,
    /// The frame budget was reached (as opposed to stopping on request).
    pub finished: bool,
    /// Set when the camera gave up for a reason a retry cannot fix.
    pub error: Option<String>,
}

impl CameraOutcome {
    fn for_camera(camera_id: impl Into<String>) -> Self {
        Self {
            camera_id: camera_id.into(),
            frames: 0,
            detections: 0,
            recorded: 0,
            reconnects: 0,
            finished: false,
            error: None,
        }
    }
}

/// Owns the camera threads and the graceful-stop flag.
pub struct Supervisor {
    stop: Arc<AtomicBool>,
}

impl Supervisor {
    pub fn new(stop: Arc<AtomicBool>) -> Self {
        Self { stop }
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Run every enabled camera that has a local source, one thread each, and
    /// join them all before returning.
    ///
    /// Webhook-fed cameras (`source == None`) get no thread: their frames come
    /// from the HTTP path, and trying to "open" them would spin forever.
    pub fn run(&self, store: &SharedStore, tasks: Vec<CameraTask>) -> Vec<CameraOutcome> {
        let mut outcomes = Vec::new();
        let mut handles: Vec<JoinHandle<CameraOutcome>> = Vec::new();

        for task in tasks
            .into_iter()
            .filter(|t| t.enabled && t.source.is_some())
        {
            let camera_id = task.camera_id.clone();
            let store = Arc::clone(store);
            let stop = Arc::clone(&self.stop);
            match thread::Builder::new()
                .name(format!("camera-{camera_id}"))
                .spawn(move || drive(store, task, stop))
            {
                Ok(handle) => handles.push(handle),
                Err(e) => {
                    let mut outcome = CameraOutcome::for_camera(camera_id);
                    outcome.error = Some(format!("could not spawn camera thread: {e}"));
                    outcomes.push(outcome);
                }
            }
        }

        // A panicking camera must not take the others down with it.
        for handle in handles {
            match handle.join() {
                Ok(outcome) => outcomes.push(outcome),
                Err(_) => tracing::error!("camera thread panicked; continuing with the rest"),
            }
        }
        outcomes
    }
}

/// Convenience for the single-camera CLI paths.
pub fn run_one(store: &SharedStore, task: CameraTask) -> CameraOutcome {
    let supervisor = Supervisor::new(Arc::new(AtomicBool::new(false)));
    supervisor
        .run(store, vec![task])
        .pop()
        .unwrap_or_else(|| CameraOutcome::for_camera("unknown"))
}

/// The whole life of one camera, on its own thread.
fn drive(store: SharedStore, task: CameraTask, stop: Arc<AtomicBool>) -> CameraOutcome {
    let mut outcome = CameraOutcome::for_camera(task.camera_id.clone());

    // Fatal on purpose: a missing model file or a missing cargo feature will not
    // fix itself, so say so once instead of retrying forever.
    let detector = match runtime::build_detector(&task.detector) {
        Ok(d) => d,
        Err(e) => {
            let reason = format!("{e:#}");
            tracing::error!(camera = %task.camera_id, error = %reason, "camera failed to start; not retrying");
            outcome.error = Some(reason);
            return outcome;
        }
    };

    let kind = task.source.as_ref().map(|s| s.kind()).unwrap_or("webhook");
    let source_desc = task
        .source
        .as_ref()
        .map(|s| s.describe())
        .unwrap_or_default();

    let mut runner = CameraRunner::new(task, detector);

    loop {
        if stop.load(Ordering::SeqCst) {
            runner.mark_stopped();
            break;
        }

        if !runner.is_open() {
            tracing::info!(source = %source_desc, "opening {kind} source");
            if let Err(e) = runner.open() {
                tracing::warn!(error = %e, "connect failed, retrying in 2s");
                if sleep_backoff(&stop) {
                    runner.mark_stopped();
                    break;
                }
                continue;
            }
        }

        let stepped = {
            let store = store.lock().expect("store mutex poisoned");
            runner.step(&store)
        };

        match stepped {
            Ok(StepOutcome::Finished) => {
                outcome.finished = true;
                break;
            }
            Ok(StepOutcome::Lost(reason)) => {
                match &reason {
                    Some(SourceError::Eof) => tracing::info!("stream ended, reconnecting"),
                    Some(e) => tracing::warn!(error = %e, "source error, reconnecting"),
                    None => tracing::warn!("no source open, reconnecting"),
                }
                if sleep_backoff(&stop) {
                    runner.mark_stopped();
                    break;
                }
            }
            Ok(StepOutcome::DetectorFailed) => {
                // The runner already warned; just back off before retrying.
                if sleep_backoff(&stop) {
                    runner.mark_stopped();
                    break;
                }
            }
            Ok(StepOutcome::Detected { .. }) | Ok(StepOutcome::Throttled) => {}
            Err(e) => {
                // Store/IO errors are not self-healing: stop this camera and
                // let the service manager or the user deal with it.
                let reason = format!("{e:#}");
                tracing::error!(camera = %runner.task().camera_id, error = %reason, "camera step failed; stopping this camera");
                runner.mark_failed(&reason);
                outcome.error = Some(reason);
                break;
            }
        }
    }

    let health = runner.health();
    outcome.frames = health.frames;
    outcome.detections = health.detections;
    outcome.recorded = health.recorded;
    outcome.reconnects = health.reconnects;
    outcome
}

/// Sleep [`BACKOFF`], returning early (and reporting `true`) when a stop is
/// requested. A plain `thread::sleep` here would make Ctrl-C wait up to 2s.
fn sleep_backoff(stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + BACKOFF;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(STOP_POLL);
    }
    stop.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{DetectorSpec, SourceSpec};
    use item_core::store::Store;
    use std::sync::Mutex;

    fn mock_task(id: &str, frames: u32, max_frames: u64) -> CameraTask {
        CameraTask {
            camera_id: id.into(),
            source: Some(SourceSpec::Mock {
                frames,
                width: 8,
                height: 8,
            }),
            detector: DetectorSpec::Null,
            detect_fps: 1.0,
            snapshot_dir: std::path::PathBuf::from("unused"),
            max_frames,
            enabled: true,
        }
    }

    fn store() -> SharedStore {
        Arc::new(Mutex::new(Store::in_memory().unwrap()))
    }

    #[test]
    fn a_mock_camera_runs_to_its_frame_budget() {
        let store = store();
        let outcome = run_one(&store, mock_task("cam", 10, 3));
        assert!(outcome.finished, "budget reached: {outcome:?}");
        assert_eq!(outcome.frames, 3);
        assert_eq!(
            outcome.detections, 1,
            "1 fps throttle: only the first frame detects"
        );
        assert_eq!(outcome.error, None);
    }

    #[test]
    fn webhook_only_cameras_get_no_thread() {
        let store = store();
        let mut webhook_task = mock_task("fed-by-frigate", 1, 1);
        webhook_task.source = None;
        let supervisor = Supervisor::new(Arc::new(AtomicBool::new(false)));
        let outcomes = supervisor.run(&store, vec![webhook_task]);
        assert!(
            outcomes.is_empty(),
            "a camera with no local source must not be driven: {outcomes:?}"
        );
    }

    #[test]
    fn a_stop_request_is_honoured_without_touching_the_camera() {
        let store = store();
        let stop = Arc::new(AtomicBool::new(true));
        let supervisor = Supervisor::new(Arc::clone(&stop));
        let outcomes = supervisor.run(&store, vec![mock_task("cam", 10, 0)]);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].frames, 0);
        assert!(!outcomes[0].finished);
        assert_eq!(outcomes[0].error, None);
    }

    #[test]
    fn disabled_cameras_are_skipped() {
        let store = store();
        let mut task = mock_task("off", 10, 1);
        task.enabled = false;
        let supervisor = Supervisor::new(Arc::new(AtomicBool::new(false)));
        assert!(supervisor.run(&store, vec![task]).is_empty());
    }

    #[cfg(feature = "yolo")]
    #[test]
    fn an_unloadable_model_fails_the_camera_instead_of_retrying_forever() {
        let store = store();
        let mut task = mock_task("cam", 10, 0);
        task.detector = DetectorSpec::Yolo {
            model: std::path::PathBuf::from("definitely-missing-model.onnx"),
            labels: vec!["thing".into()],
            input_size: 64,
            conf: 0.3,
        };
        let outcome = run_one(&store, task);
        let error = outcome.error.expect("missing model must be reported");
        assert!(error.contains("model load"), "unexpected error: {error}");
        assert_eq!(outcome.frames, 0, "never pulled a frame");
    }
}
