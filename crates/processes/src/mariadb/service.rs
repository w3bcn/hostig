use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use uuid::Uuid;

use crate::error::{ProcessError, Result};
use crate::mariadb::config::{read_password, InstancePaths, DEFAULT_INSTANCE_ID, DEFAULT_PORT};
use crate::mariadb::init::{
    basedir_from_bin, ensure_initialized, exe_in, find_mariadb_bin, generate_password,
};
use crate::mariadb::logparse::{
    log_contains_ready, log_contains_shutdown, new_log_offset, read_log_since,
};
use crate::os::{process_creation_time, process_image_path};
use crate::registry::{try_adopt, AdoptOutcome, ProcessRecord};
use crate::spawn::{spawn_server, SpawnSpec};
use hostig_ports::{check as check_port, PortState};
use hostig_runtimes::{runtime_dir, Catalog, MANIFEST_NAME};

pub const DEFAULT_MARIADB_VERSION: &str = "11.4.13";
const READY_TIMEOUT: Duration = Duration::from_secs(120);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

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

struct Inner {
    status: DbStatus,
    child: Option<std::process::Child>,
    jobs: std::collections::HashMap<String, DbJobStatus>,
}

/// Servicio MariaDB del supervisor (una instancia MVP por ahora).
pub struct DbService {
    data_dir: PathBuf,
    catalog: Catalog,
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
        let mut service = Self {
            data_dir: data_dir.clone(),
            catalog,
            inner: Mutex::new(Inner {
                status: DbStatus {
                    id: DEFAULT_INSTANCE_ID.into(),
                    state: DbState::Stopped,
                    port: DEFAULT_PORT,
                    pid: None,
                    version: DEFAULT_MARIADB_VERSION.into(),
                    error: None,
                },
                child: None,
                jobs: std::collections::HashMap::new(),
            }),
        };
        service.reconcile_on_boot()?;
        Ok(service)
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
        self.inner.lock().expect("db inner").status.clone()
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
        }

        // Puerto ocupado → no lanzamos nada.
        match check_port(request.port, None) {
            Ok(PortState::Libre) => {}
            Ok(state) => {
                let suggested =
                    hostig_ports::suggest(request.port, hostig_ports::ranges::MARIADB).ok();
                let (who, action) = match &state {
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
                                    suggested.unwrap_or(request.port.saturating_add(1))
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
                return Err(ProcessError::new(
                    "puerto ocupado",
                    format!("puerto {}: {who}", request.port),
                    action,
                ));
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
            inner.jobs.insert(job_id.clone(), status.clone());
        }

        let service = Arc::clone(self);
        thread::spawn(move || {
            let outcome = service.run_start(request);
            let mut inner = service.inner.lock().expect("db inner");
            match &outcome {
                Ok(pid) => {
                    if let Some(job) = inner.jobs.get_mut(&job_id) {
                        job.state = "succeeded".into();
                        job.pid = Some(*pid);
                    }
                    inner.status.state = DbState::Running;
                    inner.status.pid = Some(*pid);
                    inner.status.error = None;
                }
                Err(error) => {
                    let alive_pid = inner
                        .status
                        .pid
                        .filter(|p| hostig_ipc::process_is_running(*p));
                    if let Some(job) = inner.jobs.get_mut(&job_id) {
                        job.state = "failed".into();
                        job.error = Some(error.into());
                        job.pid = alive_pid;
                    }
                    inner.status.state = DbState::Failed;
                    inner.status.error = Some(error.to_string());
                    inner.status.pid = alive_pid;
                }
            }
        });

        Ok(status)
    }

    fn run_start(&self, request: StartRequest) -> Result<u32> {
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
        if !runtime_root.join(MANIFEST_NAME).is_file() {
            return Err(ProcessError::new(
                "MariaDB no instalada",
                format!("falta {}", runtime_root.display()),
                format!("hostig runtimes install mariadb@{}", request.version),
            ));
        }

        let paths = InstancePaths::new(&self.data_dir, &request.id);
        let password = if paths.password_file.is_file() {
            read_password(&paths)?
        } else {
            let password = generate_password();
            ensure_initialized(&paths, &runtime_root, request.port, &password)?;
            password
        };
        // Si ya había password pero datadir vacío, inicializa.
        if !paths.datadir.join("mysql").is_dir() {
            ensure_initialized(&paths, &runtime_root, request.port, &password)?;
        } else {
            let bin = find_mariadb_bin(&runtime_root)?;
            let basedir = basedir_from_bin(&bin);
            crate::mariadb::config::write_my_ini(&paths, &basedir, request.port)?;
            crate::mariadb::config::write_admin_cnf(&paths, request.port, &password)?;
        }

        // Asegurar que no quede my.ini en el datadir.
        let _ = fs_remove_datadir_ini(&paths);

        let bin = find_mariadb_bin(&runtime_root)?;
        let mariadbd = exe_in(&bin, "mariadbd").ok_or_else(|| {
            ProcessError::new(
                "mariadbd ausente",
                bin.display().to_string(),
                "reinstala MariaDB",
            )
        })?;

        let defaults = format!("--defaults-file={}", paths.my_ini.display());
        let log_offset = new_log_offset(&paths.error_log)?;
        let spec = SpawnSpec {
            id: request.id.clone(),
            exe: mariadbd,
            args: vec![defaults],
            path_dirs: vec![bin.clone()],
            current_dir: Some(basedir_from_bin(&bin)),
            port: request.port,
            version: request.version.clone(),
        };
        let spawned = spawn_server(&spec)?;
        let pid = spawned.record.pid;
        spawned.record.save(&self.data_dir)?;
        {
            let mut inner = self.inner.lock().expect("db inner");
            inner.child = Some(spawned.child);
            inner.status.pid = Some(pid);
        }

        let ready = wait_until_ready(
            &paths,
            request.port,
            &request.version,
            log_offset,
            READY_TIMEOUT,
        )?;
        if !ready {
            return Err(self.after_ready_timeout(&request.id, request.port, &request.version, pid));
        }
        Ok(pid)
    }

    /// Tras agotar el plazo de ready: Failed con causa/acción. Si el ping responde,
    /// intenta shutdown ordenado; si no, deja el proceso registrado. Nunca TerminateProcess.
    fn after_ready_timeout(&self, id: &str, port: u16, version: &str, pid: u32) -> ProcessError {
        let paths = InstancePaths::new(&self.data_dir, id);
        let ping_ok = admin_ping(&paths, port, version).unwrap_or(false);
        if ping_ok {
            let _ = admin_shutdown(&paths, port, version, &self.data_dir);
            // Esperamos un poco a un cierre limpio; si no, el proceso queda registrado.
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
                        "MariaDB no quedó lista",
                        "no apareció ready for connections a tiempo; se pidió shutdown tras ping OK",
                        "revisa el error log; los plazos largos de recovery son el bloque 3b (ADR 0012)",
                    );
                }
                thread::sleep(Duration::from_millis(200));
            }
            ProcessError::new(
                "MariaDB no quedó lista",
                "ping respondió pero el apagado ordenado no terminó a tiempo; el proceso sigue registrado",
                "espera o usa hostig db status; hostig db kill <pid> solo como emergencia",
            )
        } else {
            // Proceso vivo, sin ping: dejarlo registrado (p. ej. recovery InnoDB).
            ProcessError::new(
                "MariaDB no quedó lista",
                "no apareció ready for connections ni respondió el ping a tiempo; el proceso sigue registrado",
                "revisa el error log; no se mata el proceso (ADR 0003); plazos de recovery en el bloque 3b",
            )
        }
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
        if paths.admin_cnf.is_file() {
            let bin = {
                let entry = self.catalog.find("mariadb", &version).ok_or_else(|| {
                    ProcessError::new(
                        "catálogo sin MariaDB",
                        version.clone(),
                        "recompila con el catálogo",
                    )
                })?;
                find_mariadb_bin(&runtime_dir(&self.data_dir, entry))?
            };
            let admin = exe_in(&bin, "mariadb-admin").ok_or_else(|| {
                ProcessError::new(
                    "mariadb-admin ausente",
                    bin.display().to_string(),
                    "reinstala MariaDB",
                )
            })?;
            let defaults_extra = format!("--defaults-extra-file={}", paths.admin_cnf.display());
            let status = Command::new(&admin)
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
            let _ = status; // si ya estaba parado, puede fallar
        }

        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            // Reap del Child si lo tenemos: libera el objeto de proceso en Windows.
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
            // Si el proceso salió y el puerto libre pero el log aún no tiene la línea,
            // damos un poco más; al final exigimos las tres.
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
        if let Some(mut child) = inner.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }
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

fn wait_until_ready(
    paths: &InstancePaths,
    port: u16,
    version: &str,
    log_offset: u64,
    budget: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        let log = read_log_since(&paths.error_log, log_offset)?;
        if log_contains_ready(&log) && admin_ping(paths, port, version)? {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(400));
    }
    Ok(false)
}

fn mariadb_bin_for_version(data_dir: &std::path::Path, version: &str) -> Result<PathBuf> {
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

fn admin_shutdown(
    paths: &InstancePaths,
    port: u16,
    version: &str,
    data_dir: &std::path::Path,
) -> Result<()> {
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
