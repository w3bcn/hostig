//! Ajustes de Apache (ADR 0003 / 0005).

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const DEFAULT_APACHE_VERSION: &str = "2.4.69";
pub const DEFAULT_HTTP_PORT: u16 = 8080;
pub const DEFAULT_BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 16];
pub const DEFAULT_BACKOFF_WINDOW_SECS: u64 = 600;
pub const DEFAULT_MAX_RESTARTS: u32 = 5;
pub const DEFAULT_START_TOTAL_SECS: u64 = 60;
pub const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApacheSettings {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_backoff")]
    pub backoff_secs: Vec<u64>,
    #[serde(default = "default_backoff_window")]
    pub backoff_window_secs: u64,
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
    #[serde(default = "default_start_total")]
    pub start_total_secs: u64,
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
}

fn default_version() -> String {
    DEFAULT_APACHE_VERSION.into()
}
fn default_http_port() -> u16 {
    DEFAULT_HTTP_PORT
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
fn default_start_total() -> u64 {
    DEFAULT_START_TOTAL_SECS
}
fn default_health_interval() -> u64 {
    DEFAULT_HEALTH_INTERVAL_SECS
}

impl Default for ApacheSettings {
    fn default() -> Self {
        Self {
            version: DEFAULT_APACHE_VERSION.into(),
            http_port: DEFAULT_HTTP_PORT,
            backoff_secs: DEFAULT_BACKOFF_SECS.to_vec(),
            backoff_window_secs: DEFAULT_BACKOFF_WINDOW_SECS,
            max_restarts: DEFAULT_MAX_RESTARTS,
            start_total_secs: DEFAULT_START_TOTAL_SECS,
            health_interval_secs: DEFAULT_HEALTH_INTERVAL_SECS,
        }
    }
}

impl ApacheSettings {
    pub fn start_total(&self) -> Duration {
        Duration::from_secs(self.start_total_secs.max(1))
    }

    pub fn backoff_window(&self) -> Duration {
        Duration::from_secs(self.backoff_window_secs.max(1))
    }

    pub fn health_interval(&self) -> Duration {
        Duration::from_secs(self.health_interval_secs.max(1))
    }
}
