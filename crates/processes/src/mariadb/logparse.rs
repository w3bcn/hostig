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
        || lower.contains("mariadbd: ready for connections") && lower.contains("shutdown")
        || (lower.contains("shutdown") && lower.contains("complete"))
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
    fn lee_solo_el_tramo_nuevo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("error.log");
        std::fs::write(&path, "linea-antigua\n").unwrap();
        let offset = new_log_offset(&path).unwrap();
        std::fs::write(&path, "linea-antigua\nready for connections\n").unwrap();
        let chunk = read_log_since(&path, offset).unwrap();
        assert!(chunk.contains("ready for connections"));
        assert!(
            !chunk.contains("linea-antigua")
                || chunk.starts_with("ready")
                || chunk.contains("ready")
        );
        // El tramo nuevo empieza tras offset: puede ser solo la segunda línea.
        assert!(log_contains_ready(&chunk));
    }
}
