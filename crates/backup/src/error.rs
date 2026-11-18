use std::fmt;

/// Error de copia con causa y acción (mismo contrato que el resto de Hostig).
#[derive(Debug, Clone)]
pub struct BackupError {
    pub message: String,
    pub cause: String,
    pub action: String,
}

impl BackupError {
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

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Causa: {} Acción: {}",
            self.message, self.cause, self.action
        )
    }
}

impl std::error::Error for BackupError {}

impl From<std::io::Error> for BackupError {
    fn from(error: std::io::Error) -> Self {
        Self::new(
            "error de E/S en copias",
            error.to_string(),
            "revisa permisos y espacio en disco de la carpeta de datos",
        )
    }
}

pub type Result<T> = std::result::Result<T, BackupError>;
