use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{ProcessError, Result};

pub const DEFAULT_INSTANCE_ID: &str = "mariadb-11.4";
pub const DEFAULT_PORT: u16 = 3306;

const EMBEDDED_TEMPLATE: &str = include_str!("../../../../templates/mariadb/my.ini.template");

/// Rutas de una instancia bajo `{datos}/instances/{id}/`.
#[derive(Debug, Clone)]
pub struct InstancePaths {
    pub id: String,
    pub root: PathBuf,
    pub my_ini: PathBuf,
    pub datadir: PathBuf,
    pub error_log: PathBuf,
    pub pid_file: PathBuf,
    pub admin_cnf: PathBuf,
    pub password_file: PathBuf,
}

impl InstancePaths {
    pub fn new(data_dir: &Path, id: &str) -> Self {
        let root = data_dir.join("instances").join(id);
        Self {
            id: id.to_string(),
            my_ini: root.join("my.ini"),
            datadir: root.join("data"),
            error_log: root.join("logs").join("error.log"),
            pid_file: root.join("mysql.pid"),
            admin_cnf: root.join("admin.cnf"),
            password_file: root.join("admin.password"),
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(self.root.join("logs"))?;
        fs::create_dir_all(&self.datadir)?;
        Ok(())
    }
}

/// Genera el `my.ini` de Hostig (fuera del datadir) a partir de la plantilla.
pub fn generate_my_ini(paths: &InstancePaths, basedir: &Path, port: u16) -> Result<String> {
    let text = EMBEDDED_TEMPLATE
        .replace("{{INSTANCE_ID}}", &paths.id)
        .replace("{{PORT}}", &port.to_string())
        .replace("{{DATADIR}}", &path_ini(paths.datadir.as_path()))
        .replace("{{BASEDIR}}", &path_ini(basedir))
        .replace("{{ERROR_LOG}}", &path_ini(paths.error_log.as_path()))
        .replace("{{PID_FILE}}", &path_ini(paths.pid_file.as_path()));
    Ok(text)
}

pub fn write_my_ini(paths: &InstancePaths, basedir: &Path, port: u16) -> Result<()> {
    paths.ensure_dirs()?;
    let text = generate_my_ini(paths, basedir, port)?;
    fs::write(&paths.my_ini, text.as_bytes())?;
    Ok(())
}

/// Resultado de regenerar my.ini desde la plantilla (ADR 0005 / 0012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRegen {
    pub changed: bool,
    pub diff_summary: String,
    pub dangerous_directives: Vec<String>,
}

/// Regenera my.ini. Si el disco difería, resume el cambio y avisa de
/// `skip-grant-tables` / `skip-networking` en la versión anterior.
pub fn regenerate_my_ini(paths: &InstancePaths, basedir: &Path, port: u16) -> Result<ConfigRegen> {
    paths.ensure_dirs()?;
    let next = generate_my_ini(paths, basedir, port)?;
    let previous = if paths.my_ini.is_file() {
        fs::read_to_string(&paths.my_ini).unwrap_or_default()
    } else {
        String::new()
    };
    let mut dangerous = Vec::new();
    let prev_lower = previous.to_ascii_lowercase();
    if prev_lower.contains("skip-grant-tables") {
        dangerous.push("skip-grant-tables".into());
    }
    if prev_lower.contains("skip-networking") {
        dangerous.push("skip-networking".into());
    }
    let changed = normalize_ini(&previous) != normalize_ini(&next);
    let diff_summary = if !changed {
        String::new()
    } else if previous.is_empty() {
        "my.ini creado desde la plantilla".into()
    } else {
        summarize_ini_diff(&previous, &next)
    };
    fs::write(&paths.my_ini, next.as_bytes())?;
    Ok(ConfigRegen {
        changed,
        diff_summary,
        dangerous_directives: dangerous,
    })
}

fn normalize_ini(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

fn summarize_ini_diff(old: &str, new: &str) -> String {
    let old_keys: std::collections::BTreeSet<_> = ini_keys(old).into_iter().collect();
    let new_keys: std::collections::BTreeSet<_> = ini_keys(new).into_iter().collect();
    let mut parts = Vec::new();
    for k in old_keys.difference(&new_keys) {
        parts.push(format!("quitada {k}"));
    }
    for k in new_keys.difference(&old_keys) {
        parts.push(format!("añadida {k}"));
    }
    for k in old_keys.intersection(&new_keys) {
        let ov = ini_value(old, k);
        let nv = ini_value(new, k);
        if ov != nv {
            parts.push(format!("cambiada {k}: {ov:?} → {nv:?}"));
        }
    }
    if parts.is_empty() {
        "contenido distinto (sin claves parseables)".into()
    } else {
        parts.join("; ")
    }
}

fn ini_keys(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                return None;
            }
            line.split_once('=')
                .map(|(k, _)| k.trim().to_ascii_lowercase())
        })
        .collect()
}

fn ini_value(text: &str, key: &str) -> String {
    for line in text.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case(key) {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

fn path_ini(path: &Path) -> String {
    // my.ini en Windows acepta barras normales; escapamos \.
    path.display().to_string().replace('\\', "/")
}

/// Escribe `admin.cnf` protegido para mariadb-admin (--defaults-extra-file).
pub fn write_admin_cnf(paths: &InstancePaths, port: u16, password: &str) -> Result<()> {
    let body = format!(
        "[client]\nprotocol=tcp\nhost=127.0.0.1\nport={port}\nuser=root\npassword={password}\n"
    );
    fs::write(&paths.admin_cnf, body.as_bytes())?;
    hostig_ipc::restrict_to_user_and_system(&paths.admin_cnf).map_err(|error| {
        ProcessError::new(
            "no se pudieron fijar permisos de admin.cnf",
            error.to_string(),
            "revisa la ACL del usuario actual",
        )
    })?;
    fs::write(&paths.password_file, password.as_bytes())?;
    hostig_ipc::restrict_to_user_and_system(&paths.password_file).map_err(|error| {
        ProcessError::new(
            "no se pudieron fijar permisos de admin.password",
            error.to_string(),
            "revisa la ACL del usuario actual",
        )
    })?;
    Ok(())
}

pub fn read_password(paths: &InstancePaths) -> Result<String> {
    let text = fs::read_to_string(&paths.password_file)?;
    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genera_configuracion_con_placeholders_sustituidos() {
        let dir = tempfile::tempdir().unwrap();
        let paths = InstancePaths::new(dir.path(), "mariadb-11.4");
        let basedir = PathBuf::from(r"C:\runtimes\mariadb");
        let text = generate_my_ini(&paths, &basedir, 3310).unwrap();
        assert!(text.contains("port=3310"));
        assert!(text.contains("bind-address=127.0.0.1"));
        assert!(text.contains("skip-name-resolve"));
        assert!(text.contains("skip-slave-start"));
        assert!(text.contains("utf8mb4"));
        assert!(text.contains("innodb_buffer_pool_size"));
        assert!(!text.contains("{{"));
        assert!(text.contains("mariadb-11.4"));
    }
}
