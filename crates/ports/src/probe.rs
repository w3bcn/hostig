use std::io;
use std::net::{SocketAddr, TcpListener};

use crate::error::{PortError, Result};
use crate::PortState;

/// Clasifica un error de `bind` como `Reservado` si es acceso denegado.
///
/// En Windows suele ser `WSAEACCES` (10013); en Unix, `EACCES`/`EPERM`.
pub fn classify_bind_denied(error: &io::Error) -> Option<PortState> {
    let denied = match error.raw_os_error() {
        // WSAEACCES
        Some(10013) => true,
        // EPERM / EACCES
        Some(1) | Some(13) => true,
        _ => matches!(error.kind(), io::ErrorKind::PermissionDenied),
    };
    if denied {
        Some(PortState::Reservado)
    } else {
        None
    }
}

/// Intenta `bind` en `127.0.0.1:port` y suelta el socket de inmediato.
///
/// - Ok(()) — el puerto acepta bind (libre para nosotros).
/// - Err(Reservado) — el sistema lo tiene excluido (Hyper-V, WSL, Docker…).
/// - Err(OcupadoDueñoNoVisible) — bind dice ocupado aunque la tabla no lo vio.
pub fn probe_loopback(port: u16) -> std::result::Result<(), PortState> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpListener::bind(addr) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(error) => {
            if let Some(state) = classify_bind_denied(&error) {
                return Err(state);
            }
            if matches!(
                error.kind(),
                io::ErrorKind::AddrInUse | io::ErrorKind::AddrNotAvailable
            ) {
                return Err(PortState::OcupadoDueñoNoVisible);
            }
            Err(PortState::OcupadoDueñoNoVisible)
        }
    }
}

pub fn reserved_error(port: u16) -> PortError {
    PortError::new(
        "puerto reservado",
        format!(
            "puerto {port}: puerto reservado por el sistema (Hyper-V, WSL o Docker)"
        ),
        "usa el puerto sugerido",
    )
}

/// Comprueba que el rango no esté vacío antes de sugerir.
pub fn ensure_range(range: &std::ops::RangeInclusive<u16>) -> Result<()> {
    if range.is_empty() {
        return Err(PortError::new(
            "rango inválido",
            "el rango de puertos está vacío",
            "corrige el rango en el manifiesto",
        ));
    }
    Ok(())
}
