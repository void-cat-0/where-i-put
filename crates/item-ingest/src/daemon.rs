//! The resident daemon: one process running the Frigate webhook server, the
//! optional RTSP preview and N camera threads (docs/resident-ingest.md §3).
//!
//! Shutdown follows §3 exactly, because the order is the whole point:
//!
//! 1. stop accepting new requests (`with_graceful_shutdown`, and the stop flag
//!    is set first so the cameras stop pulling frames immediately);
//! 2. let the camera threads leave at a `step()` boundary, with a 10s budget --
//!    a wedged decoder must not hold the process hostage;
//! 3. write the final health snapshot so `updated_at` is the last word, then
//!    `PRAGMA wal_checkpoint(TRUNCATE)`;
//! 4. exit 0.
//!
//! Platform note: the stop sources differ. Windows has no `SIGTERM` at all, so
//! the console event (Ctrl-C) is the path there, while systemd/launchd send
//! `SIGTERM` on Unix. `tokio::signal::unix` does not exist off Unix, hence the
//! `#[cfg(unix)]` split -- see [`wait_for_signal`].

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use item_core::store::Store;

use crate::SharedStore;
use crate::config::{Config, RuntimeConfig};
use crate::health::{CameraMeta, HealthRegistry, HealthWriter};
use crate::lock::SingleInstance;
use crate::runtime::{self, CameraTask};
use crate::supervisor::Supervisor;

/// How long the camera threads get to leave before the daemon gives up (§3).
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);

/// How often the health file is refreshed (§5).
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(15);

/// Run the daemon until a stop signal arrives. Wires the stop flag to the
/// process signals; this is the entry point `--daemon` uses.
pub fn run(settings: RuntimeConfig, file: &Config) -> anyhow::Result<()> {
    let tasks = file.camera_tasks(&settings);
    run_with_stop(
        settings,
        tasks,
        Some(file),
        Arc::new(AtomicBool::new(false)),
    )
}

/// Same as [`run`], but the caller owns the stop flag. Tests flip it directly
/// instead of poking the process's signal handlers.
pub fn run_with_stop(
    settings: RuntimeConfig,
    tasks: Vec<CameraTask>,
    seed: Option<&Config>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    reject_unsupported_sources(&tasks)?;

    if let Some(dir) = std::path::Path::new(&settings.db).parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).context("creating the database directory")?;
    }

    // Before anything opens the database: two writers on one WAL file is the
    // failure that corrupts data quietly (§1).
    let lock = SingleInstance::acquire(&settings.lock_path())
        .context("refusing to start a second ingest daemon")?;
    tracing::info!(
        lock = %lock.path().display(),
        pid = std::process::id(),
        "single-instance lock acquired"
    );

    let store: SharedStore = Arc::new(Mutex::new(
        Store::open(&settings.db).context("opening sqlite store")?,
    ));

    if let Some(file) = seed {
        let guard = store.lock().expect("store mutex poisoned");
        let seeded = file
            .seed_regions(&guard)
            .context("seeding camera regions")?;
        tracing::info!(
            cameras = file.camera.len(),
            regions = seeded,
            "config regions seeded"
        );
    }

    let registry = HealthRegistry::new();
    let writer = Arc::new(Mutex::new(HealthWriter::new(
        &settings.health_file,
        &settings.detector,
        tasks
            .iter()
            .map(|task| CameraMeta {
                id: task.camera_id.clone(),
                source: task
                    .source
                    .as_ref()
                    .map(|s| s.describe())
                    .unwrap_or_else(|| "webhook".to_string()),
            })
            .collect(),
    )));
    write_health(&writer, &registry);

    let mut supervisor = Supervisor::new(Arc::clone(&stop)).with_health(registry.clone());
    supervisor.start(&store, tasks);
    tracing::info!("camera threads started");

    // The health cadence runs on a plain thread so it keeps ticking while the
    // async side is parked on a signal.
    let health_thread = {
        let writer = Arc::clone(&writer);
        let registry = registry.clone();
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("health".to_string())
            .spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let mut waited = Duration::ZERO;
                    while waited < HEALTH_INTERVAL {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                        waited += Duration::from_millis(200);
                    }
                    write_health(&writer, &registry);
                }
            })
            .context("spawning the health writer thread")?
    };

    let app = build_router(&store, &settings);
    let addr: SocketAddr = settings
        .listen
        .parse()
        .context("bad listen address (--listen / [webhook] listen)")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        if settings.webhook_enabled {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            tracing::info!(%addr, "frigate webhook server listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(wait_for_stop(Arc::clone(&stop)))
                .await?;
        } else {
            tracing::info!(
                "webhook disabled ([webhook] enabled = false); waiting for a stop signal"
            );
            wait_for_stop(Arc::clone(&stop)).await;
        }
        anyhow::Ok(())
    })?;

    // From here on: nobody new is connecting, so drain the cameras.
    stop.store(true, Ordering::SeqCst);
    tracing::info!(
        budget_s = SHUTDOWN_BUDGET.as_secs(),
        "stopping: waiting for camera threads to reach a step boundary"
    );
    supervisor.request_stop();
    let outcomes = supervisor.join(Some(SHUTDOWN_BUDGET));
    for outcome in &outcomes {
        tracing::info!(
            camera = %outcome.camera_id,
            frames = outcome.frames,
            detections = outcome.detections,
            recorded = outcome.recorded,
            reconnects = outcome.reconnects,
            finished = outcome.finished,
            error = ?outcome.error,
            "camera stopped"
        );
    }

    let _ = health_thread.join();
    // One last snapshot: after this write, `updated_at` is the process's final
    // word, which is what P1's acceptance checks.
    write_health(&writer, &registry);
    {
        let store = store.lock().expect("store mutex poisoned");
        match store.checkpoint() {
            Ok(()) => tracing::info!("wal checkpointed"),
            Err(e) => tracing::warn!(error = %e, "wal checkpoint failed"),
        }
    }

    drop(lock);
    tracing::info!("ingest daemon stopped");
    Ok(())
}

/// Refuse to start when a camera needs a source this build cannot open. That is
/// a configuration error, not a transient one, and retrying it forever would
/// only hide it (see `runtime::source_supported`).
fn reject_unsupported_sources(tasks: &[CameraTask]) -> anyhow::Result<()> {
    for task in tasks {
        if let Some(spec) = task.source.as_ref()
            && !runtime::source_supported(spec)
        {
            anyhow::bail!(
                "camera '{}' needs a {} source, which this build cannot open: \
                 rebuild with --features {}",
                task.camera_id,
                spec.kind(),
                spec.cargo_feature()
            );
        }
    }
    Ok(())
}

fn build_router(store: &SharedStore, settings: &RuntimeConfig) -> axum::Router {
    let state: crate::frigate::State = Arc::clone(store);
    let app = crate::frigate::router(state);

    // Shadow (not mut): with `rtsp` off there is nothing to merge.
    #[cfg(feature = "rtsp")]
    let app = match settings.preview_url.clone() {
        Some(url) => {
            // Credentials are part of the url; log only scheme+host.
            tracing::info!(
                target_url = %runtime::redact_url(&url),
                "web preview enabled at GET /preview"
            );
            app.merge(crate::preview::router(crate::preview::spawn_streamer(url)))
        }
        None => app,
    };

    // The preview is the only thing that reads the settings here.
    #[cfg(not(feature = "rtsp"))]
    let _ = settings;

    app
}

fn write_health(writer: &Mutex<HealthWriter>, registry: &HealthRegistry) {
    match writer.lock() {
        Ok(mut writer) => {
            // P2 makes this atomic and retries on Windows share violations; for
            // now a failure must not take the daemon down.
            if let Err(e) = writer.write(registry) {
                tracing::warn!(error = %e, path = %writer.path().display(), "health file write failed");
            }
        }
        Err(_) => tracing::warn!("health writer mutex poisoned"),
    }
}

/// Resolves when the process is asked to stop: a console stop event (Ctrl-C)
/// anywhere, plus `SIGTERM` on Unix -- that is what systemd and launchd send,
/// and Windows has no `SIGTERM` to send.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => tracing::info!("stop signal: Ctrl-C"),
                    _ = term.recv() => tracing::info!("stop signal: SIGTERM"),
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not install a SIGTERM handler; only Ctrl-C will stop the daemon");
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("stop signal: Ctrl-C");
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Windows: the console delivers Ctrl-C here. Ctrl-Break would need the
        // raw `SetConsoleCtrlHandler` API, which tokio does not wrap.
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("stop signal: console interrupt");
    }
}

/// Either a process signal or someone flipping the flag (tests, and the
/// `--daemon` path when the flag is set by another task).
async fn wait_for_stop(stop: Arc<AtomicBool>) {
    tokio::select! {
        _ = wait_for_signal() => {}
        _ = async move {
            while !stop.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CliOverrides, EnvOverrides};
    use crate::runtime::{DetectorSpec, SourceSpec};

    fn mock_task(id: &str) -> CameraTask {
        CameraTask {
            camera_id: id.into(),
            source: Some(SourceSpec::Mock {
                frames: u32::MAX,
                width: 8,
                height: 8,
            }),
            detector: DetectorSpec::Null,
            detect_fps: 1.0,
            snapshot_dir: std::path::PathBuf::from("unused"),
            max_frames: 0,
            enabled: true,
        }
    }

    struct Sandbox(std::path::PathBuf);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "item-ingest-daemon-{tag}-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }

        /// Settings pointing entirely inside the sandbox, with the web server
        /// off so the test needs no port.
        fn settings(&self) -> RuntimeConfig {
            let mut settings =
                RuntimeConfig::resolve(None, &CliOverrides::default(), &EnvOverrides::default());
            settings.db = self.path("items.db").display().to_string();
            settings.snapshots_dir = self.path("snapshots").display().to_string();
            settings.health_file = self.path("health.json").display().to_string();
            settings.webhook_enabled = false;
            settings
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn wait_for_file(path: &std::path::Path, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn unsupported_sources_are_refused_before_the_daemon_starts() {
        let mut task = mock_task("cam");
        task.source = Some(SourceSpec::Webcam { index: 0 });
        match reject_unsupported_sources(&[task]) {
            Err(e) => assert!(
                e.to_string().contains("rebuild with --features camera"),
                "{e}"
            ),
            // Only a build that can actually open a webcam may accept this.
            Ok(()) => assert!(cfg!(feature = "camera"), "no camera feature, yet accepted"),
        }
    }

    #[test]
    fn a_daemon_run_stops_on_request_and_leaves_a_final_health_file() {
        let sandbox = Sandbox::new("stop");
        let health_path = sandbox.path("health.json");
        let lock_path = sandbox.path("ingest.lock");

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let settings = sandbox.settings();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || run_with_stop(settings, vec![mock_task("cam")], None, stop))
        };

        assert!(
            wait_for_file(&health_path, Duration::from_secs(10)),
            "the daemon writes an initial health file"
        );
        assert!(lock_path.exists(), "the lock is held while running");

        let before: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&health_path).unwrap()).unwrap();

        stop.store(true, Ordering::SeqCst);
        let result = handle.join().expect("daemon thread must not panic");
        assert!(result.is_ok(), "clean shutdown returns Ok: {result:?}");

        assert!(!lock_path.exists(), "a clean stop releases the lock");
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&health_path).unwrap()).unwrap();
        assert!(
            after["updated_at"].as_str().unwrap() >= before["updated_at"].as_str().unwrap(),
            "the final snapshot must be the last word"
        );
        assert_eq!(after["cameras"][0]["state"], "stopped");
    }

    #[test]
    fn a_second_daemon_is_refused_while_the_first_holds_the_lock() {
        let sandbox = Sandbox::new("lock");
        let health_path = sandbox.path("health.json");

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let settings = sandbox.settings();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || run_with_stop(settings, vec![mock_task("cam")], None, stop))
        };
        assert!(wait_for_file(&health_path, Duration::from_secs(10)));

        let second = run_with_stop(
            sandbox.settings(),
            vec![mock_task("cam")],
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let message = format!("{:#}", second.expect_err("a second daemon must be refused"));
        assert!(message.contains("already running"), "{message}");
        assert!(
            message.contains(&std::process::id().to_string()),
            "the loser must be told which pid holds the lock: {message}"
        );

        stop.store(true, Ordering::SeqCst);
        let _ = handle.join();
    }
}
