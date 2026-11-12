//! Usuario MariaDB dedicado al health check (sin privilegios de administración).

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{ProcessError, Result};
use crate::mariadb::config::InstancePaths;
use crate::mariadb::init::{exe_in, find_mariadb_bin, generate_password};
use hostig_runtimes::{runtime_dir, Catalog, MANIFEST_NAME};

pub const HEALTH_USER: &str = "hostig_health";
pub const HEALTH_HOST: &str = "127.0.0.1";

/// Crea o actualiza `hostig_health@127.0.0.1` (solo USAGE) y escribe la
/// contraseña en un archivo solo legible por el usuario.
pub fn ensure_health_user(
    paths: &InstancePaths,
    data_dir: &Path,
    version: &str,
    port: u16,
) -> Result<(String, String)> {
    if !paths.admin_cnf.is_file() {
        return Err(ProcessError::new(
            "falta admin.cnf para crear el usuario de health",
            paths.admin_cnf.display().to_string(),
            "arranca MariaDB primero",
        ));
    }
    let password = if paths.health_password_file.is_file() {
        fs::read_to_string(&paths.health_password_file)?
            .trim()
            .to_string()
    } else {
        generate_password()
    };
    write_health_password(paths, &password)?;

    let bin = mariadb_bin(data_dir, version)?;
    let client = exe_in(&bin, "mariadb")
        .or_else(|| exe_in(&bin, "mysql"))
        .ok_or_else(|| {
            ProcessError::new(
                "cliente MariaDB ausente",
                bin.display().to_string(),
                "reinstala MariaDB del catálogo",
            )
        })?;

    let pw = escape_sql(&password);
    // USAGE por defecto; sin GRANT de datos. ALTER USER por si ya existía.
    let sql = format!(
        "CREATE USER IF NOT EXISTS '{HEALTH_USER}'@'{HEALTH_HOST}' IDENTIFIED BY '{pw}';\
         ALTER USER '{HEALTH_USER}'@'{HEALTH_HOST}' IDENTIFIED BY '{pw}';\
         REVOKE ALL PRIVILEGES, GRANT OPTION FROM '{HEALTH_USER}'@'{HEALTH_HOST}';"
    );
    let defaults_extra = format!("--defaults-extra-file={}", paths.admin_cnf.display());
    let output = Command::new(&client)
        .arg(&defaults_extra)
        .arg("--protocol=tcp")
        .arg("-h")
        .arg("127.0.0.1")
        .arg("-P")
        .arg(port.to_string())
        .arg("-e")
        .arg(&sql)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            ProcessError::new(
                "no se pudo crear el usuario de health",
                error.to_string(),
                "revisa que MariaDB acepte conexiones TCP",
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ProcessError::new(
            "CREATE/ALTER de hostig_health falló",
            stderr.trim().to_string(),
            "revisa el error log de MariaDB",
        ));
    }
    Ok((HEALTH_USER.to_string(), password))
}

pub fn write_health_password(paths: &InstancePaths, password: &str) -> Result<()> {
    paths.ensure_dirs()?;
    fs::write(&paths.health_password_file, password.as_bytes())?;
    hostig_ipc::restrict_to_user_and_system(&paths.health_password_file).map_err(|error| {
        ProcessError::new(
            "no se pudieron fijar permisos de health.password",
            error.to_string(),
            "revisa la ACL del usuario actual",
        )
    })?;
    Ok(())
}

fn escape_sql(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "''")
}

fn mariadb_bin(data_dir: &Path, version: &str) -> Result<std::path::PathBuf> {
    let catalog = Catalog::embedded().map_err(|error| {
        ProcessError::new(
            "catálogo ilegible",
            error.to_string(),
            "recompila con catalog/runtimes.toml",
        )
    })?;
    let entry = catalog.find("mariadb", version).ok_or_else(|| {
        ProcessError::new(
            "MariaDB no está en el catálogo",
            format!("falta mariadb@{version}"),
            "actualiza el catálogo",
        )
    })?;
    let runtime_root = runtime_dir(data_dir, entry);
    if !runtime_root.join(MANIFEST_NAME).is_file() {
        return Err(ProcessError::new(
            "runtime MariaDB no instalado",
            format!("falta {}", runtime_root.display()),
            format!("hostig runtimes install mariadb@{version}"),
        ));
    }
    find_mariadb_bin(&runtime_root)
}
