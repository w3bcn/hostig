//! Apache httpd (ADR 0003 / 0005).

mod config;
mod service;
mod settings;

pub use config::{
    find_server_root, httpd_in, regenerate_apache_config, ApachePaths, HealthDbParams,
    DEFAULT_APACHE_ID,
};
pub use service::{
    health_ok, ApacheJobStatus, ApacheService, ApacheStartRequest, ApacheState, ApacheStatus,
    TEST_HTTPD_ENV, WINDOWS_SHUTDOWN_METHOD,
};
pub use settings::{ApacheSettings, DEFAULT_APACHE_VERSION, DEFAULT_HTTP_PORT};
