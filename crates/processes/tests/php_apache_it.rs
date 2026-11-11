//! Integración PHP + Apache (bloque 4 + revisión). En Linux/macOS se saltan hasta la fase 1b.
//!
//! Requiere `HOSTIG_ALLOW_JOB_WITHOUT_BREAKAWAY=1` en CI/IDE (ADR 0011).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hostig_ports::{check as check_port, PortState};
use hostig_processes::apache::{self, ApacheSettings, ApacheState};
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
    ensure_runtime(data, "apache", "2.4.70");
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

fn http_body(port: u16, path: &str) -> (u16, String) {
    let (code, raw) = http_get(port, path);
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (code, body)
}

fn make_stack(data: &Path) -> Arc<StackService> {
    let db = Arc::new(DbService::with_embedded(data).expect("db"));
    let php = Arc::new(hostig_processes::php::PhpService::with_embedded(data).expect("php"));
    let apache =
        Arc::new(hostig_processes::apache::ApacheService::with_embedded(data).expect("apache"));
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

fn start_stack(stack: &Arc<StackService>, http_port: u16, db_port: u16) -> u16 {
    let job = stack
        .start_async(StackStartRequest {
            http_port: Some(http_port),
            db_port: Some(db_port),
            php_pool_size: Some(2),
            ..StackStartRequest::default()
        })
        .expect("start");
    wait_stack_job(stack, &job.job_id, Duration::from_secs(300));
    stack.apache.status().port
}

fn stop_stack(stack: &Arc<StackService>) {
    let stop = stack.stop_async().expect("stop");
    wait_stack_job(stack, &stop.job_id, Duration::from_secs(120));
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
    let children = apache_pid.map(apache::child_pids).unwrap_or_default();

    let stop = stack.stop_async().expect("stop");
    wait_stack_job(&stack, &stop.job_id, Duration::from_secs(120));

    assert_eq!(stack.db.status().state, DbState::Stopped);
    assert_eq!(stack.php.status().state, PhpState::Stopped);
    assert_eq!(stack.apache.status().state, ApacheState::Stopped);
    if let Some(pid) = db_pid {
        assert!(!hostig_ipc::process_is_running(pid));
    }
    if let Some(pid) = apache_pid {
        assert!(
            !hostig_ipc::process_is_running(pid),
            "el padre httpd debe haber salido"
        );
    }
    // Tras SetEvent/TerminateProcess el worker puede tardar un instante en salir.
    let deadline = Instant::now() + Duration::from_secs(5);
    for child in &children {
        while hostig_ipc::process_is_running(*child) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        assert!(
            !hostig_ipc::process_is_running(*child),
            "no debe quedar httpd hijo {child}"
        );
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
    assert_eq!(
        stack.php.crash_restart_count(),
        0,
        "el reciclado no debe contar para el backoff"
    );

    stop_stack(&stack);
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

    let http = start_stack(&stack, http_port, db_port);

    let workers = stack.php.status().workers;
    assert!(workers.len() >= 2);
    let victim_id = workers[0].id.clone();
    let victim = workers[0].pid.expect("pid");
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

    let (code, body) = http_get(http, "/");
    assert_eq!(code, 200, "Apache debe seguir sirviendo: {body}");
    assert_eq!(stack.php.status().state, PhpState::Running);

    stop_stack(&stack);
}

#[test]
fn puerto_http_ocupado_no_lanza_nada() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
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
fn apagado_apache_usa_evento_mpm_winnt() {
    if skip_unless_windows() {
        return;
    }
    assert_eq!(
        hostig_processes::apache::WINDOWS_SHUTDOWN_METHOD,
        "evento ap<PID>_shutdown; TerminateProcess del PID registrado como respaldo"
    );
}

#[test]
fn rewrite_wordpress_index_php_front_controller() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_stack_runtimes(tmp.path());
    persist_runtime_cache(tmp.path());

    // .htaccess tipo WordPress presente antes del arranque: no debe romper el health
    // (Alias saca /__hostig/health del DocumentRoot).
    let www = tmp.path().join("www").join("default");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(
        www.join("index.php"),
        b"<?php echo 'WP-FRONT:' . ($_SERVER['REQUEST_URI'] ?? '');\n",
    )
    .unwrap();
    std::fs::write(
        www.join(".htaccess"),
        b"RewriteEngine On\nRewriteCond %{REQUEST_FILENAME} !-f\nRewriteCond %{REQUEST_FILENAME} !-d\nRewriteRule . /index.php [L]\n",
    )
    .unwrap();

    let http_port = hostig_ports::suggest(8083, hostig_ports::ranges::HTTP.clone()).unwrap();
    let stack = make_stack(tmp.path());
    let http = start_stack(&stack, http_port, free_port());

    let (code, body) = http_body(http, "/mi-articulo/");
    assert_eq!(code, 200, "body={body}");
    assert!(
        body.contains("WP-FRONT:") && body.contains("/mi-articulo/"),
        "debe ejecutar index.php vía rewrite: {body}"
    );

    stop_stack(&stack);
}

#[test]
fn php_extension_case_and_trailing_junk_not_source() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_stack_runtimes(tmp.path());
    persist_runtime_cache(tmp.path());

    let www = tmp.path().join("www").join("default");
    std::fs::create_dir_all(&www).unwrap();
    let secret = "<?php echo 'SECRET_SOURCE_LEAK';";
    // Windows: crear config.PHP (misma entrada NTFS que .php en muchos volúmenes).
    std::fs::write(www.join("config.PHP"), secret.as_bytes()).unwrap();
    // Nombre con punto final: en NTFS a menudo se normaliza; si el archivo existe, denegar.
    let _ = std::fs::write(www.join("config.php."), secret.as_bytes());

    let http_port = hostig_ports::suggest(8084, hostig_ports::ranges::HTTP.clone()).unwrap();
    let stack = make_stack(tmp.path());
    let http = start_stack(&stack, http_port, free_port());

    let (code_upper, body_upper) = http_body(http, "/config.PHP");
    assert!(
        !body_upper.contains("<?php"),
        "config.PHP no debe devolver fuente: code={code_upper} body={body_upper}"
    );
    // Si se ejecutó como PHP, no hay <?php en la salida; si 403/404, tampoco.
    assert!(
        code_upper == 200 || code_upper == 403 || code_upper == 404,
        "code={code_upper}"
    );

    let (code_dot, body_dot) = http_body(http, "/config.php.");
    assert!(
        !body_dot.contains("<?php"),
        "config.php. no debe devolver fuente: code={code_dot} body={body_dot}"
    );
    assert!(
        code_dot == 403 || code_dot == 404 || code_dot == 200,
        "code={code_dot}"
    );

    stop_stack(&stack);
}

#[test]
fn script_filename_and_script_name_son_el_archivo_real() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_stack_runtimes(tmp.path());
    persist_runtime_cache(tmp.path());

    let www = tmp.path().join("www").join("default");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(
        www.join("probe.php"),
        b"<?php echo 'SF=' . ($_SERVER['SCRIPT_FILENAME'] ?? '') . '|SN=' . ($_SERVER['SCRIPT_NAME'] ?? '');\n",
    )
    .unwrap();

    let http_port = hostig_ports::suggest(8085, hostig_ports::ranges::HTTP.clone()).unwrap();
    let stack = make_stack(tmp.path());
    let http = start_stack(&stack, http_port, free_port());

    let (code, body) = http_body(http, "/probe.php");
    assert_eq!(code, 200, "body={body}");
    assert!(
        body.contains("probe.php") && body.contains("SF="),
        "SCRIPT_FILENAME debe apuntar al archivo real: {body}"
    );
    assert!(
        body.contains("SN=/probe.php") || body.contains("SN=\\probe.php"),
        "SCRIPT_NAME debe ser /probe.php: {body}"
    );
    assert!(
        !body.contains("fcgi-router"),
        "no debe usar el router legado: {body}"
    );

    stop_stack(&stack);
}

#[test]
fn subcarpeta_con_index_php_propio() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_stack_runtimes(tmp.path());
    persist_runtime_cache(tmp.path());

    let sub = tmp.path().join("www").join("default").join("tienda");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("index.php"), b"<?php echo 'SUBDIR-OK';\n").unwrap();

    let http_port = hostig_ports::suggest(8086, hostig_ports::ranges::HTTP.clone()).unwrap();
    let stack = make_stack(tmp.path());
    let http = start_stack(&stack, http_port, free_port());

    let (code, body) = http_body(http, "/tienda/");
    assert_eq!(code, 200, "body={body}");
    assert!(body.contains("SUBDIR-OK"), "body={body}");

    stop_stack(&stack);
}

/// Caso habitual XAMPP: WordPress en subcarpeta con permalinks, raíz con su
/// propio front controller, y denegación de `.php` con espacio final.
#[test]
fn wordpress_en_subcarpeta_no_roba_la_raiz() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_stack_runtimes(tmp.path());
    persist_runtime_cache(tmp.path());

    let www = tmp.path().join("www").join("default");
    let blog = www.join("blog");
    std::fs::create_dir_all(&blog).unwrap();
    std::fs::write(www.join("index.php"), b"<?php echo 'ROOT';\n").unwrap();
    std::fs::write(
        www.join(".htaccess"),
        b"RewriteEngine On\nRewriteCond %{REQUEST_FILENAME} !-f\nRewriteCond %{REQUEST_FILENAME} !-d\nRewriteRule . /index.php [L]\n",
    )
    .unwrap();
    std::fs::write(
        blog.join("index.php"),
        b"<?php echo 'BLOG:' . ($_SERVER['REQUEST_URI'] ?? '');\n",
    )
    .unwrap();
    std::fs::write(
        blog.join(".htaccess"),
        b"RewriteEngine On\nRewriteBase /blog/\nRewriteCond %{REQUEST_FILENAME} !-f\nRewriteCond %{REQUEST_FILENAME} !-d\nRewriteRule . /blog/index.php [L]\n",
    )
    .unwrap();
    let secret = "<?php echo 'SECRET_SOURCE_LEAK';";
    std::fs::write(www.join("config.php"), secret.as_bytes()).unwrap();

    let http_port = hostig_ports::suggest(8088, hostig_ports::ranges::HTTP.clone()).unwrap();
    let stack = make_stack(tmp.path());
    let http = start_stack(&stack, http_port, free_port());

    let (code_blog, body_blog) = http_body(http, "/blog/mi-articulo/");
    assert_eq!(code_blog, 200, "body={body_blog}");
    assert!(
        body_blog.contains("BLOG:/blog/mi-articulo/"),
        "permalink de subcarpeta debe ejecutar blog/index.php: {body_blog}"
    );
    assert!(
        !body_blog.contains("ROOT"),
        "nunca debe caer en el index de la raíz: {body_blog}"
    );

    let (code_root, body_root) = http_body(http, "/mi-articulo/");
    assert_eq!(code_root, 200, "body={body_root}");
    assert!(
        body_root.contains("ROOT"),
        "permalink de la raíz debe ejecutar index.php raíz: {body_root}"
    );

    let (code_space, body_space) = http_body(http, "/config.php%20");
    assert!(
        !body_space.contains("<?php") && !body_space.contains("SECRET_SOURCE_LEAK"),
        "config.php con espacio final no debe devolver fuente: code={code_space} body={body_space}"
    );
    assert!(
        code_space == 403 || code_space == 404,
        "esperado 403/404 para .php con espacio: code={code_space} body={body_space}"
    );

    stop_stack(&stack);
}

#[test]
fn health_usa_usuario_dedicado_no_root() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_stack_runtimes(tmp.path());
    persist_runtime_cache(tmp.path());

    let http_port = hostig_ports::suggest(8087, hostig_ports::ranges::HTTP.clone()).unwrap();
    let stack = make_stack(tmp.path());
    let _ = start_stack(&stack, http_port, free_port());

    let health = std::fs::read_to_string(
        tmp.path()
            .join("apache")
            .join("apache-default")
            .join("hostig")
            .join("health.php"),
    )
    .expect("health.php");
    assert!(
        health.contains("hostig_health"),
        "debe usar hostig_health: {health}"
    );
    assert!(!health.contains("'root'"), "no debe embeber root: {health}");
    assert!(
        tmp.path()
            .join("instances")
            .join("mariadb-11.4")
            .join("health.password")
            .is_file(),
        "debe existir health.password"
    );

    stop_stack(&stack);
}
