use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hostig_ipc::{
    connect_raw, error_response, listen, load_or_create_token, protocol_success, read_frame,
    tokens_match, write_frame, ByteStream, FrameError, Layout, Listener, MAX_FRAME_LEN,
    PROTOCOL_VERSION,
};
use hostig_runtimes::{clean_tmp_runtimes, RuntimeService};
use serde_json::{json, Value};

use crate::lock::Lock;
use crate::log::Logger;

/// Tiempo máximo de inactividad por conexión (mensaje o cierre).
const SESSION_IDLE: Duration = Duration::from_secs(5);
/// Tope de sesiones concurrentes para no agotar hilos.
const MAX_SESSIONS: usize = 32;

pub fn serve(layout: &Layout, log: &Logger, _lock: Lock) -> std::io::Result<()> {
    let token = load_or_create_token(&layout.token_path)?;
    std::fs::write(layout.pid_path.as_path(), std::process::id().to_string())?;
    match clean_tmp_runtimes(&layout.data_dir) {
        Ok(0) => {}
        Ok(n) => log.info("runtimes", &format!("limpiados {n} restos en tmp/runtimes")),
        Err(error) => log.error(
            "runtimes",
            "no se pudo limpiar tmp/runtimes",
            &error.to_string(),
            "borra a mano {datos}/tmp/runtimes si queda basura de una instalación cortada",
        ),
    }
    let runtimes = match RuntimeService::with_embedded(&layout.data_dir) {
        Ok(service) => Arc::new(service),
        Err(error) => {
            log.error(
                "runtimes",
                "no se pudo cargar el catálogo",
                &error.cause,
                &error.action,
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            ));
        }
    };
    let listener = listen(layout)?;
    log.info(
        "supervisor",
        &format!("supervisor en marcha, pid {}", std::process::id()),
    );
    let shutdown = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let result = accept_loop(
        &listener, layout, &token, log, &shutdown, &active, &runtimes,
    );
    wait_for_sessions(&active, Duration::from_secs(6));
    let _ = std::fs::remove_file(&layout.pid_path);
    if let hostig_ipc::Endpoint::Unix(path) = &layout.endpoint {
        let _ = std::fs::remove_file(path);
    }
    log.info("supervisor", "supervisor detenido");
    result
}

fn accept_loop(
    listener: &Listener,
    layout: &Layout,
    token: &str,
    log: &Logger,
    shutdown: &Arc<AtomicBool>,
    active: &Arc<AtomicUsize>,
    runtimes: &Arc<RuntimeService>,
) -> std::io::Result<()> {
    let token = Arc::new(token.to_string());
    let log = log.clone();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        let accepted = listener.accept()?;
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        if !accepted.same_user {
            log.error(
                "ipc",
                "conexión cerrada",
                "el peer no es el usuario que arrancó el supervisor",
                "usa la CLI o la aplicación del mismo usuario",
            );
            continue;
        }
        if active.load(Ordering::SeqCst) >= MAX_SESSIONS {
            log.error(
                "ipc",
                "conexión rechazada",
                "hay demasiadas conexiones simultáneas",
                "espera a que terminen otras peticiones o reinicia el supervisor",
            );
            continue;
        }
        let stream = accepted.stream;
        let token = Arc::clone(&token);
        let log = log.clone();
        let shutdown = Arc::clone(shutdown);
        let active = Arc::clone(active);
        let runtimes = Arc::clone(runtimes);
        let wake_layout = layout.clone();
        active.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let outcome = session(stream, &token, &log, &shutdown, &runtimes);
            if shutdown.load(Ordering::SeqCst) {
                wake_accept(&wake_layout);
            }
            if let Err(error) = outcome {
                if !is_timeout(&error) && !closed(&error) {
                    log.error(
                        "ipc",
                        "la sesión terminó con error",
                        &error.to_string(),
                        "el supervisor sigue en marcha; reintenta la conexión",
                    );
                }
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn wake_accept(layout: &Layout) {
    let _ = connect_raw(layout);
}

fn wait_for_sessions(active: &AtomicUsize, budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    while active.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Una conexión, una llamada. El cliente cierra después de leer la respuesta.
fn session(
    mut stream: Box<dyn ByteStream>,
    token: &str,
    log: &Logger,
    shutdown: &AtomicBool,
    runtimes: &RuntimeService,
) -> std::io::Result<()> {
    let first = match read_frame_timed(&mut stream, log, "el cliente no envió su mensaje a tiempo")
    {
        Ok(bytes) => bytes,
        Err(FrameError::TooLarge(len)) => {
            reject_large(&mut stream, log, None, len);
            return wait_for_close_timed(&mut stream, log);
        }
        Err(FrameError::Io(error)) if is_timeout(&error) || closed(&error) => return Ok(()),
        Err(FrameError::Io(error)) => return Err(error),
    };
    let request = parse(&first)?;
    let id = request_id(&request);
    let protocol = request
        .pointer("/params/protocol")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    if protocol != PROTOCOL_VERSION {
        let cause = format!(
            "el cliente habla la versión {protocol} y este supervisor la {PROTOCOL_VERSION}"
        );
        let action = "actualiza Hostig para que los dos usen la misma versión del protocolo";
        log.error("ipc", "handshake rechazado", &cause, action);
        reply_error(
            &mut stream,
            id,
            -32000,
            "versión de protocolo incompatible",
            &cause,
            action,
        )?;
        return wait_for_close_timed(&mut stream, log);
    }
    let got = request
        .pointer("/params/token")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !tokens_match(token, got) {
        let cause = "el testigo no coincide con auth.token";
        let action = "borra el testigo que guardó el cliente y vuelve a conectar";
        log.error("ipc", "handshake rechazado", cause, action);
        reply_error(&mut stream, id, -32001, "testigo rechazado", cause, action)?;
        return wait_for_close_timed(&mut stream, log);
    }
    match request_method(&request) {
        "handshake" => {
            write_frame(&mut stream, &protocol_success(id, json!({ "ok": true })))?;
        }
        "status" => {
            let body = protocol_success(
                id,
                json!({
                    "running": true,
                    "pid": std::process::id(),
                    "protocol": PROTOCOL_VERSION,
                }),
            );
            write_frame(&mut stream, &body)?;
        }
        "shutdown" => {
            shutdown.store(true, Ordering::SeqCst);
            write_frame(
                &mut stream,
                &protocol_success(id, json!({ "stopping": true })),
            )?;
        }
        "runtimes.list" => {
            let list = runtimes.list();
            write_frame(
                &mut stream,
                &protocol_success(id, json!({ "runtimes": list })),
            )?;
        }
        "runtimes.install" => match handle_runtimes_install(runtimes, &request, log) {
            Ok(status) => {
                write_frame(&mut stream, &protocol_success(id, json!(status)))?;
            }
            Err((message, cause, action)) => {
                reply_error(&mut stream, id, -32010, &message, &cause, &action)?;
            }
        },
        "runtimes.job" => match handle_runtimes_job(runtimes, &request) {
            Ok(status) => {
                write_frame(&mut stream, &protocol_success(id, json!(status)))?;
            }
            Err((message, cause, action)) => {
                reply_error(&mut stream, id, -32011, &message, &cause, &action)?;
            }
        },
        "runtimes.verify" => match handle_runtimes_verify(runtimes, &request, log) {
            Ok(rows) => {
                write_frame(
                    &mut stream,
                    &protocol_success(id, json!({ "results": rows })),
                )?;
            }
            Err((message, cause, action)) => {
                reply_error(&mut stream, id, -32012, &message, &cause, &action)?;
            }
        },
        other => {
            reply_error(
                &mut stream,
                id,
                -32601,
                "método desconocido",
                &format!("el método {other} no existe"),
                "usa status, shutdown o runtimes.*",
            )?;
        }
    }
    wait_for_close_timed(&mut stream, log)
}

fn handle_runtimes_install(
    runtimes: &RuntimeService,
    request: &Value,
    log: &Logger,
) -> Result<hostig_runtimes::JobStatus, (String, String, String)> {
    let component = request
        .pointer("/params/component")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = request
        .pointer("/params/version")
        .and_then(Value::as_str)
        .unwrap_or("");
    if component.is_empty() || version.is_empty() {
        return Err((
            "parámetros incompletos".into(),
            "faltan component o version".into(),
            "usa component@version, por ejemplo php@8.4.27".into(),
        ));
    }
    match runtimes.start_install(component, version) {
        Ok(status) => Ok(status),
        Err(error) => {
            log.error("runtimes", &error.message, &error.cause, &error.action);
            Err((error.message, error.cause, error.action))
        }
    }
}

fn handle_runtimes_job(
    runtimes: &RuntimeService,
    request: &Value,
) -> Result<hostig_runtimes::JobStatus, (String, String, String)> {
    let job_id = request
        .pointer("/params/job_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if job_id.is_empty() {
        return Err((
            "parámetros incompletos".into(),
            "falta job_id".into(),
            "usa el id que devolvió runtimes.install".into(),
        ));
    }
    runtimes
        .job(job_id)
        .map_err(|error| (error.message, error.cause, error.action))
}

fn handle_runtimes_verify(
    runtimes: &RuntimeService,
    request: &Value,
    log: &Logger,
) -> Result<Vec<hostig_runtimes::VerifyRow>, (String, String, String)> {
    let component = request.pointer("/params/component").and_then(Value::as_str);
    let version = request.pointer("/params/version").and_then(Value::as_str);
    match runtimes.verify(component, version) {
        Ok(rows) => {
            for row in &rows {
                if !row.ok {
                    log.error(
                        "runtimes",
                        "verificación fallida",
                        &row.detail,
                        "reinstala el runtime afectado",
                    );
                }
            }
            Ok(rows)
        }
        Err(error) => {
            log.error("runtimes", &error.message, &error.cause, &error.action);
            Err((error.message, error.cause, error.action))
        }
    }
}

fn read_frame_timed(
    stream: &mut Box<dyn ByteStream>,
    log: &Logger,
    cause: &str,
) -> Result<Vec<u8>, FrameError> {
    match with_idle_timeout(stream, |s| read_frame(s)) {
        IdleOutcome::Done(result) => result,
        IdleOutcome::TimedOut => {
            log.error(
                "ipc",
                "conexión cerrada por inactividad",
                cause,
                "envía el mensaje o cierra la conexión; el supervisor sigue atendiendo",
            );
            Err(FrameError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                cause,
            )))
        }
        IdleOutcome::CloneFailed(error) => {
            log.error(
                "ipc",
                "conexión cerrada",
                &format!("no se pudo clonar el descriptor para aplicar el timeout: {error}"),
                "libera descriptores o reduce la carga; el supervisor sigue atendiendo",
            );
            let _ = stream.interrupt();
            Err(FrameError::Io(error))
        }
    }
}

fn wait_for_close_timed(stream: &mut Box<dyn ByteStream>, log: &Logger) -> std::io::Result<()> {
    match with_idle_timeout(stream, |s| wait_for_close(s)) {
        IdleOutcome::Done(result) => result,
        IdleOutcome::TimedOut => {
            log.error(
                "ipc",
                "conexión cerrada por inactividad",
                "el cliente no cerró la conexión a tiempo",
                "cierra el cliente tras leer la respuesta; el supervisor sigue atendiendo",
            );
            let _ = stream.interrupt();
            Ok(())
        }
        IdleOutcome::CloneFailed(error) => {
            log.error(
                "ipc",
                "conexión cerrada",
                &format!("no se pudo clonar el descriptor para aplicar el timeout: {error}"),
                "libera descriptores o reduce la carga; el supervisor sigue atendiendo",
            );
            let _ = stream.interrupt();
            Err(error)
        }
    }
}

enum IdleOutcome<T> {
    Done(T),
    TimedOut,
    CloneFailed(std::io::Error),
}

fn with_idle_timeout<T>(
    stream: &mut Box<dyn ByteStream>,
    op: impl FnOnce(&mut dyn ByteStream) -> T,
) -> IdleOutcome<T> {
    let abort = match stream.try_clone() {
        Ok(clone) => Arc::new(std::sync::Mutex::new(Some(clone))),
        Err(error) => return IdleOutcome::CloneFailed(error),
    };
    let timed_out = Arc::new(AtomicBool::new(false));
    let abort_t = Arc::clone(&abort);
    let timed_out_t = Arc::clone(&timed_out);
    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
    let timer = std::thread::spawn(move || {
        if cancel_rx.recv_timeout(SESSION_IDLE).is_ok() {
            return;
        }
        if let Ok(mut guard) = abort_t.lock() {
            if let Some(mut clone) = guard.take() {
                let _ = clone.interrupt();
                timed_out_t.store(true, Ordering::SeqCst);
                // Drop cierra el handle duplicado sin DisconnectNamedPipe en Windows.
                drop(clone);
            }
        }
    });
    let result = op(stream.as_mut());
    let _ = cancel_tx.send(());
    if let Ok(mut guard) = abort.lock() {
        // Cierra la copia: CloseHandle / close(2), sin olvidar el descriptor.
        drop(guard.take());
    }
    let _ = timer.join();
    if timed_out.load(Ordering::SeqCst) {
        IdleOutcome::TimedOut
    } else {
        IdleOutcome::Done(result)
    }
}

fn wait_for_close<R: Read + ?Sized>(stream: &mut R) -> std::io::Result<()> {
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(0) => Ok(()),
        Ok(_) => Ok(()),
        Err(error) if closed(&error) || is_timeout(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn closed(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::UnexpectedEof
        || error.kind() == std::io::ErrorKind::ConnectionReset
        || error.kind() == std::io::ErrorKind::BrokenPipe
}

fn is_timeout(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::TimedOut || error.kind() == std::io::ErrorKind::WouldBlock
}

fn reject_large<W: std::io::Write + ?Sized>(
    stream: &mut W,
    log: &Logger,
    id: Option<u64>,
    len: u32,
) {
    let cause = format!("el mensaje declara {len} bytes y el máximo es {MAX_FRAME_LEN}");
    let action = "cierra esa conexión; el supervisor sigue en marcha";
    log.error("ipc", "mensaje rechazado", &cause, action);
    let _ = write_frame(
        stream,
        &error_response(id, -32002, "mensaje demasiado grande", &cause, action),
    );
}

fn parse(raw: &[u8]) -> std::io::Result<Value> {
    serde_json::from_slice(raw)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

fn request_id(value: &Value) -> Option<u64> {
    value.get("id").and_then(Value::as_u64)
}

fn request_method(value: &Value) -> &str {
    value.get("method").and_then(Value::as_str).unwrap_or("")
}

fn reply_error<W: std::io::Write + ?Sized>(
    stream: &mut W,
    id: Option<u64>,
    code: i64,
    message: &str,
    cause: &str,
    action: &str,
) -> std::io::Result<()> {
    write_frame(stream, &error_response(id, code, message, cause, action))?;
    Ok(())
}
