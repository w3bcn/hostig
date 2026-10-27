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
