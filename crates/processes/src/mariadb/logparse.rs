use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::Result;

/// Offset actual del error log (para leer solo el tramo nuevo tras el arranque).
pub fn new_log_offset(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    Ok(std::fs::metadata(path)?.len())
}

pub fn read_log_since(path: &Path, offset: u64) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf)
}

pub fn log_contains_ready(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("ready for connections")
}

pub fn log_contains_shutdown(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("shutdown complete")
        || lower.contains("mysqld: shutdown complete")
        || (lower.contains("mariadbd: ready for connections") && lower.contains("shutdown"))
        || (lower.contains("shutdown") && lower.contains("complete"))
}

pub fn log_contains_crash_recovery(text: &str) -> bool {
    text.to_ascii_lowercase()
        .contains("starting crash recovery")
}

/// Regla del LSN / corrupción (ADR 0003): marca Failed aunque el puerto responda.
pub fn log_contains_fatal_innodb(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("log sequence number is in the future") || lower.contains("may be corrupt")
}

pub fn last_nonempty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty())
}

/// Busca volcados nuevos (mysqld.dmp / mariadbd.dmp) creados tras `since` (unix secs).
pub fn new_dump_files(datadir: &Path, since_unix: u64) -> Result<Vec<std::path::PathBuf>> {
    let mut found = Vec::new();
    if !datadir.is_dir() {
        return Ok(found);
    }
    for entry in std::fs::read_dir(datadir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !(name.ends_with(".dmp") || name == "mysqld.dmp" || name.contains("mariadbd.dmp")) {
            continue;
        }
        let meta = entry.metadata()?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if modified >= since_unix.saturating_sub(2) {
            found.push(entry.path());
        }
    }
    // También en el directorio de la instancia (a veces el dmp cae junto al binario/cwd).
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detecta_ready_y_apagado_en_el_error_log() {
        let ready =
            "2026-10-26 12:00:00 0 [Note] mysqld: ready for connections.\nVersion: '11.4.13'";
        assert!(log_contains_ready(ready));
        assert!(!log_contains_shutdown(ready));

        let stop = "2026-10-26 12:05:00 0 [Note] mysqld: Shutdown complete\n";
        assert!(log_contains_shutdown(stop));
        assert!(!log_contains_ready(stop));
    }

    #[test]
    fn detecta_lsn_y_recovery() {
        assert!(log_contains_fatal_innodb(
            "InnoDB: log sequence number is in the future"
        ));
        assert!(log_contains_fatal_innodb("tablespace may be corrupt"));
        assert!(log_contains_crash_recovery("Starting crash recovery"));
        assert!(!log_contains_fatal_innodb("ready for connections"));
    }

    #[test]
    fn lee_solo_el_tramo_nuevo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("error.log");
        std::fs::write(&path, "linea-antigua\n").unwrap();
        let offset = new_log_offset(&path).unwrap();
        std::fs::write(&path, "linea-antigua\nready for connections\n").unwrap();
        let chunk = read_log_since(&path, offset).unwrap();
        assert!(chunk.contains("ready for connections"));
        assert!(log_contains_ready(&chunk));
    }
}
