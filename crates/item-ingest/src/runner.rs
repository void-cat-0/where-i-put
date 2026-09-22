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
use crate::events::{self, EventLog, Tracker};
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
    /// The appeared/disappeared timeline, kept per camera (§6). Its events go
    /// to `log`, which stays `None` when event observation is off.
    tracker: Tracker,
    log: Option<EventLog>,
}

impl CameraRunner {
    pub fn new(task: CameraTask, detector: Box<dyn Detector>) -> Self {
        let interval = task.detect_interval();
        let tracker = Tracker::new(interval);
        // An event log that cannot be opened is not a reason to refuse the
        // camera: `EventLog::open` already warns, and events are evidence, not
        // the product.
        let log = match task.events_path.as_deref() {
            Some(path) => match EventLog::open(path, &task.camera_id) {
                Ok(log) => Some(log),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "event log unavailable; this camera will run without events"
                    );
                    None
                }
            },
            None => None,
        };
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
            tracker,
            log,
        }
    }

    /// Is this camera publishing an event timeline?
    pub fn events_enabled(&self) -> bool {
        self.log.is_some()
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
        self.recorded += recorded.len() as u64;
        if !recorded.is_empty() {
            tracing::info!(
                frames = self.frames,
                recorded = recorded.len(),
                "detections ingested"
            );
        }
        self.emit_events(&frame.meta.camera_id, &recorded, frame.meta.captured_at);
        Ok(StepOutcome::Detected {
            recorded: recorded.len(),
        })
    }

    /// Fold this frame's sightings into the event timeline and write what they
    /// imply: an arrival per new row, a departure per key that has gone quiet
    /// (§6). Runs after every detection-bearing step, which is the same
    /// checkpoint the stop flag and health reporting hang off.
    ///
    /// Writing is best-effort throughout: an unwritable event log must not cost
    /// a camera a frame.
    fn emit_events(
        &mut self,
        camera_id: &str,
        recorded: &[crate::Recorded],
        now: chrono::DateTime<chrono::Utc>,
    ) {
        let mut events = Vec::new();
        for row in recorded {
            events.extend(self.tracker.record(
                &events::Sighting {
                    camera_id,
                    zone: &row.zone,
                    label: &row.label,
                    obs_id: row.obs_id,
                    is_new: row.is_new,
                    hits: row.hits,
                },
                now,
            ));
        }
        // Sweep after recording, so a key hit by this very frame is refreshed
        // before it can be judged stale.
        events.extend(self.tracker.sweep(now));
        self.write_events(&events);
    }

    /// Append events, logging but never propagating a failure.
    fn write_events(&mut self, events: &[crate::events::Event]) {
        let Some(log) = self.log.as_mut() else {
            return;
        };
        if events.is_empty() {
            return;
        }
        if let Err(e) = log.write_all(events) {
            tracing::warn!(
                path = %log.path().display(),
                error = %e,
                "event write failed; the timeline loses these entries"
            );
        }
    }

    /// Close every open key and write the departures (§3: called before the
    /// final health write, because a disappearance is only noticed at the next
    /// scan and there is none after this).
    pub fn flush_events(&mut self, now: chrono::DateTime<chrono::Utc>) {
        let events = self.tracker.flush_all(now);
        if !events.is_empty() {
            tracing::info!(closed = events.len(), "event timeline flushed at shutdown");
        }
        self.write_events(&events);
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
    use crate::detector::{Detector, DetectorError, NullDetector};
    use crate::runtime::{DetectorSpec, SourceSpec};
    use item_core::Detection;

    /// A detector that reports one fixed object wherever it is pointed. The
    /// event tests need real sightings, and `NullDetector` (which keeps the
    /// other tests featureless) reports none.
    struct FixedDetector {
        label: String,
    }

    impl Detector for FixedDetector {
        fn detect(
            &self,
            _rgb: &[u8],
            width: u32,
            height: u32,
        ) -> Result<Vec<Detection>, DetectorError> {
            Ok(vec![Detection {
                label: self.label.clone(),
                confidence: 0.9,
                bbox: [0.0, 0.0, width as f32, height as f32],
            }])
        }
    }

    fn events_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "item-ingest-runner-events-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Read the JSONL lines, skipping the per-process start markers.
    fn read_events(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["event"] != crate::events::DAEMON_STARTED)
            .collect()
    }

    fn event_runner(dir: &std::path::Path, frames: u32, label: &str) -> CameraRunner {
        let task = CameraTask {
            camera_id: "cam".into(),
            source: Some(SourceSpec::Mock {
                frames,
                width: 8,
                height: 8,
            }),
            detector: DetectorSpec::Null, // unused: the detector is injected
            detect_fps: 1.0,
            snapshot_dir: dir.to_path_buf(),
            events_path: Some(dir.join("events.jsonl")),
            max_frames: 0,
            enabled: true,
        };
        CameraRunner::new(
            task,
            Box::new(FixedDetector {
                label: label.to_string(),
            }),
        )
    }

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
            events_path: None,
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

    /// A detection on a fresh row writes `appeared` (§6), and the count the
    /// step reports is unchanged by the events path.
    #[test]
    fn a_new_observation_writes_appeared_and_a_merge_writes_nothing() {
        let dir = events_dir("appeared");
        let store = Store::in_memory().unwrap();
        let mut r = event_runner(&dir, 10, "keys");
        assert!(r.events_enabled());
        r.open().unwrap();

        // First detection: a new row -> appeared.
        let first = r.step(&store).unwrap();
        assert!(matches!(first, StepOutcome::Detected { recorded: 1 }));
        let events = read_events(r.log.as_ref().unwrap().path());
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["event"], "appeared");
        assert_eq!(events[0]["label"], "keys");
        assert_eq!(events[0]["camera"], "cam");
        assert_eq!(events[0]["hits"], 1);

        // Force the throttle window open and detect again: same row, so a
        // merge, which is not an event.
        r.last_detect = Some(Instant::now() - Duration::from_secs(5));
        let second = r.step(&store).unwrap();
        assert!(matches!(second, StepOutcome::Detected { recorded: 1 }));
        assert_eq!(
            read_events(r.log.as_ref().unwrap().path()).len(),
            1,
            "a merge must not append an event"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §10's acceptance, at the step level: an object that leaves produces
    /// `disappeared`, and the `seen_for_s` it reports agrees with the row's own
    /// `last_seen - first_seen` (within one detection period), with `hits` equal
    /// to the stored count.
    ///
    /// Real elapsed time is what makes that comparison meaningful -- the gap is
    /// `max(30s, 3*interval)` and the event is measured from the last sighting,
    /// so the test drives three detections across a real, if short, span.
    #[test]
    fn an_object_leaving_appears_then_disappears_with_consistent_timing() {
        let dir = events_dir("timeline");
        let store = Store::in_memory().unwrap();
        // A 20s detection interval gives a 60s gap: long enough that the
        // detections below stay inside it, short enough to reason about.
        let mut r = event_runner(&dir, 10, "keys");
        r.interval = Duration::from_secs(20);
        r.open().unwrap();

        // Three detections spread over ~2s of real time (the clock, not the
        // throttle, decides the row's span here).
        for _ in 0..3 {
            r.last_detect = Some(Instant::now() - Duration::from_secs(21));
            assert!(matches!(
                r.step(&store).unwrap(),
                StepOutcome::Detected { recorded: 1 }
            ));
            std::thread::sleep(Duration::from_millis(20));
        }

        let row = store.recent(Some("keys"), 1).unwrap().remove(0);
        let row_span = (row.last_seen - row.first_seen).num_milliseconds() as f64 / 1000.0;

        // Nothing is detected from here on; the sweep runs with a clock past
        // the gap, as a later frame would.
        let gone = r
            .tracker
            .sweep(chrono::Utc::now() + chrono::Duration::seconds(120));
        r.write_events(&gone);

        let events = read_events(r.log.as_ref().unwrap().path());
        assert_eq!(events.len(), 2, "appeared then disappeared: {events:?}");
        assert_eq!(events[0]["event"], "appeared");
        assert_eq!(events[1]["event"], "disappeared");
        assert_eq!(events[1]["obs_id"], events[0]["obs_id"]);

        // §10's check, now exactly: `seen_for_s` is the residency the row records,
        // within one detection period (the store timestamps sightings from the
        // frames it is given; the tracker from the moments the steps ran).
        let seen_for = events[1]["seen_for_s"].as_f64().unwrap();
        let slack = r.interval.as_secs_f64() + 0.5;
        assert!(
            (seen_for - row_span).abs() <= slack,
            "seen_for_s ({seen_for}s) must match last_seen - first_seen ({row_span}s) \
             within one detect period ({slack}s)"
        );
        // The reported hits are the row's own count (§6's `hits`).
        assert_eq!(events[1]["hits"], row.hit_count);
        assert_eq!(events[0]["hits"], 1, "the arrival saw it once");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Shutdown closes every open key even though the gap has not elapsed
    /// (§3/§6): there is no next scan after the process exits.
    #[test]
    fn flushing_at_shutdown_reports_keys_still_being_seen() {
        let dir = events_dir("flush");
        let store = Store::in_memory().unwrap();
        let mut r = event_runner(&dir, 10, "cup");
        r.open().unwrap();
        r.step(&store).unwrap();

        r.flush_events(chrono::Utc::now());

        let events = read_events(r.log.as_ref().unwrap().path());
        assert_eq!(events.len(), 2);
        assert_eq!(events[1]["event"], "disappeared");
        assert_eq!(events[1]["label"], "cup");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With events off the runner still works and writes no file.
    #[test]
    fn a_camera_without_an_events_path_produces_no_timeline() {
        let store = Store::in_memory().unwrap();
        let mut r = runner(10, 0); // the helper leaves events_path None
        assert!(!r.events_enabled());
        r.open().unwrap();
        assert!(matches!(
            r.step(&store).unwrap(),
            StepOutcome::Detected { recorded: 0 }
        ));
        r.flush_events(chrono::Utc::now()); // must be a harmless no-op
    }
}
