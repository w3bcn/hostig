//! Plazos y vigilancia de MariaDB (manifiesto global, ADR 0012).

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{ProcessError, Result};

/// Valores por defecto del ADR 0012 / 0003.
pub const DEFAULT_HANG_SECS: u64 = 60;
pub const DEFAULT_START_TOTAL_SECS: u64 = 15 * 60;
pub const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 5;
pub const DEFAULT_HEALTH_FAILURES: u32 = 3;
pub const DEFAULT_BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 16];
pub const DEFAULT_BACKOFF_WINDOW_SECS: u64 = 600;
pub const DEFAULT_MAX_RESTARTS: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MariadbSettings {
    /// Segundos sin líneas nuevas en el log → cuelgue (ADR 0012).
    #[serde(default = "default_hang")]
    pub hang_secs: u64,
    /// Tope total de arranque aunque el log avance.
    #[serde(default = "default_total")]
    pub start_total_secs: u64,
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    #[serde(default = "default_health_failures")]
    pub health_failures: u32,
    #[serde(default = "default_backoff")]
    pub backoff_secs: Vec<u64>,
    #[serde(default = "default_backoff_window")]
    pub backoff_window_secs: u64,
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
}

fn default_hang() -> u64 {
    DEFAULT_HANG_SECS
}
fn default_total() -> u64 {
    DEFAULT_START_TOTAL_SECS
}
fn default_health_interval() -> u64 {
    DEFAULT_HEALTH_INTERVAL_SECS
}
fn default_health_failures() -> u32 {
    DEFAULT_HEALTH_FAILURES
}
fn default_backoff() -> Vec<u64> {
    DEFAULT_BACKOFF_SECS.to_vec()
}
fn default_backoff_window() -> u64 {
    DEFAULT_BACKOFF_WINDOW_SECS
}
fn default_max_restarts() -> u32 {
    DEFAULT_MAX_RESTARTS
}

impl Default for MariadbSettings {
    fn default() -> Self {
        Self {
            hang_secs: DEFAULT_HANG_SECS,
            start_total_secs: DEFAULT_START_TOTAL_SECS,
            health_interval_secs: DEFAULT_HEALTH_INTERVAL_SECS,
            health_failures: DEFAULT_HEALTH_FAILURES,
            backoff_secs: DEFAULT_BACKOFF_SECS.to_vec(),
            backoff_window_secs: DEFAULT_BACKOFF_WINDOW_SECS,
            max_restarts: DEFAULT_MAX_RESTARTS,
        }
    }
}

impl MariadbSettings {
    pub fn hang(&self) -> Duration {
        Duration::from_secs(self.hang_secs.max(1))
    }

    pub fn start_total(&self) -> Duration {
        Duration::from_secs(self.start_total_secs.max(1))
    }

    pub fn health_interval(&self) -> Duration {
        Duration::from_secs(self.health_interval_secs.max(1))
    }

    pub fn backoff_window(&self) -> Duration {
        Duration::from_secs(self.backoff_window_secs.max(1))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GlobalManifest {
    #[serde(default)]
    pub mariadb: MariadbSettings,
}

impl GlobalManifest {
    pub fn path(data_dir: &Path) -> std::path::PathBuf {
        data_dir.join("config").join("global.toml")
    }

    /// Carga `{datos}/config/global.toml` o usa defectos ADR.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)?;
        toml::from_str(&text).map_err(|error| {
            ProcessError::new(
                "manifiesto global ilegible",
                error.to_string(),
                format!("corrige {}", path.display()),
            )
        })
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(|error| {
            ProcessError::new(
                "no se pudo serializar el manifiesto global",
                error.to_string(),
                "reintenta",
            )
        })?;
        fs::write(path, text.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carga_defectos_sin_archivo() {
        let dir = tempfile::tempdir().unwrap();
        let m = GlobalManifest::load(dir.path()).unwrap();
        assert_eq!(m.mariadb.hang_secs, 60);
        assert_eq!(m.mariadb.start_total_secs, 900);
    }

    #[test]
    fn roundtrip_toml() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = GlobalManifest::default();
        m.mariadb.hang_secs = 2;
        m.mariadb.start_total_secs = 30;
        m.save(dir.path()).unwrap();
        let loaded = GlobalManifest::load(dir.path()).unwrap();
        assert_eq!(loaded.mariadb.hang_secs, 2);
        assert_eq!(loaded.mariadb.start_total_secs, 30);
    }
}
