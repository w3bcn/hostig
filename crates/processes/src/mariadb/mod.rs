//! Instancia MariaDB: config, init, start/stop, preflight y protecciones (3a+3b).

mod config;
mod init;
mod logparse;
mod preflight;
mod quarantine;
mod service;
mod settings;

pub use config::{
    generate_my_ini, regenerate_my_ini, ConfigRegen, InstancePaths, DEFAULT_INSTANCE_ID,
    DEFAULT_PORT,
};
pub use init::ensure_initialized;
pub use logparse::{
    last_nonempty_line, log_contains_crash_recovery, log_contains_fatal_innodb, log_contains_ready,
    log_contains_shutdown, new_log_offset, read_log_since,
};
pub use preflight::{run_preflight, PreflightIssue, PreflightOutcome};
pub use quarantine::{
    list_quarantines, quarantine_detected, restore_quarantine, QuarantineManifest,
};
pub use service::{
    DbJobStatus, DbService, DbState, DbStatus, StartRequest, DEFAULT_MARIADB_VERSION,
    TEST_DB_PROBE_ENV, TEST_MARIADBD_ENV,
};
pub use settings::{GlobalManifest, MariadbSettings};
