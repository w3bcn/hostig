use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use uuid::Uuid;

use crate::error::{ProcessError, Result};
use crate::mariadb::config::{
    read_password, regenerate_my_ini, InstancePaths, DEFAULT_INSTANCE_ID, DEFAULT_PORT,
};
use crate::mariadb::init::{
    basedir_from_bin, ensure_initialized, exe_in, find_mariadb_bin, generate_password,
};
use crate::mariadb::logparse::{
    last_nonempty_line, log_contains_crash_recovery, log_contains_fatal_innodb, log_contains_ready,
    log_contains_shutdown, new_dump_files, new_log_offset, read_log_since,
};
use crate::mariadb::preflight::{run_preflight, PreflightOutcome};
use crate::mariadb::quarantine::{
    list_quarantines, quarantine_detected, restore_quarantine, QuarantineManifest,
};
use crate::mariadb::settings::{GlobalManifest, MariadbSettings};
use crate::os::{process_creation_time, process_image_path};
use crate::registry::{try_adopt, AdoptOutcome, ProcessRecord};
use crate::spawn::{spawn_server, SpawnSpec};
use hostig_ports::{check as check_port, PortState};
use hostig_runtimes::{runtime_dir, Catalog, MANIFEST_NAME};

pub const DEFAULT_MARIADB_VERSION: &str = "11.4.13";
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Si está definida, se usa este ejecutable en lugar de mariadbd del catálogo (tests).
pub const TEST_MARIADBD_ENV: &str = "HOSTIG_TEST_MARIADBD";
/// Si vale `tcp`, el ping es un connect TCP (tests con fake-mariadbd).
pub const TEST_DB_PROBE_ENV: &str = "HOSTIG_TEST_DB_PROBE";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DbState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbStatus {
    pub id: String,
    pub state: DbState,
    pub port: u16,
    pub pid: Option<u32>,
    pub version: String,
    pub error: Option<String>,
    /// True si dos arranques seguidos se colgaron en la misma línea (ADR 0012).
    #[serde(default)]
    pub hang_blocked: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbJobStatus {
    pub job_id: String,
    pub state: String,
    pub id: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub error: Option<ProcessErrorJson>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessErrorJson {
    pub message: String,
    pub cause: String,
    pub action: String,
}

impl From<&ProcessError> for ProcessErrorJson {
    fn from(error: &ProcessError) -> Self {
        Self {
            message: error.message.clone(),
            cause: error.cause.clone(),
            action: error.action.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub id: String,
    pub version: String,
    pub port: u16,
}

impl Default for StartRequest {
    fn default() -> Self {
        Self {
            id: DEFAULT_INSTANCE_ID.into(),
            version: DEFAULT_MARIADB_VERSION.into(),
            port: DEFAULT_PORT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadyOutcome {
    Ready,
    Hung {
        last_line: String,
    },
    TotalTimeout {
        last_line: String,
    },
    FatalLog {
        detail: String,
    },
    /// El proceso murió antes de quedar listo (no es cuelgue).
    Exited {
        code: Option<i32>,
        last_line: String,
    },
}

struct Inner {
    status: DbStatus,
    child: Option<std::process::Child>,
    jobs: std::collections::HashMap<String, DbJobStatus>,
    /// Última línea de cuelgue; si se repite, no reintentar (ADR 0012).
    last_hang_line: Option<String>,
    hang_blocked: bool,
    /// Marca de tiempo de reinicios en la ventana de backoff.
    restart_times: Vec<Instant>,
    stop_requested: bool,
    watchdog_started: bool,
}

/// Servicio MariaDB del supervisor (bloque 3a + 3b).
pub struct DbService {
    data_dir: PathBuf,
    catalog: Catalog,
    settings: MariadbSettings,
    inner: Mutex<Inner>,
}

impl DbService {
    pub fn with_embedded(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.into();
        let catalog = Catalog::embedded().map_err(|error| {
            ProcessError::new(
                "catálogo ilegible",
                error.to_string(),
                "recompila con catalog/runtimes.toml",
            )
        })?;
        let settings = GlobalManifest::load(&data_dir)?.mariadb;
        let mut service = Self {
            data_dir: data_dir.clone(),
            catalog,
            settings,
            inner: Mutex::new(Inner {
                status: DbStatus {
                    id: DEFAULT_INSTANCE_ID.into(),
                    state: DbState::Stopped,
                    port: DEFAULT_PORT,
                    pid: None,
                    version: DEFAULT_MARIADB_VERSION.into(),
                    error: None,
                    hang_blocked: false,
                },
                child: None,
                jobs: std::collections::HashMap::new(),
                last_hang_line: None,
                hang_blocked: false,
                restart_times: Vec::new(),
                stop_requested: false,
                watchdog_started: false,
            }),
        };
        service.reconcile_on_boot()?;
        Ok(service)
    }

    /// Sustituye plazos (tests). Persiste en el manifiesto global.
    pub fn set_settings_for_tests(&mut self, settings: MariadbSettings) -> Result<()> {
        self.settings = settings.clone();
        let mut m = GlobalManifest::load(&self.data_dir)?;
        m.mariadb = settings;
        m.save(&self.data_dir)
    }

    pub fn settings(&self) -> &MariadbSettings {
        &self.settings
    }

    fn reconcile_on_boot(&mut self) -> Result<()> {
        let outcome = try_adopt(
            &self.data_dir,
            DEFAULT_INSTANCE_ID,
            hostig_ipc::process_is_running,
            process_creation_time,
            process_image_path,
        )?;
        let mut inner = self.inner.lock().expect("db inner");
        match outcome {
            AdoptOutcome::Adopted(record) => {
                inner.status.state = DbState::Running;
                inner.status.pid = Some(record.pid);
                if record.port != 0 {
                    inner.status.port = record.port;
                }
                if !record.version.is_empty() {
                    inner.status.version = record.version.clone();
                }
                inner.status.id = record.id.clone();
                inner.status.error = None;
            }
            AdoptOutcome::Dead | AdoptOutcome::Missing => {
                inner.status.state = DbState::Stopped;
                inner.status.pid = None;
            }
            AdoptOutcome::Foreign { pid, reason } => {
                inner.status.state = DbState::Failed;
                inner.status.pid = Some(pid);
                inner.status.error = Some(format!("proceso ajeno: {reason}"));
            }
        }
        Ok(())
    }

    pub fn status(&self) -> DbStatus {
        let inner = self.inner.lock().expect("db inner");
        let mut status = inner.status.clone();
        status.hang_blocked = inner.hang_blocked;
        status
    }

    pub fn job(&self, job_id: &str) -> Result<DbJobStatus> {
        self.inner
            .lock()
            .expect("db inner")
            .jobs
            .get(job_id)
            .cloned()
            .ok_or_else(|| {
                ProcessError::new(
                    "trabajo desconocido",
                    format!("no hay job {job_id}"),
                    "usa el id que devolvió db.start o db.stop",
                )
            })
    }

    pub fn quarantine(&self) -> Result<QuarantineManifest> {
        let id = self.inner.lock().expect("db inner").status.id.clone();
        let paths = InstancePaths::new(&self.data_dir, &id);
        let manifest = quarantine_detected(&paths)?;
        // Actuar sobre el datadir desbloquea un hang_blocked previo (ADR 0012).
        let mut inner = self.inner.lock().expect("db inner");
        inner.hang_blocked = false;
        inner.last_hang_line = None;
        Ok(manifest)
    }

    pub fn quarantine_list(&self) -> Result<Vec<String>> {
        let id = self.inner.lock().expect("db inner").status.id.clone();
        let paths = InstancePaths::new(&self.data_dir, &id);
        list_quarantines(&paths)
    }

    pub fn quarantine_restore(&self, timestamp: &str) -> Result<QuarantineManifest> {
        let id = self.inner.lock().expect("db inner").status.id.clone();
        let paths = InstancePaths::new(&self.data_dir, &id);
        restore_quarantine(&paths, timestamp)
    }

    pub fn start_async(self: &Arc<Self>, request: StartRequest) -> Result<DbJobStatus> {
        {
            let inner = self.inner.lock().expect("db inner");
            if matches!(inner.status.state, DbState::Running | DbState::Starting) {
                return Ok(DbJobStatus {
                    job_id: Uuid::new_v4().to_string(),
                    state: "succeeded".into(),
                    id: inner.status.id.clone(),
                    port: inner.status.port,
                    pid: inner.status.pid,
                    error: None,
                });
            }
            // ADR 0012: dos cuelgues en la misma línea → no tercer intento hasta que la persona actúe.
            if inner.hang_blocked {
                return Err(ProcessError::new(
                    "arranque bloqueado",
                    "dos cuelgues con la misma última línea",
                    "corrige el datadir (cuarentena/inspección), hostig db stop si el proceso sigue, luego start",
                ));
            }
        }

        let expected_pid = {
            let inner = self.inner.lock().expect("db inner");
            if matches!(inner.status.state, DbState::Failed) {
                inner
                    .status
                    .pid
                    .filter(|p| hostig_ipc::process_is_running(*p))
            } else {
                None
            }
        };

        match check_port(request.port, expected_pid) {
            Ok(PortState::Libre) => {}
            Ok(state) => {
                return Err(port_busy_error(request.port, &state));
            }
            Err(error) => {
                return Err(ProcessError::new(error.message, error.cause, error.action));
            }
        }

        let job_id = Uuid::new_v4().to_string();
        let status = DbJobStatus {
            job_id: job_id.clone(),
            state: "running".into(),
            id: request.id.clone(),
            port: request.port,
            pid: None,
            error: None,
        };
        {
            let mut inner = self.inner.lock().expect("db inner");
            inner.status.state = DbState::Starting;
            inner.status.id = request.id.clone();
            inner.status.port = request.port;
            inner.status.version = request.version.clone();
            inner.status.error = None;
            inner.stop_requested = false;
            inner.jobs.insert(job_id.clone(), status.clone());
        }

        let service = Arc::clone(self);
        thread::spawn(move || {
            let outcome = service.run_start(request, false);
            service.finish_start_job(&job_id, outcome);
        });

        Ok(status)
    }

    fn finish_start_job(self: &Arc<Self>, job_id: &str, outcome: Result<u32>) {
        let mut inner = self.inner.lock().expect("db inner");
        match &outcome {
            Ok(pid) => {
                if let Some(job) = inner.jobs.get_mut(job_id) {
                    job.state = "succeeded".into();
                    job.pid = Some(*pid);
                }
                inner.status.state = DbState::Running;
                inner.status.pid = Some(*pid);
                inner.status.error = None;
                inner.last_hang_line = None;
            }
            Err(error) => {
                let alive_pid = inner
                    .status
                    .pid
                    .filter(|p| hostig_ipc::process_is_running(*p));
                if let Some(job) = inner.jobs.get_mut(job_id) {
                    job.state = "failed".into();
                    job.error = Some(error.into());
                    job.pid = alive_pid;
                }
                inner.status.state = DbState::Failed;
                inner.status.error = Some(error.to_string());
                inner.status.pid = alive_pid;
            }
        }
        if matches!(inner.status.state, DbState::Running) && !inner.watchdog_started {
            inner.watchdog_started = true;
            drop(inner);
            self.spawn_watchdog();
        }
    }

    fn run_start(&self, request: StartRequest, is_restart: bool) -> Result<u32> {
        let paths = InstancePaths::new(&self.data_dir, &request.id);

        match run_preflight(&paths)? {
            PreflightOutcome::Ok => {}
            failed => {
                return Err(failed.to_error().expect("failed"));
            }
        }

        let entry = self
            .catalog
            .find("mariadb", &request.version)
            .cloned()
            .ok_or_else(|| {
                ProcessError::new(
                    "MariaDB no está en el catálogo",
                    format!("falta mariadb@{}", request.version),
                    "actualiza el catálogo o instala esa versión",
                )
            })?;
        let runtime_root = runtime_dir(&self.data_dir, &entry);
        let test_bin = std::env::var_os(TEST_MARIADBD_ENV).map(PathBuf::from);
        if test_bin.is_none() && !runtime_root.join(MANIFEST_NAME).is_file() {
            return Err(ProcessError::new(
                "MariaDB no instalada",
                format!("falta {}", runtime_root.display()),
                format!("hostig runtimes install mariadb@{}", request.version),
            ));
        }

        let password = if paths.password_file.is_file() {
            read_password(&paths)?
        } else {
            let password = generate_password();
            if test_bin.is_none() {
                ensure_initialized(&paths, &runtime_root, request.port, &password)?;
            } else {
                paths.ensure_dirs()?;
                crate::mariadb::config::write_admin_cnf(&paths, request.port, &password)?;
            }
            password
        };
        if test_bin.is_none() {
            if !paths.datadir.join("mysql").is_dir() {
                ensure_initialized(&paths, &runtime_root, request.port, &password)?;
            }
        } else {
            paths.ensure_dirs()?;
            crate::mariadb::config::write_admin_cnf(&paths, request.port, &password)?;
        }

        let basedir = if let Some(ref exe) = test_bin {
            exe.parent().unwrap_or(exe.as_path()).to_path_buf()
        } else {
            let bin = find_mariadb_bin(&runtime_root)?;
            basedir_from_bin(&bin)
        };
        let regen = regenerate_my_ini(&paths, &basedir, request.port)?;
        if regen.changed {
            eprintln!(
                "{{\"level\":\"info\",\"module\":\"processes\",\"message\":\"my.ini regenerado\",\"cause\":\"{}\",\"action\":\"la plantilla es la fuente de verdad (ADR 0005)\"}}",
                regen.diff_summary.replace('"', "'")
            );
        }
        for d in &regen.dangerous_directives {
            eprintln!(
                "{{\"level\":\"warn\",\"module\":\"processes\",\"message\":\"directiva peligrosa sustituida\",\"cause\":\"la my.ini editada contenía {d}\",\"action\":\"no vuelvas a poner {d}; Hostig regenera la config\"}}"
            );
        }
        let _ = fs_remove_datadir_ini(&paths);

        // Si ya hay un proceso nuestro vivo (Failed), reutilizarlo / no relanzar.
        if let Some(pid) = {
            let inner = self.inner.lock().expect("db inner");
            inner
                .status
                .pid
                .filter(|p| hostig_ipc::process_is_running(*p))
        } {
            let log_offset = new_log_offset(&paths.error_log)?;
            return self.wait_ready_or_fail(&paths, &request, pid, log_offset, is_restart);
        }

        let mariadbd = if let Some(exe) = test_bin {
            exe
        } else {
            let bin = find_mariadb_bin(&runtime_root)?;
            exe_in(&bin, "mariadbd").ok_or_else(|| {
                ProcessError::new(
                    "mariadbd ausente",
                    bin.display().to_string(),
                    "reinstala MariaDB",
                )
            })?
        };

        let defaults = format!("--defaults-file={}", paths.my_ini.display());
        let log_offset = new_log_offset(&paths.error_log)?;
        let path_dirs = if let Some(parent) = mariadbd.parent() {
            vec![parent.to_path_buf()]
        } else {
            vec![]
        };
        let spec = SpawnSpec {
            id: request.id.clone(),
            exe: mariadbd,
            args: vec![defaults],
            path_dirs,
            current_dir: Some(basedir),
            port: request.port,
            version: request.version.clone(),
            env: vec![],
        };
        let spawned = spawn_server(&spec)?;
        let pid = spawned.record.pid;
        spawned.record.save(&self.data_dir)?;
        {
            let mut inner = self.inner.lock().expect("db inner");
            inner.child = Some(spawned.child);
            inner.status.pid = Some(pid);
        }

        self.wait_ready_or_fail(&paths, &request, pid, log_offset, is_restart)
    }

    fn wait_ready_or_fail(
        &self,
        paths: &InstancePaths,
        request: &StartRequest,
        pid: u32,
        log_offset: u64,
        is_restart: bool,
    ) -> Result<u32> {
        let started_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let poll_child = || self.poll_child_exited(pid);
        match wait_until_ready(
            paths,
            request.port,
            &request.version,
            log_offset,
            &self.settings,
            started_unix,
            poll_child,
        )? {
            ReadyOutcome::Ready => {
                let mut inner = self.inner.lock().expect("db inner");
                inner.last_hang_line = None;
                Ok(pid)
            }
            ReadyOutcome::Hung { last_line } => {
                self.note_hang_line(&last_line);
                Err(self.after_ready_timeout(
                    &request.id,
                    request.port,
                    &request.version,
                    pid,
                    ProcessError::new(
                        "MariaDB no quedó lista",
                        format!("colgado tras {last_line}"),
                        "revisa el error log; hostig db quarantine si el preflight lo pide",
                    ),
                ))
            }
            ReadyOutcome::TotalTimeout { last_line } => Err(self.after_ready_timeout(
                &request.id,
                request.port,
                &request.version,
                pid,
                ProcessError::new(
                    "MariaDB no quedó lista",
                    format!("tope total de arranque agotado; última línea: {last_line}"),
                    "revisa el error log; una recuperación InnoDB muy larga puede necesitar intervención",
                ),
            )),
            ReadyOutcome::FatalLog { detail } => {
                let _ = is_restart;
                Err(self.after_ready_timeout(
                    &request.id,
                    request.port,
                    &request.version,
                    pid,
                    ProcessError::new(
                        "MariaDB en estado no usable",
                        detail,
                        "no uses esta instancia hasta decidir; no se fuerza recuperación automática",
                    ),
                ))
            }
            ReadyOutcome::Exited { code, last_line } => {
                // Muerte durante el arranque: cuenta para el cupo de backoff (ADR 0003).
                self.record_startup_death();
                self.clear_dead_startup(&request.id, pid);
                let code_s = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into());
                Err(ProcessError::new(
                    "MariaDB no quedó lista",
                    format!(
                        "MariaDB terminó durante el arranque (código de salida {code_s}); última línea del log: {last_line}"
                    ),
                    "revisa el error log; hostig db start tras corregir la causa",
                ))
            }
        }
    }

    /// `Some(code)` si el hijo ya no vive; `None` si sigue en marcha.
    fn poll_child_exited(&self, pid: u32) -> Option<Option<i32>> {
        let mut inner = self.inner.lock().expect("db inner");
        if let Some(child) = inner.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code();
                    let _ = inner.child.take();
                    return Some(code);
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        drop(inner);
        if !hostig_ipc::process_is_running(pid) {
            Some(None)
        } else {
            None
        }
    }

    fn clear_dead_startup(&self, id: &str, _pid: u32) {
        let _ = ProcessRecord::remove(&self.data_dir, id);
        let mut inner = self.inner.lock().expect("db inner");
        if let Some(mut child) = inner.child.take() {
            let _ = child.try_wait();
        }
        inner.status.pid = None;
    }

    /// Registra una muerte durante el arranque en la ventana de backoff.
    fn record_startup_death(&self) {
        let mut inner = self.inner.lock().expect("db inner");
        let now = Instant::now();
        let window = self.settings.backoff_window();
        inner
            .restart_times
            .retain(|t| now.duration_since(*t) < window);
        inner.restart_times.push(now);
    }

    fn note_hang_line(&self, line: &str) {
        let mut inner = self.inner.lock().expect("db inner");
        if inner.last_hang_line.as_deref() == Some(line) {
            inner.hang_blocked = true;
        } else {
            inner.last_hang_line = Some(line.to_string());
        }
    }

    fn after_ready_timeout(
        &self,
        id: &str,
        port: u16,
        version: &str,
        pid: u32,
        base: ProcessError,
    ) -> ProcessError {
        let paths = InstancePaths::new(&self.data_dir, id);
        let ping_ok = probe_ready(&paths, port, version).unwrap_or(false);
        if ping_ok {
            let _ = admin_shutdown(&paths, port, version, &self.data_dir);
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                {
                    let mut inner = self.inner.lock().expect("db inner");
                    if let Some(child) = inner.child.as_mut() {
                        let _ = child.try_wait();
                    }
                }
                if !hostig_ipc::process_is_running(pid) {
                    let _ = ProcessRecord::remove(&self.data_dir, id);
                    let mut inner = self.inner.lock().expect("db inner");
                    if let Some(mut child) = inner.child.take() {
                        let _ = child.wait();
                    }
                    inner.status.pid = None;
                    return ProcessError::new(
                        base.message,
                        format!("{}; se pidió shutdown tras ping OK", base.cause),
                        base.action,
                    );
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
        // Reapear por si murió sin ping (no afirmar "sigue registrado" si ya no vive).
        {
            let mut inner = self.inner.lock().expect("db inner");
            if let Some(child) = inner.child.as_mut() {
                let _ = child.try_wait();
            }
        }
        if !hostig_ipc::process_is_running(pid) {
            let _ = ProcessRecord::remove(&self.data_dir, id);
            let mut inner = self.inner.lock().expect("db inner");
            if let Some(mut child) = inner.child.take() {
                let _ = child.wait();
            }
            inner.status.pid = None;
            return ProcessError::new(
                base.message,
                format!(
                    "{}; el proceso ya no está vivo (ping_ok={ping_ok})",
                    base.cause
                ),
                base.action,
            );
        }
        ProcessError::new(
            base.message,
            format!(
                "{}; el proceso sigue registrado (ping_ok={ping_ok})",
                base.cause
            ),
            base.action,
        )
    }

    pub fn stop_async(self: &Arc<Self>) -> Result<DbJobStatus> {
        let job_id = Uuid::new_v4().to_string();
        let (id, port, pid, running_status) = {
            let mut inner = self.inner.lock().expect("db inner");
            if matches!(inner.status.state, DbState::Starting) {
                return Err(ProcessError::new(
                    "arranque en curso",
                    "el estado es Starting",
                    "espera a que termine el arranque",
                ));
            }
            if matches!(inner.status.state, DbState::Stopped) {
                return Ok(DbJobStatus {
                    job_id,
                    state: "succeeded".into(),
                    id: inner.status.id.clone(),
                    port: inner.status.port,
                    pid: None,
                    error: None,
                });
            }
            inner.stop_requested = true;
            inner.status.state = DbState::Stopping;
            let status = DbJobStatus {
                job_id: job_id.clone(),
                state: "running".into(),
                id: inner.status.id.clone(),
                port: inner.status.port,
                pid: inner.status.pid,
                error: None,
            };
            inner.jobs.insert(job_id.clone(), status.clone());
            (
                inner.status.id.clone(),
                inner.status.port,
                inner.status.pid,
                status,
            )
        };

        let service = Arc::clone(self);
        let job_id_thread = job_id.clone();
        let id_thread = id.clone();
        thread::spawn(move || {
            let outcome = service.run_stop(&id_thread, port, pid);
            let mut inner = service.inner.lock().expect("db inner");
            if let Some(job) = inner.jobs.get_mut(&job_id_thread) {
                match &outcome {
                    Ok(()) => {
                        job.state = "succeeded".into();
                        job.pid = None;
                        inner.status.state = DbState::Stopped;
                        inner.status.pid = None;
                        inner.status.error = None;
                        // Parar es una actuación de la persona: desbloquea cuelgues repetidos.
                        inner.hang_blocked = false;
                        inner.last_hang_line = None;
                        if let Some(mut child) = inner.child.take() {
                            let _ = child.wait();
                        }
                    }
                    Err(error) => {
                        job.state = "failed".into();
                        job.error = Some(error.into());
                        inner.status.state = DbState::Failed;
                        inner.status.error = Some(error.to_string());
                    }
                }
            }
        });

        Ok(running_status)
    }

    fn run_stop(&self, id: &str, port: u16, pid: Option<u32>) -> Result<()> {
        let paths = InstancePaths::new(&self.data_dir, id);
        let log_offset = new_log_offset(&paths.error_log)?;
        let version = { self.inner.lock().expect("db inner").status.version.clone() };
        let _ = admin_shutdown(&paths, port, &version, &self.data_dir);

        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            {
                let mut inner = self.inner.lock().expect("db inner");
                if let Some(child) = inner.child.as_mut() {
                    let _ = child.try_wait();
                }
            }
            let alive = pid.map(hostig_ipc::process_is_running).unwrap_or(false);
            let port_free = matches!(check_port(port, None), Ok(PortState::Libre));
            let log = read_log_since(&paths.error_log, log_offset).unwrap_or_default();
            if !alive && port_free && log_contains_shutdown(&log) {
                ProcessRecord::remove(&self.data_dir, id)?;
                return Ok(());
            }
            // En modo test TCP el fake no escribe shutdown: aceptar proceso muerto + puerto libre.
            if !alive
                && port_free
                && std::env::var(TEST_DB_PROBE_ENV).ok().as_deref() == Some("tcp")
            {
                ProcessRecord::remove(&self.data_dir, id)?;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(200));
        }

        let alive = pid.map(hostig_ipc::process_is_running).unwrap_or(false);
        let port_free = matches!(check_port(port, None), Ok(PortState::Libre));
        let log = read_log_since(&paths.error_log, log_offset).unwrap_or_default();
        Err(ProcessError::new(
            "apagado incompleto",
            format!(
                "proceso_vivo={alive} puerto_libre={port_free} shutdown_en_log={}",
                log_contains_shutdown(&log)
            ),
            "revisa el error log; usa hostig db kill <pid> solo como emergencia",
        ))
    }

    pub fn kill(&self, cited_pid: u32) -> Result<()> {
        crate::spawn::emergency_kill(&self.data_dir, DEFAULT_INSTANCE_ID, cited_pid)?;
        let mut inner = self.inner.lock().expect("db inner");
        inner.status.state = DbState::Stopped;
        inner.status.pid = None;
        inner.stop_requested = true;
        if let Some(mut child) = inner.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }

    fn spawn_watchdog(self: &Arc<Self>) {
        let service = Arc::clone(self);
        thread::spawn(move || {
            let interval = service.settings.health_interval();
            let mut failures = 0u32;
            loop {
                thread::sleep(interval);
                let (state, pid, port, version, id, stop) = {
                    let inner = service.inner.lock().expect("db inner");
                    (
                        inner.status.state.clone(),
                        inner.status.pid,
                        inner.status.port,
                        inner.status.version.clone(),
                        inner.status.id.clone(),
                        inner.stop_requested,
                    )
                };
                if stop || matches!(state, DbState::Stopping | DbState::Stopped) {
                    let mut inner = service.inner.lock().expect("db inner");
                    inner.watchdog_started = false;
                    break;
                }
                if !matches!(state, DbState::Running) {
                    continue;
                }
                let Some(pid) = pid else { continue };
                // Recoger el Child si ya salió: evita zombies (Unix) y handles
                // abiertos que enmascaran la muerte (Windows).
                let child_exited = {
                    let mut inner = service.inner.lock().expect("db inner");
                    if let Some(child) = inner.child.as_mut() {
                        match child.try_wait() {
                            Ok(Some(_)) => {
                                let _ = inner.child.take();
                                true
                            }
                            Ok(None) => false,
                            Err(_) => false,
                        }
                    } else {
                        false
                    }
                };
                if child_exited || !hostig_ipc::process_is_running(pid) {
                    // Muerte inesperada → backoff.
                    if let Err(error) = service.handle_unexpected_death(&id, port, &version) {
                        let mut inner = service.inner.lock().expect("db inner");
                        inner.status.state = DbState::Failed;
                        inner.status.error = Some(error.to_string());
                        inner.watchdog_started = false;
                        break;
                    }
                    failures = 0;
                    continue;
                }
                let paths = InstancePaths::new(&service.data_dir, &id);
                match probe_ready(&paths, port, &version) {
                    Ok(true) => failures = 0,
                    _ => {
                        failures += 1;
                        if failures >= service.settings.health_failures {
                            let mut inner = service.inner.lock().expect("db inner");
                            inner.status.state = DbState::Failed;
                            inner.status.error =
                                Some(format!("health check: {failures} fallos seguidos de ping"));
                            // No matamos; dejamos el proceso. Watchdog para.
                            inner.watchdog_started = false;
                            break;
                        }
                    }
                }
            }
        });
    }

    fn handle_unexpected_death(self: &Arc<Self>, id: &str, port: u16, version: &str) -> Result<()> {
        let mut inner = self.inner.lock().expect("db inner");
        if inner.stop_requested {
            return Ok(());
        }
        if inner.hang_blocked {
            inner.status.state = DbState::Failed;
            inner.status.error =
                Some("reinicio bloqueado: dos cuelgues con la misma última línea".into());
            inner.watchdog_started = false;
            return Err(ProcessError::new(
                "reinicio bloqueado",
                "misma última línea dos veces",
                "corrige el datadir antes de hostig db start",
            ));
        }
        let now = Instant::now();
        let window = self.settings.backoff_window();
        inner
            .restart_times
            .retain(|t| now.duration_since(*t) < window);
        if inner.restart_times.len() as u32 >= self.settings.max_restarts {
            inner.status.state = DbState::Failed;
            inner.status.pid = None;
            inner.status.error = Some(format!(
                "agotados {} reinicios en {} s",
                self.settings.max_restarts, self.settings.backoff_window_secs
            ));
            inner.watchdog_started = false;
            return Err(ProcessError::new(
                "MariaDB en error estable",
                format!(
                    "más de {} muertes en {} s",
                    self.settings.max_restarts, self.settings.backoff_window_secs
                ),
                "hostig db start explícito tras revisar el error log",
            ));
        }
        let attempt = inner.restart_times.len();
        let wait_secs = self
            .settings
            .backoff_secs
            .get(attempt)
            .copied()
            .unwrap_or_else(|| *self.settings.backoff_secs.last().unwrap_or(&16));
        inner.restart_times.push(now);
        inner.status.state = DbState::Starting;
        inner.status.pid = None;
        if let Some(mut child) = inner.child.take() {
            let _ = child.try_wait();
        }
        let _ = ProcessRecord::remove(&self.data_dir, id);
        drop(inner);

        eprintln!(
            "{{\"level\":\"warn\",\"module\":\"processes\",\"message\":\"reinicio tras muerte inesperada\",\"cause\":\"intento {} espera {wait_secs}s\",\"action\":\"el preflight se repetirá antes de lanzar\"}}",
            attempt + 1
        );
        thread::sleep(Duration::from_secs(wait_secs));

        let request = StartRequest {
            id: id.to_string(),
            version: version.to_string(),
            port,
        };
        let outcome = self.run_start(request, true);
        match outcome {
            Ok(pid) => {
                let mut inner = self.inner.lock().expect("db inner");
                inner.status.state = DbState::Running;
                inner.status.pid = Some(pid);
                inner.status.error = None;
                Ok(())
            }
            Err(error) => {
                let mut inner = self.inner.lock().expect("db inner");
                inner.status.state = DbState::Failed;
                inner.status.error = Some(error.to_string());
                Err(error)
            }
        }
    }
}

fn port_busy_error(port: u16, state: &PortState) -> ProcessError {
    let suggested = hostig_ports::suggest(port, hostig_ports::ranges::MARIADB).ok();
    let (who, action) = match state {
        PortState::Ocupado {
            pid,
            name,
            path,
            action,
        } => {
            let path_s = path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            (
                format!("pid={pid} name={name} path={path_s}"),
                action.clone().unwrap_or_else(|| {
                    format!(
                        "usa el puerto sugerido ({})",
                        suggested.unwrap_or(port.saturating_add(1))
                    )
                }),
            )
        }
        PortState::Reservado => (
            "reservado por el sistema".into(),
            "usa el puerto sugerido".into(),
        ),
        PortState::OcupadoDueñoNoVisible => (
            "ocupado, dueño no visible".into(),
            "usa el puerto sugerido".into(),
        ),
        PortState::Libre => unreachable!(),
    };
    ProcessError::new("puerto ocupado", format!("puerto {port}: {who}"), action)
}

fn fs_remove_datadir_ini(paths: &InstancePaths) -> Result<()> {
    for name in ["my.ini", "my.cnf"] {
        let path = paths.datadir.join(name);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn resolve_last_line(paths: &InstancePaths, last_line: &str) -> String {
    if last_line != "(sin líneas todavía)" {
        return last_line.to_string();
    }
    let full = read_log_since(&paths.error_log, 0).unwrap_or_default();
    last_nonempty_line(&full)
        .unwrap_or("(sin líneas todavía)")
        .to_string()
}

fn wait_until_ready(
    paths: &InstancePaths,
    port: u16,
    version: &str,
    log_offset: u64,
    settings: &MariadbSettings,
    started_unix: u64,
    mut poll_exited: impl FnMut() -> Option<Option<i32>>,
) -> Result<ReadyOutcome> {
    let total_deadline = Instant::now() + settings.start_total();
    let mut last_len = 0u64;
    let mut last_progress = Instant::now();
    let mut last_line = String::from("(sin líneas todavía)");
    let hang = settings.hang();
    let mut warned_recovery = false;

    while Instant::now() < total_deadline {
        if let Some(code) = poll_exited() {
            return Ok(ReadyOutcome::Exited {
                code,
                last_line: resolve_last_line(paths, &last_line),
            });
        }
        let log = read_log_since(&paths.error_log, log_offset)?;
        let len = log.len() as u64;
        if len > last_len {
            last_len = len;
            last_progress = Instant::now();
            if let Some(line) = last_nonempty_line(&log) {
                last_line = line.to_string();
            }
        }
        if !warned_recovery && log_contains_crash_recovery(&log) {
            warned_recovery = true;
            eprintln!(
                "{{\"level\":\"warn\",\"module\":\"processes\",\"message\":\"Starting crash recovery\",\"cause\":\"recuperación InnoDB normal tras un cierre anterior\",\"action\":\"esperar; no es fallo por sí solo\"}}"
            );
        }
        if log_contains_fatal_innodb(&log) {
            return Ok(ReadyOutcome::FatalLog {
                detail: format!(
                    "regla InnoDB: {}",
                    last_nonempty_line(&log).unwrap_or("LSN/corrupt")
                ),
            });
        }
        if let Ok(dumps) = new_dump_files(&paths.datadir, started_unix) {
            if !dumps.is_empty() {
                return Ok(ReadyOutcome::FatalLog {
                    detail: format!("volcado nuevo durante el arranque: {}", dumps[0].display()),
                });
            }
        }
        if log_contains_ready(&log) && probe_ready(paths, port, version)? {
            return Ok(ReadyOutcome::Ready);
        }
        if Instant::now().duration_since(last_progress) >= hang
            && !log_contains_ready(&log)
            && !probe_ready(paths, port, version).unwrap_or(false)
        {
            // Antes de declarar cuelgue, confirmar que el proceso sigue vivo.
            if let Some(code) = poll_exited() {
                return Ok(ReadyOutcome::Exited {
                    code,
                    last_line: resolve_last_line(paths, &last_line),
                });
            }
            return Ok(ReadyOutcome::Hung {
                last_line: resolve_last_line(paths, &last_line),
            });
        }
        thread::sleep(Duration::from_millis(50));
    }
    if let Some(code) = poll_exited() {
        return Ok(ReadyOutcome::Exited {
            code,
            last_line: resolve_last_line(paths, &last_line),
        });
    }
    Ok(ReadyOutcome::TotalTimeout { last_line })
}

fn probe_ready(paths: &InstancePaths, port: u16, version: &str) -> Result<bool> {
    if std::env::var(TEST_DB_PROBE_ENV).ok().as_deref() == Some("tcp") {
        return Ok(TcpStream::connect(("127.0.0.1", port)).is_ok());
    }
    admin_ping(paths, port, version)
}

fn mariadb_bin_for_version(data_dir: &Path, version: &str) -> Result<PathBuf> {
    let catalog = Catalog::embedded().map_err(|error| {
        ProcessError::new(
            "catálogo ilegible",
            error.to_string(),
            "recompila con catalog/runtimes.toml",
        )
    })?;
    let entry = catalog.find("mariadb", version).ok_or_else(|| {
        ProcessError::new(
            "MariaDB no está en el catálogo",
            format!("falta mariadb@{version}"),
            "actualiza el catálogo o instala esa versión",
        )
    })?;
    find_mariadb_bin(&runtime_dir(data_dir, entry))
}

fn instance_data_dir(paths: &InstancePaths) -> Result<PathBuf> {
    paths
        .root
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .ok_or_else(|| {
            ProcessError::new(
                "ruta de instancia inválida",
                paths.root.display().to_string(),
                "revisa HOSTIG_DATA_DIR",
            )
        })
}

fn admin_ping(paths: &InstancePaths, port: u16, version: &str) -> Result<bool> {
    let data_dir = instance_data_dir(paths)?;
    let bin = match mariadb_bin_for_version(&data_dir, version) {
        Ok(b) => b,
        Err(_) => return Ok(false),
    };
    let admin = match exe_in(&bin, "mariadb-admin") {
        Some(p) => p,
        None => return Ok(false),
    };
    if !paths.admin_cnf.is_file() {
        return Ok(false);
    }
    let defaults_extra = format!("--defaults-extra-file={}", paths.admin_cnf.display());
    let output = Command::new(&admin)
        .arg(&defaults_extra)
        .arg("--protocol=tcp")
        .arg("-h")
        .arg("127.0.0.1")
        .arg("-P")
        .arg(port.to_string())
        .arg("ping")
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) => Ok(out.status.success()),
        Err(_) => Ok(false),
    }
}

fn admin_shutdown(paths: &InstancePaths, port: u16, version: &str, data_dir: &Path) -> Result<()> {
    if std::env::var(TEST_DB_PROBE_ENV).ok().as_deref() == Some("tcp") {
        // Señala al fake-mariadbd que salga (archivo hostig-fake.stop).
        let flag = paths
            .error_log
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("hostig-fake.stop");
        let _ = std::fs::write(&flag, b"stop");
        let _ = (port, version, data_dir);
        return Ok(());
    }
    if !paths.admin_cnf.is_file() {
        return Ok(());
    }
    let bin = mariadb_bin_for_version(data_dir, version)?;
    let admin = exe_in(&bin, "mariadb-admin").ok_or_else(|| {
        ProcessError::new(
            "mariadb-admin ausente",
            bin.display().to_string(),
            "reinstala MariaDB",
        )
    })?;
    let defaults_extra = format!("--defaults-extra-file={}", paths.admin_cnf.display());
    let _ = Command::new(&admin)
        .arg(&defaults_extra)
        .arg("--protocol=tcp")
        .arg("-h")
        .arg("127.0.0.1")
        .arg("-P")
        .arg(port.to_string())
        .arg("shutdown")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(())
}
