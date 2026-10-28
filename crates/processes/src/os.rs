use std::path::PathBuf;

use crate::error::Result;

/// Hora de creación del proceso (unidad opaca comparable, estable en la misma plataforma).
pub fn process_creation_time(pid: u32) -> Option<u64> {
    platform::process_creation_time(pid)
}

pub fn process_image_path(pid: u32) -> Option<PathBuf> {
    platform::process_image_path(pid)
}

pub fn terminate_process(pid: u32) -> Result<()> {
    platform::terminate_process(pid)
}

/// Windows: ctrl-break al grupo de proceso. Unix: no-op (se usa señal en Apache).
#[allow(dead_code)]
pub fn send_ctrl_break(process_group_pid: u32) -> Result<()> {
    platform::send_ctrl_break(process_group_pid)
}

#[cfg(windows)]
#[path = "os_windows.rs"]
mod platform;

#[cfg(unix)]
#[path = "os_unix.rs"]
mod platform;

#[cfg(not(any(windows, unix)))]
mod platform {
    use std::path::PathBuf;

    use crate::error::{ProcessError, Result};

    pub fn process_creation_time(_pid: u32) -> Option<u64> {
        None
    }
    pub fn process_image_path(_pid: u32) -> Option<PathBuf> {
        None
    }
    pub fn terminate_process(_pid: u32) -> Result<()> {
        Err(ProcessError::new(
            "plataforma no soportada",
            "terminate no disponible",
            "usa Windows, Linux o macOS",
        ))
    }

    pub fn send_ctrl_break(_pid: u32) -> Result<()> {
        Err(ProcessError::new(
            "plataforma no soportada",
            "ctrl-break no disponible",
            "usa Windows",
        ))
    }
}
