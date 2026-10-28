//! Muerte real del supervisor con MariaDB en marcha (ADR 0011).
//! Solo Windows; Linux/macOS se saltan hasta 1b/1c.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use hostig_ports::{check as check_port, PortState};
use hostig_processes::mariadb::{
    log_contains_ready, log_contains_shutdown, read_log_since, DEFAULT_INSTANCE_ID,
    DEFAULT_MARIADB_VERSION,
};
use hostig_processes::ALLOW_JOB_WITHOUT_BREAKAWAY_ENV;
use hostig_runtimes::{runtime_dir, Catalog, RuntimeService, MANIFEST_NAME};
use serde_json::Value;

fn skip_unless_windows() -> bool {
    if cfg!(windows) {
        return false;
    }
    eprintln!(
        "omitido: integración MariaDB solo en Windows hasta las fases 1b (Linux) y 1c (macOS)"
    );
    true
}

fn hostig() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hostig"))
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.local_addr().expect("addr").port()
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if !to.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn prepare_data_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tmp");
    if let Ok(cache) = std::env::var("HOSTIG_MARIADB_IT_CACHE") {
        let cached = PathBuf::from(cache).join("runtimes");
        if cached.is_dir() {
            copy_dir_recursive(&cached, &tmp.path().join("runtimes")).expect("cache");
        }
    }
    let catalog = Catalog::embedded().expect("catálogo");
    let entry = catalog
        .find("mariadb", DEFAULT_MARIADB_VERSION)
        .expect("mariadb")
        .clone();
    let root = runtime_dir(tmp.path(), &entry);
    if !root.join(MANIFEST_NAME).is_file() {
        RuntimeService::with_embedded(tmp.path())
            .expect("runtimes")
            .install_blocking("mariadb", DEFAULT_MARIADB_VERSION)
            .expect("install");
        if let Ok(cache) = std::env::var("HOSTIG_MARIADB_IT_CACHE") {
            let _ = copy_dir_recursive(
                &tmp.path().join("runtimes"),
                &PathBuf::from(cache).join("runtimes"),
            );
        }
    }
    tmp
}

fn run_hostig(data: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(hostig())
        .args(args)
        .env("HOSTIG_DATA_DIR", data)
        .env(ALLOW_JOB_WITHOUT_BREAKAWAY_ENV, "1")
        .output()
        .expect("hostig");
    let code = output.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (code, stdout, stderr)
}

fn json_stdout(stdout: &str) -> Value {
    serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!("salida no JSON: {stdout}");
    })
}

fn wait_supervisor(data: &Path, budget: Duration) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        let (code, _, _) = run_hostig(data, &["supervisor", "status"]);
        if code == 0 {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("supervisor no respondió");
}

fn terminate_pid(pid: u32) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, false, pid).expect("OpenProcess");
            TerminateProcess(handle, 1).expect("TerminateProcess");
            let _ = CloseHandle(handle);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        panic!("terminate_pid solo en Windows");
    }
}

#[test]
fn supervisor_muerto_deja_mariadb_viva_y_al_reiniciar_adopta() {
    if skip_unless_windows() {
        return;
    }
    std::env::set_var(ALLOW_JOB_WITHOUT_BREAKAWAY_ENV, "1");
    let tmp = prepare_data_dir();
    let data = tmp.path();
    let port = free_port();

    let (code, out, err) = run_hostig(data, &["supervisor", "start"]);
    assert_eq!(code, 0, "supervisor start: {out}{err}");
    wait_supervisor(data, Duration::from_secs(20));
    let status = json_stdout(&out);
    let supervisor_pid = status
        .get("pid")
        .and_then(Value::as_u64)
        .expect("pid supervisor") as u32;

    let port_s = port.to_string();
    let (code, out, err) = run_hostig(data, &["db", "start", "--port", &port_s]);
    assert_eq!(code, 0, "db start: {out}{err}");
    let started = json_stdout(&out);
    let mariadb_pid = started
        .get("pid")
        .and_then(Value::as_u64)
        .expect("pid mariadb") as u32;
    assert!(hostig_ipc::process_is_running(mariadb_pid));

    // Mata el supervisor de verdad (no drop in-process).
    terminate_pid(supervisor_pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    while hostig_ipc::process_is_running(supervisor_pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !hostig_ipc::process_is_running(supervisor_pid),
        "el supervisor debía haber muerto"
    );
    assert!(
        hostig_ipc::process_is_running(mariadb_pid),
        "MariaDB debía seguir viva tras TerminateProcess del supervisor"
    );

    let (code, out, err) = run_hostig(data, &["supervisor", "start"]);
    assert_eq!(code, 0, "supervisor restart: {out}{err}");
    wait_supervisor(data, Duration::from_secs(20));

    let (code, out, err) = run_hostig(data, &["db", "status"]);
    assert_eq!(code, 0, "db status: {out}{err}");
    let st = json_stdout(&out);
    assert_eq!(st.get("state").and_then(Value::as_str), Some("running"));
    assert_eq!(
        st.get("pid").and_then(Value::as_u64).map(|p| p as u32),
        Some(mariadb_pid),
        "debe adoptar el mismo PID"
    );

    let log_path = data
        .join("instances")
        .join(DEFAULT_INSTANCE_ID)
        .join("logs")
        .join("error.log");
    let offset = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);

    let (code, out, err) = run_hostig(data, &["db", "stop"]);
    assert_eq!(code, 0, "db stop: {out}{err}");
    assert!(!hostig_ipc::process_is_running(mariadb_pid));
    assert!(matches!(
        check_port(port, None).expect("port"),
        PortState::Libre
    ));
    let log = read_log_since(&log_path, offset).expect("log");
    assert!(
        log_contains_shutdown(&log),
        "falta línea de apagado:\n{log}"
    );
    // El arranque previo debe haber dejado ready en el log completo.
    let full = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log_contains_ready(&full),
        "falta ready for connections en el log"
    );

    let _ = run_hostig(data, &["supervisor", "stop"]);
}
