//! Cuarentena: mueve, nunca borra (ADR 0012).

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{ProcessError, Result};
use crate::mariadb::config::InstancePaths;
use crate::mariadb::preflight::{run_preflight, PreflightIssue, PreflightOutcome};

const MANIFEST_NAME: &str = "quarantine-manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuarantineEntry {
    pub original: PathBuf,
    pub stored_as: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuarantineManifest {
    pub timestamp: String,
    pub instance_id: String,
    pub entries: Vec<QuarantineEntry>,
}

/// Mueve los archivos del preflight fallido a `quarantine/{timestamp}/`.
pub fn quarantine_detected(paths: &InstancePaths) -> Result<QuarantineManifest> {
    let outcome = run_preflight(paths)?;
    let issues = match outcome {
        PreflightOutcome::Ok => {
            return Err(ProcessError::new(
                "nada que cuarentenar",
                "el preflight no encontró anomalías",
                "no hace falta hostig db quarantine",
            ));
        }
        PreflightOutcome::Failed { issues } => issues,
    };
    quarantine_issues(paths, &issues)
}

pub fn quarantine_issues(
    paths: &InstancePaths,
    issues: &[PreflightIssue],
) -> Result<QuarantineManifest> {
    let ts = timestamp_now();
    let dest_root = paths.root.join("quarantine").join(&ts);
    fs::create_dir_all(&dest_root)?;

    let mut entries = Vec::new();
    for (i, issue) in issues.iter().enumerate() {
        if !issue.path.exists() {
            continue;
        }
        let name = issue
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("item-{i}"));
        let stored = format!("{i:02}_{name}");
        let dest = dest_root.join(&stored);
        fs::rename(&issue.path, &dest).map_err(|error| {
            ProcessError::new(
                "no se pudo mover a cuarentena",
                format!("{} → {}: {error}", issue.path.display(), dest.display()),
                "revisa permisos del datadir",
            )
        })?;
        entries.push(QuarantineEntry {
            original: issue.path.clone(),
            stored_as: stored,
        });
    }

    let manifest = QuarantineManifest {
        timestamp: ts,
        instance_id: paths.id.clone(),
        entries,
    };
    let text = serde_json::to_string_pretty(&manifest).map_err(|error| {
        ProcessError::new(
            "no se pudo serializar el manifiesto de cuarentena",
            error.to_string(),
            "reintenta",
        )
    })?;
    fs::write(dest_root.join(MANIFEST_NAME), text.as_bytes())?;
    Ok(manifest)
}

pub fn list_quarantines(paths: &InstancePaths) -> Result<Vec<String>> {
    let root = paths.root.join("quarantine");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut stamps = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.path().is_dir() && entry.path().join(MANIFEST_NAME).is_file() {
            stamps.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    stamps.sort();
    stamps.reverse();
    Ok(stamps)
}

pub fn restore_quarantine(paths: &InstancePaths, timestamp: &str) -> Result<QuarantineManifest> {
    let dest_root = paths.root.join("quarantine").join(timestamp);
    let manifest_path = dest_root.join(MANIFEST_NAME);
    if !manifest_path.is_file() {
        return Err(ProcessError::new(
            "cuarentena desconocida",
            format!("no hay {}", manifest_path.display()),
            "usa hostig db quarantine list",
        ));
    }
    let text = fs::read_to_string(&manifest_path)?;
    let manifest: QuarantineManifest = serde_json::from_str(&text).map_err(|error| {
        ProcessError::new(
            "manifiesto de cuarentena ilegible",
            error.to_string(),
            "inspecciona la carpeta quarantine a mano",
        )
    })?;
    for entry in &manifest.entries {
        let src = dest_root.join(&entry.stored_as);
        if !src.exists() {
            continue;
        }
        if let Some(parent) = entry.original.parent() {
            fs::create_dir_all(parent)?;
        }
        if entry.original.exists() {
            return Err(ProcessError::new(
                "no se puede restaurar",
                format!("{} ya existe", entry.original.display()),
                "muévelo o revísalo a mano antes de restaurar",
            ));
        }
        fs::rename(&src, &entry.original)?;
    }
    Ok(manifest)
}

fn timestamp_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Compacto y ordenable; suficiente para tests y CLI.
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mariadb::preflight::run_preflight;

    #[test]
    fn cuarentena_y_restore() {
        let dir = tempfile::tempdir().unwrap();
        let paths = InstancePaths::new(dir.path(), "t");
        paths.ensure_dirs().unwrap();
        let bad = paths.datadir.join("multi-master.info");
        fs::write(&bad, "2026-01-01 [Note] bad\n").unwrap();
        assert!(matches!(
            run_preflight(&paths).unwrap(),
            PreflightOutcome::Failed { .. }
        ));
        let m = quarantine_detected(&paths).unwrap();
        assert!(!bad.exists());
        assert!(matches!(
            run_preflight(&paths).unwrap(),
            PreflightOutcome::Ok
        ));
        restore_quarantine(&paths, &m.timestamp).unwrap();
        assert!(bad.exists());
    }

    #[test]
    fn restore_no_sobrescribe_si_ya_existe() {
        let dir = tempfile::tempdir().unwrap();
        let paths = InstancePaths::new(dir.path(), "t");
        paths.ensure_dirs().unwrap();
        let bad = paths.datadir.join("multi-master.info");
        fs::write(&bad, "2026-01-01 [Note] bad\n").unwrap();
        let m = quarantine_detected(&paths).unwrap();
        assert!(!bad.exists());
        // Alguien vuelve a crear el archivo en la ruta original.
        fs::write(&bad, "otro contenido\n").unwrap();
        let err = restore_quarantine(&paths, &m.timestamp).expect_err("no sobrescribir");
        assert!(err.cause.contains("ya existe"), "cause={}", err.cause);
        assert!(
            err.action.contains("mano") || err.action.contains("muévelo"),
            "action={}",
            err.action
        );
        // El archivo en destino no se tocó.
        assert_eq!(fs::read_to_string(&bad).unwrap().trim(), "otro contenido");
        // Y lo cuarentenado sigue en quarantine.
        let stored = paths
            .root
            .join("quarantine")
            .join(&m.timestamp)
            .join("00_multi-master.info");
        assert!(stored.is_file());
    }
}
