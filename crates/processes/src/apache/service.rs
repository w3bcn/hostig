//! Apache httpd con health /__hostig/health (ADR 0003 / 0005).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use uuid::Uuid;

use crate::apache::config::{
    find_server_root, httpd_in, regenerate_apache_config, ApachePaths, HealthDbParams,
    DEFAULT_APACHE_ID,
};
use crate::apache::settings::{ApacheSettings, DEFAULT_APACHE_VERSION, DEFAULT_HTTP_PORT};
use crate::error::{ProcessError, Result};
use crate::registry::ProcessRecord;
use crate::spawn::{spawn_server, SpawnSpec};
use crate::vc::verify_binary_starts;
use hostig_ports::{check as check_port, ranges, suggest, PortState};
use hostig_runtimes::{runtime_dir, Catalog, MANIFEST_NAME};

pub const TEST_HTTPD_ENV: &str = "HOSTIG_TEST_HTTPD";

/// Vía de apagado en Windows elegida tras la prueba del bloque 4 (ver ADR 0003).
/// `TerminateProcess` **solo** del PID del `ProcessRecord` (nunca por nombre de imagen).
/// Se probaron y descartaron: (1) `httpd -k shutdown` — el ZIP Lounge lo asocia al
/// servicio Windows `Apache2.4`, no al httpd de consola con `-f`; (2) ctrl-break al
/// grupo — con `CREATE_NO_WINDOW` (ADR 0011) el proceso no tiene consola y
/// `GenerateConsoleCtrlEvent` no lo detiene. Apache no exige el apagado InnoDB de
/// MariaDB; cortar el PID registrado libera hijos y el puerto.
pub const WINDOWS_SHUTDOWN_METHOD: &str = "TerminateProcess del PID registrado";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApacheState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApacheStatus {
    pub id: String,
    pub state: ApacheState,
    pub version: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApacheJobStatus {
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
pub struct ApacheStartRequest {
    pub id: String,
    pub version: String,
    pub http_port: Option<u16>,
    pub php_ports: Vec<u16>,
    pub db_host: String,
    pub db_port: u16,
    pub db_user: String,
    pub db_password: String,
}

impl Default for ApacheStartRequest {
    fn default() -> Self {
        Self {
            id: DEFAULT_APACHE_ID.into(),
            version: DEFAULT_APACHE_VERSION.into(),
            http_port: None,
            php_ports: vec![],
            db_host: "127.0.0.1".into(),
            db_port: 3306,
            db_user: "root".into(),
            db_password: String::new(),
        }
    }
}

struct Inner {
    status: ApacheStatus,
    child: Option<std::process::Child>,
    jobs: std::collections::HashMap<String, ApacheJobStatus>,
    restart_times: Vec<Instant>,
    stop_requested: bool,
    watchdog_started: bool,
    /// Parámetros del último arranque (para reinicio tras muerte).
    last_request: Option<ApacheStartRequest>,
}

pub struct ApacheService {
    data_dir: PathBuf,
    catalog: Catalog,
    settings: Mutex<ApacheSettings>,
    inner: Mutex<Inner>,
}

impl ApacheService {
    pub fn with_embedded(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.into();
        let catalog = Catalog::embedded().map_err(|error| {
            ProcessError::new(
                "catálogo ilegible",
                error.to_string(),
                "recompila con catalog/runtimes.toml",
            )
        })?;
        let settings = load_apache_settings(&data_dir)?;
        Ok(Self {
            data_dir,
            catalog,
            settings: Mutex::new(settings),
            inner: Mutex::new(Inner {
                status: ApacheStatus {
                    id: DEFAULT_APACHE_ID.into(),
                    state: ApacheState::Stopped,
                    version: DEFAULT_APACHE_VERSION.into(),
                    port: DEFAULT_HTTP_PORT,
                    pid: None,
                    error: None,
                },
                child: None,
                jobs: std::collections::HashMap::new(),
                restart_times: Vec::new(),
                stop_requested: false,
                watchdog_started: false,
                last_request: None,
            }),
        })
    }

    pub fn set_settings_for_tests(&self, settings: ApacheSettings) -> Result<()> {
        *self.settings.lock().expect("apache settings") = settings.clone();
        save_apache_settings(&self.data_dir, &settings)
    }

    pub fn settings(&self) -> ApacheSettings {
        self.settings.lock().expect("apache settings").clone()
    }

    pub fn status(&self) -> ApacheStatus {
        self.inner.lock().expect("apache inner").status.clone()
    }

    pub fn job(&self, job_id: &str) -> Result<ApacheJobStatus> {
        self.inner
            .lock()
            .expect("apache inner")
            .jobs
            .get(job_id)
            .cloned()
            .ok_or_else(|| {
                ProcessError::new(
                    "trabajo desconocido",
                    format!("no hay job {job_id}"),
                    "usa el id del arranque o la parada",
                )
            })
    }

    pub fn start_async(self: &Arc<Self>, request: ApacheStartRequest) -> Result<ApacheJobStatus> {
        {
            let inner = self.inner.lock().expect("apache inner");
            if matches!(
                inner.status.state,
                ApacheState::Running | ApacheState::Starting
            ) {
                return Ok(ApacheJobStatus {
                    job_id: Uuid::new_v4().to_string(),
                    state: "succeeded".into(),
                    id: inner.status.id.clone(),
                    port: inner.status.port,
                    pid: inner.status.pid,
                    error: None,
                });
            }
        }
        let preferred = request
            .http_port
            .unwrap_or_else(|| self.settings().http_port);
        let port = match suggest(preferred, ranges::HTTP.clone()) {
            Ok(p) if request.http_port.is_none() => p,
            Ok(_) | Err(_) if request.http_port.is_some() => preferred,
            Err(error) => {
                return Err(ProcessError::new(
                    "sin puertos HTTP libres",
                    error.to_string(),
                    "libera un puerto en 8080-8099",
                ));
            }
            Ok(p) => p,
        };
        match check_port(port, None) {
            Ok(PortState::Libre) => {}
            Ok(state) => return Err(http_port_busy(port, &state)),
            Err(error) => {
                return Err(ProcessError::new(error.message, error.cause, error.action));
            }
        }

        let job_id = Uuid::new_v4().to_string();
        let mut req = request;
        req.http_port = Some(port);
        let job = ApacheJobStatus {
            job_id: job_id.clone(),
            state: "running".into(),
            id: req.id.clone(),
            port,
            pid: None,
            error: None,
        };
        {
            let mut inner = self.inner.lock().expect("apache inner");
            inner.status.state = ApacheState::Starting;
            inner.status.id = req.id.clone();
            inner.status.version = req.version.clone();
            inner.status.port = port;
            inner.status.error = None;
            inner.stop_requested = false;
            inner.last_request = Some(req.clone());
            inner.jobs.insert(job_id.clone(), job.clone());
        }
        let service = Arc::clone(self);
        thread::spawn(move || {
            let result = service.run_start(&req);
            service.finish_start_job(&job_id, result);
        });
        self.job(&job.job_id)
    }

    pub fn stop_async(self: &Arc<Self>) -> Result<ApacheJobStatus> {
        let (job_id_out, port, id) = {
            let inner = self.inner.lock().expect("apache inner");
            (
                Uuid::new_v4().to_string(),
                inner.status.port,
                inner.status.id.clone(),
            )
        };
        let job = ApacheJobStatus {
            job_id: job_id_out.clone(),
            state: "running".into(),
            id,
            port,
            pid: None,
            error: None,
        };
        {
            let mut inner = self.inner.lock().expect("apache inner");
            inner.status.state = ApacheState::Stopping;
            inner.stop_requested = true;
            inner.jobs.insert(job_id_out.clone(), job.clone());
        }
        let service = Arc::clone(self);
        let job_id = job_id_out.clone();
        thread::spawn(move || {
            let result = service.run_stop();
            service.finish_stop_job(&job_id, result);
        });
        self.job(&job.job_id)
    }

    fn finish_start_job(self: &Arc<Self>, job_id: &str, result: Result<u32>) {
        {
            let mut inner = self.inner.lock().expect("apache inner");
            if let Some(job) = inner.jobs.get_mut(job_id) {
                match &result {
                    Ok(pid) => {
                        job.state = "succeeded".into();
                        job.pid = Some(*pid);
                        inner.status.state = ApacheState::Running;
                        inner.status.pid = Some(*pid);
                        inner.status.error = None;
                    }
                    Err(error) => {
                        job.state = "failed".into();
                        job.error = Some(error.into());
                        inner.status.state = ApacheState::Failed;
                        inner.status.error = Some(error.cause.clone());
                    }
                }
            }
        }
        if result.is_ok() {
            self.ensure_watchdog();
        }
    }

    fn finish_stop_job(&self, job_id: &str, result: Result<()>) {
        let mut inner = self.inner.lock().expect("apache inner");
        if let Some(job) = inner.jobs.get_mut(job_id) {
            match result {
                Ok(()) => {
                    job.state = "succeeded".into();
                    inner.status.state = ApacheState::Stopped;
                    inner.status.pid = None;
                    inner.status.error = None;
                    inner.child = None;
                    inner.watchdog_started = false;
                }
                Err(error) => {
                    job.state = "failed".into();
                    job.error = Some((&error).into());
                    inner.status.state = ApacheState::Failed;
                    inner.status.error = Some(error.cause.clone());
                }
            }
        }
    }

    fn ensure_watchdog(self: &Arc<Self>) {
        let mut inner = self.inner.lock().expect("apache inner");
        if matches!(inner.status.state, ApacheState::Running) && !inner.watchdog_started {
            inner.watchdog_started = true;
            drop(inner);
            let service = Arc::clone(self);
            thread::spawn(move || service.watchdog_loop());
        }
    }

    fn run_start(&self, request: &ApacheStartRequest) -> Result<u32> {
        if !cfg!(windows) {
            return Err(ProcessError::new(
                "Apache solo en Windows en la fase 1",
                "Linux/macOS compilan httpd en la fase 1b/1c",
                "espera la fase 1b o ejecuta en Windows",
            ));
        }
        self.run_start_impl(request)
    }

    fn run_start_impl(&self, request: &ApacheStartRequest) -> Result<u32> {
        let settings = self.settings();
        let port = request.http_port.unwrap_or(settings.http_port);
        if request.php_ports.is_empty() {
            return Err(ProcessError::new(
                "Apache sin miembros PHP",
                "el pool PHP no aportó puertos al balancer",
                "arranca el pool PHP antes que Apache",
            ));
        }

        let (httpd, server_root) = resolve_httpd(&self.data_dir, &self.catalog, &request.version)?;
        verify_binary_starts(&httpd, "Apache")?;

        let paths = ApachePaths::new(&self.data_dir, &request.id);
        regenerate_apache_config(
            &paths,
            &server_root,
            port,
            &request.php_ports,
            &HealthDbParams {
                host: &request.db_host,
                port: request.db_port,
                user: &request.db_user,
                password: &request.db_password,
            },
        )?;

        let spec = SpawnSpec {
            id: request.id.clone(),
            exe: httpd.clone(),
            args: vec![
                "-f".into(),
                paths.httpd_conf.display().to_string(),
                "-d".into(),
                server_root.display().to_string(),
            ],
            path_dirs: vec![server_root.join("bin"), server_root.clone()],
            current_dir: Some(server_root.clone()),
            port,
            version: request.version.clone(),
            env: vec![],
        };
        let spawned = spawn_server(&spec)?;
        let pid = spawned.record.pid;
        spawned.record.save(&self.data_dir)?;
        {
            let mut inner = self.inner.lock().expect("apache inner");
            inner.child = Some(spawned.child);
            inner.status.pid = Some(pid);
        }

        let deadline = Instant::now() + settings.start_total();
        wait_health_or_death(&self.data_dir, &request.id, port, pid, deadline, || {
            self.poll_child_exited(pid)
        })?;
        Ok(pid)
    }

    fn poll_child_exited(&self, pid: u32) -> Option<Option<i32>> {
        let mut inner = self.inner.lock().expect("apache inner");
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

    fn run_stop(&self) -> Result<()> {
        let (id, port, pid, conf, httpd, server_root) = {
            let inner = self.inner.lock().expect("apache inner");
            let id = inner.status.id.clone();
            let port = inner.status.port;
            let pid = inner.status.pid;
            let paths = ApachePaths::new(&self.data_dir, &id);
            let version = inner.status.version.clone();
            let (httpd, server_root) = resolve_httpd(&self.data_dir, &self.catalog, &version)
                .unwrap_or_else(|_| (PathBuf::from("httpd"), self.data_dir.clone()));
            (id, port, pid, paths.httpd_conf, httpd, server_root)
        };

        // Windows: TerminateProcess del PID registrado (ver WINDOWS_SHUTDOWN_METHOD / ADR 0003).
        // También se recoge el Child; el hijo worker de mpm_winnt sale con el padre.
        let _ = (httpd, server_root, conf);
        {
            let mut inner = self.inner.lock().expect("apache inner");
            if let Some(mut child) = inner.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            } else if let Some(pid) = pid {
                let _ = crate::os::terminate_process(pid);
            }
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            {
                let mut inner = self.inner.lock().expect("apache inner");
                if let Some(child) = inner.child.as_mut() {
                    let _ = child.try_wait();
                }
            }
            let alive = pid.map(hostig_ipc::process_is_running).unwrap_or(false);
            let free = matches!(check_port(port, None), Ok(PortState::Libre));
            if !alive && free {
                let _ = ProcessRecord::remove(&self.data_dir, &id);
                let mut inner = self.inner.lock().expect("apache inner");
                if let Some(mut child) = inner.child.take() {
                    let _ = child.wait();
                }
                inner.status.pid = None;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err(ProcessError::new(
            "Apache no se detuvo a tiempo",
            format!("pid={pid:?} puerto={port} tras {WINDOWS_SHUTDOWN_METHOD}"),
            "revisa el error log; no se usa taskkill por nombre",
        ))
    }

    fn watchdog_loop(self: &Arc<Self>) {
        loop {
            thread::sleep(self.settings().health_interval());
            {
                let inner = self.inner.lock().expect("apache inner");
                if inner.stop_requested || !matches!(inner.status.state, ApacheState::Running) {
                    drop(inner);
                    let mut inner = self.inner.lock().expect("apache inner");
                    inner.watchdog_started = false;
                    return;
                }
            }
            let pid = self.inner.lock().expect("apache inner").status.pid;
            let Some(pid) = pid else { continue };
            if let Some(code) = self.poll_child_exited(pid) {
                if self.inner.lock().expect("apache inner").stop_requested {
                    return;
                }
                if let Err(error) = self.handle_unexpected_death(code) {
                    let mut inner = self.inner.lock().expect("apache inner");
                    inner.status.state = ApacheState::Failed;
                    inner.status.error = Some(error.cause);
                    inner.watchdog_started = false;
                    return;
                }
            }
        }
    }

    fn handle_unexpected_death(self: &Arc<Self>, _code: Option<i32>) -> Result<()> {
        let _ = ProcessRecord::remove(
            &self.data_dir,
            &self.inner.lock().expect("apache inner").status.id,
        );
        {
            let mut inner = self.inner.lock().expect("apache inner");
            let now = Instant::now();
            let window = self.settings().backoff_window();
            inner
                .restart_times
                .retain(|t| now.duration_since(*t) < window);
            if inner.restart_times.len() as u32 >= self.settings().max_restarts {
                return Err(ProcessError::new(
                    "Apache agotó el cupo de reinicios",
                    "demasiadas caídas en la ventana de backoff",
                    "corrige la causa y ejecuta hostig start",
                ));
            }
            let attempt = inner.restart_times.len();
            inner.restart_times.push(now);
            let delay = self
                .settings()
                .backoff_secs
                .get(attempt)
                .copied()
                .unwrap_or(16);
            let request = inner.last_request.clone();
            drop(inner);
            thread::sleep(Duration::from_secs(delay));
            let request = request.ok_or_else(|| {
                ProcessError::new(
                    "Apache sin parámetros de reinicio",
                    "no hay last_request",
                    "hostig start",
                )
            })?;
            let _ = self.run_start(&request)?;
        }
        Ok(())
    }
}

fn wait_health_or_death(
    data_dir: &Path,
    id: &str,
    port: u16,
    pid: u32,
    deadline: Instant,
    mut poll_exited: impl FnMut() -> Option<Option<i32>>,
) -> Result<()> {
    while Instant::now() < deadline {
        if let Some(code) = poll_exited() {
            let _ = ProcessRecord::remove(data_dir, id);
            let code_s = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            return Err(ProcessError::new(
                "Apache no quedó listo",
                format!("Apache terminó durante el arranque (código de salida {code_s})"),
                "revisa el error log de Apache; hostig start tras corregir",
            ));
        }
        if health_ok(port) {
            return Ok(());
        }
        if !hostig_ipc::process_is_running(pid) {
            let _ = ProcessRecord::remove(data_dir, id);
            return Err(ProcessError::new(
                "Apache no quedó listo",
                "Apache terminó durante el arranque (proceso ya no vivo)",
                "revisa el error log de Apache",
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
    if let Some(code) = poll_exited() {
        let _ = ProcessRecord::remove(data_dir, id);
        let code_s = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
        return Err(ProcessError::new(
            "Apache no quedó listo",
            format!("Apache terminó durante el arranque (código de salida {code_s})"),
            "revisa el error log",
        ));
    }
    Err(ProcessError::new(
        "Apache no quedó listo",
        format!("/__hostig/health no respondió 200 en el puerto {port}"),
        "revisa PHP, MariaDB y el error log de Apache",
    ))
}

pub fn health_ok(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    let Ok(mut stream) = TcpStream::connect(&addr) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let req = format!(
        "GET /__hostig/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200")
}

fn resolve_httpd(data_dir: &Path, catalog: &Catalog, version: &str) -> Result<(PathBuf, PathBuf)> {
    if let Ok(path) = std::env::var(TEST_HTTPD_ENV) {
        let exe = PathBuf::from(path);
        let bin = exe.parent().unwrap_or(exe.as_path()).to_path_buf();
        let root = bin.parent().unwrap_or(bin.as_path()).to_path_buf();
        return Ok((exe, root));
    }
    let entry = catalog.find("apache", version).ok_or_else(|| {
        ProcessError::new(
            "Apache no está en el catálogo",
            format!("falta apache@{version}"),
            "actualiza el catálogo o instala esa versión",
        )
    })?;
    let runtime_root = runtime_dir(data_dir, entry);
    if !runtime_root.join(MANIFEST_NAME).is_file() {
        return Err(ProcessError::new(
            "Apache no instalado",
            format!("falta {}", runtime_root.display()),
            format!("hostig runtimes install apache@{version}"),
        ));
    }
    let server_root = find_server_root(&runtime_root)?;
    let httpd = httpd_in(&server_root.join("bin")).ok_or_else(|| {
        ProcessError::new(
            "httpd ausente",
            format!("no está en {}", server_root.join("bin").display()),
            "reinstala el runtime Apache del catálogo",
        )
    })?;
    Ok((httpd, server_root))
}

fn http_port_busy(port: u16, state: &PortState) -> ProcessError {
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
                    "elige otro puerto HTTP o detén el proceso que lo ocupa".into()
                }),
            )
        }
        PortState::Reservado => (
            "reservado por el sistema".into(),
            "usa el puerto sugerido en 8080-8099".into(),
        ),
        PortState::OcupadoDueñoNoVisible => (
            "ocupado, dueño no visible".into(),
            "usa el puerto sugerido".into(),
        ),
        PortState::Libre => unreachable!(),
    };
    ProcessError::new(
        "puerto HTTP ocupado",
        format!("puerto {port}: {who}"),
        action,
    )
}

fn load_apache_settings(data_dir: &Path) -> Result<ApacheSettings> {
    let path = data_dir.join("config").join("global.toml");
    if !path.is_file() {
        return Ok(ApacheSettings::default());
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
        .get("apache")
        .cloned()
        .and_then(|v| v.try_into().ok())
        .unwrap_or_default())
}

fn save_apache_settings(data_dir: &Path, settings: &ApacheSettings) -> Result<()> {
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
    let val = toml::Value::try_from(settings).map_err(|error| {
        ProcessError::new(
            "no se pudo serializar [apache]",
            error.to_string(),
            "reintenta",
        )
    })?;
    if let toml::Value::Table(table) = &mut root {
        table.insert("apache".into(), val);
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
