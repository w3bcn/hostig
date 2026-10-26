use std::fmt;

/// Error del módulo de procesos / MariaDB con causa y acción.
#[derive(Debug, Clone)]
pub struct ProcessError {
    pub message: String,
    pub cause: String,
    pub action: String,
}

impl ProcessError {
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

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Causa: {} Acción: {}",
            self.message, self.cause, self.action
        )
    }
}

impl std::error::Error for ProcessError {}

impl From<std::io::Error> for ProcessError {
    fn from(error: std::io::Error) -> Self {
        Self::new(
            "error de E/S en procesos",
            error.to_string(),
            "revisa permisos y la carpeta de datos",
        )
    }
}

pub type Result<T> = std::result::Result<T, ProcessError>;
