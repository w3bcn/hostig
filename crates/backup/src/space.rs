//! Espacio libre en disco (para mensajes de disco lleno).

use std::path::Path;

/// Bytes libres en el volumen que contiene `path`, si se puede consultar.
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        windows_free_bytes(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

#[cfg(windows)]
fn windows_free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free_for_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut free_total: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_for_caller),
            Some(&mut total),
            Some(&mut free_total),
        )
    };
    if ok.is_ok() {
        Some(free_for_caller)
    } else if let Some(parent) = path.parent() {
        if parent != path {
            windows_free_bytes(parent)
        } else {
            None
        }
    } else {
        None
    }
}
