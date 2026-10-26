use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use uuid::Uuid;

use crate::catalog::{Catalog, RuntimeEntry};
use crate::error::{Result, RuntimeError};
use crate::install::{install_entry, runtime_dir, verify_installed, MANIFEST_NAME};

/// Máximo de trabajos terminados (éxito o fallo) que se conservan en memoria.
pub const MAX_FINISHED_JOBS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub job_id: String,
    pub state: JobState,
    pub component: String,
    pub version: String,
    pub path: Option<String>,
    pub error: Option<JobError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobError {
    pub message: String,
    pub cause: String,
    pub action: String,
}

struct JobSlot {
    status: Mutex<JobStatus>,
    done: Condvar,
}

struct SharedJobs {
    jobs: Mutex<HashMap<String, Arc<JobSlot>>>,
    /// install_key → job_id de la instalación en curso o reciente.
    inflight: Mutex<HashMap<String, String>>,
    /// job_id terminados, del más antiguo al más reciente.
    finished: Mutex<VecDeque<String>>,
}

impl SharedJobs {
    fn remember_finished(&self, job_id: &str) {
        let mut finished = self.finished.lock().expect("finished");
        finished.push_back(job_id.to_string());
        while finished.len() > MAX_FINISHED_JOBS {
            let Some(old) = finished.pop_front() else {
                break;
            };
            drop(finished);
            self.drop_finished_job(&old);
            finished = self.finished.lock().expect("finished");
        }
    }

    fn drop_finished_job(&self, job_id: &str) {
        {
            let mut jobs = self.jobs.lock().expect("jobs");
            if let Some(slot) = jobs.get(job_id) {
                let state = slot.status.lock().expect("status").state;
                if !matches!(state, JobState::Succeeded | JobState::Failed) {
                    return;
                }
            }
            jobs.remove(job_id);
        }
        let mut inflight = self.inflight.lock().expect("inflight");
        inflight.retain(|_, id| id != job_id);
    }

    #[cfg(test)]
    fn finished_len(&self) -> usize {
        self.finished.lock().expect("finished").len()
    }

    #[cfg(test)]
    fn jobs_len(&self) -> usize {
        self.jobs.lock().expect("jobs").len()
    }
}

/// Gestor de instalaciones: deduplica descargas y expone trabajos asíncronos.
pub struct RuntimeService {
    catalog: Catalog,
    data_dir: PathBuf,
    shared: Arc<SharedJobs>,
}

impl RuntimeService {
    pub fn new(data_dir: impl Into<PathBuf>, catalog: Catalog) -> Self {
        Self {
            catalog,
            data_dir: data_dir.into(),
            shared: Arc::new(SharedJobs {
                jobs: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                finished: Mutex::new(VecDeque::new()),
            }),
        }
    }

    pub fn with_embedded(data_dir: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self::new(data_dir, Catalog::embedded()?))
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn list(&self) -> Vec<ListedRuntime> {
        let host_os = crate::catalog::host_os();
        let host_arch = crate::catalog::host_arch();
        self.catalog
            .entries
            .iter()
            .map(|entry| {
                let path = runtime_dir(&self.data_dir, entry);
                let installed = path.join(MANIFEST_NAME).is_file();
                ListedRuntime {
                    component: entry.component.clone(),
                    version: entry.version.clone(),
                    os: entry.os.clone(),
                    arch: entry.arch.clone(),
                    license: entry.license.clone(),
                    notes: entry.notes.clone(),
                    installed,
                    path: if installed {
                        Some(path.display().to_string())
                    } else {
                        None
                    },
                    matches_host: entry.os == host_os && entry.arch == host_arch,
                }
            })
            .collect()
    }

    /// Arranca la instalación en segundo plano (o reutiliza la en curso).
    pub fn start_install(&self, component: &str, version: &str) -> Result<JobStatus> {
        let entry = self
            .catalog
            .find(component, version)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::new(
                    "runtime desconocido",
                    format!("no hay {component}@{version} en el catálogo"),
                    "mira hostig runtimes list",
                )
            })?;
        self.start_install_entry(entry)
    }

    fn start_install_entry(&self, entry: RuntimeEntry) -> Result<JobStatus> {
        let key = entry.install_key();
        let final_dir = runtime_dir(&self.data_dir, &entry);
        if final_dir.join(MANIFEST_NAME).is_file() {
            return Ok(JobStatus {
                job_id: Uuid::new_v4().to_string(),
                state: JobState::Succeeded,
                component: entry.component.clone(),
                version: entry.version.clone(),
                path: Some(final_dir.display().to_string()),
                error: None,
            });
        }

        let mut inflight = self.shared.inflight.lock().expect("inflight");
        if let Some(job_id) = inflight.get(&key).cloned() {
            let jobs = self.shared.jobs.lock().expect("jobs");
            if let Some(slot) = jobs.get(&job_id) {
                let status = slot.status.lock().expect("status").clone();
                if matches!(
                    status.state,
                    JobState::Queued | JobState::Running | JobState::Succeeded
                ) {
                    return Ok(status);
                }
            }
        }

        let job_id = Uuid::new_v4().to_string();
        let status = JobStatus {
            job_id: job_id.clone(),
            state: JobState::Queued,
            component: entry.component.clone(),
            version: entry.version.clone(),
            path: None,
            error: None,
        };
        let slot = Arc::new(JobSlot {
            status: Mutex::new(status.clone()),
            done: Condvar::new(),
        });
        self.shared
            .jobs
            .lock()
            .expect("jobs")
            .insert(job_id.clone(), Arc::clone(&slot));
        inflight.insert(key, job_id.clone());
        drop(inflight);

        let data_dir = self.data_dir.clone();
        let shared = Arc::clone(&self.shared);
        let finished_id = job_id.clone();
        thread::spawn(move || {
            {
                let mut guard = slot.status.lock().expect("status");
                guard.state = JobState::Running;
            }
            let outcome = install_entry(&data_dir, &entry);
            {
                let mut guard = slot.status.lock().expect("status");
                match outcome {
                    Ok(path) => {
                        guard.state = JobState::Succeeded;
                        guard.path = Some(path.display().to_string());
                        guard.error = None;
                    }
                    Err(error) => {
                        guard.state = JobState::Failed;
                        guard.error = Some(JobError {
                            message: error.message,
                            cause: error.cause,
                            action: error.action,
                        });
                    }
                }
            }
            shared.remember_finished(&finished_id);
            slot.done.notify_all();
        });
        Ok(status)
    }

    /// Instalación síncrona. Si ya hay una en curso del mismo runtime, espera la misma.
    pub fn install_blocking(&self, component: &str, version: &str) -> Result<PathBuf> {
        let status = self.start_install(component, version)?;
        self.wait_job(&status.job_id)
    }

    pub fn install_entry_blocking(&self, entry: &RuntimeEntry) -> Result<PathBuf> {
        let status = self.start_install_entry(entry.clone())?;
        self.wait_job(&status.job_id)
    }

    fn wait_job(&self, job_id: &str) -> Result<PathBuf> {
        loop {
            let slot = {
                let jobs = self.shared.jobs.lock().expect("jobs");
                jobs.get(job_id).cloned()
            };
            let Some(slot) = slot else {
                return Err(RuntimeError::new(
                    "trabajo desconocido",
                    format!("no hay job {job_id}"),
                    "usa el id que devolvió runtimes.install",
                ));
            };
            let mut guard = slot.status.lock().expect("status");
            match guard.state {
                JobState::Succeeded => {
                    return Ok(PathBuf::from(guard.path.clone().expect("path en éxito")));
                }
                JobState::Failed => {
                    let err = guard.error.clone().unwrap_or(JobError {
                        message: "instalación fallida".into(),
                        cause: "sin detalle".into(),
                        action: "reintenta".into(),
                    });
                    return Err(RuntimeError::new(err.message, err.cause, err.action));
                }
                JobState::Queued | JobState::Running => {
                    let (next, _) = slot
                        .done
                        .wait_timeout(guard, Duration::from_millis(200))
                        .expect("wait");
                    guard = next;
                    drop(guard);
                }
            }
        }
    }

    pub fn job(&self, job_id: &str) -> Result<JobStatus> {
        let slot = {
            let jobs = self.shared.jobs.lock().expect("jobs");
            jobs.get(job_id).cloned().ok_or_else(|| {
                RuntimeError::new(
                    "trabajo desconocido",
                    format!("no hay job {job_id}"),
                    "usa el id que devolvió runtimes.install",
                )
            })?
        };
        let status = slot.status.lock().expect("status").clone();
        Ok(status)
    }

    pub fn verify(&self, component: Option<&str>, version: Option<&str>) -> Result<Vec<VerifyRow>> {
        let mut rows = Vec::new();
        for entry in &self.catalog.entries {
            if let Some(c) = component {
                if entry.component != c {
                    continue;
                }
            }
            if let Some(v) = version {
                if entry.version != v {
                    continue;
                }
            }
            let path = runtime_dir(&self.data_dir, entry);
            if !path.join(MANIFEST_NAME).is_file() {
                continue;
            }
            match verify_installed(&self.data_dir, entry) {
                Ok(()) => rows.push(VerifyRow {
                    component: entry.component.clone(),
                    version: entry.version.clone(),
                    ok: true,
                    detail: "coincide con el manifiesto".into(),
                }),
                Err(error) => rows.push(VerifyRow {
                    component: entry.component.clone(),
                    version: entry.version.clone(),
                    ok: false,
                    detail: format!("{} — {}", error.cause, error.action),
                }),
            }
        }
        Ok(rows)
    }

    #[cfg(test)]
    pub fn finished_job_count(&self) -> usize {
        self.shared.finished_len()
    }

    #[cfg(test)]
    pub fn tracked_job_count(&self) -> usize {
        self.shared.jobs_len()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ListedRuntime {
    pub component: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub license: String,
    pub notes: String,
    pub installed: bool,
    pub path: Option<String>,
    pub matches_host: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyRow {
    pub component: String,
    pub version: String,
    pub ok: bool,
    pub detail: String,
}
