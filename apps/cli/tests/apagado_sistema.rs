//! Parada rápida por apagado del sistema (ADR 0011) con el falso mariadbd.
//!
//! - Linux y macOS: `SIGTERM` al supervisor.
//! - Windows: `WM_QUERYENDSESSION` y `WM_ENDSESSION` a su ventana oculta; además,
//!   la suspensión y la reanudación no paran nada.
//!
//! El falso mariadbd se compila con el paquete hostig-processes. `cargo test
//! --workspace` lo deja junto a `hostig`.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use hostig_ports::{check as check_port, PortState};
use hostig_processes::mariadb::{
    log_contains_shutdown, InstancePaths, DEFAULT_INSTANCE_ID, TEST_DB_PROBE_ENV, TEST_MARIADBD_ENV,
};
use hostig_processes::ALLOW_JOB_WITHOUT_BREAKAWAY_ENV;
use serde_json::Value;

#[cfg(windows)]
#[path = "common/win_shutdown.rs"]
mod win_shutdown;

/// Tope duro del ADR 0011 para todo el camino.
const HARD_CAP: Duration = Duration::from_secs(20);

fn hostig() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hostig"))
}

fn fake_mariadbd() -> PathBuf {
    let path = hostig()
        .parent()
        .expect("carpeta de binarios")
        .join(format!("fake-mariadbd{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "falta {}; ejecuta cargo test --workspace o cargo build -p hostig-processes --bin fake-mariadbd",
        path.display()
    );
    path
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.local_addr().expect("addr").port()
}

fn run_hostig(data: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(hostig())
        .args(args)
        .env("HOSTIG_DATA_DIR", data)
        .env(ALLOW_JOB_WITHOUT_BREAKAWAY_ENV, "1")
        .env(TEST_MARIADBD_ENV, fake_mariadbd())
        .env(TEST_DB_PROBE_ENV, "tcp")
        .output()
        .expect("hostig");
    (
        output.status.code().unwrap_or(1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn json_stdout(stdout: &str) -> Value {
    serde_json::from_str(stdout.trim()).unwrap_or_else(|_| panic!("salida no JSON: {stdout}"))
}

fn pid_of(value: &Value, what: &str) -> u32 {
    value
        .get("pid")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("falta pid de {what}: {value}")) as u32
}

fn wait_gone(pid: u32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while hostig_ipc::process_is_running(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    true
}

fn supervisor_log(data: &Path) -> String {
    std::fs::read_to_string(data.join("logs").join("hostig.log")).unwrap_or_default()
}

fn wait_log(data: &Path, needle: &str, budget: Duration) -> String {
    let deadline = Instant::now() + budget;
    loop {
        let log = supervisor_log(data);
        if log.contains(needle) || Instant::now() >= deadline {
            return log;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Supervisor + falso MariaDB en marcha. Al soltarse, si algo quedó vivo, lo para.
struct Running {
    data: tempfile::TempDir,
    port: u16,
    supervisor_pid: u32,
    mariadb_pid: u32,
}

impl Running {
    fn start() -> Self {
        let data = tempfile::tempdir().expect("tmp");
        let port = free_port();
        let (code, out, err) = run_hostig(data.path(), &["supervisor", "start"]);
        assert_eq!(code, 0, "supervisor start: {out}{err}");
        let supervisor_pid = pid_of(&json_stdout(&out), "supervisor");
        let port_s = port.to_string();
        let (code, out, err) = run_hostig(data.path(), &["db", "start", "--port", &port_s]);
        assert_eq!(code, 0, "db start: {out}{err}");
        let mariadb_pid = pid_of(&json_stdout(&out), "mariadb");
        assert!(hostig_ipc::process_is_running(mariadb_pid));
        Self {
            data,
            port,
            supervisor_pid,
            mariadb_pid,
        }
    }

    fn path(&self) -> &Path {
        self.data.path()
    }

    fn error_log(&self) -> PathBuf {
        InstancePaths::new(self.path(), DEFAULT_INSTANCE_ID).error_log
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if hostig_ipc::process_is_running(self.mariadb_pid) {
            // Señal de salida del falso mariadbd.
            if let Some(logs) = self.error_log().parent() {
                let _ = std::fs::write(logs.join("hostig-fake.stop"), b"stop");
            }
            wait_gone(self.mariadb_pid, Duration::from_secs(5));
        }
        if hostig_ipc::process_is_running(self.supervisor_pid) {
            let _ = run_hostig(self.path(), &["supervisor", "stop"]);
        }
    }
}

/// Lanza el aviso de apagado y devuelve lo que tardó en volver (Windows) o en
/// salir el supervisor (Unix).
#[cfg(unix)]
fn trigger_system_shutdown(running: &Running) -> Duration {
    let started = Instant::now();
    let status = Command::new("kill")
        .args(["-TERM", &running.supervisor_pid.to_string()])
        .status()
        .expect("kill");
    assert!(status.success(), "kill -TERM falló");
    assert!(
        wait_gone(running.supervisor_pid, HARD_CAP + Duration::from_secs(5)),
        "el supervisor no salió tras SIGTERM"
    );
    started.elapsed()
}

#[cfg(windows)]
fn trigger_system_shutdown(running: &Running) -> Duration {
    use windows::Win32::UI::WindowsAndMessaging::{WM_ENDSESSION, WM_QUERYENDSESSION};

    let hwnd = win_shutdown::find_shutdown_window(running.supervisor_pid, Duration::from_secs(10));
    let answer = win_shutdown::send(hwnd, WM_QUERYENDSESSION, 0, 0, Duration::from_secs(5));
    assert_eq!(answer, Some(1), "WM_QUERYENDSESSION debe responder TRUE");
    let started = Instant::now();
    let answer = win_shutdown::send(hwnd, WM_ENDSESSION, 1, 0, HARD_CAP + Duration::from_secs(5));
    let elapsed = started.elapsed();
    assert!(answer.is_some(), "WM_ENDSESSION no volvió en el plazo");
    // La sesión puede terminar en cuanto vuelve WM_ENDSESSION: MariaDB ya tiene
    // que estar parada en este punto, no después.
    assert!(
        !hostig_ipc::process_is_running(running.mariadb_pid),
        "MariaDB seguía viva cuando WM_ENDSESSION volvió"
    );
    assert!(
        wait_gone(running.supervisor_pid, Duration::from_secs(10)),
        "el supervisor no salió tras WM_ENDSESSION"
    );
    elapsed
}

#[test]
fn apagado_del_sistema_para_mariadb_sin_volcado_y_a_tiempo() {
    let running = Running::start();
    let log_before = std::fs::metadata(running.error_log())
        .map(|m| m.len())
        .unwrap_or(0);

    let elapsed = trigger_system_shutdown(&running);

    assert!(
        elapsed < HARD_CAP,
        "la parada rápida tardó {elapsed:?}, por encima del tope de {HARD_CAP:?}"
    );
    assert!(
        wait_gone(running.mariadb_pid, Duration::from_secs(1)),
        "MariaDB debía haber salido"
    );
    assert!(matches!(
        check_port(running.port, None).expect("port"),
        PortState::Libre
    ));
    let error_log =
        hostig_processes::mariadb::read_log_since(&running.error_log(), log_before).expect("log");
    assert!(
        log_contains_shutdown(&error_log),
        "falta la línea de apagado de MariaDB:\n{error_log}"
    );
    let backups =
        hostig_backup::list_backups(running.path(), DEFAULT_INSTANCE_ID).unwrap_or_default();
    assert!(
        backups.is_empty(),
        "el apagado del sistema no debe volcar: {backups:?}"
    );
    let log = wait_log(
        running.path(),
        "parada rápida terminada",
        Duration::from_secs(2),
    );
    assert!(log.contains("parada rápida sin volcado"), "{log}");
    assert!(log.contains("mariadb: parado en"), "{log}");
    assert!(log.contains("parada rápida terminada"), "{log}");
    assert!(!log.contains("no paró a tiempo"), "{log}");
}

#[cfg(windows)]
#[test]
fn suspension_y_reanudacion_no_paran_nada() {
    use windows::Win32::UI::WindowsAndMessaging::{
        PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, WM_POWERBROADCAST,
    };

    let running = Running::start();
    let hwnd = win_shutdown::find_shutdown_window(running.supervisor_pid, Duration::from_secs(10));
    let answer = win_shutdown::send(
        hwnd,
        WM_POWERBROADCAST,
        PBT_APMSUSPEND as usize,
        0,
        Duration::from_secs(5),
    );
    assert_eq!(answer, Some(1));
    thread::sleep(Duration::from_secs(1));
    assert!(hostig_ipc::process_is_running(running.mariadb_pid));
    let answer = win_shutdown::send(
        hwnd,
        WM_POWERBROADCAST,
        PBT_APMRESUMEAUTOMATIC as usize,
        0,
        Duration::from_secs(5),
    );
    assert_eq!(answer, Some(1));
    thread::sleep(Duration::from_secs(1));

    assert!(hostig_ipc::process_is_running(running.supervisor_pid));
    let (code, out, err) = run_hostig(running.path(), &["db", "status"]);
    assert_eq!(code, 0, "db status: {out}{err}");
    let status = json_stdout(&out);
    assert_eq!(status.get("state").and_then(Value::as_str), Some("running"));
    assert_eq!(
        status.get("pid").and_then(Value::as_u64).map(|p| p as u32),
        Some(running.mariadb_pid),
        "la suspensión no debe reiniciar MariaDB"
    );
    let log = wait_log(running.path(), "se reanuda", Duration::from_secs(2));
    assert!(log.contains("no se para ningún servicio"), "{log}");
    assert!(log.contains("se reanuda"), "{log}");
    assert!(!log.contains("parada rápida"), "{log}");
    let backups =
        hostig_backup::list_backups(running.path(), DEFAULT_INSTANCE_ID).unwrap_or_default();
    assert!(backups.is_empty(), "la suspensión no debe volcar");
    // Running::drop para el falso mariadbd y el supervisor.
}
