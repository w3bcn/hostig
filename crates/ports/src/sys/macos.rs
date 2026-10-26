//! Listeners TCP vía libproc (`proc_listpids`, `proc_pidfdinfo`).
//!
//! Layouts alineados con las estructuras de XNU (`sys/proc_info.h`). Si no
//! podemos leer los fds de un proceso, no afirmamos que el puerto esté libre:
//! el probe de bind del caller cubre ese caso como dueño no visible.

use std::ffi::{c_char, c_int, c_longlong, c_short, c_uchar, c_uint, c_ushort, c_void};
use std::mem::{self, MaybeUninit};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use super::{is_relevant_addr, FoundListener};
use crate::error::{PortError, Result};

const PROC_ALL_PIDS: u32 = 1;
const PROC_PIDLISTFDS: c_int = 1;
const PROC_PIDFDSOCKETINFO: c_int = 3;
const PROX_FDTYPE_SOCKET: u32 = 2;
const IPPROTO_TCP: c_int = 6;
const AF_INET: c_int = 2;
const AF_INET6: c_int = 30;
const INI_IPV4: u8 = 0x1;
const INI_IPV6: u8 = 0x2;
const TCPS_LISTEN: c_int = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcFdInfo {
    proc_fd: i32,
    proc_fdtype: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VinfoStat {
    vst_dev: u32,
    vst_mode: u16,
    vst_nlink: u16,
    vst_ino: u64,
    vst_uid: c_uint,
    vst_gid: c_uint,
    vst_atime: i64,
    vst_atimensec: i64,
    vst_mtime: i64,
    vst_mtimensec: i64,
    vst_ctime: i64,
    vst_ctimensec: i64,
    vst_birthtime: i64,
    vst_birthtimensec: i64,
    vst_size: c_longlong,
    vst_blocks: i64,
    vst_blksize: i32,
    vst_flags: u32,
    vst_gen: u32,
    vst_rdev: u32,
    vst_qspare: [i64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockbufInfo {
    sbi_cc: u32,
    sbi_hiwat: u32,
    sbi_mbcnt: u32,
    sbi_mbmax: u32,
    sbi_lowat: u32,
    sbi_flags: c_short,
    sbi_timeo: c_short,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InAddr {
    s_addr: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct In4in6Addr {
    i46a_pad32: [c_uint; 3],
    i46a_addr4: InAddr,
}

#[repr(C)]
#[derive(Clone, Copy)]
union In6AddrBytes {
    __u6_addr8: [c_uchar; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct In6Addr {
    __u6_addr: In6AddrBytes,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InSockAddr {
    ina_46: In4in6Addr,
    ina_6: In6Addr,
    _align: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InSockinfoV4 {
    in4_tos: c_uchar,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InSockinfoV6 {
    in6_hlim: u8,
    in6_cksum: c_int,
    in6_ifindex: c_ushort,
    in6_hops: c_short,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InSockinfo {
    insi_fport: c_int,
    insi_lport: c_int,
    insi_gencnt: u64,
    insi_flags: u32,
    insi_flow: u32,
    insi_vflag: u8,
    insi_ip_ttl: u8,
    rfu_1: u32,
    insi_faddr: InSockAddr,
    insi_laddr: InSockAddr,
    insi_v4: InSockinfoV4,
    insi_v6: InSockinfoV6,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TcpSockinfo {
    tcpsi_ini: InSockinfo,
    tcpsi_state: c_int,
    tcpsi_timer: [c_int; 4],
    tcpsi_mss: c_int,
    tcpsi_flags: u32,
    rfu_1: u32,
    tcpsi_tp: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
union Softc {
    pri_in: InSockinfo,
    pri_tcp: TcpSockinfo,
    _align: [u64; 66],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SocketInfo {
    soi_stat: VinfoStat,
    soi_so: u64,
    soi_pcb: u64,
    soi_type: c_int,
    soi_protocol: c_int,
    soi_family: c_int,
    soi_options: c_short,
    soi_linger: c_short,
    soi_state: c_short,
    soi_qlen: c_short,
    soi_incqlen: c_short,
    soi_qlimit: c_short,
    soi_timeo: c_short,
    soi_error: c_ushort,
    soi_oobmark: u32,
    soi_rcv: SockbufInfo,
    soi_snd: SockbufInfo,
    soi_kind: c_int,
    rfu_1: u32,
    soi_proto: Softc,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcFileinfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: c_longlong,
    fi_type: i32,
    fi_guardflags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SocketFdInfo {
    pfi: ProcFileinfo,
    psi: SocketInfo,
}

#[link(name = "proc")]
extern "C" {
    fn proc_listpids(type_: u32, typeinfo: u32, buffer: *mut c_void, buffersize: c_int) -> c_int;
    fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
    fn proc_pidfdinfo(
        pid: c_int,
        fd: c_int,
        flavor: c_int,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
}

pub fn list_relevant_listeners() -> Result<Vec<FoundListener>> {
    let pids = list_pids()?;
    let mut out = Vec::new();
    for pid in pids {
        if let Ok(list) = listeners_for_pid(pid) {
            out.extend(list);
        }
    }
    Ok(out)
}

pub fn process_name_and_path(pid: u32) -> (String, Option<PathBuf>) {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { proc_pidpath(pid as c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return (format!("pid-{pid}"), None);
    }
    let path = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
    let name = PathBuf::from(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("pid-{pid}"));
    (name, Some(PathBuf::from(path)))
}

fn list_pids() -> Result<Vec<u32>> {
    let bytes = unsafe { proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Err(PortError::new(
            "no se pudo listar procesos",
            "proc_listpids falló",
            "reintenta la comprobación de puertos",
        ));
    }
    let count = (bytes as usize) / mem::size_of::<c_int>();
    let mut buf = vec![0i32; count];
    let got = unsafe {
        proc_listpids(
            PROC_ALL_PIDS,
            0,
            buf.as_mut_ptr().cast(),
            (buf.len() * mem::size_of::<c_int>()) as c_int,
        )
    };
    if got <= 0 {
        return Err(PortError::new(
            "no se pudo listar procesos",
            "proc_listpids (relleno) falló",
            "reintenta la comprobación de puertos",
        ));
    }
    let n = (got as usize) / mem::size_of::<c_int>();
    Ok(buf[..n]
        .iter()
        .copied()
        .filter(|p| *p > 0)
        .map(|p| p as u32)
        .collect())
}

fn listeners_for_pid(pid: u32) -> std::result::Result<Vec<FoundListener>, ()> {
    let size_hint =
        unsafe { proc_pidinfo(pid as c_int, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if size_hint <= 0 {
        return Err(());
    }
    let count = (size_hint as usize) / mem::size_of::<ProcFdInfo>();
    let mut fds = vec![
        ProcFdInfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        count
    ];
    let got = unsafe {
        proc_pidinfo(
            pid as c_int,
            PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (fds.len() * mem::size_of::<ProcFdInfo>()) as c_int,
        )
    };
    if got <= 0 {
        return Err(());
    }
    let n = (got as usize) / mem::size_of::<ProcFdInfo>();
    let mut out = Vec::new();
    for fd in &fds[..n] {
        if fd.proc_fdtype != PROX_FDTYPE_SOCKET {
            continue;
        }
        if let Some(listener) = socket_listener(pid, fd.proc_fd) {
            out.push(listener);
        }
    }
    Ok(out)
}

fn socket_listener(pid: u32, fd: i32) -> Option<FoundListener> {
    let mut info = MaybeUninit::<SocketFdInfo>::uninit();
    let rc = unsafe {
        proc_pidfdinfo(
            pid as c_int,
            fd,
            PROC_PIDFDSOCKETINFO,
            info.as_mut_ptr().cast(),
            mem::size_of::<SocketFdInfo>() as c_int,
        )
    };
    if rc <= 0 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    if info.psi.soi_protocol != IPPROTO_TCP {
        return None;
    }
    let tcp = unsafe { info.psi.soi_proto.pri_tcp };
    if tcp.tcpsi_state != TCPS_LISTEN {
        return None;
    }
    // `insi_lport` en orden de red; en LE los dos primeros bytes LE son el puerto BE.
    let le = tcp.tcpsi_ini.insi_lport.to_le_bytes();
    let port = u16::from_be_bytes([le[0], le[1]]);
    let addr = local_addr(info.psi.soi_family, &tcp.tcpsi_ini)?;
    if !is_relevant_addr(addr) {
        return None;
    }
    Some(FoundListener {
        port,
        addr,
        pid: Some(pid),
    })
}

fn local_addr(family: c_int, sock: &InSockinfo) -> Option<IpAddr> {
    match family {
        AF_INET => {
            let raw = unsafe { sock.insi_laddr.ina_46.i46a_addr4.s_addr };
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(raw))))
        }
        AF_INET6 => {
            let vflag = sock.insi_vflag;
            if vflag & INI_IPV6 != 0 {
                let bytes = unsafe { sock.insi_laddr.ina_6.__u6_addr.__u6_addr8 };
                Some(IpAddr::V6(Ipv6Addr::from(bytes)))
            } else if vflag & INI_IPV4 != 0 {
                let raw = unsafe { sock.insi_laddr.ina_46.i46a_addr4.s_addr };
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(raw))))
            } else {
                let bytes = unsafe { sock.insi_laddr.ina_6.__u6_addr.__u6_addr8 };
                Some(IpAddr::V6(Ipv6Addr::from(bytes)))
            }
        }
        _ => None,
    }
}

// Silencia avisos de campos no leídos en layouts FFI.
#[allow(dead_code)]
fn _ffi_keepalive(_: c_char) {}
