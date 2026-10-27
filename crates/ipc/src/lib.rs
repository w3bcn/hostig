//! Encuadre, testigo y transporte entre la CLI y `hostig-supervisor`.
//!
//! El transporte no es la crate `interprocess`: su listener no deja fijar una DACL
//! solo para el usuario actual y SYSTEM. En Unix el socket es el de la biblioteca
//! estándar, porque expone el descriptor para `SO_PEERCRED` / `getpeereid`. En
//! Windows el named pipe lo crea la crate `windows`, que sí acepta esa DACL.

mod frame;
mod paths;
mod peer;
mod protocol;
mod token;
mod transport;

pub use frame::{read_frame, write_frame, FrameError, MAX_FRAME_LEN};
pub use paths::{default_data_dir, Endpoint, Layout};
pub use peer::restrict_to_user_and_system;
pub use protocol::success as protocol_success;
pub use protocol::{
    error_response, rpc_action, rpc_cause, rpc_message, Handshake, Status, PROTOCOL_VERSION,
};
pub use token::{load_or_create_token, tokens_match};
pub use transport::{connect as connect_raw, listen, Accepted, ByteStream, Listener};

#[cfg(unix)]
pub use transport::{check_unix_socket_path, unix_socket_path_max};

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    Rejected {
        message: String,
        cause: String,
        action: String,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(error) => write!(f, "{error}"),
            ClientError::Rejected {
                message,
                cause,
                action,
            } => {
                write!(f, "{message} Causa: {cause} Acción: {action}")
            }
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        ClientError::Io(error)
    }
}

/// Cada llamada abre su conexión, envía una petición y la cierra.
/// Encadenar otra petición en el mismo named pipe bloquea a los dos lados.
pub struct Client;

impl Client {
    pub fn status(layout: &Layout) -> Result<Status, ClientError> {
        let token = load_or_create_token(&layout.token_path)?;
        let raw = call(
            layout,
            &token,
            PROTOCOL_VERSION,
            "status",
            serde_json::json!({}),
        )?;
        Ok(protocol::parse_status(&raw)?)
    }

    pub fn shutdown(layout: &Layout) -> Result<(), ClientError> {
        let token = load_or_create_token(&layout.token_path)?;
        let raw = call(
            layout,
            &token,
            PROTOCOL_VERSION,
            "shutdown",
            serde_json::json!({}),
        )?;
        check_success(&raw)
    }

    pub fn connect_with_token(
        layout: &Layout,
        token: &str,
        protocol: u32,
    ) -> Result<(), ClientError> {
        let raw = call(layout, token, protocol, "handshake", serde_json::json!({}))?;
        check_success(&raw)
    }

    pub fn connect(layout: &Layout) -> Result<(), ClientError> {
        let token = load_or_create_token(&layout.token_path)?;
        Self::connect_with_token(layout, &token, PROTOCOL_VERSION)
    }

    /// Petición JSON-RPC genérica. `extra` se fusiona con protocol y token.
    pub fn call(
        layout: &Layout,
        method: &str,
        extra: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        let token = load_or_create_token(&layout.token_path)?;
        let raw = call(layout, &token, PROTOCOL_VERSION, method, extra)?;
        parse_result(&raw)
    }
}

fn map_connect_error(error: std::io::Error) -> ClientError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        ClientError::Rejected {
            message: "extremo remoto no confiable".into(),
            cause: error.to_string(),
            action: "arranca el supervisor con tu usuario o elimina un socket/pipe ajeno".into(),
        }
    } else {
        ClientError::Io(error)
    }
}

fn call(
    layout: &Layout,
    token: &str,
    protocol: u32,
    method: &str,
    extra: serde_json::Value,
) -> Result<Vec<u8>, ClientError> {
    let mut stream = connect_raw(layout).map_err(map_connect_error)?;
    let mut params = serde_json::json!({ "protocol": protocol, "token": token });
    if let Some(map) = extra.as_object() {
        for (key, value) in map {
            params[key] = value.clone();
        }
    }
    write_frame(&mut stream, &protocol::request(1, method, params)).map_err(FrameError::into_io)?;
    // Un solo handle: write+read. Duplicar el named pipe en Windows dejaba
    // wait_for_close del servidor bloqueado hasta el último CloseHandle.
    let raw = read_frame(&mut stream).map_err(FrameError::into_io)?;
    drop(stream);
    Ok(raw)
}

fn check_success(raw: &[u8]) -> Result<(), ClientError> {
    parse_result(raw).map(|_| ())
}

fn parse_result(raw: &[u8]) -> Result<serde_json::Value, ClientError> {
    let value: serde_json::Value = serde_json::from_slice(raw).map_err(|error| {
        ClientError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    if value.get("error").is_some() {
        return Err(ClientError::Rejected {
            message: protocol::rpc_message(&value),
            cause: protocol::rpc_cause(&value),
            action: protocol::rpc_action(&value),
        });
    }
    Ok(value
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

/// `true` si el pid sigue en marcha de verdad.
///
/// Zombies (Unix) y procesos ya salidos con handle abierto (Windows) no cuentan.
/// Un pid 0 no está vivo.
pub fn process_is_running(pid: u32) -> bool {
    peer::process_is_running(pid)
}

/// Detalle de la consulta de liveness (para tests y diagnóstico en CI).
pub fn process_liveness_detail(pid: u32) -> String {
    peer::process_liveness_detail(pid)
}

/// Descriptores o handles abiertos del proceso (para tests de fuga).
pub fn process_open_resources(pid: u32) -> std::io::Result<u32> {
    peer::process_open_resources(pid)
}
