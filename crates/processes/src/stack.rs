//! Orquestación MariaDB → PHP → Apache (ADR 0003).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use uuid::Uuid;

use crate::apache::{ApacheService, ApacheStartRequest, ApacheState};
use crate::error::{ProcessError, Result};
use crate::mariadb::{
    ensure_health_user, DbService, DbState, InstancePaths, StartRequest as DbStartRequest,
    DEFAULT_INSTANCE_ID, DEFAULT_MARIADB_VERSION, DEFAULT_PORT as DB_DEFAULT_PORT,
};
use crate::php::{PhpService, PhpStartRequest, PhpState};

#[derive(Debug, Clone, Serialize)]
pub struct StackStatus {
    pub mariadb: serde_json::Value,
    pub php: serde_json::Value,
    pub apache: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct StackJobStatus {
    pub job_id: String,
    pub state: String,
    pub error: Option<ProcessErrorJson>,
    pub status: Option<StackStatus>,
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

#[derive(Debug, Clone, Default)]
pub struct StackStartRequest {
    pub db_port: Option<u16>,
    pub db_version: Option<String>,
    pub http_port: Option<u16>,
    pub php_pool_size: Option<u32>,
    pub php_fcgi_max_requests: Option<u32>,
}

struct Inner {
    jobs: std::collections::HashMap<String, StackJobStatus>,
}

/// Orquesta el stack completo del MVP.
pub struct StackService {
    pub db: Arc<DbService>,
    pub php: Arc<PhpService>,
    pub apache: Arc<ApacheService>,
    data_dir: std::path::PathBuf,
    inner: Mutex<Inner>,
    /// Apagado del sistema en curso: no se aceptan arranques nuevos (ADR 0011).
    closing: AtomicBool,
}

/// Tope duro de la parada rápida por apagado del sistema (ADR 0011).
pub const SYSTEM_SHUTDOWN_BUDGET: Duration = Duration::from_secs(20);
/// Espera máxima a Apache y a PHP en la parada rápida. Lo que sobra del tope es para MariaDB.
const SYSTEM_SHUTDOWN_FRONT_STEP: Duration = Duration::from_secs(3);

/// Resultado de un paso de la parada rápida.
#[derive(Debug, Clone, Serialize)]
pub struct SystemStopStep {
    pub service: &'static str,
    /// `false` si el servicio ya estaba parado y no se tocó.
    pub attempted: bool,
    pub elapsed_ms: u128,
    pub error: Option<ProcessErrorJson>,
}

/// Informe de la parada rápida: tiempos por servicio y total.
#[derive(Debug, Clone, Serialize)]
pub struct SystemStopReport {
    pub steps: Vec<SystemStopStep>,
    pub total_ms: u128,
    pub budget_ms: u128,
}

impl SystemStopReport {
    /// MariaDB quedó parada (o no estaba en marcha) dentro del plazo.
    pub fn mariadb_ok(&self) -> bool {
        self.steps
            .iter()
            .find(|step| step.service == "mariadb")
            .is_none_or(|step| step.error.is_none())
    }
}

impl StackService {
    pub fn new(
        data_dir: impl Into<std::path::PathBuf>,
        db: Arc<DbService>,
        php: Arc<PhpService>,
        apache: Arc<ApacheService>,
    ) -> Self {
        Self {
            db,
            php,
            apache,
            data_dir: data_dir.into(),
            inner: Mutex::new(Inner {
                jobs: std::collections::HashMap::new(),
            }),
            closing: AtomicBool::new(false),
        }
    }

    /// Marca el apagado del sistema: a partir de aquí `start_async` falla.
    pub fn begin_system_shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// Parada rápida por apagado o cierre de sesión (ADR 0011).
    ///
    /// Síncrona: vuelve cuando MariaDB está parada o cuando vence `budget`.
    /// Orden Apache → PHP → MariaDB, sin volcado. Apache y PHP tienen como mucho
    /// unos segundos cada uno; si no paran a tiempo se sigue igualmente, porque lo
    /// que no puede quedar a medias es la base. A MariaDB nunca se le hace
    /// `TerminateProcess`: si vence el plazo, su parada ordenada sigue en segundo
    /// plano y el informe lleva el error.
    pub fn system_shutdown_stop(&self, budget: Duration) -> SystemStopReport {
        self.begin_system_shutdown();
        let started = Instant::now();
        let deadline = started + budget;
        let mut steps = Vec::with_capacity(3);

        let apache_running = !matches!(
            self.apache.status().state,
            ApacheState::Stopped | ApacheState::Failed
        );
        steps.push(timed_step("apache", apache_running, || {
            let job = self.apache.stop_async()?;
            wait_apache_job(
                &self.apache,
                &job.job_id,
                front_budget(deadline, SYSTEM_SHUTDOWN_FRONT_STEP),
            )
        }));

        let php_running = !matches!(
            self.php.status().state,
            PhpState::Stopped | PhpState::Failed
        );
        steps.push(timed_step("php", php_running, || {
            let job = self.php.stop_async()?;
            wait_php_job(
                &self.php,
                &job.job_id,
                front_budget(deadline, SYSTEM_SHUTDOWN_FRONT_STEP),
            )
        }));

        let db_running = !matches!(self.db.status().state, DbState::Stopped | DbState::Failed);
        steps.push(timed_step("mariadb", db_running, || {
            let job = self.db.stop_async_with(crate::mariadb::StopOptions {
                system_shutdown: true,
                ..crate::mariadb::StopOptions::default()
            })?;
            wait_db_job(
                &self.db,
                &job.job_id,
                deadline.saturating_duration_since(Instant::now()),
            )
        }));

        SystemStopReport {
            steps,
            total_ms: started.elapsed().as_millis(),
            budget_ms: budget.as_millis(),
        }
    }

    pub fn status(&self) -> StackStatus {
        StackStatus {
            mariadb: serde_json::to_value(self.db.status()).unwrap_or_default(),
            php: serde_json::to_value(self.php.status()).unwrap_or_default(),
            apache: serde_json::to_value(self.apache.status()).unwrap_or_default(),
        }
    }

    pub fn job(&self, job_id: &str) -> Result<StackJobStatus> {
        self.inner
            .lock()
            .expect("stack inner")
            .jobs
            .get(job_id)
            .cloned()
            .ok_or_else(|| {
                ProcessError::new(
                    "trabajo desconocido",
                    format!("no hay job {job_id}"),
                    "usa el id de hostig start o hostig stop",
                )
            })
    }

    pub fn start_async(self: &Arc<Self>, request: StackStartRequest) -> Result<StackJobStatus> {
        if self.is_closing() {
            return Err(closing_error());
        }
        // Si el puerto HTTP ya está ocupado, no lanzar nada.
        if let Some(port) = request.http_port {
            match hostig_ports::check(port, None) {
                Ok(hostig_ports::PortState::Libre) => {}
                Ok(state) => {
                    return Err(http_busy_before_start(port, &state));
                }
                Err(error) => {
                    return Err(ProcessError::new(error.message, error.cause, error.action));
                }
            }
        } else {
            // Comprobar el preferido 8080: si está ocupado y no hay alternativa forzada, fallar
            // solo cuando el usuario fijó el puerto. Si no, Apache usará suggest.
            // Requisito: "Puerto HTTP ocupado: start no lanza nada y nombra al ocupante"
            // → si 8080 (defecto) está ocupado y no se pasó otro, fallar sin arrancar.
            let preferred = self.apache.settings().http_port;
            match hostig_ports::check(preferred, None) {
                Ok(hostig_ports::PortState::Libre) => {}
                Ok(state) => {
                    return Err(http_busy_before_start(preferred, &state));
                }
                Err(error) => {
                    return Err(ProcessError::new(error.message, error.cause, error.action));
                }
            }
        }

        let job_id = Uuid::new_v4().to_string();
        let job = StackJobStatus {
            job_id: job_id.clone(),
            state: "running".into(),
            error: None,
            status: None,
        };
        self.inner
            .lock()
            .expect("stack inner")
            .jobs
            .insert(job_id.clone(), job.clone());
        let service = Arc::clone(self);
        thread::spawn(move || {
            let result = service.run_start(request);
            service.finish_job(&job_id, result);
        });
        self.job(&job.job_id)
    }

    pub fn stop_async(self: &Arc<Self>) -> Result<StackJobStatus> {
        self.stop_async_with(crate::mariadb::StopOptions::default())
    }

    pub fn stop_async_with(
        self: &Arc<Self>,
        db_options: crate::mariadb::StopOptions,
    ) -> Result<StackJobStatus> {
        let job_id = Uuid::new_v4().to_string();
        let job = StackJobStatus {
            job_id: job_id.clone(),
            state: "running".into(),
            error: None,
            status: None,
        };
        self.inner
            .lock()
            .expect("stack inner")
            .jobs
            .insert(job_id.clone(), job.clone());
        let service = Arc::clone(self);
        thread::spawn(move || {
            let result = service.run_stop(db_options);
            service.finish_job(&job_id, result);
        });
        self.job(&job.job_id)
    }

    fn finish_job(&self, job_id: &str, result: Result<()>) {
        let mut inner = self.inner.lock().expect("stack inner");
        if let Some(job) = inner.jobs.get_mut(job_id) {
            match result {
                Ok(()) => {
                    job.state = "succeeded".into();
                    job.status = Some(StackStatus {
                        mariadb: serde_json::to_value(self.db.status()).unwrap_or_default(),
                        php: serde_json::to_value(self.php.status()).unwrap_or_default(),
                        apache: serde_json::to_value(self.apache.status()).unwrap_or_default(),
                    });
                }
                Err(error) => {
                    job.state = "failed".into();
                    job.error = Some((&error).into());
                    job.status = Some(StackStatus {
                        mariadb: serde_json::to_value(self.db.status()).unwrap_or_default(),
                        php: serde_json::to_value(self.php.status()).unwrap_or_default(),
                        apache: serde_json::to_value(self.apache.status()).unwrap_or_default(),
                    });
                }
            }
        }
    }

    fn run_start(&self, request: StackStartRequest) -> Result<()> {
        // 1. MariaDB
        let db_job = self.db.start_async(DbStartRequest {
            id: DEFAULT_INSTANCE_ID.into(),
            version: request
                .db_version
                .unwrap_or_else(|| DEFAULT_MARIADB_VERSION.into()),
            port: request.db_port.unwrap_or(DB_DEFAULT_PORT),
        })?;
        wait_db_job(&self.db, &db_job.job_id, Duration::from_secs(60 * 5))?;
        if self.db.status().state != DbState::Running {
            return Err(ProcessError::new(
                "MariaDB no quedó en marcha",
                self.db
                    .status()
                    .error
                    .unwrap_or_else(|| "estado inesperado".into()),
                "hostig db status",
            ));
        }

        // 2. PHP pool
        let php_job = self.php.start_async(PhpStartRequest {
            pool_size: request.php_pool_size,
            fcgi_max_requests: request.php_fcgi_max_requests,
            ..PhpStartRequest::default()
        })?;
        wait_php_job(&self.php, &php_job.job_id, Duration::from_secs(120))?;
        if self.php.status().state != PhpState::Running {
            let _ = self.db.stop_async();
            return Err(ProcessError::new(
                "PHP no quedó en marcha",
                self.php
                    .status()
                    .error
                    .unwrap_or_else(|| "estado inesperado".into()),
                "revisa el runtime PHP",
            ));
        }
        let php_ports = self.php.ports();

        // Usuario dedicado al health (solo USAGE), no la cuenta root.
        let paths = InstancePaths::new(&self.data_dir, DEFAULT_INSTANCE_ID);
        let db_status = self.db.status();
        let db_port = db_status.port;
        let (health_user, health_password) =
            ensure_health_user(&paths, &self.data_dir, &db_status.version, db_port)?;

        // 3. Apache
        let apache_job = self.apache.start_async(ApacheStartRequest {
            http_port: request.http_port,
            php_ports,
            db_host: "127.0.0.1".into(),
            db_port,
            db_user: health_user,
            db_password: health_password,
            ..ApacheStartRequest::default()
        })?;
        if let Err(error) =
            wait_apache_job(&self.apache, &apache_job.job_id, Duration::from_secs(120))
        {
            let _ = self.run_stop(crate::mariadb::StopOptions::default());
            return Err(error);
        }
        if self.apache.status().state != ApacheState::Running {
            let _ = self.run_stop(crate::mariadb::StopOptions::default());
            return Err(ProcessError::new(
                "Apache no quedó en marcha",
                self.apache
                    .status()
                    .error
                    .unwrap_or_else(|| "estado inesperado".into()),
                "revisa el runtime Apache y /__hostig/health",
            ));
        }
        Ok(())
    }

    fn run_stop(&self, db_options: crate::mariadb::StopOptions) -> Result<()> {
        // Orden inverso: Apache → PHP → MariaDB (volcado con writers quietos).
        if !matches!(
            self.apache.status().state,
            ApacheState::Stopped | ApacheState::Failed
        ) {
            let job = self.apache.stop_async()?;
            wait_apache_job(&self.apache, &job.job_id, Duration::from_secs(60))?;
        }
        if !matches!(
            self.php.status().state,
            PhpState::Stopped | PhpState::Failed
        ) {
            let job = self.php.stop_async()?;
            wait_php_job(&self.php, &job.job_id, Duration::from_secs(60))?;
        }
        if !matches!(self.db.status().state, DbState::Stopped | DbState::Failed) {
            let job = self.db.stop_async_with(db_options)?;
            wait_db_job(&self.db, &job.job_id, Duration::from_secs(60))?;
        }
        Ok(())
    }
}

fn closing_error() -> ProcessError {
    ProcessError::new(
        "el sistema se está apagando",
        "el supervisor está en la parada rápida del apagado o del cierre de sesión",
        "vuelve a arrancar cuando el equipo se haya reiniciado",
    )
}

/// Lo que queda hasta `deadline`, sin pasar de `cap`.
fn front_budget(deadline: Instant, cap: Duration) -> Duration {
    deadline.saturating_duration_since(Instant::now()).min(cap)
}

fn timed_step(
    service: &'static str,
    running: bool,
    stop: impl FnOnce() -> Result<()>,
) -> SystemStopStep {
    let started = Instant::now();
    let error = if running {
        stop().err().map(|error| ProcessErrorJson::from(&error))
    } else {
        None
    };
    SystemStopStep {
        service,
        attempted: running,
        elapsed_ms: started.elapsed().as_millis(),
        error,
    }
}

fn http_busy_before_start(port: u16, state: &hostig_ports::PortState) -> ProcessError {
    let (who, action) = match state {
        hostig_ports::PortState::Ocupado {
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
                    .unwrap_or_else(|| "libera el puerto o elige otro".into()),
            )
        }
        hostig_ports::PortState::Reservado => (
            "reservado por el sistema".into(),
            "usa otro puerto en 8080-8099".into(),
        ),
        hostig_ports::PortState::OcupadoDueñoNoVisible => {
            ("ocupado, dueño no visible".into(), "usa otro puerto".into())
        }
        hostig_ports::PortState::Libre => unreachable!(),
    };
    ProcessError::new(
        "puerto HTTP ocupado",
        format!("puerto {port}: {who}; no se ha lanzado MariaDB ni PHP ni Apache"),
        action,
    )
}

fn wait_db_job(db: &DbService, job_id: &str, budget: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let job = db.job(job_id)?;
        match job.state.as_str() {
            "succeeded" => return Ok(()),
            "failed" => {
                let err = job.error.expect("error");
                return Err(ProcessError::new(err.message, err.cause, err.action));
            }
            _ if std::time::Instant::now() > deadline => {
                return Err(ProcessError::new(
                    "trabajo MariaDB sin respuesta",
                    format!("job {job_id}"),
                    "hostig db status",
                ));
            }
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn wait_php_job(php: &PhpService, job_id: &str, budget: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let job = php.job(job_id)?;
        match job.state.as_str() {
            "succeeded" => return Ok(()),
            "failed" => {
                let err = job.error.expect("error");
                return Err(ProcessError::new(err.message, err.cause, err.action));
            }
            _ if std::time::Instant::now() > deadline => {
                return Err(ProcessError::new(
                    "trabajo PHP sin respuesta",
                    format!("job {job_id}"),
                    "hostig status",
                ));
            }
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn wait_apache_job(apache: &ApacheService, job_id: &str, budget: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let job = apache.job(job_id)?;
        match job.state.as_str() {
            "succeeded" => return Ok(()),
            "failed" => {
                let err = job.error.expect("error");
                return Err(ProcessError::new(err.message, err.cause, err.action));
            }
            _ if std::time::Instant::now() > deadline => {
                return Err(ProcessError::new(
                    "trabajo Apache sin respuesta",
                    format!("job {job_id}"),
                    "hostig status",
                ));
            }
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
}
