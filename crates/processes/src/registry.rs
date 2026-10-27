use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{ProcessError, Result};

/// Registro persistente de un proceso lanzado por el supervisor (`{datos}/state/`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessRecord {
    pub id: String,
    pub exe: PathBuf,
    pub args: Vec<String>,
    pub pid: u32,
    /// Hora de creación del proceso (FILETIME en Windows; nanos desde epoch en Unix).
    pub creation_time: u64,
    pub registered_at_unix_ms: u64,
    /// Puerto TCP de la instancia (0 si no aplica).
    #[serde(default)]
    pub port: u16,
    /// Versión del runtime asociada.
    #[serde(default)]
    pub version: String,
}

impl ProcessRecord {
    pub fn state_path(data_dir: &Path, id: &str) -> PathBuf {
        data_dir.join("state").join(format!("{id}.json"))
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::state_path(data_dir, &self.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|error| {
            ProcessError::new(
                "no se pudo serializar el registro",
                error.to_string(),
                "reintenta el arranque",
            )
        })?;
        fs::write(&path, text)?;
        Ok(())
    }

    pub fn load(data_dir: &Path, id: &str) -> Result<Option<Self>> {
        let path = Self::state_path(data_dir, id);
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        let record: Self = serde_json::from_str(&text).map_err(|error| {
            ProcessError::new(
                "registro de proceso ilegible",
                error.to_string(),
                format!("borra o repara {}", path.display()),
            )
        })?;
        Ok(Some(record))
    }

    pub fn remove(data_dir: &Path, id: &str) -> Result<()> {
        let path = Self::state_path(data_dir, id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Resultado de intentar adoptar un registro al reiniciar el supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptOutcome {
    /// PID vivo, creación y ruta coinciden.
    Adopted(ProcessRecord),
    /// El PID ya no vive.
    Dead,
    /// Vive pero no es nuestro (hora o ruta distinta).
    Foreign { pid: u32, reason: String },
    /// No había registro.
    Missing,
}

/// Adopta solo si el PID vive, la hora de creación coincide y la ruta del exe es la nuestra.
pub fn try_adopt(
    data_dir: &Path,
    id: &str,
    living: impl Fn(u32) -> bool,
    creation_of: impl Fn(u32) -> Option<u64>,
    image_of: impl Fn(u32) -> Option<PathBuf>,
) -> Result<AdoptOutcome> {
    let Some(record) = ProcessRecord::load(data_dir, id)? else {
        return Ok(AdoptOutcome::Missing);
    };
    if !living(record.pid) {
        return Ok(AdoptOutcome::Dead);
    }
    let Some(creation) = creation_of(record.pid) else {
        return Ok(AdoptOutcome::Foreign {
            pid: record.pid,
            reason: "no se pudo leer la hora de creación del proceso".into(),
        });
    };
    if creation != record.creation_time {
        return Ok(AdoptOutcome::Foreign {
            pid: record.pid,
            reason: format!(
                "hora de creación distinta (registro {} vs proceso {creation})",
                record.creation_time
            ),
        });
    }
    let Some(image) = image_of(record.pid) else {
        return Ok(AdoptOutcome::Foreign {
            pid: record.pid,
            reason: "no se pudo leer la ruta del ejecutable".into(),
        });
    };
    if !paths_equal(&image, &record.exe) {
        return Ok(AdoptOutcome::Foreign {
            pid: record.pid,
            reason: format!(
                "ruta distinta (registro {} vs proceso {})",
                record.exe.display(),
                image.display()
            ),
        });
    }
    Ok(AdoptOutcome::Adopted(record))
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    let na = normalize_path(a);
    let nb = normalize_path(b);
    na == nb
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn adopcion_rechaza_pid_con_hora_de_creacion_distinta() {
        let dir = tempfile::tempdir().unwrap();
        let record = ProcessRecord {
            id: "svc".into(),
            exe: PathBuf::from(r"C:\Hostig\mariadbd.exe"),
            args: vec!["--defaults-file=x".into()],
            pid: 4242,
            creation_time: 1000,
            registered_at_unix_ms: now_unix_ms(),
            port: 3306,
            version: "11.4.13".into(),
        };
        record.save(dir.path()).unwrap();

        let creation = AtomicU64::new(9999);
        let outcome = try_adopt(
            dir.path(),
            "svc",
            |_| true,
            |_| Some(creation.load(Ordering::SeqCst)),
            |_| Some(PathBuf::from(r"C:\Hostig\mariadbd.exe")),
        )
        .unwrap();
        match outcome {
            AdoptOutcome::Foreign { pid, reason } => {
                assert_eq!(pid, 4242);
                assert!(reason.contains("hora de creación"), "{reason}");
            }
            other => panic!("se esperaba Foreign, fue {other:?}"),
        }
    }

    #[test]
    fn adopcion_acepta_cuando_todo_coincide() {
        let dir = tempfile::tempdir().unwrap();
        let record = ProcessRecord {
            id: "svc".into(),
            exe: PathBuf::from(r"C:\Hostig\mariadbd.exe"),
            args: vec![],
            pid: 7,
            creation_time: 42,
            registered_at_unix_ms: 1,
            port: 3310,
            version: "11.4.13".into(),
        };
        record.save(dir.path()).unwrap();
        let outcome = try_adopt(
            dir.path(),
            "svc",
            |_| true,
            |_| Some(42),
            |_| Some(PathBuf::from(r"C:\Hostig\mariadbd.exe")),
        )
        .unwrap();
        assert!(matches!(outcome, AdoptOutcome::Adopted(_)));
    }
}
