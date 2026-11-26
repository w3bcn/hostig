mod lock;
mod log;
mod os_events;
mod server;

use hostig_ipc::Layout;

use lock::Lock;
use log::Logger;

pub use log::Logger as SupervisorLog;

/// Arranca el supervisor, o sale enseguida si ya hay uno con el mismo candado.
pub fn run() -> i32 {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => {
            report(
                "no se pudo resolver la carpeta de datos",
                &error.to_string(),
                "define HOSTIG_DATA_DIR o revisa LOCALAPPDATA / XDG_DATA_HOME",
            );
            return 1;
        }
    };
    if let Err(error) = layout.ensure() {
        report(
            "no se pudo crear la carpeta de datos",
            &error.to_string(),
            "comprueba permisos de esa carpeta",
        );
        return 1;
    }
    let log = match Logger::open(&layout.log_dir, 1024 * 1024, 3) {
        Ok(log) => log,
        Err(error) => {
            report(
                "no se pudo abrir el log",
                &error.to_string(),
                "comprueba permisos de la carpeta logs",
            );
            return 1;
        }
    };
    match Lock::try_acquire(&layout.lock_path) {
        Ok(None) => {
            log.info(
                "supervisor",
                "ya hay un supervisor; este proceso no crea otro",
            );
            0
        }
        Ok(Some(lock)) => match server::serve(&layout, &log, lock) {
            Ok(()) => 0,
            Err(error) => {
                let message = error.to_string();
                let (cause, action) = if message.contains("ruta del socket mide") {
                    (
                        message,
                        "usa una carpeta de datos con una ruta más corta".to_string(),
                    )
                } else {
                    (message, "mira este log y vuelve a arrancar".to_string())
                };
                log.error("supervisor", "el supervisor se detuvo", &cause, &action);
                1
            }
        },
        Err(error) => {
            log.error(
                "supervisor",
                "no se pudo tomar el candado",
                &error.to_string(),
                "cierra el otro proceso o borra un supervisor.lock huérfano",
            );
            1
        }
    }
}

fn report(message: &str, cause: &str, action: &str) {
    let line = serde_json::json!({
        "level": "error",
        "module": "supervisor",
        "message": message,
        "cause": cause,
        "action": action,
    });
    eprintln!("{line}");
}
