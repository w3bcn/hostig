//! Falso mariadbd para tests del bloque 3b.
//!
//! Uso:
//!   fake-mariadbd --defaults-file=PATH [--hostig-script=PATH]
//!
//! El guion (por defecto junto al error log como `hostig-fake.script`) tiene líneas:
//!   DELAY_MS TEXTO...     escribe TEXTO en el error log tras DELAY_MS
//!   LISTEN                abre TCP en el puerto del my.ini
//!   EXIT N                termina con código N
//!   HANG                  duerme para siempre (hasta kill)

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let defaults = args
        .iter()
        .find_map(|a| a.strip_prefix("--defaults-file=").map(PathBuf::from));
    let script_arg = args
        .iter()
        .find_map(|a| a.strip_prefix("--hostig-script=").map(PathBuf::from));

    let Some(defaults) = defaults else {
        eprintln!("fake-mariadbd: falta --defaults-file");
        std::process::exit(2);
    };
    let ini = fs::read_to_string(&defaults).unwrap_or_default();
    let port = ini_value(&ini, "port")
        .and_then(|s| s.parse().ok())
        .unwrap_or(3306);
    let error_log = ini_value(&ini, "log-error")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("error.log"));
    if let Some(parent) = error_log.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let script_path = script_arg.unwrap_or_else(|| {
        error_log
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("hostig-fake.script")
    });
    let script = fs::read_to_string(&script_path).unwrap_or_else(|_| {
        "0 fake-mariadbd started\n100 ready for connections.\nLISTEN\nHANG\n".into()
    });

    let mut _listener: Option<TcpListener> = None;
    for raw in script.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.eq_ignore_ascii_case("LISTEN") {
            match TcpListener::bind(("127.0.0.1", port)) {
                Ok(l) => {
                    l.set_nonblocking(true).ok();
                    // Hilo que acepta y descarta para que el connect del probe funcione.
                    let l2 = l.try_clone().expect("clone listener");
                    thread::spawn(move || loop {
                        if let Ok((stream, _)) = l2.accept() {
                            drop(stream);
                        } else {
                            thread::sleep(Duration::from_millis(20));
                        }
                    });
                    _listener = Some(l);
                    append_log(&error_log, &format!("listening on {port}"));
                }
                Err(error) => append_log(&error_log, &format!("LISTEN failed: {error}")),
            }
            continue;
        }
        if line.eq_ignore_ascii_case("HANG") {
            let stop_flag = error_log
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("hostig-fake.stop");
            loop {
                if stop_flag.is_file() {
                    let _ = fs::remove_file(&stop_flag);
                    append_log(&error_log, "Shutdown complete");
                    std::process::exit(0);
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        // EXIT [code] o DELAY_MS EXIT [code]
        if let Some(rest) = line.strip_prefix("EXIT ") {
            let code: i32 = rest.trim().parse().unwrap_or(1);
            std::process::exit(code);
        }
        if line.eq_ignore_ascii_case("EXIT") {
            std::process::exit(1);
        }
        // DELAY_MS [rest...] — si rest es EXIT, sale tras la espera.
        let mut parts = line.splitn(2, char::is_whitespace);
        let delay: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let text = parts.next().unwrap_or("").trim();
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay));
        }
        if text.eq_ignore_ascii_case("EXIT") || text.to_ascii_uppercase().starts_with("EXIT ") {
            let code = text
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            std::process::exit(code);
        }
        if !text.is_empty() {
            append_log(&error_log, text);
        }
    }
    // Si el guion termina sin HANG/EXIT, quedamos vivos escuchando.
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn ini_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case(key) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn append_log(path: &Path, line: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}
