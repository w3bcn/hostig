use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, NO_ERROR, WIN32_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6TABLE_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
    TCP_TABLE_OWNER_PID_LISTENER,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::{is_relevant_addr, FoundListener};
use crate::error::{PortError, Result};

pub fn list_relevant_listeners() -> Result<Vec<FoundListener>> {
    let mut out = Vec::new();
    out.extend(ipv4_listeners()?);
    out.extend(ipv6_listeners()?);
    Ok(out)
}

pub fn process_name_and_path(pid: u32) -> (String, Option<PathBuf>) {
    match query_image_path(pid) {
        Some(path) => {
            let name = Path::new(&path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("pid-{pid}"));
            (name, Some(PathBuf::from(path)))
        }
        None => (format!("pid-{pid}"), None),
    }
}

fn ipv4_listeners() -> Result<Vec<FoundListener>> {
    let buf = tcp_table(AF_INET.0 as u32)?;
    let table = unsafe { &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID) };
    let count = table.dwNumEntries as usize;
    let rows = unsafe { std::slice::from_raw_parts(table.table.as_ptr(), count) };
    let mut out = Vec::new();
    for row in rows {
        let port = port_from_dword(row.dwLocalPort);
        // dwLocalAddr viene en orden de red.
        let addr = IpAddr::V4(Ipv4Addr::from(u32::from_be(row.dwLocalAddr)));
        if !is_relevant_addr(addr) {
            continue;
        }
        let pid = row.dwOwningPid;
        out.push(FoundListener {
            port,
            addr,
            pid: if pid == 0 { None } else { Some(pid) },
        });
    }
    Ok(out)
}

fn ipv6_listeners() -> Result<Vec<FoundListener>> {
    let buf = tcp_table(AF_INET6.0 as u32)?;
    let table = unsafe { &*(buf.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID) };
    let count = table.dwNumEntries as usize;
    let rows = unsafe { std::slice::from_raw_parts(table.table.as_ptr(), count) };
    let mut out = Vec::new();
    for row in rows {
        let port = port_from_dword(row.dwLocalPort);
        let octets = row.ucLocalAddr;
        let addr = IpAddr::V6(Ipv6Addr::from(octets));
        if !is_relevant_addr(addr) {
            continue;
        }
        let pid = row.dwOwningPid;
        out.push(FoundListener {
            port,
            addr,
            pid: if pid == 0 { None } else { Some(pid) },
        });
    }
    Ok(out)
}

fn port_from_dword(value: u32) -> u16 {
    u16::from_be((value & 0xffff) as u16)
}

fn tcp_table(family: u32) -> Result<Vec<u8>> {
    let mut size = 0u32;
    let first = unsafe {
        GetExtendedTcpTable(
            None,
            &mut size,
            false,
            family,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if WIN32_ERROR(first) != ERROR_INSUFFICIENT_BUFFER && WIN32_ERROR(first) != NO_ERROR {
        return Err(PortError::new(
            "no se pudo leer la tabla TCP",
            format!("GetExtendedTcpTable (tamaño) devolvió {first}"),
            "reintenta; si persiste, revisa permisos del proceso",
        ));
    }
    let mut buf = vec![0u8; size as usize];
    let mut attempts = 0;
    loop {
        let code = unsafe {
            GetExtendedTcpTable(
                Some(buf.as_mut_ptr().cast()),
                &mut size,
                false,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if WIN32_ERROR(code) == NO_ERROR {
            buf.truncate(size as usize);
            return Ok(buf);
        }
        if WIN32_ERROR(code) == ERROR_INSUFFICIENT_BUFFER {
            buf.resize(size as usize, 0);
            attempts += 1;
            if attempts > 8 {
                break;
            }
            continue;
        }
        return Err(PortError::new(
            "no se pudo leer la tabla TCP",
            format!("GetExtendedTcpTable devolvió {code}"),
            "reintenta; si persiste, revisa permisos del proceso",
        ));
    }
    Err(PortError::new(
        "no se pudo leer la tabla TCP",
        "GetExtendedTcpTable no estabilizó el tamaño del búfer",
        "reintenta la comprobación de puertos",
    ))
}

fn query_image_path(pid: u32) -> Option<String> {
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
        if result.is_err() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}
