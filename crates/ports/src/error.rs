use std::fmt;

/// Error del módulo de puertos con causa y acción (ADR 0004 / log estructurado).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortError {
    pub message: String,
    pub cause: String,
    pub action: String,
}

impl PortError {
    pub fn new(
        message: impl Into<String>,
        cause: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            cause: cause.into(),
            action: action.into(),
        }
    }

    pub fn privileged(port: u16) -> Self {
        Self::new(
            "puerto privilegiado",
            format!("el puerto {port} requiere privilegios elevados"),
            "usar el modo con helper, o un puerto alto",
        )
    }

    pub fn range_full(range: std::ops::RangeInclusive<u16>) -> Self {
        Self::new(
            "rango de puertos lleno",
            format!(
                "no queda ningún puerto libre en {}-{}",
                range.start(),
                range.end()
            ),
            "elige un puerto a mano en el manifiesto",
        )
    }
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Causa: {} Acción: {}",
            self.message, self.cause, self.action
        )
    }
}

impl std::error::Error for PortError {}

pub type Result<T> = std::result::Result<T, PortError>;
