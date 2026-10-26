//! Supervisión de procesos de servidor (ADR 0003) e instancia MariaDB (bloque 3a).

mod error;
pub mod mariadb;
mod os;
mod registry;
mod spawn;

pub use error::{ProcessError, Result};
pub use registry::{try_adopt, AdoptOutcome, ProcessRecord};
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
