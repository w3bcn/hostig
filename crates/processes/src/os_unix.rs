use std::path::PathBuf;

use crate::error::{ProcessError, Result};

pub fn process_creation_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Campo 22 (1-based): starttime en ticks de reloj.
        let start = stat.split_whitespace().nth(21)?.parse::<u64>().ok()?;
        Some(start)
    }
    #[cfg(target_os = "macos")]
    {
        hostig_ipc::macos_process_start_ms(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

pub fn process_image_path(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

pub fn send_ctrl_break(_pid: u32) -> Result<()> {
    Err(ProcessError::new(
        "ctrl-break solo en Windows",
        "en Unix Apache recibe SIGWINCH/SIGTERM",
        "usa el apagado Unix",
    ))
}

pub fn signal_apache_shutdown_event(_pid: u32) -> Result<()> {
    Err(ProcessError::new(
        "evento apPID_shutdown solo en Windows",
        "en Unix Apache recibe SIGWINCH/SIGTERM",
        "usa el apagado Unix",
    ))
}

pub fn child_pids(parent_pid: u32) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(pid_s) = name.to_str() else {
                    continue;
                };
                let Ok(pid) = pid_s.parse::<u32>() else {
                    continue;
                };
                if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                    // Campo 4 (1-based): ppid.
                    if let Some(ppid) = stat.split_whitespace().nth(3).and_then(|s| s.parse().ok())
                    {
                        if ppid == parent_pid {
                            out.push(pid);
                        }
                    }
                }
            }
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = parent_pid;
        Vec::new()
    }
}

pub fn terminate_process(pid: u32) -> Result<()> {
    let result = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    result.map_err(|error| {
        ProcessError::new(
            "kill falló",
            error.to_string(),
            "revisa permisos; esto es solo el camino de emergencia",
        )
    })
}
