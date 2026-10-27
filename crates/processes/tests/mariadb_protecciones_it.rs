//! Protecciones MariaDB (bloque 3b) con falso mariadbd — Windows, Linux y macOS.
//!
//! Plazos cortos vía MariadbSettings; no esperas reales de minutos.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use hostig_processes::mariadb::{
    regenerate_my_ini, run_preflight, DbService, DbState, InstancePaths, MariadbSettings,
    PreflightOutcome, StartRequest, DEFAULT_INSTANCE_ID, DEFAULT_MARIADB_VERSION,
    TEST_DB_PROBE_ENV, TEST_MARIADBD_ENV,
};
use hostig_processes::ALLOW_JOB_WITHOUT_BREAKAWAY_ENV;

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn allow_job_for_tests() {
    std::env::set_var(ALLOW_JOB_WITHOUT_BREAKAWAY_ENV, "1");
}

fn fake_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-mariadbd"))
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.local_addr().expect("addr").port()
}

fn write_script(paths: &InstancePaths, body: &str) {
    paths.ensure_dirs().expect("dirs");
    let script = paths
        .error_log
        .parent()
        .expect("logs")
        .join("hostig-fake.script");
    std::fs::write(script, body).expect("script");
}

fn short_settings() -> MariadbSettings {
    MariadbSettings {
        hang_secs: 1,
        start_total_secs: 8,
        health_interval_secs: 1,
        health_failures: 3,
        backoff_secs: vec![0, 0, 0, 0, 0],
        backoff_window_secs: 120,
        max_restarts: 5,
    }
}

fn service_with_fake(tmp: &Path, settings: MariadbSettings) -> Arc<DbService> {
    allow_job_for_tests();
    std::env::set_var(TEST_MARIADBD_ENV, fake_bin());
    std::env::set_var(TEST_DB_PROBE_ENV, "tcp");
    let mut service = DbService::with_embedded(tmp).expect("db");
    service.set_settings_for_tests(settings).expect("settings");
    Arc::new(service)
}

fn wait_job(service: &DbService, job_id: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        let job = service.job(job_id).expect("job");
        if job.state != "running" {
            return;
        }
        if Instant::now() >= deadline {
            panic!("job {job_id} no terminó en {budget:?}: {job:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_state(service: &DbService, want: DbState, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        let st = service.status();
        if st.state == want {
            return;
        }
        if Instant::now() >= deadline {
            panic!("estado {:?} no llegó a {:?}: {:?}", st.state, want, st);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn start_and_wait(service: &Arc<DbService>, port: u16) -> hostig_processes::mariadb::DbJobStatus {
    let job = service
        .start_async(StartRequest {
            id: DEFAULT_INSTANCE_ID.into(),
            version: DEFAULT_MARIADB_VERSION.into(),
            port,
        })
        .expect("start_async");
    wait_job(service, &job.job_id, Duration::from_secs(20));
    service.job(&job.job_id).expect("job final")
}

/// Señala al fake (stop file) y espera a que el servicio quede parado.
fn cleanup_service(service: &Arc<DbService>, paths: &InstancePaths) {
    let flag = paths
        .error_log
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("hostig-fake.stop");
    let _ = std::fs::write(&flag, b"stop");
    if let Ok(job) = service.stop_async() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(j) = service.job(&job.job_id) {
                if j.state != "running" {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    thread::sleep(Duration::from_millis(100));
}

#[test]
fn preflight_multi_master_corrupto_y_cuarentena_deshacible() {
    let _guard = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let paths = InstancePaths::new(tmp.path(), DEFAULT_INSTANCE_ID);
    paths.ensure_dirs().unwrap();
    let bad = paths.datadir.join("multi-master.info");
    std::fs::write(&bad, "2026-10-21 12:00:00 [Note] basura\n").unwrap();
    assert!(matches!(
        run_preflight(&paths).unwrap(),
        PreflightOutcome::Failed { .. }
    ));

    let service = service_with_fake(tmp.path(), short_settings());
    let manifest = service.quarantine().expect("quarantine");
    assert!(!bad.exists());
    assert!(matches!(
        run_preflight(&paths).unwrap(),
        PreflightOutcome::Ok
    ));
    let listed = service.quarantine_list().unwrap();
    assert!(listed.contains(&manifest.timestamp));
    service
        .quarantine_restore(&manifest.timestamp)
        .expect("restore");
    assert!(bad.exists());
}

#[test]
fn arranque_colgado_falla_con_colgado_tras() {
    let _guard = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let paths = InstancePaths::new(tmp.path(), DEFAULT_INSTANCE_ID);
    write_script(&paths, "0 Server socket created on IP: '127.0.0.1'\nHANG\n");
    let service = service_with_fake(tmp.path(), short_settings());
    let port = free_port();
    let job = start_and_wait(&service, port);
    assert_eq!(job.state, "failed");
    let err = job.error.expect("error");
    assert!(err.cause.contains("colgado tras"), "cause={}", err.cause);
    assert!(err.cause.contains("Server socket created"));
    let st = service.status();
    assert_eq!(st.state, DbState::Failed);
    cleanup_service(&service, &paths);
}

#[test]
fn recuperacion_lenta_con_log_avanzando_no_corta_antes_del_tope() {
    let _guard = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let paths = InstancePaths::new(tmp.path(), DEFAULT_INSTANCE_ID);
    // Log avanza cada ~100 ms; hang=5s no debe dispararse; tope total=2s sí.
    let mut script = String::from("0 Starting crash recovery\n");
    for i in 0..40 {
        script.push_str(&format!("100 InnoDB: recovering page {i}\n"));
    }
    script.push_str("HANG\n");
    write_script(&paths, &script);

    let settings = MariadbSettings {
        hang_secs: 5,
        start_total_secs: 2,
        ..short_settings()
    };
    let service = service_with_fake(tmp.path(), settings);
    assert_eq!(service.settings().start_total_secs, 2);
    assert_eq!(service.settings().hang_secs, 5);
    let port = free_port();
    let started = Instant::now();
    let job = start_and_wait(&service, port);
    let elapsed = started.elapsed();
    assert_eq!(job.state, "failed");
    let cause = job.error.expect("error").cause;
    assert!(
        cause.contains("tope total") || cause.contains("agotado"),
        "cause={cause}"
    );
    assert!(
        !cause.contains("colgado tras"),
        "no debió ser cuelgue: {cause}"
    );
    // Debió respetar el tope (~2s), no cortar por cuelgue (5s).
    assert!(
        elapsed >= Duration::from_millis(1500),
        "cortó demasiado pronto: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "esperó el hang o más: {elapsed:?}"
    );
    cleanup_service(&service, &paths);
}

#[test]
fn misma_ultima_linea_dos_veces_bloquea_tercer_intento() {
    let _guard = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let paths = InstancePaths::new(tmp.path(), DEFAULT_INSTANCE_ID);
    write_script(&paths, "0 Server socket created\nHANG\n");
    let service = service_with_fake(tmp.path(), short_settings());
    let port = free_port();

    let j1 = start_and_wait(&service, port);
    assert_eq!(j1.state, "failed");
    assert!(j1.error.as_ref().unwrap().cause.contains("colgado tras"));

    // Proceso colgado aún vivo: segundo start reutiliza y vuelve a colgar.
    let j2 = start_and_wait(&service, port);
    assert_eq!(j2.state, "failed");
    assert!(service.status().hang_blocked);

    let third = service.start_async(StartRequest {
        id: DEFAULT_INSTANCE_ID.into(),
        version: DEFAULT_MARIADB_VERSION.into(),
        port,
    });
    let err = third.expect_err("tercer intento bloqueado");
    assert!(
        err.message.contains("bloqueado") || err.cause.contains("misma última línea"),
        "{err}"
    );

    // Actuar: stop desbloquea.
    cleanup_service(&service, &paths);
    assert!(!service.status().hang_blocked);
}

#[test]
fn lsn_en_el_futuro_failed_aunque_responda_el_puerto() {
    let _guard = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let paths = InstancePaths::new(tmp.path(), DEFAULT_INSTANCE_ID);
    write_script(
        &paths,
        "0 InnoDB: log sequence number is in the future\n0 ready for connections.\nLISTEN\nHANG\n",
    );
    let service = service_with_fake(tmp.path(), short_settings());
    let port = free_port();
    let job = start_and_wait(&service, port);
    assert_eq!(job.state, "failed");
    let cause = job.error.expect("error").cause;
    assert!(
        cause.to_ascii_lowercase().contains("future")
            || cause.to_ascii_lowercase().contains("innodb")
            || cause.contains("no usable"),
        "cause={cause}"
    );
    assert_eq!(service.status().state, DbState::Failed);
    cleanup_service(&service, &paths);
}

#[test]
fn config_con_skip_grant_tables_se_regenera_con_aviso() {
    let _guard = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let paths = InstancePaths::new(tmp.path(), DEFAULT_INSTANCE_ID);
    paths.ensure_dirs().unwrap();
    std::fs::write(&paths.my_ini, "[mysqld]\nskip-grant-tables\nport=3306\n").unwrap();
    let regen = regenerate_my_ini(&paths, Path::new("C:/fake-basedir"), 3310).unwrap();
    assert!(regen.changed);
    assert!(regen
        .dangerous_directives
        .iter()
        .any(|d| d == "skip-grant-tables"));
    let text = std::fs::read_to_string(&paths.my_ini).unwrap();
    assert!(!text.to_ascii_lowercase().contains("skip-grant-tables"));
    assert!(text.contains("3310"));
}

#[test]
fn muerte_inesperada_reinicia_con_backoff_y_sexta_queda_en_error() {
    let _guard = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let paths = InstancePaths::new(tmp.path(), DEFAULT_INSTANCE_ID);
    // Listo, escucha un rato y muere → vigilancia reinicia.
    // 400 ms da margen en CI macOS para ver ready+ping antes del EXIT.
    write_script(&paths, "0 ready for connections.\nLISTEN\n400 EXIT 1\n");
    let settings = MariadbSettings {
        hang_secs: 3,
        start_total_secs: 8,
        health_interval_secs: 1,
        health_failures: 3,
        backoff_secs: vec![0, 0, 0, 0, 0],
        backoff_window_secs: 120,
        max_restarts: 5,
    };
    let service = service_with_fake(tmp.path(), settings);
    let port = free_port();
    let job = start_and_wait(&service, port);
    assert_eq!(job.state, "succeeded", "{job:?}");
    wait_state(&service, DbState::Running, Duration::from_secs(5));

    // Tras 5 reinicios en la ventana, la 6ª muerte deja Failed estable.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let st = service.status();
        if st.state == DbState::Failed {
            let err = st.error.unwrap_or_default();
            assert!(
                err.contains("reinicio")
                    || err.contains("error estable")
                    || err.contains("muertes"),
                "error inesperado: {err}"
            );
            break;
        }
        if Instant::now() >= deadline {
            panic!("no llegó a Failed estable: {:?}", service.status());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn preflight_impide_arranque_con_multi_master_corrupto() {
    let _guard = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let paths = InstancePaths::new(tmp.path(), DEFAULT_INSTANCE_ID);
    paths.ensure_dirs().unwrap();
    std::fs::write(
        paths.datadir.join("multi-master.info"),
        "2026-01-01 [Note] bad\n",
    )
    .unwrap();
    write_script(&paths, "0 ready for connections.\nLISTEN\nHANG\n");
    let service = service_with_fake(tmp.path(), short_settings());
    let port = free_port();
    let job = start_and_wait(&service, port);
    assert_eq!(job.state, "failed");
    let err = job.error.expect("error");
    assert!(
        err.message.contains("preflight") || err.cause.contains("multi-master"),
        "{err:?}"
    );
    assert!(err.action.contains("quarantine") || err.action.contains("cuarentena"));
}
