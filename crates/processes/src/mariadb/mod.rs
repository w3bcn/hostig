//! Instancia MariaDB: config, init, start, stop (bloque 3a; sin preflight 3b ni dump 5).

mod config;
mod init;
mod logparse;
mod service;

pub use config::{generate_my_ini, InstancePaths, DEFAULT_INSTANCE_ID, DEFAULT_PORT};
pub use init::ensure_initialized;
pub use logparse::{log_contains_ready, log_contains_shutdown, new_log_offset, read_log_since};
pub use service::{
    DbJobStatus, DbService, DbState, DbStatus, StartRequest, DEFAULT_MARIADB_VERSION,
};
