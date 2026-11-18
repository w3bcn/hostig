//! Metadata de comparación y decisión de volcado (ADR 0007).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{BackupError, Result};

pub const DEFAULT_BACKUP_RETENTION: usize = 10;
pub const DEFAULT_BACKUP_MAX_AGE_SECS: u64 = 24 * 60 * 60;

pub const SYSTEM_SCHEMAS: &[&str] = &["mysql", "sys", "information_schema", "performance_schema"];

/// Contadores Com_* de escritura y DDL usados para detectar cambios.
pub const WRITE_COM_VARS: &[&str] = &[
    "Com_insert",
    "Com_update",
    "Com_delete",
    "Com_replace",
    "Com_create_table",
    "Com_alter_table",
    "Com_drop_table",
    "Com_truncate",
    "Com_rename_table",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupSettings {
    pub retention: usize,
    pub max_age_secs: u64,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            retention: DEFAULT_BACKUP_RETENTION,
            max_age_secs: DEFAULT_BACKUP_MAX_AGE_SECS,
        }
    }
}

/// Instantánea del servidor en el momento de la copia (o de la decisión).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerSnapshot {
    /// Instante de arranque aproximado: reloj del supervisor − Uptime.
    pub boot_id_unix: u64,
    pub uptime_secs: u64,
    pub com_counters: BTreeMap<String, u64>,
    /// Hay tablas de usuario con motor distinto de InnoDB.
    pub has_non_innodb: bool,
}

/// Metadata guardada junto a cada copia correcta.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupMeta {
    pub schema: String,
    pub created_unix: u64,
    pub boot_id_unix: u64,
    pub com_counters: BTreeMap<String, u64>,
    pub has_non_innodb: bool,
}

impl BackupMeta {
    pub fn from_snapshot(schema: &str, snap: &ServerSnapshot, created_unix: u64) -> Self {
        Self {
            schema: schema.to_string(),
            created_unix,
            boot_id_unix: snap.boot_id_unix,
            com_counters: snap.com_counters.clone(),
            has_non_innodb: snap.has_non_innodb,
        }
    }
}

pub fn backup_dir(data_dir: &Path, instance_id: &str, schema: &str) -> PathBuf {
    data_dir.join("backups").join(instance_id).join(schema)
}

pub fn meta_path_for(sql_gz: &Path) -> PathBuf {
    let mut s = sql_gz.as_os_str().to_os_string();
    s.push(".meta.json");
    PathBuf::from(s)
}

pub fn load_meta(path: &Path) -> Result<BackupMeta> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|error| {
        BackupError::new(
            "metadata de copia ilegible",
            error.to_string(),
            format!("revisa o borra {}", path.display()),
        )
    })
}

pub fn save_meta(path: &Path, meta: &BackupMeta) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(meta).map_err(|error| {
        BackupError::new(
            "no se pudo serializar la metadata de copia",
            error.to_string(),
            "reintenta el volcado",
        )
    })?;
    fs::write(path, text.as_bytes())?;
    Ok(())
}

/// Última copia correcta (con `.meta.json` válido) por mtime/nombre.
pub fn latest_good_meta(
    data_dir: &Path,
    instance_id: &str,
    schema: &str,
) -> Result<Option<(PathBuf, BackupMeta)>> {
    let dir = backup_dir(data_dir, instance_id, schema);
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut best: Option<(PathBuf, BackupMeta, u64)> = None;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".sql.gz") {
            continue;
        }
        let meta_path = meta_path_for(&path);
        if !meta_path.is_file() {
            continue;
        }
        let Ok(meta) = load_meta(&meta_path) else {
            continue;
        };
        let stamp = meta.created_unix;
        let replace = match &best {
            None => true,
            Some((_, _, prev)) => stamp >= *prev,
        };
        if replace {
            best = Some((path, meta, stamp));
        }
    }
    Ok(best.map(|(p, m, _)| (p, m)))
}

/// Decide si hace falta volcar según ADR 0007. Ante la duda → sí.
pub fn needs_dump(
    snap: Option<&ServerSnapshot>,
    previous: Option<&BackupMeta>,
    settings: &BackupSettings,
    now_unix: u64,
) -> bool {
    let Some(snap) = snap else {
        // No se pudo leer el servidor: ante la duda, volcar.
        return true;
    };
    let Some(prev) = previous else {
        return true;
    };
    if now_unix.saturating_sub(prev.created_unix) > settings.max_age_secs {
        return true;
    }
    if snap.boot_id_unix != prev.boot_id_unix {
        return true;
    }
    if snap.has_non_innodb {
        return true;
    }
    for key in WRITE_COM_VARS {
        let cur = snap.com_counters.get(*key).copied().unwrap_or(0);
        let old = prev.com_counters.get(*key).copied().unwrap_or(0);
        if cur > old {
            return true;
        }
        // Contador ausente ahora pero presente antes: duda → volcar.
        if prev.com_counters.contains_key(*key) && !snap.com_counters.contains_key(*key) {
            return true;
        }
    }
    false
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(boot: u64, insert: u64, non_innodb: bool) -> ServerSnapshot {
        let mut com = BTreeMap::new();
        com.insert("Com_insert".into(), insert);
        for k in WRITE_COM_VARS {
            com.entry((*k).to_string()).or_insert(0);
        }
        ServerSnapshot {
            boot_id_unix: boot,
            uptime_secs: 10,
            com_counters: com,
            has_non_innodb: non_innodb,
        }
    }

    #[test]
    fn sin_copia_previa_vuelca() {
        let s = snap(100, 0, false);
        assert!(needs_dump(Some(&s), None, &BackupSettings::default(), 200));
    }

    #[test]
    fn sin_escrituras_y_reciente_no_vuelca() {
        let s = snap(100, 5, false);
        let prev = BackupMeta::from_snapshot("app", &s, 150);
        assert!(!needs_dump(
            Some(&s),
            Some(&prev),
            &BackupSettings::default(),
            200
        ));
    }

    #[test]
    fn contador_sube_vuelca() {
        let prev_snap = snap(100, 5, false);
        let prev = BackupMeta::from_snapshot("app", &prev_snap, 150);
        let cur = snap(100, 6, false);
        assert!(needs_dump(
            Some(&cur),
            Some(&prev),
            &BackupSettings::default(),
            200
        ));
    }

    #[test]
    fn reinicio_vuelca() {
        let prev_snap = snap(100, 5, false);
        let prev = BackupMeta::from_snapshot("app", &prev_snap, 150);
        let cur = snap(180, 0, false);
        assert!(needs_dump(
            Some(&cur),
            Some(&prev),
            &BackupSettings::default(),
            200
        ));
    }

    #[test]
    fn myisam_siempre_vuelca() {
        let prev_snap = snap(100, 5, false);
        let prev = BackupMeta::from_snapshot("app", &prev_snap, 150);
        let cur = snap(100, 5, true);
        assert!(needs_dump(
            Some(&cur),
            Some(&prev),
            &BackupSettings::default(),
            200
        ));
    }

    #[test]
    fn copia_vieja_vuelca() {
        let s = snap(100, 5, false);
        let prev = BackupMeta::from_snapshot("app", &s, 10);
        let settings = BackupSettings {
            retention: 10,
            max_age_secs: 24 * 3600,
        };
        assert!(needs_dump(
            Some(&s),
            Some(&prev),
            &settings,
            10 + 24 * 3600 + 1
        ));
    }

    #[test]
    fn sin_snapshot_vuelca() {
        assert!(needs_dump(None, None, &BackupSettings::default(), 1));
    }
}
