//! Supervisión de procesos de servidor (ADR 0003) e instancia MariaDB (bloque 3a).

mod error;
pub mod job;
pub mod mariadb;
mod os;
mod registry;
mod spawn;

pub use error::{ProcessError, Result};
pub use job::{
    allow_job_without_breakaway, current_job_spawn_policy, decide_after_breakaway_denied,
    BreakawayFallback, JobSpawnPolicy, ALLOW_JOB_WITHOUT_BREAKAWAY_ENV,
};
pub use registry::{try_adopt, AdoptOutcome, ProcessRecord};
#[cfg(windows)]
pub use spawn::spawn_detached_windows;
pub use spawn::{emergency_kill, spawn_server, SpawnSpec, SpawnedProcess};

/// El módulo está implementado (fase 1 bloque 3a).
pub fn implemented() -> bool {
    true
}

#[cfg(test)]
mod platform_skip_tests {
    #[test]
    fn modulo_implementado() {
        assert!(super::implemented());
    }
}
