//! Pool de php-cgi en Windows (ADR 0005 / 0003).

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use uuid::Uuid;

use crate::error::{ProcessError, Result};
use crate::php::config::{
    find_php_root, php_cgi_in, regenerate_php_ini, PhpPaths, DEFAULT_POOL_ID,
};
use crate::php::settings::{PhpSettings, DEFAULT_PHP_VERSION};
use crate::registry::ProcessRecord;
use crate::spawn::{spawn_server, SpawnSpec};
use crate::vc::verify_binary_starts;
use hostig_ports::{check as check_port, ranges, suggest, PortState};
use hostig_runtimes::{runtime_dir, Catalog, MANIFEST_NAME};

/// Si está definida, se usa este ejecutable en lugar de php-cgi del catálogo (tests).
pub const TEST_PHPCGI_ENV: &str = "HOSTIG_TEST_PHPCGI";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PhpState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhpWorkerStatus {
    pub id: String,
    pub pid: Option<u32>,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhpStatus {
    pub id: String,
    pub state: PhpState,
    pub version: String,
    pub workers: Vec<PhpWorkerStatus>,
    pub error: Option<String>,
    /// Reinicios por caída (no reciclado) en la ventana de backoff.
    pub crash_restarts: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhpJobStatus {
    pub job_id: String,
    pub state: String,
    pub id: String,
    pub workers: Vec<PhpWorkerStatus>,
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
pub struct PhpStartRequest {
    pub id: String,
    pub version: String,
    pub pool_size: Option<u32>,
    pub fcgi_max_requests: Option<u32>,
}

impl Default for PhpStartRequest {
    fn default() -> Self {
        Self {
            id: DEFAULT_POOL_ID.into(),
            version: DEFAULT_PHP_VERSION.into(),
            pool_size: None,
            fcgi_max_requests: None,
        }
    }
}

struct Worker {
    id: String,
    port: u16,
    /// Tras el spawn el `Child` vive en el hilo waiter; aquí solo el PID.
    pid: Option<u32>,
}

struct WorkerExit {
    id: String,
    port: u16,
    index: usize,
    /// `Some(code)` tras `Child::wait`; `None` interior si el SO no dio código.
    code: Option<Option<i32>>,
}

struct Inner {
    status: PhpStatus,
    workers: Vec<Worker>,
    jobs: std::collections::HashMap<String, PhpJobStatus>,
    restart_times: Vec<Instant>,
    stop_requested: bool,
    watchdog_started: bool,
    ports: Vec<u16>,
}

/// Servicio del pool PHP (Windows: N × php-cgi -b).
pub struct PhpService {
    data_dir: PathBuf,
    catalog: Catalog,
    settings: Mutex<PhpSettings>,
    inner: Mutex<Inner>,
    /// Salidas de workers (reciclado o caída), detectadas al instante con `Child::wait`.
    exit_tx: Sender<WorkerExit>,
    exit_rx: Mutex<Receiver<WorkerExit>>,
}

impl PhpService {
    pub fn with_embedded(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.into();
        let catalog = Catalog::embedded().map_err(|error| {
            ProcessError::new(
                "catálogo ilegible",
                error.to_string(),
                "recompila con catalog/runtimes.toml",
            )
        })?;
        let settings = load_php_settings(&data_dir)?;
        let (exit_tx, exit_rx) = mpsc::channel();
        Ok(Self {
            data_dir,
            catalog,
            settings: Mutex::new(settings),
            inner: Mutex::new(Inner {
                status: PhpStatus {
                    id: DEFAULT_POOL_ID.into(),
                    state: PhpState::Stopped,
                    version: DEFAULT_PHP_VERSION.into(),
                    workers: vec![],
                    error: None,
                    crash_restarts: 0,
                },
                workers: vec![],
                jobs: std::collections::HashMap::new(),
                restart_times: Vec::new(),
                stop_requested: false,
                watchdog_started: false,
                ports: vec![],
            }),
            exit_tx,
            exit_rx: Mutex::new(exit_rx),
        })
    }

    pub fn set_settings_for_tests(&self, settings: PhpSettings) -> Result<()> {
        *self.settings.lock().expect("php settings") = settings.clone();
        save_php_settings(&self.data_dir, &settings)
    }

    pub fn settings(&self) -> PhpSettings {
        self.settings.lock().expect("php settings").clone()
    }

    pub fn status(&self) -> PhpStatus {
        let mut inner = self.inner.lock().expect("php inner");
        refresh_worker_status(&mut inner);
        inner.status.clone()
    }

    pub fn ports(&self) -> Vec<u16> {
        self.inner.lock().expect("php inner").ports.clone()
    }

    pub fn job(&self, job_id: &str) -> Result<PhpJobStatus> {
        self.inner
            .lock()
            .expect("php inner")
            .jobs
            .get(job_id)
            .cloned()
            .ok_or_else(|| {
                ProcessError::new(
                    "trabajo desconocido",
                    format!("no hay job {job_id}"),
                    "usa el id que devolvió el arranque o la parada",
                )
            })
    }

    pub fn crash_restart_count(&self) -> u32 {
        let inner = self.inner.lock().expect("php inner");
        let window = self.settings().backoff_window();
        let now = Instant::now();
        inner
            .restart_times
            .iter()
            .filter(|t| now.duration_since(**t) < window)
            .count() as u32
    }

    pub fn start_async(self: &Arc<Self>, request: PhpStartRequest) -> Result<PhpJobStatus> {
        {
            let inner = self.inner.lock().expect("php inner");
            if matches!(inner.status.state, PhpState::Running | PhpState::Starting) {
                return Ok(PhpJobStatus {
                    job_id: Uuid::new_v4().to_string(),
                    state: "succeeded".into(),
                    id: inner.status.id.clone(),
                    workers: inner.status.workers.clone(),
                    error: None,
                });
            }
        }
        let job_id = Uuid::new_v4().to_string();
        let job = PhpJobStatus {
            job_id: job_id.clone(),
            state: "running".into(),
            id: request.id.clone(),
            workers: vec![],
            error: None,
        };
        {
            let mut inner = self.inner.lock().expect("php inner");
            inner.status.state = PhpState::Starting;
            inner.status.id = request.id.clone();
            inner.status.version = request.version.clone();
            inner.status.error = None;
            inner.stop_requested = false;
            inner.jobs.insert(job_id.clone(), job.clone());
        }
        let service = Arc::clone(self);
        thread::spawn(move || {
            let result = service.run_start(&request);
            service.finish_start_job(&job_id, result);
        });
        self.job(&job.job_id)
    }

    pub fn stop_async(self: &Arc<Self>) -> Result<PhpJobStatus> {
        let job_id = Uuid::new_v4().to_string();
        let id = self.inner.lock().expect("php inner").status.id.clone();
        let job = PhpJobStatus {
            job_id: job_id.clone(),
            state: "running".into(),
            id,
            workers: vec![],
            error: None,
        };
        {
            let mut inner = self.inner.lock().expect("php inner");
            inner.status.state = PhpState::Stopping;
            inner.stop_requested = true;
            inner.jobs.insert(job_id.clone(), job.clone());
        }
        let service = Arc::clone(self);
        thread::spawn(move || {
            let result = service.run_stop();
            service.finish_stop_job(&job_id, result);
        });
        self.job(&job.job_id)
    }

    fn finish_start_job(self: &Arc<Self>, job_id: &str, result: Result<()>) {
        {
            let mut inner = self.inner.lock().expect("php inner");
            refresh_worker_status(&mut inner);
            match &result {
                Ok(()) => {
                    let workers = inner.status.workers.clone();
                    if let Some(job) = inner.jobs.get_mut(job_id) {
                        job.state = "succeeded".into();
                        job.workers = workers;
                    }
                    inner.status.state = PhpState::Running;
                    inner.status.error = None;
                }
                Err(error) => {
                    if let Some(job) = inner.jobs.get_mut(job_id) {
                        job.state = "failed".into();
                        job.error = Some(error.into());
                    }
                    inner.status.state = PhpState::Failed;
                    inner.status.error = Some(error.cause.clone());
                }
            }
        }
        if result.is_ok() {
            self.ensure_watchdog();
        }
    }

    fn finish_stop_job(&self, job_id: &str, result: Result<()>) {
        let mut inner = self.inner.lock().expect("php inner");
        if let Some(job) = inner.jobs.get_mut(job_id) {
            match result {
                Ok(()) => {
                    job.state = "succeeded".into();
                    inner.status.state = PhpState::Stopped;
                    inner.status.workers.clear();
                    inner.status.error = None;
                    inner.workers.clear();
                    inner.ports.clear();
                    inner.watchdog_started = false;
                }
                Err(error) => {
                    job.state = "failed".into();
                    job.error = Some((&error).into());
                    inner.status.state = PhpState::Failed;
                    inner.status.error = Some(error.cause.clone());
                }
            }
        }
    }

    fn ensure_watchdog(self: &Arc<Self>) {
        let mut inner = self.inner.lock().expect("php inner");
        if matches!(inner.status.state, PhpState::Running) && !inner.watchdog_started {
            inner.watchdog_started = true;
            drop(inner);
            let service = Arc::clone(self);
            thread::spawn(move || service.watchdog_loop());
        }
    }

    fn run_start(&self, request: &PhpStartRequest) -> Result<()> {
        if !cfg!(windows) {
            return Err(ProcessError::new(
                "PHP pool solo en Windows en la fase 1",
                "Linux/macOS usan php-fpm en la fase 1b/1c",
                "espera la fase 1b o ejecuta en Windows",
            ));
        }
        self.run_start_impl(request)
    }

    fn run_start_impl(&self, request: &PhpStartRequest) -> Result<()> {
        let settings = self.settings();
        let pool_size = request.pool_size.unwrap_or(settings.pool_size).max(1);
        let max_requests = request
            .fcgi_max_requests
            .unwrap_or(settings.fcgi_max_requests)
            .max(1);

        let (php_cgi, php_root) = resolve_php_cgi(&self.data_dir, &self.catalog, &request.version)?;
        verify_binary_starts(&php_cgi, "PHP")?;

        let paths = PhpPaths::new(&self.data_dir, &request.id);
        regenerate_php_ini(&paths, &php_root, &settings.timezone)?;

        let mut ports = Vec::with_capacity(pool_size as usize);
        let mut preferred = *ranges::PHP_WINDOWS.start();
        for _ in 0..pool_size {
            let port = suggest(preferred, ranges::PHP_WINDOWS.clone()).map_err(|error| {
                ProcessError::new(
                    "sin puertos PHP libres",
                    error.to_string(),
                    "libera puertos en 9000-9099 o reduce el tamaño del pool",
                )
            })?;
            match check_port(port, None) {
                Ok(PortState::Libre) => {}
                Ok(state) => return Err(port_busy_error(port, &state)),
                Err(error) => {
                    return Err(ProcessError::new(error.message, error.cause, error.action));
                }
            }
            ports.push(port);
            preferred = port.saturating_add(1);
        }

        // Desfase de cupo por miembro para que no reciclén a la vez (ADR 0005: el
        // balancer sigue sirviendo con los demás mientras uno se sustituye).
        let stagger = (max_requests / pool_size.max(1)).max(5);
        let mut workers = Vec::new();
        for (index, port) in ports.iter().copied().enumerate() {
            let worker_max = max_requests.saturating_add(index as u32 * stagger);
            match self.spawn_worker(
                &request.id,
                index,
                port,
                &php_cgi,
                &php_root,
                &paths,
                worker_max,
                &request.version,
            ) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    for mut w in workers {
                        let _ = stop_child(&mut w);
                        let _ = ProcessRecord::remove(&self.data_dir, &w.id);
                    }
                    return Err(error);
                }
            }
        }

        let deadline = Instant::now() + settings.start_total();
        for worker in &workers {
            wait_port_ready(worker.port, worker.pid, deadline)?;
        }

        let mut inner = self.inner.lock().expect("php inner");
        inner.workers = workers;
        inner.ports = ports;
        refresh_worker_status(&mut inner);
        inner.status.version = request.version.clone();
        inner.status.id = request.id.clone();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_worker(
        &self,
        pool_id: &str,
        index: usize,
        port: u16,
        php_cgi: &Path,
        php_root: &Path,
        paths: &PhpPaths,
        max_requests: u32,
        version: &str,
    ) -> Result<Worker> {
        let member_id = format!("{pool_id}-{index}");
        let bind = format!("127.0.0.1:{port}");
        let spec = SpawnSpec {
            id: member_id.clone(),
            exe: php_cgi.to_path_buf(),
            args: vec![
                "-b".into(),
                bind,
                "-c".into(),
                paths.php_ini.display().to_string(),
            ],
            path_dirs: vec![php_root.to_path_buf()],
            current_dir: Some(php_root.to_path_buf()),
            port,
            version: version.to_string(),
            env: vec![("PHP_FCGI_MAX_REQUESTS".into(), max_requests.to_string())],
        };
        let spawned = spawn_server(&spec)?;
        let pid = spawned.record.pid;
        spawned.record.save(&self.data_dir)?;
        let mut child = spawned.child;
        let exit_tx = self.exit_tx.clone();
        let wait_id = member_id.clone();
        let wait_port = port;
        let wait_index = index;
        thread::spawn(move || {
            let code = match child.wait() {
                Ok(status) => Some(status.code()),
                Err(_) => Some(None),
            };
            let _ = exit_tx.send(WorkerExit {
                id: wait_id,
                port: wait_port,
                index: wait_index,
                code,
            });
        });
        Ok(Worker {
            id: member_id,
            port,
            pid: Some(pid),
        })
    }

    fn run_stop(&self) -> Result<()> {
        let mut workers = {
            let mut inner = self.inner.lock().expect("php inner");
            std::mem::take(&mut inner.workers)
        };
        // Termina solo los PID registrados (no taskkill por nombre de imagen).
        for worker in &mut workers {
            let _ = stop_child(worker);
            let _ = ProcessRecord::remove(&self.data_dir, &worker.id);
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        for worker in &workers {
            while Instant::now() < deadline {
                if worker
                    .pid
                    .map(|p| !hostig_ipc::process_is_running(p))
                    .unwrap_or(true)
                {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if let Some(pid) = worker.pid {
                if hostig_ipc::process_is_running(pid) {
                    return Err(ProcessError::new(
                        "PHP no se detuvo a tiempo",
                        format!("{} (pid {pid}) sigue vivo", worker.id),
                        "revisa el proceso; no se usa taskkill por nombre",
                    ));
                }
            }
        }
        Ok(())
    }

    fn watchdog_loop(self: &Arc<Self>) {
        let poll = Duration::from_millis(50);
        loop {
            {
                let inner = self.inner.lock().expect("php inner");
                if inner.stop_requested || !matches!(inner.status.state, PhpState::Running) {
                    drop(inner);
                    let mut inner = self.inner.lock().expect("php inner");
                    inner.watchdog_started = false;
                    return;
                }
            }
            let death = {
                let rx = self.exit_rx.lock().expect("php exit_rx");
                match rx.recv_timeout(poll) {
                    Ok(death) => Some(death),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let mut inner = self.inner.lock().expect("php inner");
                        inner.watchdog_started = false;
                        return;
                    }
                }
            };
            let Some(death) = death else {
                continue;
            };
            if self.inner.lock().expect("php inner").stop_requested {
                return;
            }
            // Descarta salidas de un stop ordenado (PID ya limpiado).
            {
                let mut inner = self.inner.lock().expect("php inner");
                let still = inner
                    .workers
                    .iter()
                    .any(|w| w.id == death.id && w.pid.is_some());
                if !still {
                    continue;
                }
                if let Some(slot) = inner.workers.iter_mut().find(|w| w.id == death.id) {
                    slot.pid = None;
                }
            }
            if let Err(error) = self.handle_worker_exit(death) {
                let mut inner = self.inner.lock().expect("php inner");
                inner.status.state = PhpState::Failed;
                inner.status.error = Some(error.cause);
                inner.watchdog_started = false;
                return;
            }
        }
    }

    fn handle_worker_exit(self: &Arc<Self>, death: WorkerExit) -> Result<()> {
        let _ = ProcessRecord::remove(&self.data_dir, &death.id);
        let recycle = matches!(death.code, Some(Some(0)));
        if !recycle {
            let mut inner = self.inner.lock().expect("php inner");
            let now = Instant::now();
            let window = self.settings().backoff_window();
            inner
                .restart_times
                .retain(|t| now.duration_since(*t) < window);
            if inner.restart_times.len() as u32 >= self.settings().max_restarts {
                return Err(ProcessError::new(
                    "PHP agotó el cupo de reinicios",
                    format!(
                        "{} salió con código {:?} demasiadas veces",
                        death.id, death.code
                    ),
                    "corrige la causa y ejecuta hostig start",
                ));
            }
            let attempt = inner.restart_times.len();
            inner.restart_times.push(now);
            inner.status.crash_restarts = inner.restart_times.len() as u32;
            let delay = self
                .settings()
                .backoff_secs
                .get(attempt)
                .copied()
                .unwrap_or(16);
            drop(inner);
            thread::sleep(Duration::from_secs(delay));
        }

        let settings = self.settings();
        let (pool_id, version, pool_len) = {
            let inner = self.inner.lock().expect("php inner");
            (
                inner.status.id.clone(),
                inner.status.version.clone(),
                inner.workers.len().max(1) as u32,
            )
        };
        let (php_cgi, php_root) = resolve_php_cgi(&self.data_dir, &self.catalog, &version)?;
        let paths = PhpPaths::new(&self.data_dir, &pool_id);
        let stagger = (settings.fcgi_max_requests / pool_len).max(5);
        let worker_max = settings
            .fcgi_max_requests
            .saturating_add(death.index as u32 * stagger);
        let worker = self.spawn_worker(
            &pool_id,
            death.index,
            death.port,
            &php_cgi,
            &php_root,
            &paths,
            worker_max,
            &version,
        )?;
        let deadline = Instant::now() + settings.start_total();
        wait_port_ready(worker.port, worker.pid, deadline)?;
        let mut inner = self.inner.lock().expect("php inner");
        if let Some(slot) = inner.workers.iter_mut().find(|w| w.id == death.id) {
            *slot = worker;
        } else {
            inner.workers.push(worker);
        }
        refresh_worker_status(&mut inner);
        Ok(())
    }
}

fn refresh_worker_status(inner: &mut Inner) {
    inner.status.workers = inner
        .workers
        .iter()
        .map(|w| PhpWorkerStatus {
            id: w.id.clone(),
            pid: w.pid,
            port: w.port,
        })
        .collect();
    inner.status.crash_restarts = inner.restart_times.len() as u32;
}

fn stop_child(worker: &mut Worker) -> Result<()> {
    if let Some(pid) = worker.pid.take() {
        let _ = crate::os::terminate_process(pid);
    }
    Ok(())
}

fn wait_port_ready(port: u16, pid: Option<u32>, deadline: Instant) -> Result<()> {
    while Instant::now() < deadline {
        if let Some(pid) = pid {
            if !hostig_ipc::process_is_running(pid) {
                return Err(ProcessError::new(
                    "PHP no quedó listo",
                    format!(
                        "PHP terminó durante el arranque; php-cgi en 127.0.0.1:{port} ya no está vivo"
                    ),
                    "revisa php_error.log; hostig start tras corregir",
                ));
            }
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(ProcessError::new(
        "PHP no quedó listo",
        format!("127.0.0.1:{port} no aceptó conexiones a tiempo"),
        "revisa php_error.log y el runtime",
    ))
}

fn resolve_php_cgi(
    data_dir: &Path,
    catalog: &Catalog,
    version: &str,
) -> Result<(PathBuf, PathBuf)> {
    if let Ok(path) = std::env::var(TEST_PHPCGI_ENV) {
        let exe = PathBuf::from(path);
        let root = exe.parent().unwrap_or(exe.as_path()).to_path_buf();
        return Ok((exe, root));
    }
    let entry = catalog.find("php", version).ok_or_else(|| {
        ProcessError::new(
            "PHP no está en el catálogo",
            format!("falta php@{version}"),
            "actualiza el catálogo o instala esa versión",
        )
    })?;
    let runtime_root = runtime_dir(data_dir, entry);
    if !runtime_root.join(MANIFEST_NAME).is_file() {
        return Err(ProcessError::new(
            "PHP no instalado",
            format!("falta {}", runtime_root.display()),
            format!("hostig runtimes install php@{version}"),
        ));
    }
    let php_root = find_php_root(&runtime_root)?;
    let cgi = php_cgi_in(&php_root).ok_or_else(|| {
        ProcessError::new(
            "php-cgi ausente",
            format!("no está en {}", php_root.display()),
            "reinstala el runtime PHP del catálogo",
        )
    })?;
    Ok((cgi, php_root))
}

fn port_busy_error(port: u16, state: &PortState) -> ProcessError {
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
                action
                    .clone()
                    .unwrap_or_else(|| "elige otro puerto o detén el proceso que lo ocupa".into()),
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
    ProcessError::new(
        "puerto PHP ocupado",
        format!("puerto {port}: {who}"),
        action,
    )
}

fn load_php_settings(data_dir: &Path) -> Result<PhpSettings> {
    let path = data_dir.join("config").join("global.toml");
    if !path.is_file() {
        return Ok(PhpSettings::default());
    }
    let text = std::fs::read_to_string(&path)?;
    let value: toml::Value = toml::from_str(&text).map_err(|error| {
        ProcessError::new(
            "manifiesto global ilegible",
            error.to_string(),
            format!("corrige {}", path.display()),
        )
    })?;
    Ok(value
        .get("php")
        .cloned()
        .and_then(|v| v.try_into().ok())
        .unwrap_or_default())
}

fn save_php_settings(data_dir: &Path, settings: &PhpSettings) -> Result<()> {
    let path = data_dir.join("config").join("global.toml");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut root: toml::Value = if path.is_file() {
        toml::from_str(&std::fs::read_to_string(&path)?)
            .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()))
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    let php_val = toml::Value::try_from(settings).map_err(|error| {
        ProcessError::new(
            "no se pudo serializar [php]",
            error.to_string(),
            "reintenta",
        )
    })?;
    if let toml::Value::Table(table) = &mut root {
        table.insert("php".into(), php_val);
    }
    let text = toml::to_string_pretty(&root).map_err(|error| {
        ProcessError::new(
            "no se pudo serializar el manifiesto",
            error.to_string(),
            "reintenta",
        )
    })?;
    std::fs::write(path, text.as_bytes())?;
    Ok(())
}
