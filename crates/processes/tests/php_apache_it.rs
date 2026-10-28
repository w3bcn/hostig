//! Integración PHP + Apache (bloque 4). En Linux/macOS se saltan hasta la fase 1b.
//!
//! Requiere `HOSTIG_ALLOW_JOB_WITHOUT_BREAKAWAY=1` en CI/IDE (ADR 0011).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hostig_ports::{check as check_port, PortState};
use hostig_processes::apache::{ApacheSettings, ApacheState};
use hostig_processes::mariadb::{DbService, DbState, DEFAULT_MARIADB_VERSION};
use hostig_processes::php::{PhpSettings, PhpState, DEFAULT_PHP_VERSION};
use hostig_processes::{StackService, StackStartRequest, ALLOW_JOB_WITHOUT_BREAKAWAY_ENV};
use hostig_runtimes::{runtime_dir, Catalog, RuntimeService, MANIFEST_NAME};

fn skip_unless_windows() -> bool {
    if cfg!(windows) {
        return false;
    }
    eprintln!(
        "omitido: integración PHP/Apache solo en Windows hasta la fase 1b (Linux) / 1c (macOS)"
    );
    true
}

fn allow_job_for_tests() {
    std::env::set_var(ALLOW_JOB_WITHOUT_BREAKAWAY_ENV, "1");
}

fn data_dir() -> tempfile::TempDir {
    if let Ok(cache) = std::env::var("HOSTIG_STACK_IT_CACHE") {
        let path = PathBuf::from(cache);
        std::fs::create_dir_all(&path).expect("cache dir");
        let tmp = tempfile::tempdir().expect("tmp");
        let cached = path.join("runtimes");
        if cached.is_dir() {
            copy_dir_recursive(&cached, &tmp.path().join("runtimes")).expect("copiar cache");
        }
        return tmp;
    }
    // Reutilizar caché MariaDB si existe.
    if let Ok(cache) = std::env::var("HOSTIG_MARIADB_IT_CACHE") {
        let path = PathBuf::from(cache);
        let tmp = tempfile::tempdir().expect("tmp");
        let cached = path.join("runtimes");
        if cached.is_dir() {
            let _ = copy_dir_recursive(&cached, &tmp.path().join("runtimes"));
        }
        return tmp;
    }
    tempfile::tempdir().expect("tmp")
}

fn persist_runtime_cache(tmp: &Path) {
    let cache = std::env::var("HOSTIG_STACK_IT_CACHE")
        .or_else(|_| std::env::var("HOSTIG_MARIADB_IT_CACHE"));
    let Ok(cache) = cache else {
        return;
    };
    let src = tmp.join("runtimes");
    if !src.is_dir() {
        return;
    }
    let dest = PathBuf::from(cache).join("runtimes");
    let _ = std::fs::create_dir_all(&dest);
    let _ = copy_dir_recursive(&src, &dest);
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

fn ensure_runtime(data: &Path, component: &str, version: &str) {
    let catalog = Catalog::embedded().expect("catálogo");
    let entry = catalog
        .find(component, version)
        .expect("en catálogo")
        .clone();
    let root = runtime_dir(data, &entry);
    if root.join(MANIFEST_NAME).is_file() {
        return;
    }
    let service = RuntimeService::with_embedded(data).expect("runtimes");
    service
        .install_blocking(component, version)
        .unwrap_or_else(|e| panic!("instalar {component}@{version}: {e}"));
}

fn ensure_stack_runtimes(data: &Path) {
    ensure_runtime(data, "mariadb", DEFAULT_MARIADB_VERSION);
    ensure_runtime(data, "php", DEFAULT_PHP_VERSION);
    ensure_runtime(data, "apache", "2.4.69");
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.local_addr().expect("addr").port()
}

fn wait_stack_job(stack: &StackService, job_id: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        let job = stack.job(job_id).expect("job");
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

fn http_get(port: u16, path: &str) -> (u16, String) {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    let code = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, text)
}

fn make_stack(data: &Path) -> Arc<StackService> {
    let db = Arc::new(DbService::with_embedded(data).expect("db"));
    let php = Arc::new(hostig_processes::php::PhpService::with_embedded(data).expect("php"));
    let apache =
        Arc::new(hostig_processes::apache::ApacheService::with_embedded(data).expect("apache"));
    // Plazos cortos en IT donde aplica.
    php.set_settings_for_tests(PhpSettings {
        pool_size: 2,
        start_total_secs: 60,
        health_interval_secs: 1,
        ..PhpSettings::default()
    })
    .unwrap();
    apache
        .set_settings_for_tests(ApacheSettings {
            start_total_secs: 60,
            health_interval_secs: 1,
            ..ApacheSettings::default()
        })
        .unwrap();
    Arc::new(StackService::new(data, db, php, apache))
}

#[test]
fn ciclo_completo_start_health_stop() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_stack_runtimes(tmp.path());
    persist_runtime_cache(tmp.path());

    // Puerto HTTP dentro del rango ADR 0004.
    let http_port = hostig_ports::suggest(8080, hostig_ports::ranges::HTTP.clone()).unwrap_or(8080);
    let db_port = free_port();
    let stack = make_stack(tmp.path());
    let job = stack
        .start_async(StackStartRequest {
            http_port: Some(http_port),
            db_port: Some(db_port),
            php_pool_size: Some(2),
            ..StackStartRequest::default()
        })
        .expect("start");
    wait_stack_job(&stack, &job.job_id, Duration::from_secs(300));

    let status = stack.status();
    let http = status.apache.get("port").and_then(|v| v.as_u64()).unwrap() as u16;
    let (code, body) = http_get(http, "/__hostig/health");
    assert_eq!(code, 200, "health body={body}");

    let db_pid = status
        .mariadb
        .get("pid")
        .and_then(|v| v.as_u64())
        .map(|p| p as u32);
    let apache_pid = status
        .apache
        .get("pid")
        .and_then(|v| v.as_u64())
        .map(|p| p as u32);

    let stop = stack.stop_async().expect("stop");
    wait_stack_job(&stack, &stop.job_id, Duration::from_secs(120));

    assert_eq!(stack.db.status().state, DbState::Stopped);
    assert_eq!(stack.php.status().state, PhpState::Stopped);
    assert_eq!(stack.apache.status().state, ApacheState::Stopped);
    if let Some(pid) = db_pid {
        assert!(!hostig_ipc::process_is_running(pid));
    }
    if let Some(pid) = apache_pid {
        assert!(!hostig_ipc::process_is_running(pid));
    }
    assert!(matches!(check_port(http, None), Ok(PortState::Libre)));
    assert!(matches!(check_port(db_port, None), Ok(PortState::Libre)));
}

#[test]
fn reciclado_fcgi_max_requests_no_cuenta_backoff() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_stack_runtimes(tmp.path());
    persist_runtime_cache(tmp.path());

    let http_port = hostig_ports::suggest(8081, hostig_ports::ranges::HTTP.clone()).unwrap();
    let db_port = free_port();
    let stack = make_stack(tmp.path());
    // Pool pequeño y MAX_REQUESTS bajo para forzar reciclado.
    stack
        .php
        .set_settings_for_tests(PhpSettings {
            pool_size: 2,
            fcgi_max_requests: 20,
            health_interval_secs: 1,
            ..PhpSettings::default()
        })
        .unwrap();

    let job = stack
        .start_async(StackStartRequest {
            http_port: Some(http_port),
            db_port: Some(db_port),
            php_pool_size: Some(2),
            php_fcgi_max_requests: Some(20),
            ..StackStartRequest::default()
        })
        .expect("start");
    wait_stack_job(&stack, &job.job_id, Duration::from_secs(300));
    let http = stack.apache.status().port;

    for i in 0..200 {
        let (code, body) = http_get(http, "/");
        assert_eq!(code, 200, "petición {i}: {body}");
    }
    // Tras 200 peticiones con MAX_REQUESTS=20 debe haber reciclados; no caídas.
    assert_eq!(
        stack.php.crash_restart_count(),
        0,
        "el reciclado no debe contar para el backoff"
    );

    let stop = stack.stop_async().expect("stop");
    wait_stack_job(&stack, &stop.job_id, Duration::from_secs(120));
}

#[test]
fn muerte_php_cgi_reinicia_y_apache_sigue() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_stack_runtimes(tmp.path());
    persist_runtime_cache(tmp.path());

    let http_port = hostig_ports::suggest(8082, hostig_ports::ranges::HTTP.clone()).unwrap();
    let db_port = free_port();
    let stack = make_stack(tmp.path());
    stack
        .php
        .set_settings_for_tests(PhpSettings {
            pool_size: 2,
            health_interval_secs: 1,
            backoff_secs: vec![1, 1, 1, 1, 1],
            ..PhpSettings::default()
        })
        .unwrap();

    let job = stack
        .start_async(StackStartRequest {
            http_port: Some(http_port),
            db_port: Some(db_port),
            php_pool_size: Some(2),
            ..StackStartRequest::default()
        })
        .expect("start");
    wait_stack_job(&stack, &job.job_id, Duration::from_secs(300));

    let workers = stack.php.status().workers;
    assert!(workers.len() >= 2);
    let victim_id = workers[0].id.clone();
    let victim = workers[0].pid.expect("pid");
    // Muerte no reciclada: solo el PID registrado (no taskkill por nombre).
    hostig_processes::emergency_kill(tmp.path(), &victim_id, victim).expect("matar worker");

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if stack.php.crash_restart_count() >= 1 {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    assert!(
        stack.php.crash_restart_count() >= 1,
        "la muerte debe contar para el backoff"
    );

    let http = stack.apache.status().port;
    let (code, body) = http_get(http, "/");
    assert_eq!(code, 200, "Apache debe seguir sirviendo: {body}");
    assert_eq!(stack.php.status().state, PhpState::Running);

    let stop = stack.stop_async().expect("stop");
    wait_stack_job(&stack, &stop.job_id, Duration::from_secs(120));
}

#[test]
fn puerto_http_ocupado_no_lanza_nada() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    // No hace falta instalar runtimes: debe fallar antes.
    let listener = TcpListener::bind(("127.0.0.1", 8080)).expect("hold 8080");
    let _ = &listener;
    thread::sleep(Duration::from_millis(50));

    let stack = make_stack(tmp.path());
    let err = stack
        .start_async(StackStartRequest {
            http_port: Some(8080),
            ..StackStartRequest::default()
        })
        .expect_err("debe fallar");
    assert!(
        err.message.contains("puerto HTTP ocupado") || err.cause.contains("8080"),
        "{err:?}"
    );
    assert!(
        err.cause.contains("no se ha lanzado") || err.message.contains("ocupado"),
        "debe dejar claro que no lanzó nada: {err:?}"
    );
    assert_eq!(stack.db.status().state, DbState::Stopped);
    assert_eq!(stack.php.status().state, PhpState::Stopped);
    assert_eq!(stack.apache.status().state, ApacheState::Stopped);
    drop(listener);
}

#[test]
fn apagado_apache_usa_httpd_k_shutdown() {
    if skip_unless_windows() {
        return;
    }
    assert_eq!(
        hostig_processes::apache::WINDOWS_SHUTDOWN_METHOD,
        "TerminateProcess del PID registrado"
    );
    // Descartados en la prueba del bloque 4: httpd -k shutdown (servicio Windows)
    // y ctrl-break (CREATE_NO_WINDOW sin consola).
}
