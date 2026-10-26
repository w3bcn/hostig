use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{ProcessError, Result};
use crate::registry::{now_unix_ms, ProcessRecord};

/// Especificación para lanzar un servidor (MariaDB, PHP, Apache).
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub id: String,
    pub exe: PathBuf,
    pub args: Vec<String>,
    /// Directorios que se anteponen al PATH controlado (p. ej. `bin` del runtime).
    pub path_dirs: Vec<PathBuf>,
    pub current_dir: Option<PathBuf>,
    pub port: u16,
    pub version: String,
}

/// Proceso vivo con metadatos para el registro.
#[derive(Debug)]
pub struct SpawnedProcess {
    pub record: ProcessRecord,
    pub child: std::process::Child,
}

/// Lanza el ejecutable con PATH controlado, sin ventana de consola, e intenta
/// `CREATE_BREAKAWAY_FROM_JOB` en Windows (ADR 0011).
pub fn spawn_server(spec: &SpawnSpec) -> Result<SpawnedProcess> {
    if !spec.exe.is_file() {
        return Err(ProcessError::new(
            "ejecutable ausente",
            format!("{} no existe", spec.exe.display()),
            "instala el runtime con hostig runtimes install",
        ));
    }

    let mut command = Command::new(&spec.exe);
    command.args(&spec.args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.env_clear();
    apply_controlled_env(&mut command, &spec.path_dirs);
    if let Some(dir) = &spec.current_dir {
        command.current_dir(dir);
    }

    #[cfg(windows)]
    let child = spawn_windows(&mut command, &spec.exe)?;
    #[cfg(not(windows))]
    let child = command.spawn().map_err(|error| {
        ProcessError::new(
            "no se pudo lanzar el proceso",
            format!("{}: {error}", spec.exe.display()),
            "revisa la ruta del runtime y los permisos",
        )
    })?;

    let pid = child.id();
    let creation_time = crate::os::process_creation_time(pid).ok_or_else(|| {
        ProcessError::new(
            "no se pudo leer la hora de creación",
            format!("pid {pid}"),
            "reintenta el arranque",
        )
    })?;

    let record = ProcessRecord {
        id: spec.id.clone(),
        exe: spec.exe.clone(),
        args: spec.args.clone(),
        pid,
        creation_time,
        registered_at_unix_ms: now_unix_ms(),
        port: spec.port,
        version: spec.version.clone(),
    };

    Ok(SpawnedProcess { record, child })
}

/// Windows: intenta `CREATE_BREAKAWAY_FROM_JOB` (ADR 0011). Si el job padre lo
/// deniega (ERROR_ACCESS_DENIED), reintenta sin ese flag para no bloquear CI/IDE;
/// en ese caso MariaDB queda en el job padre (sin kill-on-close por nuestra parte).
#[cfg(windows)]
fn spawn_windows(command: &mut Command, exe: &Path) -> Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    command.creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP);
    match command.spawn() {
        Ok(child) => Ok(child),
        Err(error) if error.raw_os_error() == Some(5) => {
            eprintln!(
                "{{\"level\":\"warn\",\"module\":\"processes\",\"message\":\"CREATE_BREAKAWAY_FROM_JOB denegado; reintento sin breakaway\",\"cause\":\"{error}\",\"action\":\"arranca el supervisor fuera de un job restrictivo si puedes (ADR 0011)\"}}"
            );
            command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
            command.spawn().map_err(|retry| {
                ProcessError::new(
                    "no se pudo lanzar el proceso",
                    format!("{}: {retry}", exe.display()),
                    "el job padre denegó el breakaway y el reintento también falló; arranca el supervisor fuera de ese job (ADR 0011)",
                )
            })
        }
        Err(error) => Err(ProcessError::new(
            "no se pudo lanzar el proceso",
            format!("{}: {error}", exe.display()),
            "revisa la ruta del runtime y los permisos",
        )),
    }
}

fn apply_controlled_env(command: &mut Command, path_dirs: &[PathBuf]) {
    let mut path_parts: Vec<String> = path_dirs.iter().map(|p| p.display().to_string()).collect();
    #[cfg(windows)]
    {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        path_parts.push(format!(r"{system_root}\System32"));
        path_parts.push(system_root);
        command.env(
            "SystemRoot",
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()),
        );
        command.env(
            "WINDIR",
            std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()),
        );
        command.env(
            "USERPROFILE",
            std::env::var("USERPROFILE").unwrap_or_default(),
        );
        command.env(
            "TMP",
            std::env::var("TMP").unwrap_or_else(|_| std::env::temp_dir().display().to_string()),
        );
        command.env(
            "TEMP",
            std::env::var("TEMP").unwrap_or_else(|_| std::env::temp_dir().display().to_string()),
        );
    }
    #[cfg(unix)]
    {
        path_parts.push("/usr/bin".into());
        path_parts.push("/bin".into());
        command.env("HOME", std::env::var("HOME").unwrap_or_default());
        command.env("TMPDIR", std::env::temp_dir());
    }
    let sep = if cfg!(windows) { ";" } else { ":" };
    command.env("PATH", path_parts.join(sep));
    command.env("LANG", "C");
}

/// Mata de emergencia solo el PID del registro, verificando hora de creación.
pub fn emergency_kill(data_dir: &Path, id: &str, cited_pid: u32) -> Result<()> {
    let record = ProcessRecord::load(data_dir, id)?.ok_or_else(|| {
        ProcessError::new(
            "sin registro",
            format!("no hay proceso registrado como {id}"),
            "usa hostig db status",
        )
    })?;
    if record.pid != cited_pid {
        return Err(ProcessError::new(
            "PID no coincide",
            format!("citaste {cited_pid} y el registro tiene {}", record.pid),
            "cita el PID exacto de hostig db status",
        ));
    }
    let Some(creation) = crate::os::process_creation_time(cited_pid) else {
        return Err(ProcessError::new(
            "proceso ausente",
            format!("pid {cited_pid} no vive o no se puede consultar"),
            "nada que matar",
        ));
    };
    if creation != record.creation_time {
        return Err(ProcessError::new(
            "hora de creación distinta",
            "el PID fue reutilizado por otro proceso",
            "no se mata: cita de nuevo tras hostig db status",
        ));
    }
    crate::os::terminate_process(cited_pid)?;
    ProcessRecord::remove(data_dir, id)?;
    Ok(())
}
