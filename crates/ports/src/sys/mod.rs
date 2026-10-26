use std::net::IpAddr;
use std::path::PathBuf;

use crate::error::Result;

/// Listener TCP relevante (wildcard o loopback) con dueño si se pudo resolver.
#[derive(Debug, Clone)]
pub struct FoundListener {
    pub port: u16,
    #[allow(dead_code)]
    pub addr: IpAddr,
    /// `None` si el socket existe pero no pudimos leer el proceso (otro usuario).
    pub pid: Option<u32>,
}

pub fn list_relevant_listeners() -> Result<Vec<FoundListener>> {
    platform::list_relevant_listeners()
}

pub fn process_name_and_path(pid: u32) -> (String, Option<PathBuf>) {
    platform::process_name_and_path(pid)
}

pub fn is_relevant_addr(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_unspecified() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_loopback(),
    }
}

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;
    use crate::error::PortError;

    pub fn list_relevant_listeners() -> Result<Vec<FoundListener>> {
        Err(PortError::new(
            "plataforma no soportada",
            "este sistema operativo no tiene detector de puertos",
            "usa Windows, Linux o macOS",
        ))
    }

    pub fn process_name_and_path(_pid: u32) -> (String, Option<PathBuf>) {
        ("?".into(), None)
    }
}
