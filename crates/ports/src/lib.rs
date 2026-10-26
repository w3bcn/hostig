//! Gestión de puertos (ADR 0004): `check` y `suggest` sin procesos externos.

mod error;
mod probe;
pub mod ranges;
mod sys;

pub use error::{PortError, Result};
pub use probe::{
    classify_bind_denied, reserved_cause, reserved_cause_unix, reserved_cause_windows,
    reserved_error,
};

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::PathBuf;

use serde::Serialize;

use sys::FoundListener;

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
        /// Acción sugerida (p. ej. HTTP.sys / PID 4 en Windows).
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<String>,
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

/// Tabla TCP + caché de procesos, cargada una vez por operación.
struct PortTable {
    listeners: Vec<FoundListener>,
    processes: HashMap<u32, OccupantInfo>,
}

#[derive(Clone, Debug)]
pub(crate) struct OccupantInfo {
    pub(crate) name: String,
    pub(crate) path: Option<PathBuf>,
    pub(crate) action: Option<String>,
}

impl PortTable {
    fn load() -> Result<Self> {
        Ok(Self {
            listeners: sys::list_relevant_listeners()?,
            processes: HashMap::new(),
        })
    }

    fn occupant(&mut self, pid: u32) -> OccupantInfo {
        self.processes
            .entry(pid)
            .or_insert_with(|| occupant_info(pid))
            .clone()
    }

    /// Evalúa un puerto contra la tabla ya cargada; el bind-probe solo si parece libre.
    fn check(&mut self, port: u16, expected_pid: Option<u32>) -> Result<PortState> {
        if ranges::is_privileged(port) {
            return Err(PortError::privileged(port));
        }

        let on_port: Vec<FoundListener> = self
            .listeners
            .iter()
            .filter(|l| l.port == port)
            .cloned()
            .collect();

        if !on_port.is_empty() {
            return Ok(self.state_from_listeners(&on_port, expected_pid));
        }

        match probe::probe_loopback(port) {
            Ok(()) => Ok(PortState::Libre),
            Err(PortState::Reservado) => Ok(PortState::Reservado),
            Err(other) => Ok(other),
        }
    }

    fn state_from_listeners(
        &mut self,
        on_port: &[FoundListener],
        expected_pid: Option<u32>,
    ) -> PortState {
        // Dueño no legible → nunca Libre.
        if on_port.iter().any(|l| l.pid.is_none()) {
            return PortState::OcupadoDueñoNoVisible;
        }

        let foreign: Vec<u32> = on_port
            .iter()
            .filter_map(|l| l.pid)
            .filter(|pid| expected_pid != Some(*pid))
            .collect();

        // Todos los listeners son del pid esperado → sin conflicto.
        if foreign.is_empty() {
            return PortState::Libre;
        }

        let pid = foreign[0];
        let info = self.occupant(pid);
        PortState::Ocupado {
            pid,
            name: info.name,
            path: info.path,
            action: info.action,
        }
    }

    fn suggest(&mut self, preferred: u16, range: RangeInclusive<u16>) -> Result<u16> {
        probe::ensure_range(&range)?;

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
            match self.check(port, None) {
                Ok(PortState::Libre) => return Ok(port),
                Ok(_) => continue,
                Err(error) if error.message == "puerto privilegiado" => continue,
                Err(error) => return Err(error),
            }
        }
        Err(PortError::range_full(range))
    }

    fn check_row(
        &mut self,
        port: u16,
        expected_pid: Option<u32>,
    ) -> std::result::Result<PortCheckRow, PortError> {
        match self.check(port, expected_pid) {
            Ok(state) => {
                let suggested = if state.is_conflict() {
                    ranges::for_port(port)
                        .and_then(|range| self.suggest(port, range).ok())
                        .or_else(|| self.suggest(port, port..=port.saturating_add(20)).ok())
                } else {
                    None
                };
                Ok(row_from_state(port, state, suggested))
            }
            Err(error) => Err(error),
        }
    }
}

pub(crate) fn occupant_info(pid: u32) -> OccupantInfo {
    #[cfg(windows)]
    if pid == 4 {
        return OccupantInfo {
            name: "System (HTTP.sys)".into(),
            path: None,
            action: Some(
                "un servicio de Windows (IIS, WinRM, publicación web) usa este puerto; usa el puerto sugerido o detén ese servicio"
                    .into(),
            ),
        };
    }

    let (name, path) = sys::process_name_and_path(pid);
    OccupantInfo {
        name,
        path,
        action: None,
    }
}

fn row_from_state(port: u16, state: PortState, suggested: Option<u16>) -> PortCheckRow {
    let (pid, name, path, cause, action) = match &state {
        PortState::Ocupado {
            pid,
            name,
            path,
            action,
        } => (
            Some(*pid),
            Some(name.clone()),
            path.clone(),
            None,
            action.clone(),
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
    PortCheckRow {
        port,
        status: state.as_status_str().into(),
        conflict: state.is_conflict(),
        pid,
        name,
        path,
        suggested,
        cause,
        action,
    }
}

/// El módulo está implementado (fase 1 bloque 2).
pub fn implemented() -> bool {
    true
}

/// Comprueba un puerto.
///
/// Si **todos** los listeners del puerto pertenecen a `expected_pid`, no hay
/// conflicto (`Libre`). Si alguno es de otro proceso (u otro dueño no visible),
/// hay conflicto. Tras ver la tabla TCP, si parece libre se hace un `bind` en
/// `127.0.0.1` para detectar rangos reservados.
pub fn check(port: u16, expected_pid: Option<u32>) -> Result<PortState> {
    PortTable::load()?.check(port, expected_pid)
}

/// Primer puerto libre del `range`, empezando por `preferred` (y envolviendo).
/// Lee la tabla de listeners una sola vez.
pub fn suggest(preferred: u16, range: RangeInclusive<u16>) -> Result<u16> {
    PortTable::load()?.suggest(preferred, range)
}

/// Comprueba varios puertos con una sola lectura de la tabla TCP/procesos.
pub fn check_many(
    ports: &[u16],
    expected_pid: Option<u32>,
) -> Vec<std::result::Result<PortCheckRow, PortError>> {
    let mut table = match PortTable::load() {
        Ok(table) => table,
        Err(error) => {
            return ports.iter().map(|_| Err(error.clone())).collect();
        }
    };
    ports
        .iter()
        .map(|port| table.check_row(*port, expected_pid))
        .collect()
}

#[cfg(test)]
mod tests;
