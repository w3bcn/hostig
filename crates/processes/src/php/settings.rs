//! Ajustes del pool PHP (ADR 0005 / 0003).

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const DEFAULT_POOL_SIZE: u32 = 4;
pub const DEFAULT_FCGI_MAX_REQUESTS: u32 = 500;
pub const DEFAULT_PHP_VERSION: &str = "8.4.27";
pub const DEFAULT_TIMEZONE: &str = "UTC";
pub const DEFAULT_BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 16];
pub const DEFAULT_BACKOFF_WINDOW_SECS: u64 = 600;
pub const DEFAULT_MAX_RESTARTS: u32 = 5;
pub const DEFAULT_START_TOTAL_SECS: u64 = 60;
pub const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhpSettings {
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,
    #[serde(default = "default_max_requests")]
    pub fcgi_max_requests: u32,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
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

fn default_pool_size() -> u32 {
    DEFAULT_POOL_SIZE
}
fn default_max_requests() -> u32 {
    DEFAULT_FCGI_MAX_REQUESTS
}
fn default_version() -> String {
    DEFAULT_PHP_VERSION.into()
}
fn default_timezone() -> String {
    DEFAULT_TIMEZONE.into()
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

impl Default for PhpSettings {
    fn default() -> Self {
        Self {
            pool_size: DEFAULT_POOL_SIZE,
            fcgi_max_requests: DEFAULT_FCGI_MAX_REQUESTS,
            version: DEFAULT_PHP_VERSION.into(),
            timezone: DEFAULT_TIMEZONE.into(),
            backoff_secs: DEFAULT_BACKOFF_SECS.to_vec(),
            backoff_window_secs: DEFAULT_BACKOFF_WINDOW_SECS,
            max_restarts: DEFAULT_MAX_RESTARTS,
            start_total_secs: DEFAULT_START_TOTAL_SECS,
            health_interval_secs: DEFAULT_HEALTH_INTERVAL_SECS,
        }
    }
}

impl PhpSettings {
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
