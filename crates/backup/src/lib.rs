//! Volcados lógicos de MariaDB antes de parar (ADR 0007).

mod dump;
mod error;
mod meta;
mod space;

pub use dump::{
    ensure_backups_before_stop, list_backups, list_user_schemas, BackupEntry, DumpContext,
};
pub use error::{BackupError, Result};
pub use meta::{
    backup_dir, latest_good_meta, load_meta, meta_path_for, needs_dump, write_status_vars,
    BackupMeta, BackupSettings, ServerSnapshot, BOOT_ID_TOLERANCE_SECS,
    DEFAULT_BACKUP_MAX_AGE_SECS, DEFAULT_BACKUP_RETENTION, HANDLER_WRITE_VARS, SYSTEM_SCHEMAS,
    WRITE_COM_VARS,
};

/// El módulo de copias está implementado (fase 1 bloque 5).
pub fn implemented() -> bool {
    true
}

/// Frase de confirmación exigida por `stop --without-backup`.
pub const WITHOUT_BACKUP_CONFIRM: &str = "SIN-COPIA";

#[cfg(test)]
mod tests {
    #[test]
    fn modulo_implementado() {
        assert!(super::implemented());
    }
}
