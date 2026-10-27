//! Consulta de procesos del sistema (vivo / zombie / salido).

/// `true` solo si el proceso sigue en ejecución real.
///
/// Un zombie (Unix) o un proceso ya salido cuyo handle sigue abierto (Windows)
/// no cuenta como vivo: `kill(pid, 0)` / `OpenProcess` bastan para ver la
/// entrada, pero no para saber si aún corre.
pub fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        windows_process_is_running(pid)
    }
    #[cfg(target_os = "linux")]
    {
        linux_process_is_running(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_process_is_running(pid)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        // Otros Unix: sin detección de zombie específica.
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
    }
}

#[cfg(windows)]
fn windows_process_is_running(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE,
    };
    // STILL_ACTIVE (259): un proceso puede salir con código 259; por eso
    // cruzamos con WaitForSingleObject(0) — señalizado = ya terminó.
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let Ok(handle) = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        ) else {
            return false;
        };
        let wait = WaitForSingleObject(handle, 0);
        if wait == WAIT_OBJECT_0 {
            let _ = CloseHandle(handle);
            return false;
        }
        if wait != WAIT_TIMEOUT {
            // Acceso denegado u otro: no afirmamos que vive.
            let _ = CloseHandle(handle);
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        let _ = CloseHandle(handle);
        ok.is_ok() && code == STILL_ACTIVE
    }
}

#[cfg(target_os = "linux")]
fn linux_process_is_running(pid: u32) -> bool {
    // kill(0) responde OK también para zombies; mirar el estado en /proc.
    if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err() {
        return false;
    }
    !linux_is_zombie(pid)
}

#[cfg(target_os = "linux")]
fn linux_is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Formato: pid (comm) state ... — comm puede tener espacios y ')'.
    let Some(close) = stat.rfind(')') else {
        return false;
    };
    let rest = stat[close + 1..].trim_start();
    rest.starts_with('Z')
}

#[cfg(target_os = "macos")]
fn macos_process_is_running(pid: u32) -> bool {
    matches!(macos_proc_state(pid), MacosProcState::Running { .. })
}

/// Resultado de sysctl(KERN_PROC_PID) → kp_proc.p_stat (vía helper C con kinfo_proc).
#[cfg(target_os = "macos")]
#[derive(Clone)]
enum MacosProcState {
    Running { p_stat: i8 },
    Zombie { p_stat: i8 },
    Gone,
    QueryFailed { errno: i32 },
}

#[cfg(target_os = "macos")]
impl std::fmt::Debug for MacosProcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running { p_stat } => write!(f, "Running {{ p_stat={p_stat} }}"),
            Self::Zombie { p_stat } => write!(f, "Zombie {{ p_stat={p_stat} }}"),
            Self::Gone => write!(f, "Gone"),
            Self::QueryFailed { errno } => write!(f, "QueryFailed {{ errno={errno} }}"),
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_sysctl {
    extern "C" {
        pub fn hostig_macos_proc_p_stat(
            pid: libc::c_int,
            out_stat: *mut libc::c_char,
        ) -> libc::c_int;
        pub fn hostig_macos_proc_start_ms(pid: libc::c_int, out_ms: *mut u64) -> libc::c_int;
    }
}

#[cfg(target_os = "macos")]
fn macos_proc_state(pid: u32) -> MacosProcState {
    // libc no exporta kinfo_proc en Apple; el helper C usa <sys/sysctl.h>
    // con sysctl(CTL_KERN, KERN_PROC, KERN_PROC_PID, pid) y kp_proc.p_stat.
    let mut p_stat: libc::c_char = 0;
    let rc = unsafe { macos_sysctl::hostig_macos_proc_p_stat(pid as libc::c_int, &mut p_stat) };
    if rc == 0 {
        let p_stat = p_stat as i8;
        // SZOMB de <sys/proc.h>, también en libc::SZOMB.
        if p_stat as u32 == libc::SZOMB {
            return MacosProcState::Zombie { p_stat };
        }
        return MacosProcState::Running { p_stat };
    }
    let err = std::io::Error::last_os_error();
    let errno = err.raw_os_error().unwrap_or(0);
    if errno == libc::ESRCH {
        return MacosProcState::Gone;
    }
    eprintln!(
        "{{\"level\":\"error\",\"module\":\"ipc\",\"message\":\"sysctl KERN_PROC_PID falló\",\"cause\":\"pid={pid} errno={errno} ({err})\",\"action\":\"no se afirma que el proceso viva; revisa permisos o el estado del kernel\"}}"
    );
    MacosProcState::QueryFailed { errno }
}

/// Hora de arranque del proceso en epoch ms (sysctl → p_starttime). Solo macOS.
#[cfg(target_os = "macos")]
pub fn macos_process_start_ms(pid: u32) -> Option<u64> {
    let mut ms = 0u64;
    let rc = unsafe { macos_sysctl::hostig_macos_proc_start_ms(pid as libc::c_int, &mut ms) };
    if rc == 0 {
        Some(ms)
    } else {
        None
    }
}

/// Detalle de liveness para mensajes de test / diagnóstico.
pub fn process_liveness_detail(pid: u32) -> String {
    if pid == 0 {
        return "pid=0".into();
    }
    #[cfg(target_os = "macos")]
    {
        format!("{:?}", macos_proc_state(pid))
    }
    #[cfg(target_os = "linux")]
    {
        let kill_ok = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok();
        let zombie = linux_is_zombie(pid);
        format!("linux kill_ok={kill_ok} zombie={zombie}")
    }
    #[cfg(windows)]
    {
        format!(
            "windows process_is_running={}",
            windows_process_is_running(pid)
        )
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        format!("running={}", process_is_running(pid))
    }
}

/// Descriptores (Unix) o handles (Windows) abiertos por el proceso.
pub fn process_open_resources(pid: u32) -> std::io::Result<u32> {
    #[cfg(target_os = "linux")]
    {
        let dir = std::fs::read_dir(format!("/proc/{pid}/fd"))?;
        Ok(dir.count() as u32)
    }
    #[cfg(target_os = "macos")]
    {
        macos_fd_count(pid)
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let mut count = 0u32;
            let result = GetProcessHandleCount(handle, &mut count);
            let _ = CloseHandle(handle);
            result.map_err(|error| std::io::Error::other(error.to_string()))?;
            Ok(count)
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        Err(std::io::Error::other(
            "conteo de recursos no disponible en este sistema",
        ))
    }
}

#[cfg(target_os = "macos")]
fn macos_fd_count(pid: u32) -> std::io::Result<u32> {
    // PROC_PIDLISTFDS = 1; cada entrada mide 8 bytes (proc_fdinfo).
    const PROC_PIDLISTFDS: i32 = 1;
    const FDINFO_SIZE: usize = 8;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let written = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                PROC_PIDLISTFDS,
                0,
                buf.as_mut_ptr().cast(),
                buf.len() as i32,
            )
        };
        if written <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written = written as usize;
        if written < buf.len() {
            return Ok((written / FDINFO_SIZE) as u32);
        }
        buf.resize(buf.len() * 2, 0);
    }
}

/// Restringe el archivo al usuario actual (y SYSTEM en Windows). Misma política que `auth.token`.
#[cfg(unix)]
pub fn restrict_to_user_and_system(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(windows)]
pub fn restrict_to_user_and_system(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    let sid = current_user_sid()?;
    let sddl = format!("D:P(A;;FA;;;SY)(A;;FA;;;{sid})");
    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(io_err)?;
        let path_wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let result = SetFileSecurityW(
            PCWSTR(path_wide.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        );
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        result.ok().map_err(io_err)?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn current_user_sid() -> std::io::Result<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::TOKEN_QUERY;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(io_err)?;
        let sid = sid_of_token(token);
        let _ = CloseHandle(token);
        sid
    }
}

#[cfg(windows)]
pub fn process_user_sid(pid: u32) -> std::io::Result<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::TOKEN_QUERY;
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(io_err)?;
        let mut token = windows::Win32::Foundation::HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        let _ = CloseHandle(process);
        opened.map_err(io_err)?;
        let sid = sid_of_token(token);
        let _ = CloseHandle(token);
        sid
    }
}

#[cfg(windows)]
fn sid_of_token(token: windows::Win32::Foundation::HANDLE) -> std::io::Result<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::LocalFree;
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_USER};
    unsafe {
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return Err(std::io::Error::other(
                "GetTokenInformation no devolvió tamaño",
            ));
        }
        let mut buffer = vec![0u8; needed as usize];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut _),
            needed,
            &mut needed,
        )
        .map_err(io_err)?;
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(io_err)?;
        let sid = text
            .to_string()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(text.0 as *mut _)));
        Ok(sid)
    }
}

#[cfg(windows)]
pub fn pipe_sddl() -> std::io::Result<String> {
    let sid = current_user_sid()?;
    Ok(format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"))
}

#[cfg(windows)]
fn io_err(error: windows::core::Error) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::process_is_running;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    #[cfg(windows)]
    #[test]
    fn la_dacl_del_pipe_es_el_usuario_y_system() {
        let sddl = super::pipe_sddl().unwrap();
        assert!(sddl.contains("(A;;GA;;;SY)"));
        assert!(sddl.contains(&super::current_user_sid().unwrap()));
        assert!(!sddl.contains("WD"));
        assert!(!sddl.contains("AU"));
    }

    /// Un hijo que termina y no se recoge (zombie / handle abierto) no cuenta como vivo.
    #[test]
    fn proceso_terminado_sin_reap_no_cuenta_como_vivo() {
        use super::process_liveness_detail;

        let mut child = if cfg!(windows) {
            Command::new("cmd")
                .args(["/C", "exit", "0"])
                .spawn()
                .expect("spawn")
        } else {
            Command::new("true").spawn().expect("spawn")
        };
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !process_is_running(pid) {
                // No llamamos a wait/reap todavía: el criterio debe valer igual.
                let detail = process_liveness_detail(pid);
                assert!(
                    !process_is_running(pid),
                    "pid {pid} no debe contar como vivo tras salir; consulta={detail}"
                );
                let _ = child.wait();
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let detail = process_liveness_detail(pid);
        let _ = child.wait();
        panic!("pid {pid} seguía contando como vivo tras salir; consulta del sistema: {detail}");
    }

    #[test]
    fn pid_cero_no_esta_vivo() {
        assert!(!process_is_running(0));
    }
}
