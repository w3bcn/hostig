//! Ayudas de Windows para los tests del apagado del sistema (ADR 0011).
//!
//! La ventana oculta del supervisor se busca por su nombre de clase y por el PID
//! dueño, para no tocar la de otro supervisor que corra en paralelo.

use std::thread;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, SendMessageTimeoutW, SMTO_ABORTIFHUNG,
};

pub fn find_shutdown_window(pid: u32, budget: Duration) -> HWND {
    let deadline = Instant::now() + budget;
    loop {
        let mut after: Option<HWND> = None;
        loop {
            // SAFETY: búsqueda de ventanas de nivel superior por clase.
            let found = unsafe {
                FindWindowExW(
                    None,
                    after,
                    w!("HostigSupervisorShutdownWindow"),
                    PCWSTR::null(),
                )
            };
            let Ok(hwnd) = found else {
                break;
            };
            if hwnd.is_invalid() {
                break;
            }
            let mut owner = 0u32;
            // SAFETY: hwnd viene de FindWindowExW.
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
            if owner == pid {
                return hwnd;
            }
            after = Some(hwnd);
        }
        if Instant::now() >= deadline {
            panic!("no apareció la ventana de apagado del supervisor {pid}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Envía un mensaje y espera la respuesta del WindowProc. `None` si venció el plazo.
pub fn send(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
    timeout: Duration,
) -> Option<usize> {
    let mut result = 0usize;
    // SAFETY: hwnd es una ventana válida; result vive durante la llamada.
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            msg,
            WPARAM(wparam),
            LPARAM(lparam),
            SMTO_ABORTIFHUNG,
            u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX),
            Some(&mut result),
        )
    };
    if ok.0 == 0 {
        None
    } else {
        Some(result)
    }
}
