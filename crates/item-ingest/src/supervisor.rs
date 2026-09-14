//! `Supervisor`: one OS thread per camera, plus the graceful-stop flag
//! (docs/resident-ingest.md §2/§3).
//!
//! It does exactly three things: start threads, watch the stop flag, and reopen
//! a camera's source after a backoff. A panicking or erroring camera never
//! escapes into the process: `step()` classifies its errors and returns, and a
//! genuinely fatal condition (e.g. a model that cannot be loaded) marks that
//! camera `failed` and stops retrying instead of spamming the log.
//!
//! Scope note (P1): the backoff is still the historical fixed 2s -- exponential
//! backoff with a 60s ceiling is P2, and the event checkpoints that hook into
//! the same `step()` boundary are P4 (docs/resident-ingest.md §10). The store
//! lock is taken coarsely, once per `step()`; moving annotation + JPEG encoding
//! out of it (§3) needs `ingest_detections` to take the shared handle, which is
//! still open work.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::SharedStore;
use crate::health::HealthRegistry;
use crate::runner::{CameraRunner, StepOutcome};
use crate::runtime::{self, CameraTask};
use crate::source::SourceError;

/// Reconnect backoff: doubles from the historical fixed 2s up to a 60s
/// ceiling, so an unplugged camera costs a handful of log lines per hour
/// instead of one every two seconds (docs/resident-ingest.md §10, P2).
#[derive(Debug, Clone)]
pub struct Backoff {
    current: Duration,
}

impl Backoff {
    /// Where the historical fixed delay used to sit.
    pub const INITIAL: Duration = Duration::from_secs(2);
    /// The worst case: one attempt a minute per broken camera.
    pub const MAX: Duration = Duration::from_secs(60);

    pub fn new() -> Self {
        Self {
            current: Self::INITIAL,
        }
    }

    /// The delay to apply now, then double it for the next attempt.
    pub fn next(&mut self) -> Duration {
        let wait = self.current;
        self.current = (self.current * 2).min(Self::MAX);
        wait
    }

    /// A source came back: the next outage starts from the base delay again.
    pub fn reset(&mut self) {
        self.current = Self::INITIAL;
    }

    pub fn current(&self) -> Duration {
        self.current
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

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
    handles: Vec<JoinHandle<CameraOutcome>>,
    /// Outcomes that never got a thread (spawn failure), kept so `join` reports
    /// them alongside the real ones.
    failures: Vec<CameraOutcome>,
    health: Option<HealthRegistry>,
}

impl Supervisor {
    pub fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            stop,
            handles: Vec::new(),
            failures: Vec::new(),
            health: None,
        }
    }

    /// Publish each camera's health into `registry` as it runs. The daemon
    /// installs one so `health.json` has something to say.
    pub fn with_health(mut self, registry: HealthRegistry) -> Self {
        self.health = Some(registry);
        self
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Spawn one thread per enabled camera that has a local source. Returns
    /// immediately; use [`Supervisor::join`] to wait.
    ///
    /// Webhook-fed cameras (`source == None`) get no thread: their frames come
    /// from the HTTP path, and trying to "open" them would spin forever.
    pub fn start(&mut self, store: &SharedStore, tasks: Vec<CameraTask>) {
        for task in tasks
            .into_iter()
            .filter(|t| t.enabled && t.source.is_some())
        {
            let camera_id = task.camera_id.clone();
            let store = Arc::clone(store);
            let stop = Arc::clone(&self.stop);
            let health = self.health.clone();
            match thread::Builder::new()
                .name(format!("camera-{camera_id}"))
                .spawn(move || drive(store, task, stop, health))
            {
                Ok(handle) => self.handles.push(handle),
                Err(e) => {
                    let mut outcome = CameraOutcome::for_camera(camera_id);
                    outcome.error = Some(format!("could not spawn camera thread: {e}"));
                    self.failures.push(outcome);
                }
            }
        }
    }

    /// Wait for every camera thread, at most `timeout`. A camera still running
    /// when the deadline passes is abandoned with a warning -- the daemon has a
    /// shutdown budget (§3) and must not hang on a wedged decoder.
    pub fn join(mut self, timeout: Option<Duration>) -> Vec<CameraOutcome> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut outcomes = std::mem::take(&mut self.failures);
        let total = self.handles.len();

        for (index, handle) in std::mem::take(&mut self.handles).into_iter().enumerate() {
            loop {
                if handle.is_finished() {
                    match handle.join() {
                        Ok(outcome) => outcomes.push(outcome),
                        Err(_) => {
                            tracing::error!("camera thread panicked; continuing with the rest")
                        }
                    }
                    break;
                }
                if let Some(deadline) = deadline
                    && Instant::now() >= deadline
                {
                    tracing::warn!(
                        abandoned = total - index,
                        "camera threads did not stop in time; abandoning them"
                    );
                    return outcomes;
                }
                thread::sleep(STOP_POLL);
            }
        }
        outcomes
    }

    /// `start` + `join` with no timeout: the single-camera CLI paths.
    pub fn run(mut self, store: &SharedStore, tasks: Vec<CameraTask>) -> Vec<CameraOutcome> {
        self.start(store, tasks);
        self.join(None)
    }
}

/// Convenience for the single-camera CLI paths.
pub fn run_one(store: &SharedStore, task: CameraTask) -> CameraOutcome {
    let camera_id = task.camera_id.clone();
    let supervisor = Supervisor::new(Arc::new(AtomicBool::new(false)));
    supervisor
        .run(store, vec![task])
        .pop()
        .unwrap_or_else(|| CameraOutcome::for_camera(camera_id))
}

/// The whole life of one camera, on its own thread.
fn drive(
    store: SharedStore,
    task: CameraTask,
    stop: Arc<AtomicBool>,
    health: Option<HealthRegistry>,
) -> CameraOutcome {
    let mut outcome = CameraOutcome::for_camera(task.camera_id.clone());

    // Fatal on purpose: a missing model file or a missing cargo feature will not
    // fix itself, so say so once instead of retrying forever.
    let detector = match runtime::build_detector(&task.detector) {
        Ok(d) => d,
        Err(e) => {
            let reason = format!("{e:#}");
            tracing::error!(camera = %task.camera_id, error = %reason, "camera failed to start; not retrying");
            // §5 wants `failed` to be visible: a camera that never got as far as
            // running must not look like one that is merely still starting.
            if let Some(registry) = health.as_ref() {
                registry.publish(
                    &task.camera_id,
                    crate::runner::CameraHealth {
                        state: crate::runner::CameraState::Failed,
                        last_error: Some(reason.clone()),
                        ..Default::default()
                    },
                );
            }
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
    let camera_id = runner.task().camera_id.clone();

    macro_rules! publish {
        () => {
            if let Some(registry) = health.as_ref() {
                registry.publish(&camera_id, runner.health());
            }
        };
    }

    // Reconnect delays double up to the ceiling (§10 P2).
    let mut backoff = Backoff::new();

    loop {
        if stop.load(Ordering::SeqCst) {
            runner.mark_stopped();
            break;
        }

        if !runner.is_open() {
            tracing::info!(source = %source_desc, "opening {kind} source");
            if let Err(e) = runner.open() {
                let delay = backoff.next();
                tracing::warn!(error = %e, retry_in_s = delay.as_secs(), "connect failed");
                publish!();
                if sleep_backoff(&stop, delay) {
                    runner.mark_stopped();
                    break;
                }
                continue;
            }
            // The source is up: publish `running` now, so a camera that is
            // connected but has not produced a frame yet does not read as
            // "starting" in health.json.
            publish!();
        }

        let stepped = {
            let store = store.lock().expect("store mutex poisoned");
            runner.step(&store)
        };
        publish!();

        match stepped {
            Ok(StepOutcome::Finished) => {
                outcome.finished = true;
                break;
            }
            Ok(StepOutcome::Lost(reason)) => {
                let delay = backoff.next();
                match &reason {
                    Some(SourceError::Eof) => {
                        tracing::info!(retry_in_s = delay.as_secs(), "stream ended, reconnecting")
                    }
                    Some(e) => tracing::warn!(
                        error = %e,
                        retry_in_s = delay.as_secs(),
                        "source error, reconnecting"
                    ),
                    None => {
                        tracing::warn!(retry_in_s = delay.as_secs(), "no source open, reconnecting")
                    }
                }
                if sleep_backoff(&stop, delay) {
                    runner.mark_stopped();
                    break;
                }
            }
            Ok(StepOutcome::DetectorFailed) => {
                // The runner already warned about the detector itself; this is
                // only the pause before trying the next frame.
                let delay = backoff.next();
                tracing::warn!(
                    retry_in_s = delay.as_secs(),
                    "detector unavailable; backing off before the next frame"
                );
                if sleep_backoff(&stop, delay) {
                    runner.mark_stopped();
                    break;
                }
            }
            // A real frame arrived: the next outage starts from the base delay.
            Ok(StepOutcome::Detected { .. }) | Ok(StepOutcome::Throttled) => backoff.reset(),
            Err(e) => {
                // Store/IO errors are not self-healing: stop this camera and
                // let the service manager or the user deal with it.
                let reason = format!("{e:#}");
                tracing::error!(camera = %camera_id, error = %reason, "camera step failed; stopping this camera");
                runner.mark_failed(&reason);
                outcome.error = Some(reason);
                break;
            }
        }
    }

    let final_health = runner.health();
    outcome.frames = final_health.frames;
    outcome.detections = final_health.detections;
    outcome.recorded = final_health.recorded;
    outcome.reconnects = final_health.reconnects;
    if let Some(registry) = health.as_ref() {
        registry.publish(&camera_id, final_health);
    }
    outcome
}

/// Sleep `delay`, returning early (and reporting `true`) when a stop is
/// requested. A plain `thread::sleep` would make Ctrl-C wait out the whole
/// backoff, which at the 60s ceiling would be unacceptable.
fn sleep_backoff(stop: &AtomicBool, delay: Duration) -> bool {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(STOP_POLL.min(delay));
    }
    stop.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CameraState;
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

    fn idle() -> Supervisor {
        Supervisor::new(Arc::new(AtomicBool::new(false)))
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
        let outcomes = idle().run(&store, vec![webhook_task]);
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
        assert!(idle().run(&store, vec![task]).is_empty());
    }

    #[test]
    fn a_running_camera_publishes_health_while_it_works() {
        let store = store();
        let registry = HealthRegistry::new();
        let supervisor = idle().with_health(registry.clone());
        let outcomes = supervisor.run(&store, vec![mock_task("cam", 10, 2)]);

        assert_eq!(outcomes[0].frames, 2);
        let published = registry.get("cam").expect("health published");
        assert_eq!(published.frames, 2);
        assert_eq!(published.state, CameraState::Stopped);
    }

    #[test]
    fn an_empty_supervisor_joins_immediately() {
        let store = store();
        assert!(idle().run(&store, Vec::new()).is_empty());
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
    #[test]
    fn backoff_doubles_to_the_ceiling_and_resets() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.next(), Duration::from_secs(2));
        assert_eq!(backoff.next(), Duration::from_secs(4));
        assert_eq!(backoff.next(), Duration::from_secs(8));
        assert_eq!(backoff.next(), Duration::from_secs(16));
        assert_eq!(backoff.next(), Duration::from_secs(32));
        assert_eq!(backoff.next(), Duration::from_secs(60), "capped");
        assert_eq!(backoff.next(), Duration::from_secs(60), "stays capped");

        // A camera that comes back must not inherit the old penalty.
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_secs(2));
    }
}
