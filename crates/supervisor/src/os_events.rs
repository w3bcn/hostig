//! Avisos del sistema operativo: apagado, cierre de sesión y suspensión (ADR 0011).
//!
//! - Windows: ventana de nivel superior oculta con su bucle de mensajes en un hilo
//!   propio (`WM_QUERYENDSESSION` / `WM_ENDSESSION`, `WTS_SESSION_LOGOFF`) y
//!   `RegisterSuspendResumeNotification` con `DEVICE_NOTIFY_CALLBACK`.
//! - Unix: `SIGTERM` entra en la misma parada rápida.
//!
//! La parada rápida no vuelca la base, tiene un tope de 20 s y nunca mata a MariaDB.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hostig_ipc::{connect_raw, Layout};
use hostig_processes::{StackService, SystemStopReport, SYSTEM_SHUTDOWN_BUDGET};

use crate::log::Logger;

/// Objetivo de la parada rápida. Pasarse no es un error, pero queda en el log.
const SYSTEM_SHUTDOWN_TARGET: Duration = Duration::from_secs(5);

/// Nombre de clase de la ventana oculta. Los tests de Windows la buscan por este nombre.
#[cfg_attr(not(windows), allow(dead_code))]
pub const SHUTDOWN_WINDOW_CLASS: &str = "HostigSupervisorShutdownWindow";

/// Estado compartido entre el hilo de avisos y el servidor.
pub(crate) struct SystemEvents {
    stack: Arc<StackService>,
    log: Logger,
    shutdown: Arc<AtomicBool>,
    layout: Layout,
    /// `Some` cuando la parada rápida ya se hizo: un segundo aviso no la repite.
    done: Mutex<Option<bool>>,
}

impl SystemEvents {
    pub(crate) fn new(
        stack: Arc<StackService>,
        log: Logger,
        shutdown: Arc<AtomicBool>,
        layout: Layout,
    ) -> Arc<Self> {
        Arc::new(Self {
            stack,
            log,
            shutdown,
            layout,
            done: Mutex::new(None),
        })
    }

    /// Parada rápida sin volcado. Síncrona: en Windows tiene que acabar antes de
    /// que `WM_ENDSESSION` vuelva. Devuelve si MariaDB quedó parada a tiempo.
    pub(crate) fn fast_stop(&self, reason: &str) -> bool {
        // El candado se mantiene durante toda la parada: un segundo aviso espera
        // a que acabe la primera en lugar de volver antes de tiempo.
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ok) = *done {
            return ok;
        }
        self.log.info(
            "apagado",
            &format!(
                "{reason}: parada rápida sin volcado (objetivo {} s, tope {} s)",
                SYSTEM_SHUTDOWN_TARGET.as_secs(),
                SYSTEM_SHUTDOWN_BUDGET.as_secs()
            ),
        );
        let report = self.stack.system_shutdown_stop(SYSTEM_SHUTDOWN_BUDGET);
        self.log_report(&report);
        let ok = report.mariadb_ok();
        *done = Some(ok);
        ok
    }

    /// Pide al servidor que salga: marca y despierta el accept.
    pub(crate) fn request_exit(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = connect_raw(&self.layout);
    }

    /// Como `request_exit`, pero desde otro hilo y con un margen, para que el
    /// WindowProc conteste a `WM_ENDSESSION` antes de que el proceso termine.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn request_exit_soon(self: &Arc<Self>) {
        let events = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("hostig-salida".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                events.request_exit();
            });
    }

    fn log_report(&self, report: &SystemStopReport) {
        for step in &report.steps {
            if !step.attempted {
                self.log
                    .info("apagado", &format!("{}: ya estaba parado", step.service));
                continue;
            }
            match &step.error {
                None => self.log.info(
                    "apagado",
                    &format!("{}: parado en {} ms", step.service, step.elapsed_ms),
                ),
                Some(error) => self.log.error(
                    "apagado",
                    &format!(
                        "{}: no paró a tiempo ({} ms): {}",
                        step.service, step.elapsed_ms, error.message
                    ),
                    &error.cause,
                    if step.service == "mariadb" {
                        "MariaDB sigue con su parada ordenada; no se le hace kill. Si el sistema la corta, el arranque siguiente pasa por recuperación (ADR 0003)"
                    } else {
                        "se sigue con el resto de servicios"
                    },
                ),
            }
        }
        let total = format!(
            "parada rápida terminada en {} ms (tope {} ms)",
            report.total_ms, report.budget_ms
        );
        if report.total_ms > SYSTEM_SHUTDOWN_TARGET.as_millis() {
            self.log.info(
                "apagado",
                &format!(
                    "{total}; por encima del objetivo de {} s",
                    SYSTEM_SHUTDOWN_TARGET.as_secs()
                ),
            );
        } else {
            self.log.info("apagado", &total);
        }
    }

    /// Suspensión: no se para nada ni se vuelca (ADR 0011).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn on_suspend(&self, source: &str) {
        self.log.info(
            "suspensión",
            &format!(
                "{source}: el equipo se suspende; no se para ningún servicio ({})",
                self.states()
            ),
        );
    }

    /// Reanudación: el health check de cada servicio sigue en su vigilante; aquí
    /// se deja constancia del estado. Como en la suspensión no se paró nada, no
    /// hay nada que volver a arrancar.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn on_resume(&self, source: &str) {
        self.log.info(
            "suspensión",
            &format!(
                "{source}: el equipo se reanuda; estado tras la reanudación: {}",
                self.states()
            ),
        );
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn states(&self) -> String {
        let status = self.stack.status();
        let state = |value: &serde_json::Value| {
            value
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("desconocido")
                .to_string()
        };
        format!(
            "mariadb={} php={} apache={}",
            state(&status.mariadb),
            state(&status.php),
            state(&status.apache)
        )
    }
}

/// Paso previo, antes de crear ningún hilo. En Unix bloquea `SIGTERM` para que
/// solo lo reciba el hilo de avisos (`sigwait`). Los hijos no heredan la máscara:
/// `std::process::Command` la limpia antes del `exec`.
pub(crate) fn prepare() -> std::io::Result<()> {
    platform::prepare()
}

/// Arranca la escucha de avisos del sistema.
pub(crate) fn install(events: Arc<SystemEvents>) -> std::io::Result<()> {
    platform::install(events)
}

#[cfg(unix)]
mod platform {
    use std::sync::Arc;

    use nix::sys::signal::{SigSet, SigmaskHow, Signal};

    use super::SystemEvents;

    fn set() -> SigSet {
        let mut set = SigSet::empty();
        set.add(Signal::SIGTERM);
        set
    }

    pub(super) fn prepare() -> std::io::Result<()> {
        nix::sys::signal::pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set()), None)
            .map_err(std::io::Error::from)
    }

    pub(super) fn install(events: Arc<SystemEvents>) -> std::io::Result<()> {
        std::thread::Builder::new()
            .name("hostig-señales".into())
            .spawn(move || loop {
                match set().wait() {
                    Ok(Signal::SIGTERM) => {
                        events.fast_stop("SIGTERM");
                        events.request_exit();
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        events.log.error(
                            "apagado",
                            "no se pudo esperar a SIGTERM",
                            &error.to_string(),
                            "el apagado del sistema no tendrá parada rápida; usa hostig stop antes de apagar",
                        );
                        return;
                    }
                }
            })
            .map(|_| ())
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::sync::mpsc;
    use std::sync::{Arc, OnceLock};

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Power::{
        RegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
    };
    use windows::Win32::System::RemoteDesktop::{
        WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
    };
    use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
        TranslateMessage, DEVICE_NOTIFY_CALLBACK, ENDSESSION_LOGOFF, MSG, PBT_APMRESUMEAUTOMATIC,
        PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, WINDOW_EX_STYLE, WM_ENDSESSION, WM_POWERBROADCAST,
        WM_QUERYENDSESSION, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_OVERLAPPED, WTS_SESSION_LOGOFF,
    };

    use super::SystemEvents;

    static EVENTS: OnceLock<Arc<SystemEvents>> = OnceLock::new();

    pub(super) fn prepare() -> std::io::Result<()> {
        Ok(())
    }

    pub(super) fn install(events: Arc<SystemEvents>) -> std::io::Result<()> {
        if EVENTS.set(events).is_err() {
            return Err(std::io::Error::other(
                "los avisos del sistema ya estaban instalados",
            ));
        }
        // La ventana y su bucle viven en un hilo dedicado: el WindowProc puede
        // quedarse hasta 20 s en la parada rápida sin bloquear el servidor.
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("hostig-ventana-apagado".into())
            .spawn(move || {
                // SAFETY: llamadas Win32 sobre una ventana de este mismo hilo.
                let hwnd = match unsafe { create_window() } {
                    Ok(hwnd) => hwnd,
                    Err(error) => {
                        let _ = tx.send(Err(error.to_string()));
                        return;
                    }
                };
                // SAFETY: hwnd es una ventana válida creada en este hilo.
                if let Err(error) =
                    unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }
                {
                    if let Some(events) = EVENTS.get() {
                        events.log.error(
                            "apagado",
                            "no se pudo registrar el aviso de cierre de sesión (WTS)",
                            &error.to_string(),
                            "el cierre de sesión sigue llegando por WM_ENDSESSION",
                        );
                    }
                }
                let _ = tx.send(Ok(()));
                // SAFETY: bucle de mensajes estándar del hilo dueño de la ventana.
                unsafe { message_loop() };
            })?;
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(cause)) => return Err(std::io::Error::other(cause)),
            Err(_) => {
                return Err(std::io::Error::other(
                    "el hilo de la ventana de apagado terminó antes de tiempo",
                ))
            }
        }
        // SAFETY: los parámetros quedan en memoria estática hasta el final del proceso.
        unsafe { register_power() }
    }

    unsafe fn create_window() -> windows::core::Result<HWND> {
        let module = unsafe { GetModuleHandleW(PCWSTR::null())? };
        let instance = HINSTANCE::from(module);
        let class_name: Vec<u16> = super::SHUTDOWN_WINDOW_CLASS
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // Se filtra a propósito: la clase vive lo que vive el proceso.
        let class_name = PCWSTR(Box::leak(class_name.into_boxed_slice()).as_ptr());
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(windows::core::Error::from_win32());
        }
        // Ventana de nivel superior (no HWND_MESSAGE) que nunca se muestra: así
        // recibe WM_QUERYENDSESSION y WM_ENDSESSION (ADR 0011).
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                w!("Hostig supervisor"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        }
    }

    unsafe fn message_loop() {
        let mut msg = MSG::default();
        loop {
            let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if got.0 <= 0 {
                return;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
        }
    }

    unsafe fn register_power() -> std::io::Result<()> {
        let params: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS =
            Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(power_callback),
                Context: std::ptr::null_mut(),
            }));
        let recipient = HANDLE(params as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as *mut c_void);
        match unsafe { RegisterSuspendResumeNotification(recipient, DEVICE_NOTIFY_CALLBACK) } {
            // El registro dura lo que dura el proceso; no se desregistra.
            Ok(_handle) => Ok(()),
            Err(error) => {
                if let Some(events) = EVENTS.get() {
                    events.log.error(
                        "suspensión",
                        "no se pudo registrar el aviso de suspensión",
                        &error.to_string(),
                        "la suspensión no para nada igualmente; solo falta el registro en el log",
                    );
                }
                Ok(())
            }
        }
    }

    fn handle_power(kind: u32, source: &str) {
        let Some(events) = EVENTS.get() else {
            return;
        };
        match kind {
            PBT_APMSUSPEND => events.on_suspend(source),
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => events.on_resume(source),
            _ => {}
        }
    }

    unsafe extern "system" fn power_callback(
        _context: *const c_void,
        kind: u32,
        _setting: *const c_void,
    ) -> u32 {
        handle_power(kind, "RegisterSuspendResumeNotification");
        0
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_QUERYENDSESSION => {
                // Se bloquea el apagado con un motivo visible mientras dura la parada.
                let _ = unsafe {
                    ShutdownBlockReasonCreate(
                        hwnd,
                        w!("Hostig está deteniendo MariaDB de forma ordenada"),
                    )
                };
                if let Some(events) = EVENTS.get() {
                    events.log.info(
                        "apagado",
                        &format!("WM_QUERYENDSESSION recibido ({})", kind(lparam)),
                    );
                }
                LRESULT(1)
            }
            WM_ENDSESSION => {
                if let Some(events) = EVENTS.get() {
                    if wparam.0 != 0 {
                        events.fast_stop(&format!("WM_ENDSESSION ({})", kind(lparam)));
                        events.request_exit_soon();
                    } else {
                        events.log.info(
                            "apagado",
                            "el apagado o el cierre de sesión se canceló; todo sigue en marcha",
                        );
                    }
                }
                let _ = unsafe { ShutdownBlockReasonDestroy(hwnd) };
                LRESULT(0)
            }
            WM_WTSSESSION_CHANGE => {
                if wparam.0 as u32 == WTS_SESSION_LOGOFF {
                    if let Some(events) = EVENTS.get() {
                        let _ = unsafe {
                            ShutdownBlockReasonCreate(
                                hwnd,
                                w!("Hostig está deteniendo MariaDB de forma ordenada"),
                            )
                        };
                        events.fast_stop("WTS_SESSION_LOGOFF (cierre de sesión)");
                        events.request_exit_soon();
                        let _ = unsafe { ShutdownBlockReasonDestroy(hwnd) };
                    }
                }
                LRESULT(0)
            }
            WM_POWERBROADCAST => {
                // El camino principal es el callback; la ventana de nivel superior
                // también recibe la difusión y lleva al mismo manejador.
                handle_power(wparam.0 as u32, "WM_POWERBROADCAST");
                LRESULT(1)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn kind(lparam: LPARAM) -> &'static str {
        if (lparam.0 as u32) & ENDSESSION_LOGOFF != 0 {
            "cierre de sesión"
        } else {
            "apagado del sistema"
        }
    }
}
