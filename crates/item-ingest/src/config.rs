//! Declarative camera/region config, loaded by the daemon and seeded into
//! the store at startup (regions are rows in SQLite, so everything else --
//! webhook, rtsp loop, query -- keeps reading from the store, not from TOML).
//!
//! config.toml shape:
//! ```toml
//! [[camera]]
//! id = "living"
//! url = "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102"
//!
//! [camera.regions]          # rects in that camera's pixel space
//! desk = [100, 100, 700, 400]
//! floor = [0, 400, 1280, 720]
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub camera: Vec<CameraConfig>,
}

#[derive(Debug, Deserialize)]
pub struct CameraConfig {
    pub id: String,
    /// RTSP url (may embed credentials); absent for webhook-fed cameras.
    #[serde(default)]
    pub url: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
