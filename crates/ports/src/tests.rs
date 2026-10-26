use std::io;
use std::net::{Ipv6Addr, TcpListener};
use std::time::Duration;

use crate::probe::classify_bind_denied;
use crate::ranges::{self, HTTP};
use crate::{check, suggest, PortState};

fn hold_listener(addr: &str) -> (TcpListener, u16) {
    let listener = TcpListener::bind(addr).expect("bind de prueba");
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[test]
fn listener_propio_se_detecta_ocupado_con_pid_y_ejecutable() {
    let (_listener, port) = hold_listener("127.0.0.1:0");
    // Dar tiempo a que el SO registre el listener en la tabla.
    std::thread::sleep(Duration::from_millis(50));
    let state = check(port, None).expect("check");
    match state {
        PortState::Ocupado { pid, name, path } => {
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
    // Reservamos un puerto efímero y lo soltamos: debería quedar libre.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    std::thread::sleep(Duration::from_millis(50));
    let state = check(port, None).expect("check");
    assert_eq!(state, PortState::Libre, "el puerto {port} debería estar libre");
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
fn suggest_salta_puertos_ocupados() {
    // Ocupamos un puerto libre del rango HTTP y pedimos suggest desde él.
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
    let error = io::Error::from_raw_os_error(if cfg!(windows) { 10013 } else { 13 });
    assert_eq!(classify_bind_denied(&error), Some(PortState::Reservado));

    let other = io::Error::from_raw_os_error(if cfg!(windows) { 10048 } else { 98 });
    assert_eq!(classify_bind_denied(&other), None);
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
