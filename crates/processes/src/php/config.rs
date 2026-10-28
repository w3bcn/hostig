//! Generación de php.ini y rutas del pool (ADR 0005).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{ProcessError, Result};

pub const DEFAULT_POOL_ID: &str = "php-default";

const EMBEDDED_TEMPLATE: &str = include_str!("../../../../templates/php/php.ini.template");

/// Rutas generadas del pool PHP bajo `{datos}/php/{id}/`.
#[derive(Debug, Clone)]
pub struct PhpPaths {
    pub id: String,
    pub root: PathBuf,
    pub php_ini: PathBuf,
    pub error_log: PathBuf,
    pub session_path: PathBuf,
    pub upload_tmp: PathBuf,
    pub sys_temp: PathBuf,
}

impl PhpPaths {
    pub fn new(data_dir: &Path, id: &str) -> Self {
        let root = data_dir.join("php").join(id);
        Self {
            id: id.to_string(),
            php_ini: root.join("php.ini"),
            error_log: root.join("logs").join("php_error.log"),
            session_path: root.join("sessions"),
            upload_tmp: root.join("tmp").join("uploads"),
            sys_temp: root.join("tmp").join("sys"),
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.root.join("logs"))?;
        fs::create_dir_all(&self.session_path)?;
        fs::create_dir_all(&self.upload_tmp)?;
        fs::create_dir_all(&self.sys_temp)?;
        Ok(())
    }
}

/// Localiza el directorio del ZIP NTS de PHP (`php-cgi` en la raíz).
pub fn find_php_root(runtime_root: &Path) -> Result<PathBuf> {
    if php_cgi_in(runtime_root).is_some() {
        return Ok(runtime_root.to_path_buf());
    }
    if let Ok(entries) = fs::read_dir(runtime_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && php_cgi_in(&path).is_some() {
                return Ok(path);
            }
        }
    }
    Err(ProcessError::new(
        "binarios PHP no encontrados",
        format!("no hay php-cgi bajo {}", runtime_root.display()),
        "instala php del catálogo con hostig runtimes install",
    ))
}

pub fn php_cgi_in(root: &Path) -> Option<PathBuf> {
    let with_exe = root.join("php-cgi.exe");
    if with_exe.is_file() {
        return Some(with_exe);
    }
    let plain = root.join("php-cgi");
    if plain.is_file() {
        return Some(plain);
    }
    None
}

pub fn extension_dir(php_root: &Path) -> PathBuf {
    php_root.join("ext")
}

/// Regenera php.ini desde la plantilla. Siempre escribe la salida del generador.
pub fn regenerate_php_ini(paths: &PhpPaths, php_root: &Path, timezone: &str) -> Result<()> {
    paths.ensure_dirs()?;
    let ext = extension_dir(php_root);
    let text = EMBEDDED_TEMPLATE
        .replace("{{ERROR_LOG}}", &path_ini(&paths.error_log))
        .replace("{{EXTENSION_DIR}}", &path_ini(&ext))
        .replace("{{TIMEZONE}}", timezone)
        .replace("{{UPLOAD_TMP_DIR}}", &path_ini(&paths.upload_tmp))
        .replace("{{SYS_TEMP_DIR}}", &path_ini(&paths.sys_temp))
        .replace("{{SESSION_SAVE_PATH}}", &path_ini(&paths.session_path));
    fs::write(&paths.php_ini, text.as_bytes())?;
    Ok(())
}

fn path_ini(path: &Path) -> String {
    // php.ini en Windows acepta barras normales; escapamos la barra invertida.
    path.display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genera_ini_con_extension_dir_absoluto() {
        let dir = tempfile::tempdir().unwrap();
        let paths = PhpPaths::new(dir.path(), DEFAULT_POOL_ID);
        let php_root = dir.path().join("runtime-php");
        fs::create_dir_all(php_root.join("ext")).unwrap();
        regenerate_php_ini(&paths, &php_root, "Europe/Madrid").unwrap();
        let text = fs::read_to_string(&paths.php_ini).unwrap();
        assert!(text.contains("extension_dir="));
        assert!(text.contains("mysqli"));
        assert!(text.contains("Europe/Madrid"));
        assert!(!text.contains("{{"));
        // Nada de rutas de sistema típicas.
        assert!(!text.to_ascii_lowercase().contains("c:/windows"));
        assert!(!text.contains("%TEMP%"));
    }
}
