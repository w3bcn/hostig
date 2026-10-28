//! Comprobación del runtime de Visual C++ antes del primer arranque (ADR 0002).

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{ProcessError, Result};

/// NTSTATUS: STATUS_DLL_NOT_FOUND.
pub const STATUS_DLL_NOT_FOUND: u32 = 0xC000_0135;
/// Win32: ERROR_MOD_NOT_FOUND.
pub const ERROR_MOD_NOT_FOUND: i32 = 126;

/// Ejecuta `{exe} -v` y traduce fallos por DLL ausente a un error de Hostig.
/// No lanza `vc_redist` (ADR 0002): solo diagnostica.
pub fn verify_binary_starts(exe: &Path, component: &str) -> Result<()> {
    if !exe.is_file() {
        return Err(ProcessError::new(
            "ejecutable ausente",
            format!("{} no existe", exe.display()),
            "instala el runtime con hostig runtimes install",
        ));
    }
    let mut command = Command::new(exe);
    command.arg("-v");
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    // PATH mínimo: carpeta del exe (DLL locales) + System32.
    if let Some(dir) = exe.parent() {
        let mut path = dir.display().to_string();
        #[cfg(windows)]
        {
            let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            path = format!("{path};{system_root}\\System32;{system_root}");
            command.env("SystemRoot", &system_root);
            command.env(
                "WINDIR",
                std::env::var("WINDIR").unwrap_or_else(|_| system_root.clone()),
            );
        }
        command.env("PATH", path);
    }
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            let code = status.code().unwrap_or(-1);
            if is_dll_not_found_exit(code) {
                return Err(dll_missing_error(component));
            }
            Err(ProcessError::new(
                format!("{component} no arranca"),
                format!("{} -v salió con código {code}", exe.display()),
                "revisa el runtime instalado o reinstálalo del catálogo",
            ))
        }
        Err(error) => {
            #[cfg(windows)]
            if error.raw_os_error() == Some(ERROR_MOD_NOT_FOUND) {
                return Err(dll_missing_error(component));
            }
            Err(ProcessError::new(
                format!("no se pudo ejecutar {component}"),
                format!("{}: {error}", exe.display()),
                "revisa la ruta del runtime y los permisos",
            ))
        }
    }
}

fn is_dll_not_found_exit(code: i32) -> bool {
    // En Windows el código de salida puede ser el NTSTATUS como u32 reinterpretado.
    if code as u32 == STATUS_DLL_NOT_FOUND {
        return true;
    }
    // Algunos entornos reportan el valor con signo.
    code == STATUS_DLL_NOT_FOUND as i32
}

fn dll_missing_error(component: &str) -> ProcessError {
    ProcessError::new(
        format!("falta el runtime de Visual C++ que necesita {component}"),
        format!("STATUS_DLL_NOT_FOUND (0x{STATUS_DLL_NOT_FOUND:08X}) al ejecutar {component}"),
        "instala el redistribuible VC++ adecuado en esta máquina, o espera al bloque 6 (DLL en la carpeta del runtime, ADR 0002); Hostig no lanza vc_redist",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconoce_codigo_dll_not_found() {
        assert!(is_dll_not_found_exit(STATUS_DLL_NOT_FOUND as i32));
        assert!(is_dll_not_found_exit(STATUS_DLL_NOT_FOUND as i32));
        assert!(!is_dll_not_found_exit(1));
    }
}
