use std::fmt;

/// Error del módulo de runtimes con causa y acción para el log / IPC.
#[derive(Debug)]
pub struct RuntimeError {
    pub message: String,
    pub cause: String,
    pub action: String,
}

impl RuntimeError {
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
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Causa: {} Acción: {}",
            self.message, self.cause, self.action
        )
    }
}

impl std::error::Error for RuntimeError {}

impl From<std::io::Error> for RuntimeError {
    fn from(error: std::io::Error) -> Self {
        Self::new(
            "error de E/S en runtimes",
            error.to_string(),
            "revisa permisos y espacio en la carpeta de datos",
        )
    }
}

pub type Result<T> = std::result::Result<T, RuntimeError>;
