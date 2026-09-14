//! `CameraRunner`: one camera's decode -> throttle -> detect -> persist
//! pipeline expressed as repeatedly-callable steps (docs/resident-ingest.md §2).
//!
//! `step()` deliberately never loops internally. The stop flag, health
//! reporting and (in P4) the event checkpoints all hang off that single check
//! point, and the [`crate::supervisor::Supervisor`] owns reopening + backoff.

use std::time::{Duration, Instant};

use anyhow::Result;
use item_core::store::Store;

use crate::detector::Detector;
use crate::runtime::CameraTask;
use crate::source::{FrameSource, SourceError};

/// What one `step()` did.
#[derive(Debug)]
pub enum StepOutcome {
    /// A frame arrived, but detection was throttled for this interval.
    Throttled,
    /// Detection ran on this frame.
    Detected { recorded: usize },
    /// The detector errored; this frame was skipped. A downed VLM sidecar must
    /// not kill the loop, so the caller backs off and keeps going.
    DetectorFailed,
    /// The source is gone (EOF, error, or never opened): reopen it.
    Lost(Option<SourceError>),
    /// The frame budget is exhausted; stop cleanly.
    Finished,
}

/// Coarse camera state, mirroring the `cameras[].state` field of the health
/// snapshot (docs/resident-ingest.md §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraState {
    Starting,
    Running,
    Reconnecting,
    /// Gave up; needs a human.
    Failed,
    Stopped,
}

impl CameraState {
    pub fn as_str(self) -> &'static str {
        match self {
            CameraState::Starting => "starting",
            CameraState::Running => "running",
            CameraState::Reconnecting => "reconnecting",
            CameraState::Failed => "failed",
            CameraState::Stopped => "stopped",
        }
    }
}

/// Everything the outside world may ask about one camera.
#[derive(Debug, Clone)]
pub struct CameraHealth {
    pub state: CameraState,
    pub frames: u64,
    pub detections: u64,
    pub recorded: u64,
    pub reconnects: u64,
    /// Age of the newest frame; `None` before the first one.
    pub last_frame_age: Option<Duration>,
    pub last_error: Option<String>,
    pub inference_ms_ewma: f64,
}

impl Default for CameraHealth {
    /// A camera that has not reported anything yet.
    fn default() -> Self {
        Self {
            state: CameraState::Starting,
            frames: 0,
            detections: 0,
            recorded: 0,
            reconnects: 0,
            last_frame_age: None,
            last_error: None,
            inference_ms_ewma: 0.0,
        }
    }
}

/// Owns one `FrameSource` and one `Detector`.
///
/// Threading: `open()` and every `step()` must happen on the SAME thread --
/// nokhwa's camera carries COM apartment affinity with its creating thread
/// (see `source::nokhwa`).
pub struct CameraRunner {
    task: CameraTask,
    detector: Box<dyn Detector>,
    source: Option<Box<dyn FrameSource>>,
    interval: Duration,
    /// `None` = the next frame is due immediately. (The historical loop seeded
    /// the equivalent state with `Instant::now() - interval`.)
    last_detect: Option<Instant>,
    opens: u64,
    frames: u64,
    detections: u64,
    recorded: u64,
    last_frame_at: Option<Instant>,
    last_error: Option<String>,
    inference_ms_ewma: f64,
    state: CameraState,
}

impl CameraRunner {
    pub fn new(task: CameraTask, detector: Box<dyn Detector>) -> Self {
        let interval = task.detect_interval();
        Self {
            task,
            detector,
            source: None,
            interval,
            last_detect: None,
            opens: 0,
            frames: 0,
            detections: 0,
            recorded: 0,
            last_frame_at: None,
            last_error: None,
            inference_ms_ewma: 0.0,
            state: CameraState::Starting,
        }
    }

    pub fn task(&self) -> &CameraTask {
        &self.task
    }

    pub fn state(&self) -> CameraState {
        self.state
    }

    /// Is a live source attached?
    pub fn is_open(&self) -> bool {
        self.source.is_some()
    }

    /// Build the frame source. Call this on the thread that will own it.
    pub fn open(&mut self) -> Result<()> {
        self.state = CameraState::Starting;
        match crate::runtime::build_source(&self.task) {
            Ok(src) => {
                self.source = Some(src);
                self.opens += 1;
                self.state = CameraState::Running;
                self.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.last_error = Some(format!("{e:#}"));
                self.state = CameraState::Reconnecting;
                Err(e)
            }
        }
    }

    /// Drop the source so the next `open()` rebuilds it.
    pub fn close(&mut self) {
        self.source = None;
    }

    /// One short step: pull a frame, throttle, detect, persist.
    pub fn step(&mut self, store: &Store) -> Result<StepOutcome> {
        if self.task.max_frames > 0 && self.frames >= self.task.max_frames {
            self.state = CameraState::Stopped;
            return Ok(StepOutcome::Finished);
        }

        let Some(source) = self.source.as_mut() else {
            self.state = CameraState::Reconnecting;
            return Ok(StepOutcome::Lost(None));
        };

        let frame = match source.next_frame() {
            Ok(f) => f,
            Err(e) => {
                self.last_error = Some(e.to_string());
                self.state = CameraState::Reconnecting;
                self.source = None;
                return Ok(StepOutcome::Lost(Some(e)));
            }
        };

        self.frames += 1;
        self.last_frame_at = Some(Instant::now());

        let due = match self.last_detect {
            None => true,
            Some(t) => t.elapsed() >= self.interval,
        };
        if !due {
            return Ok(StepOutcome::Throttled); // decode-only frame
        }
        self.last_detect = Some(Instant::now());

        let started = Instant::now();
        let dets = match self
            .detector
            .detect(&frame.rgb, frame.meta.width, frame.meta.height)
        {
            Ok(d) => d,
            Err(e) => {
                // A downed VLM sidecar (or a transient ort error) must not kill
                // the loop; drop this frame and let the caller back off.
                tracing::warn!(error = %e, "detector failed, skipping frame");
                self.last_error = Some(e.to_string());
                return Ok(StepOutcome::DetectorFailed);
            }
        };
        self.observe_inference(started.elapsed());
        self.detections += 1;

        let recorded = crate::ingest_detections(
            store,
            &frame.meta,
            &dets,
            0.45,
            0.0, // the detector already floors at conf
            Some((&frame.rgb, self.task.snapshot_dir.as_path())),
        )?;
        self.recorded += recorded as u64;
        if recorded > 0 {
            tracing::info!(frames = self.frames, recorded, "detections ingested");
        }
        Ok(StepOutcome::Detected { recorded })
    }

    pub fn health(&self) -> CameraHealth {
        CameraHealth {
            state: self.state,
            frames: self.frames,
            detections: self.detections,
            recorded: self.recorded,
            // The first open is not a reconnect.
            reconnects: self.opens.saturating_sub(1),
            last_frame_age: self.last_frame_at.map(|t| t.elapsed()),
            last_error: self.last_error.clone(),
            inference_ms_ewma: self.inference_ms_ewma,
        }
    }

    /// Give up on this camera (needs a human).
    pub fn mark_failed(&mut self, reason: impl Into<String>) {
        self.last_error = Some(reason.into());
        self.state = CameraState::Failed;
    }

    pub fn mark_stopped(&mut self) {
        self.state = CameraState::Stopped;
    }

    /// Exponentially-weighted mean of inference time, for the health file.
    fn observe_inference(&mut self, elapsed: Duration) {
        let ms = elapsed.as_secs_f64() * 1e3;
        self.inference_ms_ewma = if self.inference_ms_ewma == 0.0 {
            ms
        } else {
            0.2 * ms + 0.8 * self.inference_ms_ewma
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::NullDetector;
    use crate::runtime::{DetectorSpec, SourceSpec};

    fn runner(frames: u32, max_frames: u64) -> CameraRunner {
        let task = CameraTask {
            camera_id: "cam".into(),
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
        };
        CameraRunner::new(task, Box::new(NullDetector))
    }

    #[test]
    fn step_is_a_short_callable_step_not_a_loop() {
        let store = Store::in_memory().unwrap();
        let mut r = runner(3, 0);
        r.open().unwrap();

        // The first frame is due immediately, the rest are throttled at 1 fps.
        assert!(matches!(
            r.step(&store).unwrap(),
            StepOutcome::Detected { recorded: 0 }
        ));
        assert!(matches!(r.step(&store).unwrap(), StepOutcome::Throttled));
        assert!(matches!(r.step(&store).unwrap(), StepOutcome::Throttled));
        // MockSource is exhausted: the caller must reopen, not spin.
        assert!(matches!(
            r.step(&store).unwrap(),
            StepOutcome::Lost(Some(SourceError::Eof))
        ));

        let h = r.health();
        assert_eq!(h.frames, 3);
        assert_eq!(h.detections, 1);
        assert_eq!(h.reconnects, 0, "the first open is not a reconnect");
        assert_eq!(h.state, CameraState::Reconnecting);
    }

    #[test]
    fn frame_budget_finishes_cleanly() {
        let store = Store::in_memory().unwrap();
        let mut r = runner(10, 2);
        r.open().unwrap();
        assert!(matches!(
            r.step(&store).unwrap(),
            StepOutcome::Detected { .. }
        ));
        assert!(matches!(r.step(&store).unwrap(), StepOutcome::Throttled));
        assert!(matches!(r.step(&store).unwrap(), StepOutcome::Finished));
        assert_eq!(r.health().state, CameraState::Stopped);
        assert_eq!(r.health().frames, 2);
    }

    #[test]
    fn stepping_without_an_open_source_asks_to_reopen_instead_of_panicking() {
        let store = Store::in_memory().unwrap();
        let mut r = runner(1, 0);
        assert!(matches!(r.step(&store).unwrap(), StepOutcome::Lost(None)));
        assert_eq!(r.health().state, CameraState::Reconnecting);
    }

    #[test]
    fn reopening_counts_as_a_reconnect() {
        let store = Store::in_memory().unwrap();
        let mut r = runner(1, 0);
        r.open().unwrap();
        let _ = r.step(&store).unwrap(); // drains the single mock frame
        r.open().unwrap();
        assert_eq!(r.health().reconnects, 1);
        assert_eq!(r.health().state, CameraState::Running);
    }
}
