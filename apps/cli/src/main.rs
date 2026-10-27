use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use hostig_ipc::{process_is_running, Client, ClientError, Layout};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "hostig", about = "Cliente del supervisor de Hostig")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Pregunta el estado al supervisor que ya está en marcha.
    Status,
    /// Arranca o detiene el proceso hostig-supervisor.
    Supervisor {
        #[command(subcommand)]
        action: SupervisorCommand,
    },
    /// Catálogo e instalación de runtimes (ADR 0002).
    Runtimes {
        #[command(subcommand)]
        action: RuntimesCommand,
    },
    /// Consulta de puertos (ADR 0004).
    Ports {
        #[command(subcommand)]
        action: PortsCommand,
    },
    /// Instancia MariaDB local (ADR 0003).
    Db {
        #[command(subcommand)]
        action: DbCommand,
    },
}

#[derive(Subcommand)]
enum SupervisorCommand {
    /// Lo lanza desacoplado si todavía no responde.
    Start,
    /// Pide el apagado y espera a que el proceso termine.
    Stop,
}

#[derive(Subcommand)]
enum RuntimesCommand {
    /// Lista las entradas del catálogo y si están instaladas.
    List,
    /// Instala un runtime. Formato: componente@version (p. ej. php@8.4.27).
    Install { spec: String },
    /// Recalcula hashes de lo instalado frente al manifiesto.
    Verify {
        /// Opcional: componente@version para acotar.
        spec: Option<String>,
    },
    /// Consulta el estado de una instalación asíncrona.
    Job { job_id: String },
}

#[derive(Subcommand)]
enum PortsCommand {
    /// Comprueba uno o más puertos: libre, ocupado (pid/nombre/ruta) o reservado.
    Check {
        /// Puertos a inspeccionar.
        ports: Vec<u16>,
    },
}

#[derive(Subcommand)]
enum DbCommand {
    /// Inicializa si hace falta, arranca MariaDB y espera a que quede lista.
    Start {
        /// Puerto TCP (defecto 3306). No se cambia solo si está ocupado.
        #[arg(long)]
        port: Option<u16>,
        /// Versión del catálogo (defecto 11.4.13).
        #[arg(long)]
        version: Option<String>,
    },
    /// Apagado ordenado con mariadb-admin shutdown.
    Stop,
    /// Estado de la instancia (pid, puerto, estado).
    Status,
    /// Emergencia: mata solo el PID registrado si coincide la hora de creación.
    Kill {
        /// PID que debe coincidir con el registro del supervisor.
        pid: u32,
    },
    /// Cuarentena de archivos anómalos del datadir (ADR 0012).
    Quarantine {
        #[command(subcommand)]
        action: Option<QuarantineCommand>,
    },
}

#[derive(Subcommand)]
enum QuarantineCommand {
    /// Lista timestamps de cuarentenas.
    List,
    /// Restaura una cuarentena por timestamp.
    Restore { timestamp: String },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Commands::Status => status(),
        Commands::Supervisor { action } => match action {
            SupervisorCommand::Start => start(),
            SupervisorCommand::Stop => stop(),
        },
        Commands::Runtimes { action } => match action {
            RuntimesCommand::List => runtimes_list(),
            RuntimesCommand::Install { spec } => runtimes_install(&spec),
            RuntimesCommand::Verify { spec } => runtimes_verify(spec.as_deref()),
            RuntimesCommand::Job { job_id } => runtimes_job(&job_id),
        },
        Commands::Ports { action } => match action {
            PortsCommand::Check { ports } => ports_check(&ports),
        },
        Commands::Db { action } => match action {
            DbCommand::Start { port, version } => db_start(port, version.as_deref()),
            DbCommand::Stop => db_stop(),
            DbCommand::Status => db_status(),
            DbCommand::Kill { pid } => db_kill(pid),
            DbCommand::Quarantine { action } => match action {
                None => db_quarantine(),
                Some(QuarantineCommand::List) => db_quarantine_list(),
                Some(QuarantineCommand::Restore { timestamp }) => db_quarantine_restore(&timestamp),
            },
        },
    };
    std::process::exit(code);
}

fn status() -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    match Client::status(&layout) {
        Ok(state) => {
            print_state(&state_json(state.running, state.pid, state.protocol, None));
            0
        }
        Err(error) => fail_client(error),
    }
}

fn start() -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    if let Err(error) = check_endpoint_path(&layout) {
        return error;
    }
    if let Ok(state) = Client::status(&layout) {
        print_state(&state_json(
            state.running,
            state.pid,
            state.protocol,
            Some(true),
        ));
        return 0;
    }
    let bin = match supervisor_bin() {
        Ok(bin) => bin,
        Err(error) => return fail(&error.to_string(), "compila el workspace con cargo build"),
    };
    let mut child = match spawn_detached(&bin) {
        Ok(child) => child,
        Err(error) => {
            return fail(
                &error.to_string(),
                "comprueba que hostig-supervisor está junto al ejecutable hostig",
            )
        }
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(state) = Client::status(&layout) {
            print_state(&state_json(
                state.running,
                state.pid,
                state.protocol,
                Some(false),
            ));
            return 0;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return fail(
                    &format!("hostig-supervisor terminó con {status}"),
                    "mira el log en la carpeta de datos",
                );
            }
            Ok(None) => {}
            Err(error) => {
                return fail(
                    &error.to_string(),
                    "vuelve a lanzar hostig supervisor start",
                )
            }
        }
        if Instant::now() > deadline {
            return fail(
                "el supervisor no llegó a responder",
                "mira el log en la carpeta de datos y el puerto o el pipe",
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop() -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    let state = match Client::status(&layout) {
        Ok(state) => state,
        Err(error) => return fail_client(error),
    };
    if let Err(error) = Client::shutdown(&layout) {
        return fail_client(error);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_is_running(state.pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if process_is_running(state.pid) {
        return fail(
            "el proceso sigue en marcha después de shutdown",
            "cierra hostig-supervisor desde el administrador de tareas",
        );
    }
    let released = Instant::now() + Duration::from_secs(5);
    while Instant::now() < released {
        if Client::status(&layout).is_err() {
            println!(
                "{}",
                json!({
                    "running": false,
                    "pid": state.pid,
                    "protocol": state.protocol,
                })
            );
            return 0;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    fail(
        "el proceso terminó pero el socket o el pipe sigue aceptando conexiones",
        "espera un segundo y vuelve a ejecutar hostig supervisor stop",
    )
}

fn runtimes_list() -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    match Client::call(&layout, "runtimes.list", json!({})) {
        Ok(result) => {
            println!("{result}");
            0
        }
        Err(error) => fail_client(error),
    }
}

fn runtimes_install(spec: &str) -> i32 {
    let (component, version) = match parse_spec(spec) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    let started = match Client::call(
        &layout,
        "runtimes.install",
        json!({ "component": component, "version": version }),
    ) {
        Ok(result) => result,
        Err(error) => return fail_client(error),
    };
    let job_id = started.get("job_id").and_then(Value::as_str).unwrap_or("");
    if job_id.is_empty() {
        println!("{started}");
        return 0;
    }
    // La instalación puede durar minutos; el IPC corta a los 5 s, así que
    // sondeamos runtimes.job hasta el estado final.
    let deadline = Instant::now() + Duration::from_secs(60 * 30);
    loop {
        let status = match Client::call(&layout, "runtimes.job", json!({ "job_id": job_id })) {
            Ok(value) => value,
            Err(error) => return fail_client(error),
        };
        let state = status.get("state").and_then(Value::as_str).unwrap_or("");
        match state {
            "succeeded" => {
                println!("{status}");
                return 0;
            }
            "failed" => {
                let cause = status
                    .pointer("/error/cause")
                    .and_then(Value::as_str)
                    .unwrap_or("la instalación falló");
                let action = status
                    .pointer("/error/action")
                    .and_then(Value::as_str)
                    .unwrap_or("revisa el log del supervisor");
                return fail(cause, action);
            }
            _ => {
                if Instant::now() > deadline {
                    return fail(
                        "la instalación no terminó a tiempo",
                        &format!("consulta hostig runtimes job {job_id}"),
                    );
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

fn runtimes_verify(spec: Option<&str>) -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    let mut params = json!({});
    if let Some(spec) = spec {
        let (component, version) = match parse_spec(spec) {
            Ok(pair) => pair,
            Err(code) => return code,
        };
        params = json!({ "component": component, "version": version });
    }
    match Client::call(&layout, "runtimes.verify", params) {
        Ok(result) => {
            println!("{result}");
            let failed = result
                .get("results")
                .and_then(Value::as_array)
                .map(|rows| rows.iter().any(|row| row.get("ok") == Some(&json!(false))))
                .unwrap_or(false);
            if failed {
                1
            } else {
                0
            }
        }
        Err(error) => fail_client(error),
    }
}

fn ports_check(ports: &[u16]) -> i32 {
    if ports.is_empty() {
        return fail(
            "no se indicó ningún puerto",
            "usa hostig ports check <puerto> [<puerto>...]",
        );
    }
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    match Client::call(&layout, "ports.check", json!({ "ports": ports })) {
        Ok(result) => {
            if let Some(rows) = result.get("results").and_then(Value::as_array) {
                for row in rows {
                    print_port_row(row);
                }
            } else {
                println!("{result}");
            }
            let conflict = result
                .get("results")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter().any(|row| {
                        row.get("conflict") == Some(&json!(true))
                            || row.get("status") == Some(&json!("error"))
                    })
                })
                .unwrap_or(false);
            if conflict {
                1
            } else {
                0
            }
        }
        Err(error) => fail_client(error),
    }
}

fn print_port_row(row: &Value) {
    let port = row.get("port").and_then(Value::as_u64).unwrap_or(0);
    let status = row
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("desconocido");
    let mut line = format!("{port} {status}");
    if let Some(pid) = row.get("pid").and_then(Value::as_u64) {
        line.push_str(&format!(" pid={pid}"));
    }
    if let Some(name) = row.get("name").and_then(Value::as_str) {
        line.push_str(&format!(" name={name}"));
    }
    if let Some(path) = row.get("path").and_then(Value::as_str) {
        line.push_str(&format!(" path={path}"));
    }
    if let Some(suggested) = row.get("suggested").and_then(Value::as_u64) {
        line.push_str(&format!(" sugerido={suggested}"));
    }
    if let Some(cause) = row.get("cause").and_then(Value::as_str) {
        line.push_str(&format!(" ({cause})"));
    }
    if let Some(action) = row.get("action").and_then(Value::as_str) {
        line.push_str(&format!(" → {action}"));
    }
    println!("{line}");
}

fn runtimes_job(job_id: &str) -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    match Client::call(&layout, "runtimes.job", json!({ "job_id": job_id })) {
        Ok(result) => {
            println!("{result}");
            0
        }
        Err(error) => fail_client(error),
    }
}

fn db_start(port: Option<u16>, version: Option<&str>) -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    let mut params = json!({});
    if let Some(port) = port {
        params["port"] = json!(port);
    }
    if let Some(version) = version {
        params["version"] = json!(version);
    }
    let started = match Client::call(&layout, "db.start", params) {
        Ok(result) => result,
        Err(error) => return fail_client(error),
    };
    poll_db_job(&layout, &started, Duration::from_secs(60 * 5))
}

fn db_stop() -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    let started = match Client::call(&layout, "db.stop", json!({})) {
        Ok(result) => result,
        Err(error) => return fail_client(error),
    };
    poll_db_job(&layout, &started, Duration::from_secs(60))
}

fn db_status() -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    match Client::call(&layout, "db.status", json!({})) {
        Ok(result) => {
            println!("{result}");
            0
        }
        Err(error) => fail_client(error),
    }
}

fn db_kill(pid: u32) -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    match Client::call(&layout, "db.kill", json!({ "pid": pid })) {
        Ok(result) => {
            println!("{result}");
            0
        }
        Err(error) => fail_client(error),
    }
}

fn db_quarantine() -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    match Client::call(&layout, "db.quarantine", json!({})) {
        Ok(result) => {
            println!("{result}");
            0
        }
        Err(error) => fail_client(error),
    }
}

fn db_quarantine_list() -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    match Client::call(&layout, "db.quarantine.list", json!({})) {
        Ok(result) => {
            println!("{result}");
            0
        }
        Err(error) => fail_client(error),
    }
}

fn db_quarantine_restore(timestamp: &str) -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return fail(&error.to_string(), "revisa HOSTIG_DATA_DIR"),
    };
    match Client::call(
        &layout,
        "db.quarantine.restore",
        json!({ "timestamp": timestamp }),
    ) {
        Ok(result) => {
            println!("{result}");
            0
        }
        Err(error) => fail_client(error),
    }
}

fn poll_db_job(layout: &Layout, started: &Value, budget: Duration) -> i32 {
    let job_id = started.get("job_id").and_then(Value::as_str).unwrap_or("");
    if job_id.is_empty() {
        println!("{started}");
        return 0;
    }
    let state = started.get("state").and_then(Value::as_str).unwrap_or("");
    if state == "succeeded" || state == "failed" {
        if state == "failed" {
            return fail_db_job(started);
        }
        println!("{started}");
        return 0;
    }
    let deadline = Instant::now() + budget;
    loop {
        let status = match Client::call(layout, "db.job", json!({ "job_id": job_id })) {
            Ok(value) => value,
            Err(error) => return fail_client(error),
        };
        let state = status.get("state").and_then(Value::as_str).unwrap_or("");
        match state {
            "succeeded" => {
                println!("{status}");
                return 0;
            }
            "failed" => return fail_db_job(&status),
            _ => {
                if Instant::now() > deadline {
                    return fail(
                        "la operación de base de datos no terminó a tiempo",
                        &format!("consulta el estado con hostig db status (job {job_id})"),
                    );
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

fn fail_db_job(status: &Value) -> i32 {
    let cause = status
        .pointer("/error/cause")
        .and_then(Value::as_str)
        .unwrap_or("la operación falló");
    let action = status
        .pointer("/error/action")
        .and_then(Value::as_str)
        .unwrap_or("revisa el log del supervisor y el error log de la instancia");
    fail(cause, action)
}

fn parse_spec(spec: &str) -> Result<(&str, &str), i32> {
    let Some((component, version)) = spec.split_once('@') else {
        return Err(fail(
            &format!("especificación inválida: {spec}"),
            "usa componente@version, por ejemplo php@8.4.27",
        ));
    };
    if component.is_empty() || version.is_empty() {
        return Err(fail(
            &format!("especificación inválida: {spec}"),
            "usa componente@version, por ejemplo php@8.4.27",
        ));
    }
    Ok((component, version))
}

fn state_json(running: bool, pid: u32, protocol: u32, already_running: Option<bool>) -> Value {
    let mut value = json!({
        "running": running,
        "pid": pid,
        "protocol": protocol,
    });
    if let Some(already) = already_running {
        value["already_running"] = json!(already);
    }
    value
}

fn print_state(value: &Value) {
    println!("{value}");
}

fn fail_client(error: ClientError) -> i32 {
    match error {
        ClientError::Rejected {
            message,
            cause,
            action,
        } => fail(&format!("{message}. {cause}"), &action),
        ClientError::Io(error) => fail(
            &error.to_string(),
            "arranca el supervisor con hostig supervisor start",
        ),
    }
}

fn fail(cause: &str, action: &str) -> i32 {
    let line = json!({
        "level": "error",
        "module": "cli",
        "cause": cause,
        "action": action,
    });
    eprintln!("{line}");
    1
}

fn check_endpoint_path(layout: &Layout) -> Result<(), i32> {
    #[cfg(unix)]
    {
        use hostig_ipc::Endpoint;
        use std::os::unix::ffi::OsStrExt;
        if let Endpoint::Unix(path) = &layout.endpoint {
            if hostig_ipc::check_unix_socket_path(path).is_err() {
                let len = path.as_os_str().as_bytes().len();
                let max = hostig_ipc::unix_socket_path_max();
                return Err(fail(
                    &format!("la ruta del socket mide {len} caracteres y el máximo es {max}"),
                    "usar una carpeta de datos con una ruta más corta",
                ));
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = layout;
    }
    Ok(())
}

fn supervisor_bin() -> std::io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let dir = current.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "el ejecutable no tiene directorio",
        )
    })?;
    let name = if cfg!(windows) {
        "hostig-supervisor.exe"
    } else {
        "hostig-supervisor"
    };
    let path = dir.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no está {}", path.display()),
        ))
    }
}

fn spawn_detached(bin: &std::path::Path) -> std::io::Result<std::process::Child> {
    // El pipe de salida de esta CLI no debe heredarse: si el supervisor lo
    // conserva, quien hizo `output()` se queda esperando un cierre que no llega.
    #[cfg(windows)]
    prevent_inheriting_stdio();
    let mut command = Command::new(bin);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        command.spawn()
    }
    #[cfg(windows)]
    {
        // Misma política ADR 0011 que MariaDB: breakaway, y si falla consultar kill-on-close.
        hostig_processes::spawn_detached_windows(&mut command).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{}: {} ({})", error.message, error.cause, error.action),
            )
        })
    }
}

#[cfg(windows)]
fn prevent_inheriting_stdio() {
    use std::os::windows::io::AsRawHandle;
    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    unsafe {
        let _ = SetHandleInformation(std::io::stdout().as_raw_handle(), HANDLE_FLAG_INHERIT, 0);
        let _ = SetHandleInformation(std::io::stderr().as_raw_handle(), HANDLE_FLAG_INHERIT, 0);
    }
}
