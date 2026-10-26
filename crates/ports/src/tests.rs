use std::io::{Read, Write};
use std::net::{Ipv6Addr, TcpListener};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::probe::{classify_bind_denied, reserved_cause_unix, reserved_cause_windows};
use crate::ranges::{self, HTTP};
use crate::{check, suggest, PortState};

fn hold_listener(addr: &str) -> (TcpListener, u16) {
    let listener = TcpListener::bind(addr).expect("bind de prueba");
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// Si este proceso es el hijo del test multi-listener, retiene ::1:puerto y no vuelve.
fn maybe_become_ipv6_holder() {
    let Ok(port_s) = std::env::var("HOSTIG_PORTS_TEST_HOLD") else {
        return;
    };
    let port: u16 = port_s.parse().expect("puerto HOLD inválido");
    let listener = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("HOSTIG_PORTS_TEST_HOLD: no se pudo bind ::1:{port}: {error}");
            std::process::exit(2);
        }
    };
    std::mem::forget(listener);
    println!("HOSTIG_PORTS_TEST_READY");
    let _ = std::io::stdout().flush();
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn listener_propio_se_detecta_ocupado_con_pid_y_ejecutable() {
    let (_listener, port) = hold_listener("127.0.0.1:0");
    std::thread::sleep(Duration::from_millis(50));
    let state = check(port, None).expect("check");
    match state {
        PortState::Ocupado {
            pid, name, path, ..
        } => {
            assert_eq!(pid, std::process::id());
            assert!(!name.is_empty(), "nombre vacío");
            if let Some(path) = path {
                assert!(
                    path.exists() || path.to_string_lossy().contains("hostig"),
                    "ruta inesperada: {}",
                    path.display()
                );
            }
        }
        other => panic!("se esperaba Ocupado, fue {other:?}"),
    }
}

#[test]
fn con_pid_esperado_no_hay_conflicto() {
    let (_listener, port) = hold_listener("127.0.0.1:0");
    std::thread::sleep(Duration::from_millis(50));
    let state = check(port, Some(std::process::id())).expect("check");
    assert_eq!(state, PortState::Libre);
}

#[test]
fn puerto_libre_se_detecta_libre() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    std::thread::sleep(Duration::from_millis(50));
    let state = check(port, None).expect("check");
    assert_eq!(
        state,
        PortState::Libre,
        "el puerto {port} debería estar libre"
    );
}

#[test]
fn listener_solo_ipv6_se_detecta() {
    let listener = match TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) {
        Ok(l) => l,
        Err(error) => {
            eprintln!("IPv6 no disponible en este entorno: {error}; se omite el test");
            return;
        }
    };
    let port = listener.local_addr().unwrap().port();
    std::thread::sleep(Duration::from_millis(50));
    let state = check(port, None).expect("check");
    match state {
        PortState::Ocupado { pid, .. } => assert_eq!(pid, std::process::id()),
        other => panic!("se esperaba Ocupado en ::1, fue {other:?}"),
    }
    drop(listener);
}

#[test]
fn dos_listeners_distinto_pid_con_esperado_parcial_es_conflicto() {
    maybe_become_ipv6_holder();

    // Comprobar IPv6 antes de abrir el IPv4 (si falla, omitimos con mensaje claro).
    let probe_v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, 0));
    let Ok(probe_v6) = probe_v6 else {
        eprintln!(
            "IPv6 no disponible en este entorno; se omite el test de listeners duales con pids distintos"
        );
        return;
    };
    drop(probe_v6);

    let listener_v4 = TcpListener::bind("127.0.0.1:0").expect("bind IPv4");
    let port = listener_v4.local_addr().unwrap().port();

    let exe = std::env::current_exe().expect("exe de test");
    let mut child = Command::new(&exe)
        .arg("--exact")
        .arg("tests::dos_listeners_distinto_pid_con_esperado_parcial_es_conflicto")
        // Sin --nocapture, libtest captura stdout y READY no llega al pipe.
        .arg("--nocapture")
        .env("HOSTIG_PORTS_TEST_HOLD", port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hijo IPv6");

    let mut stdout = child.stdout.take().expect("stdout hijo");
    let ready = wait_for_ready(&mut stdout, Duration::from_secs(5));
    if !ready {
        let _ = child.kill();
        let err = {
            let mut s = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut s);
            }
            s
        };
        let _ = child.wait();
        panic!("el hijo no señaló READY a tiempo; stderr={err}");
    }

    std::thread::sleep(Duration::from_millis(80));
    let state = check(port, Some(std::process::id())).expect("check");
    assert!(
        state.is_conflict(),
        "con un listener ajeno en el mismo puerto debe haber conflicto, fue {state:?}"
    );
    match &state {
        PortState::Ocupado { pid, .. } => {
            assert_ne!(
                *pid,
                std::process::id(),
                "el conflicto debe atribuirse al pid del otro listener"
            );
        }
        PortState::OcupadoDueñoNoVisible => {}
        other => panic!("se esperaba Ocupado (u dueño no visible), fue {other:?}"),
    }

    let _ = child.kill();
    let _ = child.wait();
    drop(listener_v4);
}

fn wait_for_ready(stdout: &mut impl Read, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64];
    while Instant::now() < deadline {
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if String::from_utf8_lossy(&buf).contains("HOSTIG_PORTS_TEST_READY") {
                    return true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    false
}

#[test]
fn suggest_salta_puertos_ocupados() {
    let preferred = suggest(*HTTP.start(), HTTP).expect("encontrar base libre");
    let _hold = TcpListener::bind(("127.0.0.1", preferred)).expect("ocupar preferido");
    std::thread::sleep(Duration::from_millis(50));
    let suggested = suggest(preferred, HTTP).expect("suggest");
    assert_ne!(suggested, preferred);
    assert!(HTTP.contains(&suggested));
    assert_eq!(check(suggested, None).unwrap(), PortState::Libre);
}

#[test]
fn bind_con_acceso_denegado_se_clasifica_como_reservado() {
    let error = std::io::Error::from_raw_os_error(if cfg!(windows) { 10013 } else { 13 });
    assert_eq!(classify_bind_denied(&error), Some(PortState::Reservado));

    let other = std::io::Error::from_raw_os_error(if cfg!(windows) { 10048 } else { 98 });
    assert_eq!(classify_bind_denied(&other), None);
}

#[test]
fn mensajes_de_reservado_segun_sistema() {
    let port = 8080u16;
    let win = reserved_cause_windows(port);
    let unix = reserved_cause_unix(port);
    assert!(
        win.contains("Hyper-V, WSL o Docker"),
        "causa Windows inesperada: {win}"
    );
    assert!(
        unix.contains("sin privilegios") && unix.contains("1024"),
        "causa Unix inesperada: {unix}"
    );
    assert_ne!(win, unix);
}

#[test]
fn puertos_privilegiados_se_rechazan() {
    for port in [80u16, 443, 53] {
        let err = check(port, None).expect_err("privilegiado");
        assert!(err.message.contains("privilegiado"), "{err}");
        assert!(err.action.contains("helper") || err.action.contains("puerto alto"));
        assert!(ranges::is_privileged(port));
    }
}

#[test]
fn modulo_implementado() {
    assert!(crate::implemented());
}

#[cfg(windows)]
#[test]
fn pid_4_se_presenta_como_http_sys() {
    // No hace falta un listener real: ejercitamos el formateo del ocupante.
    let info = crate::occupant_info(4);
    assert_eq!(info.name, "System (HTTP.sys)");
    assert!(
        info.action.as_deref().unwrap_or("").contains("IIS, WinRM"),
        "{:?}",
        info.action
    );
}
