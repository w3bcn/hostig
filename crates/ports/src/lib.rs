//! Gestión de puertos (ADR 0004): `check` y `suggest` sin procesos externos.

mod error;
mod probe;
pub mod ranges;
mod sys;

pub use error::{PortError, Result};
pub use probe::{classify_bind_denied, reserved_error};

use std::ops::RangeInclusive;
use std::path::PathBuf;

use serde::Serialize;

/// Resultado de inspeccionar un puerto.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PortState {
    Libre,
    Ocupado {
        pid: u32,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    OcupadoDueñoNoVisible,
    Reservado,
}

impl PortState {
    pub fn is_conflict(&self) -> bool {
        !matches!(self, PortState::Libre)
    }

    pub fn as_status_str(&self) -> &'static str {
        match self {
            PortState::Libre => "libre",
            PortState::Ocupado { .. } => "ocupado",
            PortState::OcupadoDueñoNoVisible => "ocupado_dueno_no_visible",
            PortState::Reservado => "reservado",
        }
    }
}

/// Fila lista para IPC / CLI.
#[derive(Debug, Clone, Serialize)]
pub struct PortCheckRow {
    pub port: u16,
    pub status: String,
    pub conflict: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// El módulo está implementado (fase 1 bloque 2).
pub fn implemented() -> bool {
    true
}

/// Comprueba un puerto.
///
/// Si `expected_pid` coincide con el ocupante, no hay conflicto (`Libre`).
/// Tras ver la tabla TCP, si parece libre se hace un `bind` en 127.0.0.1
/// para detectar rangos reservados (Hyper-V / WSL / Docker).
pub fn check(port: u16, expected_pid: Option<u32>) -> Result<PortState> {
    if ranges::is_privileged(port) {
        return Err(PortError::privileged(port));
    }

    let listeners = sys::list_relevant_listeners()?;
    let on_port: Vec<_> = listeners.into_iter().filter(|l| l.port == port).collect();

    if let Some(found) = on_port.iter().find(|l| l.pid.is_some()) {
        let pid = found.pid.expect("filtrado");
        if expected_pid == Some(pid) {
            return Ok(PortState::Libre);
        }
        let (name, path) = sys::process_name_and_path(pid);
        return Ok(PortState::Ocupado { pid, name, path });
    }

    if on_port.iter().any(|l| l.pid.is_none()) {
        return Ok(PortState::OcupadoDueñoNoVisible);
    }

    match probe::probe_loopback(port) {
        Ok(()) => Ok(PortState::Libre),
        Err(PortState::Reservado) => Ok(PortState::Reservado),
        Err(other) => Ok(other),
    }
}

/// Primer puerto libre del `range`, empezando por `preferred` (y envolviendo).
/// Salta ocupados y reservados.
pub fn suggest(preferred: u16, range: RangeInclusive<u16>) -> Result<u16> {
    probe::ensure_range(&range)?;
    if ranges::is_privileged(preferred) && range.contains(&preferred) {
        // El preferido privilegiado no se ofrece; se busca otro en el rango.
    }

    let start = if range.contains(&preferred) {
        preferred
    } else {
        *range.start()
    };

    let mut candidates: Vec<u16> = (start..=*range.end()).collect();
    if start > *range.start() {
        candidates.extend(*range.start()..start);
    }

    for port in candidates {
        if ranges::is_privileged(port) {
            continue;
        }
        match check(port, None) {
            Ok(PortState::Libre) => return Ok(port),
            Ok(_) => continue,
            Err(error) if error.message == "puerto privilegiado" => continue,
            Err(error) => return Err(error),
        }
    }
    Err(PortError::range_full(range))
}

/// Comprueba varios puertos y adjunta sugerencia cuando hay conflicto.
pub fn check_many(ports: &[u16], expected_pid: Option<u32>) -> Vec<std::result::Result<PortCheckRow, PortError>> {
    ports
        .iter()
        .map(|port| check_row(*port, expected_pid))
        .collect()
}

fn check_row(port: u16, expected_pid: Option<u32>) -> std::result::Result<PortCheckRow, PortError> {
    match check(port, expected_pid) {
        Ok(state) => {
            let suggested = if state.is_conflict() {
                ranges::for_port(port)
                    .and_then(|range| suggest(port, range).ok())
                    .or_else(|| {
                        // Si el puerto no cae en un rango ADR, prueba desde él en un
                        // vecindario corto solo para dar una pista en la CLI.
                        suggest(port, port..=port.saturating_add(20)).ok()
                    })
            } else {
                None
            };
            let (pid, name, path, cause, action) = match &state {
                PortState::Ocupado { pid, name, path } => (
                    Some(*pid),
                    Some(name.clone()),
                    path.clone(),
                    None,
                    None,
                ),
                PortState::Reservado => {
                    let err = reserved_error(port);
                    (None, None, None, Some(err.cause), Some(err.action))
                }
                PortState::OcupadoDueñoNoVisible => (
                    None,
                    None,
                    None,
                    Some("hay un listener cuyo dueño no es visible".into()),
                    Some("usa el puerto sugerido o elige otro en el manifiesto".into()),
                ),
                PortState::Libre => (None, None, None, None, None),
            };
            Ok(PortCheckRow {
                port,
                status: state.as_status_str().into(),
                conflict: state.is_conflict(),
                pid,
                name,
                path,
                suggested,
                cause,
                action,
            })
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
