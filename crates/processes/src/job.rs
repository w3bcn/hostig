//! Consulta del Job Object del proceso actual (ADR 0011).

use crate::error::{ProcessError, Result};

/// `HOSTIG_ALLOW_JOB_WITHOUT_BREAKAWAY=1` permite arrancar aunque el job tenga
/// kill-on-close y el breakaway falle. Solo para tests/CI; no usar en producción.
pub const ALLOW_JOB_WITHOUT_BREAKAWAY_ENV: &str = "HOSTIG_ALLOW_JOB_WITHOUT_BREAKAWAY";

pub fn allow_job_without_breakaway() -> bool {
    matches!(
        std::env::var(ALLOW_JOB_WITHOUT_BREAKAWAY_ENV).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    )
}

/// Resultado de evaluar si es seguro dejar un hijo en el job padre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobSpawnPolicy {
    /// No estamos en un job, o el job no mata al cerrarse.
    SafeWithoutBreakaway,
    /// El job tiene `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    KillOnClose,
    /// No se pudo determinar (tratar como restrictivo salvo override).
    Unknown(String),
}

/// Política del job del proceso actual respecto a hijos sin breakaway.
pub fn current_job_spawn_policy() -> JobSpawnPolicy {
    #[cfg(windows)]
    {
        windows_job_spawn_policy()
    }
    #[cfg(not(windows))]
    {
        JobSpawnPolicy::SafeWithoutBreakaway
    }
}

/// Si el breakaway falló: error si kill-on-close (salvo override), o aviso si es seguro.
pub fn decide_after_breakaway_denied() -> Result<BreakawayFallback> {
    if allow_job_without_breakaway() {
        return Ok(BreakawayFallback::ProceedWithWarning {
            detail: format!(
                "{ALLOW_JOB_WITHOUT_BREAKAWAY_ENV}=1: se permite quedar en el job padre (solo tests/CI)"
            ),
        });
    }
    match current_job_spawn_policy() {
        JobSpawnPolicy::SafeWithoutBreakaway => Ok(BreakawayFallback::ProceedWithWarning {
            detail: "CREATE_BREAKAWAY_FROM_JOB denegado; el job padre no tiene kill-on-close"
                .into(),
        }),
        JobSpawnPolicy::KillOnClose => Err(ProcessError::new(
            "no se puede lanzar el proceso",
            "el proceso padre está en un job que mataría MariaDB al cerrarse",
            "arranca Hostig desde fuera de ese terminal o herramienta",
        )),
        JobSpawnPolicy::Unknown(detail) => Err(ProcessError::new(
            "no se puede lanzar el proceso",
            format!("CREATE_BREAKAWAY_FROM_JOB denegado y no se pudo comprobar el job: {detail}"),
            "arranca Hostig desde fuera de ese terminal o herramienta",
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakawayFallback {
    ProceedWithWarning { detail: String },
}

#[cfg(windows)]
fn windows_job_spawn_policy() -> JobSpawnPolicy {
    use windows::core::BOOL;
    use windows::Win32::System::JobObjects::{
        IsProcessInJob, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let process = GetCurrentProcess();
        let mut in_job = BOOL(0);
        if IsProcessInJob(process, None, &mut in_job).is_err() {
            return JobSpawnPolicy::Unknown("IsProcessInJob falló".into());
        }
        if !in_job.as_bool() {
            return JobSpawnPolicy::SafeWithoutBreakaway;
        }

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let mut ret = 0u32;
        // JobHandle NULL = job del proceso que llama (documentación de Microsoft).
        if let Err(error) = QueryInformationJobObject(
            None,
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of_val(&info) as u32,
            Some(&mut ret),
        ) {
            return JobSpawnPolicy::Unknown(format!("QueryInformationJobObject: {error}"));
        }
        let flags = info.BasicLimitInformation.LimitFlags;
        if flags.contains(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE) {
            JobSpawnPolicy::KillOnClose
        } else {
            JobSpawnPolicy::SafeWithoutBreakaway
        }
    }
}
