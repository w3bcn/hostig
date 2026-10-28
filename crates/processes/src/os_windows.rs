use std::path::PathBuf;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_BREAK_EVENT,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

use crate::error::{ProcessError, Result};

pub fn process_creation_time(pid: u32) -> Option<u64> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut creation = windows::Win32::Foundation::FILETIME::default();
        let mut exit = windows::Win32::Foundation::FILETIME::default();
        let mut kernel = windows::Win32::Foundation::FILETIME::default();
        let mut user = windows::Win32::Foundation::FILETIME::default();
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(handle);
        ok.ok()?;
        Some(filetime_to_u64(creation))
    }
}

pub fn process_image_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )))
    }
}

pub fn terminate_process(pid: u32) -> Result<()> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
        .map_err(|error| {
            ProcessError::new(
                "no se pudo abrir el proceso",
                error.to_string(),
                "cita un PID que aún exista",
            )
        })?;
        let result = TerminateProcess(handle, 1);
        let _ = CloseHandle(handle);
        result.map_err(|error| {
            ProcessError::new(
                "TerminateProcess falló",
                error.to_string(),
                "revisa permisos; esto es solo el camino de emergencia",
            )
        })?;
    }
    Ok(())
}

/// Envía `CTRL_BREAK_EVENT` al grupo de proceso (PID del líder con
/// `CREATE_NEW_PROCESS_GROUP`). Usado para apagar httpd de consola (ADR 0003).
pub fn send_ctrl_break(process_group_pid: u32) -> Result<()> {
    unsafe {
        // Adjuntar a la consola del objetivo (si la tiene) o al grupo.
        let _ = FreeConsole();
        if AttachConsole(process_group_pid).is_err() {
            // Sin consola compartida: GenerateConsoleCtrlEvent al pgid sigue
            // siendo la vía documentada para procesos CREATE_NEW_PROCESS_GROUP.
        }
        let _ = SetConsoleCtrlHandler(None, true);
        let result = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_group_pid);
        let _ = SetConsoleCtrlHandler(None, false);
        let _ = FreeConsole();
        result.map_err(|error| {
            ProcessError::new(
                "no se pudo enviar ctrl-break",
                format!("pid/grupo {process_group_pid}: {error}"),
                "reintenta hostig stop; no se usa taskkill por nombre",
            )
        })?;
    }
    Ok(())
}

fn filetime_to_u64(ft: windows::Win32::Foundation::FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

#[allow(dead_code)]
fn _handle_keepalive(_: HANDLE) {}
