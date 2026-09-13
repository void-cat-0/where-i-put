//! Declarative camera/region config, plus the runtime settings it resolves to.
//!
//! Regions are rows in SQLite, so everything else -- webhook, camera loop,
//! query -- keeps reading from the store, not from TOML.
//!
//! Precedence, lowest to highest (docs/resident-ingest.md §4):
//!
//! ```text
//! built-in defaults -> config.toml -> environment -> explicit CLI flags
//! ```
//!
//! Every field a config file may supply is therefore an `Option` on the CLI
//! side: clap's `default_value` makes "the user asked for X" and "X is the
//! default" indistinguishable, and that distinction is the whole job here.
//!
//! config.toml shape (`[daemon]`/`[webhook]` are optional; a bare `[[camera]]`
//! list keeps working exactly as before):
//!
//! ```toml
//! [daemon]
//! db = "data/items.db"
//! snapshots_dir = "data/snapshots"
//! health_file = "data/health.json"
//! rescan_config_secs = 0          # 0 = no hot reload
//! checkpoint_secs = 300
//!
//! [webhook]
//! enabled = true
//! listen = "127.0.0.1:8477"
//! preview_url = ""                # non-empty opens /preview (rtsp feature)
//!
//! [[camera]]
//! id = "living"
//! enabled = true
//! url = "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102"
//! detector = "yolo"               # overrides the global default
//! detect_fps = 1.0                # overrides the global default
//! targets = ""                    # only meaningful for detector = "vlm"
//!
//! [camera.regions]                # rects in that camera's pixel space
//! desk = [0.0, 360.0, 1280.0, 720.0]
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// Built-in defaults: the lowest-precedence layer of the merge.
pub mod defaults {
    pub const DB: &str = "data/items.db";
    pub const SNAPSHOTS_DIR: &str = "data/snapshots";
    pub const HEALTH_FILE: &str = "data/health.json";
    pub const LISTEN: &str = "127.0.0.1:8477";
    pub const CAMERA_ID: &str = "rtsp-0";
    pub const DETECTOR: &str = "yolo";
    pub const MODEL: &str = "models/yolov8n.onnx";
    pub const INPUT_SIZE: usize = 640;
    pub const CONF: f32 = 0.3;
    pub const VLM_TIMEOUT_SECS: u64 = 60;
    pub const VLM_COORDS: &str = "norm1000";
    pub const CHECKPOINT_SECS: u64 = 300;
    /// Grounding is one HTTP round trip per detection against a local LLM, so
    /// the VLM default is far below the local-detector one.
    pub const DETECT_FPS_VLM: f64 = 0.2;
    pub const DETECT_FPS_LOCAL: f64 = 1.0;
}

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub webhook: WebhookConfig,
    #[serde(default)]
    pub camera: Vec<CameraConfig>,
}

#[derive(Debug, Deserialize, Default)]
pub struct DaemonConfig {
    pub db: Option<String>,
    pub snapshots_dir: Option<String>,
    pub health_file: Option<String>,
    /// 0 = never reload config.toml while running (phase 2 feature).
    pub rescan_config_secs: Option<u64>,
    pub checkpoint_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct WebhookConfig {
    pub enabled: Option<bool>,
    pub listen: Option<String>,
    pub preview_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CameraConfig {
    pub id: String,
    /// RTSP url (may embed credentials); absent for webhook-fed cameras.
    #[serde(default)]
    pub url: Option<String>,
    /// `false` keeps the camera out of the daemon without deleting its regions.
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub detector: Option<String>,
    #[serde(default)]
    pub detect_fps: Option<f64>,
    /// Vocabulary for `detector = "vlm"`.
    #[serde(default)]
    pub targets: Option<String>,
    /// name -> [x0, y0, x1, y1] in camera pixels; first match wins (BTreeMap
    /// order is alphabetical, so name zones deliberately if they overlap).
    #[serde(default)]
    pub regions: BTreeMap<String, [f32; 4]>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("parsing {path}: {source}")]
    Parse {
        #[source]
        source: toml::de::Error,
        path: std::path::PathBuf,
    },
    #[error("store: {0}")]
    Store(#[from] item_core::store::StoreError),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse {
            source: e,
            path: path.to_path_buf(),
        })
    }

    /// Upsert every camera's regions into the store. Idempotent; safe on
    /// every daemon start. Returns how many region rows were seeded.
    pub fn seed_regions(&self, store: &item_core::store::Store) -> Result<usize, ConfigError> {
        let mut n = 0;
        for cam in &self.camera {
            for (name, rect) in &cam.regions {
                store.upsert_region(&cam.id, name, *rect)?;
                n += 1;
            }
        }
        Ok(n)
    }
}

/// The CLI-layer values that are actually present: `None` means the flag was
/// not given, which is exactly what lets config.toml win over a default.
#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    pub db: Option<String>,
    pub snapshots_dir: Option<String>,
    pub listen: Option<String>,
    pub camera_id: Option<String>,
    pub detector: Option<String>,
    pub model: Option<String>,
    pub input_size: Option<usize>,
    pub conf: Option<f32>,
    pub detect_fps: Option<f64>,
    pub vlm_base_url: Option<String>,
    pub vlm_model: Option<String>,
    pub vlm_timeout: Option<u64>,
    pub vlm_coords: Option<String>,
    pub targets: Option<String>,
    pub preview_url: Option<String>,
}

/// The environment layer. Same two variables item-web's ask bar reads, so one
/// shell setup can drive both sides.
#[derive(Debug, Default, Clone)]
pub struct EnvOverrides {
    pub vlm_base_url: Option<String>,
    pub vlm_model: Option<String>,
}

impl EnvOverrides {
    pub fn from_env() -> Self {
        Self {
            vlm_base_url: std::env::var("ITEM_VLM_BASE_URL").ok(),
            vlm_model: std::env::var("ITEM_VLM_MODEL").ok(),
        }
    }
}

/// The resolved settings: pure data, safe to log, safe to unit-test.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeConfig {
    pub db: String,
    pub snapshots_dir: String,
    pub health_file: String,
    /// 0 = do not reload config.toml at runtime (phase 2).
    pub rescan_config_secs: u64,
    pub checkpoint_secs: u64,
    pub webhook_enabled: bool,
    pub listen: String,
    /// `None` = the preview endpoint stays off.
    pub preview_url: Option<String>,
    pub camera_id: String,
    pub detector: String,
    /// `None` = derive from [`RuntimeConfig::detect_fps`].
    pub detect_fps: Option<f64>,
    pub targets: String,
    pub model: String,
    pub input_size: usize,
    pub conf: f32,
    pub vlm_base_url: Option<String>,
    pub vlm_model: Option<String>,
    pub vlm_timeout: u64,
    pub vlm_coords: String,
}

impl RuntimeConfig {
    /// Merge the four layers. See the module doc for the precedence.
    ///
    /// `camera_id` deliberately does **not** come from config.toml: it is the
    /// identity the historical single-camera CLI paths attribute frames to, and
    /// changing it under an existing command line would rewrite where
    /// observations land. A `[[camera]]` row is consulted only once the id is
    /// known, to pick up that camera's `detector`/`detect_fps`/`targets`.
    pub fn resolve(file: Option<&Config>, cli: &CliOverrides, env: &EnvOverrides) -> Self {
        let daemon = file.map(|c| &c.daemon);
        let webhook = file.map(|c| &c.webhook);
        let row = cli
            .camera_id
            .as_deref()
            .and_then(|id| file.and_then(|c| c.camera.iter().find(|cam| cam.id == id)));

        Self {
            db: take(&cli.db, daemon.and_then(|d| d.db.as_deref()), defaults::DB),
            snapshots_dir: take(
                &cli.snapshots_dir,
                daemon.and_then(|d| d.snapshots_dir.as_deref()),
                defaults::SNAPSHOTS_DIR,
            ),
            health_file: daemon
                .and_then(|d| d.health_file.clone())
                .unwrap_or_else(|| defaults::HEALTH_FILE.to_string()),
            rescan_config_secs: daemon.and_then(|d| d.rescan_config_secs).unwrap_or(0),
            checkpoint_secs: daemon
                .and_then(|d| d.checkpoint_secs)
                .unwrap_or(defaults::CHECKPOINT_SECS),
            webhook_enabled: webhook.and_then(|w| w.enabled).unwrap_or(true),
            listen: take(
                &cli.listen,
                webhook.and_then(|w| w.listen.as_deref()),
                defaults::LISTEN,
            ),
            // An empty string means "off", whether it came from TOML or a flag.
            preview_url: cli
                .preview_url
                .clone()
                .or_else(|| webhook.and_then(|w| w.preview_url.clone()))
                .filter(|u| !u.is_empty()),
            camera_id: cli
                .camera_id
                .clone()
                .unwrap_or_else(|| defaults::CAMERA_ID.to_string()),
            detector: take(
                &cli.detector,
                row.and_then(|c| c.detector.as_deref()),
                defaults::DETECTOR,
            ),
            detect_fps: cli.detect_fps.or_else(|| row.and_then(|c| c.detect_fps)),
            targets: take(
                &cli.targets,
                row.and_then(|c| c.targets.as_deref()),
                default_targets(),
            ),
            model: cli
                .model
                .clone()
                .unwrap_or_else(|| defaults::MODEL.to_string()),
            input_size: cli.input_size.unwrap_or(defaults::INPUT_SIZE),
            conf: cli.conf.unwrap_or(defaults::CONF),
            vlm_base_url: cli
                .vlm_base_url
                .clone()
                .or_else(|| env.vlm_base_url.clone()),
            vlm_model: cli.vlm_model.clone().or_else(|| env.vlm_model.clone()),
            vlm_timeout: cli.vlm_timeout.unwrap_or(defaults::VLM_TIMEOUT_SECS),
            vlm_coords: cli
                .vlm_coords
                .clone()
                .unwrap_or_else(|| defaults::VLM_COORDS.to_string()),
        }
    }

    /// How often detection runs: an explicit value wins; otherwise 0.2 for the
    /// VLM sidecar (one HTTP round trip per detection) and 1.0 for everything
    /// else. This is the historical `effective_detect_fps` rule, moved into the
    /// resolved config so it can be unit-tested.
    pub fn detect_fps(&self) -> f64 {
        self.detect_fps.unwrap_or(if self.detector == "vlm" {
            defaults::DETECT_FPS_VLM
        } else {
            defaults::DETECT_FPS_LOCAL
        })
    }
}

/// CLI value, else file value, else the built-in default.
fn take(cli: &Option<String>, file: Option<&str>, default: &str) -> String {
    cli.clone()
        .or_else(|| file.map(str::to_string))
        .unwrap_or_else(|| default.to_string())
}

#[cfg(feature = "vlm")]
fn default_targets() -> &'static str {
    crate::detector::vlm::DEFAULT_TARGETS
}

#[cfg(not(feature = "vlm"))]
fn default_targets() -> &'static str {
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        toml::from_str(text).expect("test config parses")
    }

    fn cli() -> CliOverrides {
        CliOverrides::default()
    }

    #[test]
    fn parses_and_seeds() {
        let text = r#"
[[camera]]
id = "living"
url = "rtsp://x@host/path"
[camera.regions]
desk = [100.0, 100.0, 700.0, 400.0]
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.camera.len(), 1);
        assert_eq!(cfg.camera[0].url.as_deref(), Some("rtsp://x@host/path"));
        let store = item_core::store::Store::in_memory().unwrap();
        assert_eq!(cfg.seed_regions(&store).unwrap(), 1);
        assert_eq!(
            store.zone_for_point("living", (200.0, 200.0)).unwrap(),
            "desk"
        );
    }

    #[test]
    fn an_empty_config_reproduces_the_historical_cli_defaults() {
        let cfg = RuntimeConfig::resolve(None, &cli(), &EnvOverrides::default());
        assert_eq!(cfg.db, "data/items.db");
        assert_eq!(cfg.snapshots_dir, "data/snapshots");
        assert_eq!(cfg.health_file, "data/health.json");
        assert_eq!(cfg.listen, "127.0.0.1:8477");
        assert_eq!(cfg.camera_id, "rtsp-0");
        assert_eq!(cfg.detector, "yolo");
        assert_eq!(cfg.detect_fps, None);
        assert_eq!(cfg.detect_fps(), 1.0);
        assert_eq!(cfg.model, "models/yolov8n.onnx");
        assert_eq!(cfg.input_size, 640);
        assert_eq!(cfg.conf, 0.3);
        assert_eq!(cfg.vlm_timeout, 60);
        assert_eq!(cfg.vlm_coords, "norm1000");
        assert!(cfg.webhook_enabled);
        assert_eq!(cfg.preview_url, None);
        assert_eq!(cfg.vlm_base_url, None);
    }

    #[test]
    fn file_beats_defaults() {
        let file = parse(
            r#"
[daemon]
db = "from-file.db"
snapshots_dir = "from-file-snaps"
health_file = "from-file-health.json"
checkpoint_secs = 42

[webhook]
listen = "0.0.0.0:9000"
enabled = false

[[camera]]
id = "living"
detector = "vlm"
detect_fps = 0.5
targets = "keys,wallet"
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default());
        assert_eq!(cfg.db, "from-file.db");
        assert_eq!(cfg.snapshots_dir, "from-file-snaps");
        assert_eq!(cfg.health_file, "from-file-health.json");
        assert_eq!(cfg.checkpoint_secs, 42);
        assert_eq!(cfg.listen, "0.0.0.0:9000");
        assert!(!cfg.webhook_enabled);
        assert_eq!(
            cfg.detector, "yolo",
            "no --camera-id: no camera row matches"
        );
    }

    #[test]
    fn cli_beats_file() {
        let file = parse(
            r#"
[daemon]
db = "from-file.db"
snapshots_dir = "from-file-snaps"

[webhook]
listen = "0.0.0.0:9000"

[[camera]]
id = "living"
detector = "vlm"
detect_fps = 0.5
"#,
        );
        let cli = CliOverrides {
            db: Some("from-cli.db".into()),
            snapshots_dir: Some("from-cli-snaps".into()),
            listen: Some("127.0.0.1:1234".into()),
            camera_id: Some("living".into()),
            detector: Some("yolo".into()),
            detect_fps: Some(3.0),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(Some(&file), &cli, &EnvOverrides::default());
        assert_eq!(cfg.db, "from-cli.db");
        assert_eq!(cfg.snapshots_dir, "from-cli-snaps");
        assert_eq!(cfg.listen, "127.0.0.1:1234");
        assert_eq!(cfg.detector, "yolo");
        assert_eq!(cfg.detect_fps(), 3.0);
    }

    #[test]
    fn a_matching_camera_row_supplies_what_the_cli_left_silent() {
        let file = parse(
            r#"
[[camera]]
id = "living"
detector = "vlm"
detect_fps = 0.5
targets = "keys,wallet"

[[camera]]
id = "garage"
detector = "yolo"
"#,
        );
        let cli = CliOverrides {
            camera_id: Some("living".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(Some(&file), &cli, &EnvOverrides::default());
        assert_eq!(cfg.camera_id, "living");
        assert_eq!(cfg.detector, "vlm");
        assert_eq!(cfg.detect_fps(), 0.5);
        assert_eq!(cfg.targets, "keys,wallet");

        // An id that matches nothing must not silently borrow another camera.
        let cli = CliOverrides {
            camera_id: Some("nope".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(Some(&file), &cli, &EnvOverrides::default());
        assert_eq!(cfg.detector, "yolo");
        assert_eq!(cfg.detect_fps(), 1.0);
    }

    #[test]
    fn camera_id_never_comes_from_the_file() {
        let file = parse(
            r#"
[[camera]]
id = "living"
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default());
        assert_eq!(
            cfg.camera_id, "rtsp-0",
            "identity must stay stable for an unchanged command line"
        );
    }

    #[test]
    fn env_sits_between_file_and_cli() {
        let env = EnvOverrides {
            vlm_base_url: Some("http://from-env/v1".into()),
            vlm_model: Some("from-env-model".into()),
        };
        let cfg = RuntimeConfig::resolve(None, &cli(), &env);
        assert_eq!(cfg.vlm_base_url.as_deref(), Some("http://from-env/v1"));
        assert_eq!(cfg.vlm_model.as_deref(), Some("from-env-model"));

        let cli = CliOverrides {
            vlm_base_url: Some("http://from-cli/v1".into()),
            vlm_model: Some("from-cli-model".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(None, &cli, &env);
        assert_eq!(cfg.vlm_base_url.as_deref(), Some("http://from-cli/v1"));
        assert_eq!(cfg.vlm_model.as_deref(), Some("from-cli-model"));
    }

    #[test]
    fn detect_fps_falls_back_on_the_detector_choice() {
        let cli = CliOverrides {
            detector: Some("vlm".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(None, &cli, &EnvOverrides::default());
        assert_eq!(cfg.detect_fps(), 0.2);

        let cli = CliOverrides {
            detector: Some("yolo".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(None, &cli, &EnvOverrides::default());
        assert_eq!(cfg.detect_fps(), 1.0);
    }

    #[test]
    fn an_empty_preview_url_means_off() {
        let file = parse(
            r#"
[webhook]
preview_url = ""
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default());
        assert_eq!(cfg.preview_url, None);

        let file = parse(
            r#"
[webhook]
preview_url = "rtsp://cam/stream"
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default());
        assert_eq!(cfg.preview_url.as_deref(), Some("rtsp://cam/stream"));
    }
}
