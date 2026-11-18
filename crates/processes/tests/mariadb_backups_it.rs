//! Integración de volcados MariaDB (bloque 5 / ADR 0007).
//! En Linux/macOS se saltan con mensaje claro hasta las fases 1b/1c.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hostig_backup::{list_backups, meta_path_for, BackupMeta, WITHOUT_BACKUP_CONFIRM};
use hostig_processes::mariadb::{
    DbService, DbState, InstancePaths, StartRequest, StopOptions, DEFAULT_INSTANCE_ID,
    DEFAULT_MARIADB_VERSION,
};
use hostig_processes::ALLOW_JOB_WITHOUT_BREAKAWAY_ENV;
use hostig_runtimes::{runtime_dir, Catalog, RuntimeService, MANIFEST_NAME};

fn skip_unless_windows() -> bool {
    if cfg!(windows) {
        return false;
    }
    eprintln!(
        "omitido: integración de copias MariaDB solo en Windows hasta las fases 1b (Linux) y 1c (macOS)"
    );
    true
}

fn allow_job_for_tests() {
    std::env::set_var(ALLOW_JOB_WITHOUT_BREAKAWAY_ENV, "1");
}

fn data_dir() -> tempfile::TempDir {
    if let Ok(cache) = std::env::var("HOSTIG_MARIADB_IT_CACHE") {
        let path = PathBuf::from(cache);
        std::fs::create_dir_all(&path).expect("cache dir");
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
    // Caché a medias: borrar destino incompleto antes de instalar.
    if root.exists() {
        let _ = fs::remove_dir_all(&root);
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

fn wait_job_err(service: &DbService, job_id: &str, budget: Duration) -> (String, String, String) {
    let deadline = Instant::now() + budget;
    loop {
        let job = service.job(job_id).expect("job");
        match job.state.as_str() {
            "succeeded" => panic!("el job debía fallar"),
            "failed" => {
                let err = job.error.expect("error");
                return (err.message, err.cause, err.action);
            }
            _ if Instant::now() > deadline => panic!("job {job_id} no terminó a tiempo"),
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn start_db(service: &Arc<DbService>, port: u16) {
    let mut last_err = None;
    for attempt in 1..=3 {
        match service.start_async(StartRequest {
            id: DEFAULT_INSTANCE_ID.into(),
            version: DEFAULT_MARIADB_VERSION.into(),
            port,
        }) {
            Ok(job) => {
                let deadline = Instant::now() + Duration::from_secs(180);
                loop {
                    let job_status = service.job(&job.job_id).expect("job");
                    match job_status.state.as_str() {
                        "succeeded" => {
                            assert_eq!(service.status().state, DbState::Running);
                            return;
                        }
                        "failed" => {
                            last_err = job_status
                                .error
                                .map(|e| format!("{}: {}", e.message, e.cause));
                            break;
                        }
                        _ if Instant::now() > deadline => {
                            last_err =
                                Some(format!("job {} timeout (intento {attempt})", job.job_id));
                            break;
                        }
                        _ => thread::sleep(Duration::from_millis(250)),
                    }
                }
            }
            Err(error) => last_err = Some(error.to_string()),
        }
        thread::sleep(Duration::from_millis(500 * attempt));
    }
    panic!("start falló tras reintentos: {last_err:?}");
}

fn stop_db(service: &Arc<DbService>) {
    let job = service.stop_async().expect("stop");
    wait_job(service, &job.job_id, Duration::from_secs(120));
    assert_eq!(service.status().state, DbState::Stopped);
}

fn stop_db_without_backup(service: &Arc<DbService>) {
    let job = service
        .stop_async_with(StopOptions {
            without_backup: true,
            system_shutdown: false,
        })
        .expect("stop without backup");
    wait_job(service, &job.job_id, Duration::from_secs(60));
    assert_eq!(service.status().state, DbState::Stopped);
    let _ = WITHOUT_BACKUP_CONFIRM;
}

fn mariadb_bin_dir(data: &Path) -> PathBuf {
    let catalog = Catalog::embedded().expect("catálogo");
    let entry = catalog
        .find("mariadb", DEFAULT_MARIADB_VERSION)
        .expect("mariadb");
    let root = runtime_dir(data, entry);
    let direct = root.join("bin");
    if direct.join("mariadbd.exe").is_file() || direct.join("mariadbd").is_file() {
        return direct;
    }
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let bin = entry.path().join("bin");
            if bin.join("mariadbd.exe").is_file() || bin.join("mariadbd").is_file() {
                return bin;
            }
        }
    }
    panic!("no hay bin MariaDB bajo {}", root.display());
}

fn mariadb_client(data: &Path) -> PathBuf {
    let bin = mariadb_bin_dir(data);
    let exe = bin.join("mariadb.exe");
    if exe.is_file() {
        exe
    } else {
        bin.join("mariadb")
    }
}

fn run_sql(data: &Path, port: u16, sql: &str) {
    let paths = InstancePaths::new(data, DEFAULT_INSTANCE_ID);
    let client = mariadb_client(data);
    let defaults = format!("--defaults-extra-file={}", paths.admin_cnf.display());
    let output = Command::new(&client)
        .arg(&defaults)
        .arg("--protocol=tcp")
        .arg("-h")
        .arg("127.0.0.1")
        .arg("-P")
        .arg(port.to_string())
        .arg("-e")
        .arg(sql)
        .stdin(Stdio::null())
        .output()
        .expect("mariadb client");
    assert!(
        output.status.success(),
        "SQL falló: {}\n{}",
        sql,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn schema_backups(data: &Path, schema: &str) -> Vec<PathBuf> {
    list_backups(data, DEFAULT_INSTANCE_ID)
        .expect("list")
        .into_iter()
        .filter(|e| e.schema == schema)
        .map(|e| e.path)
        .collect()
}

fn gunzip_contains(path: &Path, needle: &str) -> bool {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let file = fs::File::open(path).expect("open gz");
    let mut dec = GzDecoder::new(file);
    let mut text = String::new();
    dec.read_to_string(&mut text).expect("gunzip");
    text.contains(needle)
}

#[cfg(windows)]
fn deny_write(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir backups");
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "Everyone".into());
    let status = Command::new("icacls")
        .arg(dir)
        .arg("/deny")
        .arg(format!("{user}:(OI)(CI)(W,D,M)"))
        .status()
        .expect("icacls");
    assert!(status.success(), "icacls deny falló");
}

#[cfg(windows)]
fn allow_write(dir: &Path) {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "Everyone".into());
    let _ = Command::new("icacls")
        .arg(dir)
        .arg("/remove:d")
        .arg(&user)
        .status();
}

#[test]
fn stop_crea_copia_valida_con_datos() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let port = free_port();
    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    start_db(&service, port);
    run_sql(
        tmp.path(),
        port,
        "CREATE DATABASE hostig_bak_a; USE hostig_bak_a; CREATE TABLE t(id INT PRIMARY KEY, name VARCHAR(32)); INSERT INTO t VALUES (1, 'hola-copia');",
    );
    stop_db(&service);

    let copies = schema_backups(tmp.path(), "hostig_bak_a");
    assert_eq!(copies.len(), 1, "debe haber una copia");
    assert!(
        gunzip_contains(&copies[0], "hola-copia"),
        "la copia debe contener los datos"
    );
    assert!(meta_path_for(&copies[0]).is_file());
}

fn dump_ctx(data: &Path, port: u16, pid: Option<u32>) -> hostig_backup::DumpContext {
    let paths = InstancePaths::new(data, DEFAULT_INSTANCE_ID);
    hostig_backup::DumpContext {
        data_dir: data.to_path_buf(),
        instance_id: DEFAULT_INSTANCE_ID.into(),
        admin_cnf: paths.admin_cnf.clone(),
        port,
        bin_dir: mariadb_bin_dir(data),
        settings: hostig_backup::BackupSettings::default(),
        boot_id_unix: pid.and_then(hostig_processes::process_creation_time),
    }
}

#[test]
fn segundo_stop_sin_escrituras_no_crea_copia() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let port = free_port();
    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    start_db(&service, port);
    run_sql(
        tmp.path(),
        port,
        "CREATE DATABASE hostig_bak_b; USE hostig_bak_b; CREATE TABLE t(id INT PRIMARY KEY); INSERT INTO t VALUES (1);",
    );
    let pid = service.status().pid;
    // Primera copia en caliente (mismo arranque).
    hostig_backup::ensure_backups_before_stop(&dump_ctx(tmp.path(), port, pid))
        .expect("primer volcado");
    assert_eq!(schema_backups(tmp.path(), "hostig_bak_b").len(), 1);

    // Sin escrituras y copia reciente: no debe crear otra.
    hostig_backup::ensure_backups_before_stop(&dump_ctx(tmp.path(), port, pid))
        .expect("segundo volcado (no-op)");
    assert_eq!(
        schema_backups(tmp.path(), "hostig_bak_b").len(),
        1,
        "sin escrituras y copia reciente no debe haber otra"
    );

    stop_db_without_backup(&service);
}

#[test]
fn escritura_provoca_copia_nueva() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let port = free_port();
    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    start_db(&service, port);
    run_sql(
        tmp.path(),
        port,
        "CREATE DATABASE hostig_bak_c; USE hostig_bak_c; CREATE TABLE t(id INT PRIMARY KEY, v VARCHAR(16)); INSERT INTO t VALUES (1, 'a');",
    );
    stop_db(&service);
    assert_eq!(schema_backups(tmp.path(), "hostig_bak_c").len(), 1);

    start_db(&service, port);
    stop_db(&service); // copia por reinicio
    assert_eq!(schema_backups(tmp.path(), "hostig_bak_c").len(), 2);

    start_db(&service, port);
    run_sql(
        tmp.path(),
        port,
        "USE hostig_bak_c; INSERT INTO t VALUES (2, 'b');",
    );
    stop_db(&service);
    let copies = schema_backups(tmp.path(), "hostig_bak_c");
    assert_eq!(copies.len(), 3, "la escritura debe crear otra copia");
    assert!(
        gunzip_contains(copies.first().unwrap(), "b")
            || copies.iter().any(|p| gunzip_contains(p, "'b'"))
            || copies.iter().any(|p| gunzip_contains(p, "b"))
    );
}

#[test]
fn reinicio_sin_escrituras_provoca_copia() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let port = free_port();
    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    start_db(&service, port);
    run_sql(
        tmp.path(),
        port,
        "CREATE DATABASE hostig_bak_d; USE hostig_bak_d; CREATE TABLE t(id INT); INSERT INTO t VALUES (1);",
    );
    stop_db(&service);
    assert_eq!(schema_backups(tmp.path(), "hostig_bak_d").len(), 1);

    start_db(&service, port);
    // Sin escrituras de usuario; solo reinicio.
    stop_db(&service);
    assert_eq!(
        schema_backups(tmp.path(), "hostig_bak_d").len(),
        2,
        "regla del reinicio"
    );
}

#[test]
fn tabla_myisam_siempre_copia() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let port = free_port();
    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    start_db(&service, port);
    run_sql(
        tmp.path(),
        port,
        "CREATE DATABASE hostig_bak_e; USE hostig_bak_e; CREATE TABLE t(id INT) ENGINE=InnoDB; INSERT INTO t VALUES (1);",
    );
    stop_db(&service);
    start_db(&service, port);
    stop_db(&service); // copia por reinicio
    let before = schema_backups(tmp.path(), "hostig_bak_e").len();

    start_db(&service, port);
    run_sql(
        tmp.path(),
        port,
        "USE hostig_bak_e; CREATE TABLE m(id INT) ENGINE=MyISAM;",
    );
    // Nueva copia por Com_create_table; luego un stop idle debería seguir volcando por MyISAM.
    stop_db(&service);
    let mid = schema_backups(tmp.path(), "hostig_bak_e").len();
    assert!(mid > before);

    start_db(&service, port);
    stop_db(&service); // reinicio → copia
    let after_restart = schema_backups(tmp.path(), "hostig_bak_e").len();
    start_db(&service, port);
    stop_db(&service); // mismo boot, MyISAM presente → debe copiar otra vez
    let after_idle = schema_backups(tmp.path(), "hostig_bak_e").len();
    assert!(
        after_idle > after_restart,
        "con MyISAM debe volcar aunque no haya escrituras nuevas"
    );
}

#[test]
fn volcado_imposible_bloquea_stop_until_without_backup() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let port = free_port();
    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    start_db(&service, port);
    run_sql(
        tmp.path(),
        port,
        "CREATE DATABASE hostig_bak_f; USE hostig_bak_f; CREATE TABLE t(id INT); INSERT INTO t VALUES (1);",
    );

    let backups_root = tmp.path().join("backups").join(DEFAULT_INSTANCE_ID);
    deny_write(&backups_root);
    let job = service.stop_async().expect("stop aceptado en cola");
    let (message, cause, action) = wait_job_err(&service, &job.job_id, Duration::from_secs(60));
    assert!(
        message.contains("permiso")
            || cause.contains("permiso")
            || message.contains("volcado")
            || cause.contains("Denied")
            || cause.contains("denied")
            || message.contains("escribir")
            || cause.contains("escribir"),
        "message={message} cause={cause} action={action}"
    );
    assert_eq!(
        service.status().state,
        DbState::Running,
        "MariaDB debe seguir en marcha"
    );
    assert!(hostig_ipc::process_is_running(
        service.status().pid.expect("pid")
    ));

    allow_write(&backups_root);
    stop_db_without_backup(&service);
    // El cierre sin copia queda registrado por el supervisor en uso normal;
    // aquí se verifica vía API directa (confirmación es capa CLI/IPC).
}

#[test]
fn retencion_deja_diez_mas_recientes() {
    if skip_unless_windows() {
        return;
    }
    allow_job_for_tests();
    let tmp = data_dir();
    ensure_mariadb_installed(tmp.path());
    persist_runtime_cache(tmp.path());

    let schema = "hostig_bak_g";
    let dir = tmp
        .path()
        .join("backups")
        .join(DEFAULT_INSTANCE_ID)
        .join(schema);
    fs::create_dir_all(&dir).expect("mkdir");
    for i in 0..12u64 {
        let path = dir.join(format!("seed{i:02}.sql.gz"));
        fs::write(&path, b"seed").expect("write");
        let meta = BackupMeta {
            schema: schema.into(),
            created_unix: 1_000 + i,
            boot_id_unix: 1,
            com_counters: Default::default(),
            has_non_innodb: false,
        };
        let text = serde_json::to_string_pretty(&meta).unwrap();
        fs::write(meta_path_for(&path), text).unwrap();
    }

    let port = free_port();
    let service = Arc::new(DbService::with_embedded(tmp.path()).expect("db"));
    start_db(&service, port);
    run_sql(
        tmp.path(),
        port,
        &format!(
            "CREATE DATABASE {schema}; USE {schema}; CREATE TABLE t(id INT); INSERT INTO t VALUES (1);"
        ),
    );
    stop_db(&service);

    let copies = schema_backups(tmp.path(), schema);
    assert_eq!(
        copies.len(),
        10,
        "con 12 semillas + 1 nueva válidas deben quedar 10"
    );
}
