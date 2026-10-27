use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{ProcessError, Result};
use crate::mariadb::config::{write_admin_cnf, write_my_ini, InstancePaths};

/// Localiza `bin/` del runtime MariaDB instalado (busca mariadbd.exe / mariadbd).
/// No asume un nombre de carpeta de versión fijo: prueba `bin/` en la raíz y un nivel.
pub fn find_mariadb_bin(runtime_root: &Path) -> Result<PathBuf> {
    let direct = runtime_root.join("bin");
    if exe_in(&direct, "mariadbd").is_some() {
        return Ok(direct);
    }
    if let Ok(entries) = fs::read_dir(runtime_root) {
        for entry in entries.flatten() {
            let bin = entry.path().join("bin");
            if exe_in(&bin, "mariadbd").is_some() {
                return Ok(bin);
            }
        }
    }
    Err(ProcessError::new(
        "binarios MariaDB no encontrados",
        format!("no hay mariadbd bajo {}", runtime_root.display()),
        "instala mariadb del catálogo con hostig runtimes install",
    ))
}

pub fn exe_in(bin: &Path, name: &str) -> Option<PathBuf> {
    let with_exe = bin.join(format!("{name}.exe"));
    if with_exe.is_file() {
        return Some(with_exe);
    }
    let plain = bin.join(name);
    if plain.is_file() {
        return Some(plain);
    }
    None
}

pub fn basedir_from_bin(bin: &Path) -> PathBuf {
    bin.parent().unwrap_or(bin).to_path_buf()
}

/// Inicializa el datadir si está vacío. En Windows usa `mariadb-install-db.exe`.
///
/// Observación (Windows): `mariadb-install-db.exe` escribe su propio `my.ini`
/// dentro del datadir (y puede anexar port/datadir). Tras la init, Hostig
/// elimina ese `data/my.ini` y deja como única configuración efectiva
/// `instances/{id}/my.ini`, pasada siempre como primer argumento
/// `--defaults-file=...` a `mariadbd`.
pub fn ensure_initialized(
    paths: &InstancePaths,
    runtime_root: &Path,
    port: u16,
    password: &str,
) -> Result<()> {
    paths.ensure_dirs()?;
    let bin = find_mariadb_bin(runtime_root)?;
    let basedir = basedir_from_bin(&bin);
    write_my_ini(paths, &basedir, port)?;
    write_admin_cnf(paths, port, password)?;

    if datadir_has_mysql_system(paths) {
        remove_datadir_my_ini(paths)?;
        return Ok(());
    }

    let install_db = exe_in(&bin, "mariadb-install-db").ok_or_else(|| {
        ProcessError::new(
            "mariadb-install-db ausente",
            format!("no está en {}", bin.display()),
            "reinstala el runtime MariaDB del catálogo",
        )
    })?;

    // Windows: --password en install-db es el mecanismo del publicador.
    // No registramos servicio (--service omitido).
    let status = Command::new(&install_db)
        .arg(format!("--datadir={}", paths.datadir.display()))
        .arg(format!("--password={password}"))
        .arg(format!("--port={port}"))
        .status()
        .map_err(|error| {
            ProcessError::new(
                "falló mariadb-install-db",
                error.to_string(),
                "revisa el runtime y el datadir vacío",
            )
        })?;
    if !status.success() {
        return Err(ProcessError::new(
            "mariadb-install-db terminó con error",
            format!("código {status}"),
            "borra instances/{id}/data si quedó a medias y reintenta",
        ));
    }

    remove_datadir_my_ini(paths)?;
    // Reescribimos nuestra config por si install-db tocó algo fuera.
    write_my_ini(paths, &basedir, port)?;
    Ok(())
}

fn datadir_has_mysql_system(paths: &InstancePaths) -> bool {
    paths.datadir.join("mysql").is_dir()
}

fn remove_datadir_my_ini(paths: &InstancePaths) -> Result<()> {
    for name in ["my.ini", "my.cnf"] {
        let path = paths.datadir.join(name);
        if path.exists() {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

pub fn generate_password() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let id2 = uuid::Uuid::new_v4().simple().to_string();
    format!("{id}{id2}")[..48].to_string()
}
