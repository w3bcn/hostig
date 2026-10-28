//! Supervisión de procesos de servidor (ADR 0003): MariaDB, PHP y Apache.

pub mod apache;
mod error;
pub mod job;
pub mod mariadb;
mod os;
pub mod php;
mod registry;
mod spawn;
pub mod stack;
mod vc;

pub use error::{ProcessError, Result};
pub use job::{
    allow_job_without_breakaway, current_job_spawn_policy, decide_after_breakaway_denied,
    BreakawayFallback, JobSpawnPolicy, ALLOW_JOB_WITHOUT_BREAKAWAY_ENV,
};
pub use registry::{try_adopt, AdoptOutcome, ProcessRecord};
#[cfg(windows)]
pub use spawn::spawn_detached_windows;
pub use spawn::{emergency_kill, spawn_server, SpawnSpec, SpawnedProcess};
pub use stack::{StackJobStatus, StackService, StackStartRequest, StackStatus};
pub use vc::{verify_binary_starts, STATUS_DLL_NOT_FOUND};

/// El módulo está implementado (fase 1 bloque 4).
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
