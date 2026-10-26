//! Integración MariaDB (bloque 3a). En Linux/macOS se saltan hasta las fases 1b/1c.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hostig_ports::{check as check_port, PortState};
use hostig_processes::mariadb::{
    log_contains_shutdown, new_log_offset, read_log_since, DbService, DbState, InstancePaths,
    StartRequest, DEFAULT_INSTANCE_ID, DEFAULT_MARIADB_VERSION,
};
use hostig_processes::{emergency_kill, ProcessRecord};
use hostig_runtimes::{runtime_dir, Catalog, RuntimeService, MANIFEST_NAME};

fn skip_unless_windows() -> bool {
    if cfg!(windows) {
        return false;
    }
    eprintln!(
        "omitido: integración MariaDB solo en Windows hasta las fases 1b (Linux) y 1c (macOS)"
    );
    true
}

fn data_dir() -> tempfile::TempDir {
    if let Ok(cache) = std::env::var("HOSTIG_MARIADB_IT_CACHE") {
        let path = PathBuf::from(cache);
        std::fs::create_dir_all(&path).expect("cache dir");
        // TempDir que no borra el caché de runtimes: usamos un subdir efímero
        // con enlace/copia del runtime instalado si existe.
        let tmp = tempfile::tempdir().expect("tmp");
        let cached_runtimes = path.join("runtimes");
        if cached_runtimes.is_dir() {
            copy_dir_recursive(&cached_runtimes, &tmp.path().join("runtimes"))
                .expect("copiar runtimes cacheados");
        }
        return tmp;
    }
    tempfile::tempdir().expect("tmp")
}

fn persist_runtime_cache(tmp: &Path) {
    let Ok(cache) = std::env::var("HOSTIG_MARIADB_IT_CACHE") else {
        return;
    };
    let src = tmp.join("runtimes");
    if !src.is_dir() {
        return;
    }
    let dest = PathBuf::from(cache).join("runtimes");
    let _ = std::fs::create_dir_all(&dest);
    if let Err(error) = copy_dir_recursive(&src, &dest) {
        eprintln!("aviso: no se pudo persistir caché de runtimes: {error}");
    }
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

fn ensure_mariadb_installed(data: &Path) {
    let catalog = Catalog::embedded().expect("catálogo");
    let entry = catalog
        .find("mariadb", DEFAULT_MARIADB_VERSION)
        .expect("mariadb en catálogo")
        .clone();
    let root = runtime_dir(data, &entry);
    if root.join(MANIFEST_NAME).is_file() {
        return;
    }
    let service = RuntimeService::with_embedded(data).expect("runtimes");
    service
        .install_blocking("mariadb", DEFAULT_MARIADB_VERSION)
        .expect("instalar mariadb del catálogo");
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.local_addr().expect("addr").port()
}

fn wait_job(service: &DbService, job_id: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        let job = service.job(job_id).expect("job");
        match job.state.as_str() {
            "succeeded" => return,
            "failed" => {
                let err = job.error.expect("error");
                panic!("{}: {} ({})", err.message, err.cause, err.action);
            }
            _ if Instant::now() > deadline => panic!("job {job_id} no terminó a tiempo"),
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
}

#[test]
fn ciclo_completo_iniciar_ping_apagar() {
    if skip_unless_windows() {
        return;
    }
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let port = free_port();
    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    let job = service
        .start_async(StartRequest {
            id: DEFAULT_INSTANCE_ID.into(),
            version: DEFAULT_MARIADB_VERSION.into(),
            port,
        })
        .expect("start");
    wait_job(&service, &job.job_id, Duration::from_secs(180));

    let status = service.status();
    assert_eq!(status.state, DbState::Running);
    let pid = status.pid.expect("pid");
    assert!(hostig_ipc::process_is_running(pid));

    let paths = InstancePaths::new(tmp.path(), DEFAULT_INSTANCE_ID);
    let offset = new_log_offset(&paths.error_log).expect("offset");
    let stop = service.stop_async().expect("stop");
    wait_job(&service, &stop.job_id, Duration::from_secs(60));

    assert_eq!(service.status().state, DbState::Stopped);
    assert!(!hostig_ipc::process_is_running(pid));
    assert!(matches!(
        check_port(port, None).expect("port"),
        PortState::Libre
    ));
    let log = read_log_since(&paths.error_log, offset).expect("log");
    assert!(
        log_contains_shutdown(&log),
        "falta línea de apagado en el error log:\n{log}"
    );
}

#[test]
fn start_con_puerto_ocupado_no_lanza_y_nombra_ocupante() {
    if skip_unless_windows() {
        return;
    }
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let holder = TcpListener::bind(("127.0.0.1", 0)).expect("holder");
    let port = holder.local_addr().expect("addr").port();
    // Mantener el listener vivo.
    let _keep = holder;

    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    let err = service
        .start_async(StartRequest {
            id: DEFAULT_INSTANCE_ID.into(),
            version: DEFAULT_MARIADB_VERSION.into(),
            port,
        })
        .expect_err("debe fallar por puerto ocupado");
    assert!(
        err.message.contains("puerto") || err.cause.contains("puerto"),
        "{err}"
    );
    assert!(
        err.cause.contains("pid=") || err.cause.contains("ocupado"),
        "debe nombrar al ocupante: {}",
        err.cause
    );
    assert!(
        ProcessRecord::load(tmp.path(), DEFAULT_INSTANCE_ID)
            .expect("load")
            .is_none(),
        "no debe haber lanzado ni registrado proceso"
    );
}

#[test]
fn supervisor_muerto_deja_mariadb_viva_y_al_reiniciar_adopta() {
    if skip_unless_windows() {
        return;
    }
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let port = free_port();
    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    let job = service
        .start_async(StartRequest {
            id: DEFAULT_INSTANCE_ID.into(),
            version: DEFAULT_MARIADB_VERSION.into(),
            port,
        })
        .expect("start");
    wait_job(&service, &job.job_id, Duration::from_secs(180));
    let pid = service.status().pid.expect("pid");
    assert!(hostig_ipc::process_is_running(pid));

    // Simula la muerte del supervisor: soltamos el servicio sin stop.
    // El Child se dropea; MariaDB debe seguir (sin Job kill-on-close).
    drop(service);
    thread::sleep(Duration::from_millis(500));
    assert!(
        hostig_ipc::process_is_running(pid),
        "MariaDB debía seguir viva tras morir el supervisor"
    );

    let again = DbService::with_embedded(tmp.path()).expect("db reinicio");
    let status = again.status();
    assert_eq!(status.state, DbState::Running);
    assert_eq!(status.pid, Some(pid));

    // Limpieza: emergencia + o stop ordenado.
    let service = Arc::new(again);
    let stop = service.stop_async().expect("stop");
    if let Err(error) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_job(&service, &stop.job_id, Duration::from_secs(60));
    })) {
        let _ = emergency_kill(tmp.path(), DEFAULT_INSTANCE_ID, pid);
        std::panic::resume_unwind(error);
    }
}
