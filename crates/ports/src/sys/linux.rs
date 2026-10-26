use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use super::{is_relevant_addr, FoundListener};
use crate::error::{PortError, Result};

const LISTEN: u8 = 0x0a;

pub fn list_relevant_listeners() -> Result<Vec<FoundListener>> {
    let mut rows = Vec::new();
    rows.extend(parse_proc_net("/proc/net/tcp", false)?);
    rows.extend(parse_proc_net("/proc/net/tcp6", true)?);

    let inode_to_pid = map_socket_inodes();
    let mut out = Vec::new();
    for (port, addr, inode) in rows {
        if !is_relevant_addr(addr) {
            continue;
        }
        let pid = inode_to_pid.get(&inode).copied();
        out.push(FoundListener { port, addr, pid });
    }
    Ok(out)
}

pub fn process_name_and_path(pid: u32) -> (String, Option<PathBuf>) {
    let path = PathBuf::from(format!("/proc/{pid}/exe"));
    match fs::read_link(&path) {
        Ok(target) => {
            let name = target
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("pid-{pid}"));
            (name, Some(target))
        }
        Err(_) => {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let name = if comm.is_empty() {
                format!("pid-{pid}")
            } else {
                comm
            };
            (name, None)
        }
    }
}

fn parse_proc_net(path: &str, v6: bool) -> Result<Vec<(u16, IpAddr, u64)>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(PortError::new(
                "no se pudo leer la tabla TCP",
                format!("{path}: {error}"),
                "comprueba que /proc está montado",
            ));
        }
    };
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 10 {
            continue;
        }
        let state = u8::from_str_radix(cols[3], 16).unwrap_or(0);
        if state != LISTEN {
            continue;
        }
        let inode: u64 = cols[9].parse().unwrap_or(0);
        if inode == 0 {
            continue;
        }
        if let Some((addr, port)) = if v6 {
            parse_local_v6(cols[1])
        } else {
            parse_local_v4(cols[1])
        } {
            out.push((port, addr, inode));
        }
    }
    Ok(out)
}

fn parse_local_v4(field: &str) -> Option<(IpAddr, u16)> {
    let (addr_s, port_s) = field.split_once(':')?;
    let addr_n = u32::from_str_radix(addr_s, 16).ok()?;
    let port = u16::from_str_radix(port_s, 16).ok()?;
    let bytes = addr_n.to_le_bytes();
    Some((IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])), port))
}

fn parse_local_v6(field: &str) -> Option<(IpAddr, u16)> {
    let (addr_s, port_s) = field.split_once(':')?;
    if addr_s.len() != 32 {
        return None;
    }
    let port = u16::from_str_radix(port_s, 16).ok()?;
    let mut bytes = [0u8; 16];
    for (i, chunk) in addr_s.as_bytes().chunks(8).enumerate() {
        let hex = std::str::from_utf8(chunk).ok()?;
        let word = u32::from_str_radix(hex, 16).ok()?;
        let le = word.to_le_bytes();
        bytes[i * 4..i * 4 + 4].copy_from_slice(&le);
    }
    Some((IpAddr::V6(Ipv6Addr::from(bytes)), port))
}

fn map_socket_inodes() -> std::collections::HashMap<u64, u32> {
    let mut map = std::collections::HashMap::new();
    let Ok(proc) = fs::read_dir("/proc") else {
        return map;
    };
    for entry in proc.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(link) = fs::read_link(fd.path()) else {
                continue;
            };
            if let Some(inode) = parse_socket_inode(&link) {
                map.entry(inode).or_insert(pid);
            }
        }
    }
    map
}

fn parse_socket_inode(path: &Path) -> Option<u64> {
    let s = path.to_str()?;
    let rest = s.strip_prefix("socket:[")?.strip_suffix(']')?;
    rest.parse().ok()
}
