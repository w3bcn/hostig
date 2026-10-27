//! Preflight del datadir antes de arrancar (ADR 0012).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{ProcessError, Result};
use crate::mariadb::config::InstancePaths;

/// Anomalía detectada en el preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightIssue {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightOutcome {
    Ok,
    Failed { issues: Vec<PreflightIssue> },
}

impl PreflightOutcome {
    pub fn to_error(&self) -> Option<ProcessError> {
        match self {
            PreflightOutcome::Ok => None,
            PreflightOutcome::Failed { issues } => {
                let detail = issues
                    .iter()
                    .map(|i| format!("{} ({})", i.path.display(), i.reason))
                    .collect::<Vec<_>>()
                    .join("; ");
                Some(ProcessError::new(
                    "preflight de MariaDB falló",
                    detail,
                    "hostig db quarantine para mover los archivos anómalos",
                ))
            }
        }
    }
}

/// Inspecciona el datadir: multi-master.info y archivos de replicación.
pub fn run_preflight(paths: &InstancePaths) -> Result<PreflightOutcome> {
    let mut issues = Vec::new();
    if !paths.datadir.is_dir() {
        return Ok(PreflightOutcome::Ok);
    }

    let mm = paths.datadir.join("multi-master.info");
    if mm.is_file() {
        if let Some(reason) = multi_master_problem(&mm)? {
            issues.push(PreflightIssue {
                path: mm.clone(),
                reason,
            });
        }
    }

    // Hostig no configura replicación: cualquier archivo de replicación es anomalía.
    for entry in fs::read_dir(&paths.datadir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if is_replication_orphan_name(&name_s) {
            // multi-master.info ya se evaluó arriba; si es válido y vacío no es huérfano de lista.
            if name_s == "multi-master.info" {
                continue;
            }
            issues.push(PreflightIssue {
                path: entry.path(),
                reason: "archivo de replicación en instancia Hostig (anomalía)".into(),
            });
        }
    }

    // multi-master.info válido no vacío también es anomalía en Hostig (no configuramos réplica),
    // pero el ADR solo exige vacío o nombres válidos. Si tiene nombres válidos, aún es raro;
    // el ADR: "en una instancia de Hostig, cualquiera es anomalía" para archivos de réplica.
    // multi-master.info con nombres válidos = configuración de réplica → anomalía.
    if mm.is_file() && multi_master_problem(&mm)?.is_none() {
        let text = fs::read_to_string(&mm)?;
        if !text.trim().is_empty() {
            issues.push(PreflightIssue {
                path: mm,
                reason: "multi-master.info no vacío: Hostig no configura replicación".into(),
            });
        }
    }

    if issues.is_empty() {
        Ok(PreflightOutcome::Ok)
    } else {
        Ok(PreflightOutcome::Failed { issues })
    }
}

fn is_replication_orphan_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "multi-master.info"
        || lower.starts_with("master-") && lower.ends_with(".info")
        || lower.starts_with("relay-log-") && lower.ends_with(".info")
        || lower.starts_with("mysql-relay-bin-")
        || lower.starts_with("mysql-relay-bin.")
}

/// `None` si el contenido es aceptable (vacío o solo nombres de conexión válidos).
fn multi_master_problem(path: &Path) -> Result<Option<String>> {
    let text = fs::read_to_string(path)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains(' ')
            || line.contains('[')
            || line.to_ascii_lowercase().contains("note")
            || line.to_ascii_lowercase().contains("error")
            || looks_like_timestamp(line)
        {
            return Ok(Some(format!(
                "línea {}: contenido inválido (no es nombre de conexión)",
                idx + 1
            )));
        }
        // Nombre de conexión: sin espacios, alfanumérico / _ / -
        if !line
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Ok(Some(format!(
                "línea {}: nombre de conexión inválido",
                idx + 1
            )));
        }
    }
    Ok(None)
}

fn looks_like_timestamp(line: &str) -> bool {
    // 2026-10-21 o 12:00:00
    let bytes = line.as_bytes();
    if bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        return true;
    }
    line.contains(':') && line.chars().filter(|c| c.is_ascii_digit()).count() >= 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn multi_master_corrupto_detectado() {
        let dir = tempdir().unwrap();
        let paths = InstancePaths::new(dir.path(), "t");
        paths.ensure_dirs().unwrap();
        fs::write(
            paths.datadir.join("multi-master.info"),
            "2026-10-21 12:00:00 [Note] something\n",
        )
        .unwrap();
        let out = run_preflight(&paths).unwrap();
        assert!(matches!(out, PreflightOutcome::Failed { .. }));
    }

    #[test]
    fn datadir_limpio_ok() {
        let dir = tempdir().unwrap();
        let paths = InstancePaths::new(dir.path(), "t");
        paths.ensure_dirs().unwrap();
        assert_eq!(run_preflight(&paths).unwrap(), PreflightOutcome::Ok);
    }

    #[test]
    fn relay_huérfano_detectado() {
        let dir = tempdir().unwrap();
        let paths = InstancePaths::new(dir.path(), "t");
        paths.ensure_dirs().unwrap();
        fs::write(paths.datadir.join("mysql-relay-bin.000001"), b"x").unwrap();
        let out = run_preflight(&paths).unwrap();
        assert!(matches!(out, PreflightOutcome::Failed { .. }));
    }
}
