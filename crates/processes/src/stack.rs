//! Orquestación MariaDB → PHP → Apache (ADR 0003).

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use uuid::Uuid;

use crate::apache::{ApacheService, ApacheStartRequest, ApacheState};
use crate::error::{ProcessError, Result};
use crate::mariadb::{
    read_password, DbService, DbState, InstancePaths, StartRequest as DbStartRequest,
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
            let result = service.run_stop();
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

        // Credenciales admin MariaDB para el health.
        let paths = InstancePaths::new(&self.data_dir, DEFAULT_INSTANCE_ID);
        let password = read_password(&paths).unwrap_or_default();
        let db_port = self.db.status().port;

        // 3. Apache
        let apache_job = self.apache.start_async(ApacheStartRequest {
            http_port: request.http_port,
            php_ports,
            db_host: "127.0.0.1".into(),
            db_port,
            db_user: "root".into(),
            db_password: password,
            ..ApacheStartRequest::default()
        })?;
        wait_apache_job(&self.apache, &apache_job.job_id, Duration::from_secs(120))?;
        if self.apache.status().state != ApacheState::Running {
            let _ = self.apache.stop_async();
            let _ = self.php.stop_async();
            let _ = self.db.stop_async();
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

    fn run_stop(&self) -> Result<()> {
        // Orden inverso: Apache → PHP → MariaDB.
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
            let job = self.db.stop_async()?;
            wait_db_job(&self.db, &job.job_id, Duration::from_secs(60))?;
        }
        Ok(())
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
