use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tempfile::tempdir;

use crate::catalog::{Catalog, RuntimeEntry};
use crate::download::{download_to_file_with, sha256_file, DownloadTimeouts};
use crate::extract::extract_zip;
use crate::install::{clean_tmp_runtimes, runtime_dir, MANIFEST_NAME};
use crate::jobs::MAX_FINISHED_JOBS;
use crate::RuntimeService;

fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Cursor;
    let cursor = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in files {
        zip.start_file(*name, options).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

struct TestServer {
    base: String,
    hits: Arc<AtomicUsize>,
    _join: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn spawn(
        handler: impl Fn(&str, &Arc<AtomicUsize>) -> (u16, Vec<u8>) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_thread = Arc::clone(&hits);
        let handler = Arc::new(handler);
        let join = thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let req = String::from_utf8_lossy(&buf);
                let path = req
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let (status, body) = handler(&path, &hits_thread);
                let reason = if status == 200 { "OK" } else { "Error" };
                let header = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            hits,
            _join: Some(join),
        }
    }
}

fn entry_with_urls(urls: Vec<String>, sha256: &str, filename: &str) -> RuntimeEntry {
    RuntimeEntry {
        component: "fixture".into(),
        version: "1.0.0".into(),
        os: "windows".into(),
        arch: "x86_64".into(),
        urls,
        filename: filename.into(),
        sha256: sha256.into(),
        license: "Apache-2.0".into(),
        notes: "test".into(),
    }
}

#[test]
fn hash_incorrecto_no_deja_instalacion_ni_temporal() {
    let zip = make_zip(&[("bin/tool.txt", b"hola")]);
    let server = TestServer::spawn({
        let zip = zip.clone();
        move |path, hits| {
            hits.fetch_add(1, Ordering::SeqCst);
            if path == "/bad.zip" {
                (200, zip.clone())
            } else {
                (404, Vec::new())
            }
        }
    });
    let dir = tempdir().unwrap();
    let entry = entry_with_urls(
        vec![format!("{}/bad.zip", server.base)],
        &sha256_bytes(b"otro-contenido"),
        "bad.zip",
    );
    let catalog = Catalog {
        entries: vec![entry.clone()],
    };
    let service = RuntimeService::new(dir.path(), catalog);
    let err = service.install_entry_blocking(&entry).unwrap_err();
    assert!(err.message.contains("hash") || err.cause.contains("SHA-256"));
    let final_dir = runtime_dir(dir.path(), &entry);
    assert!(!final_dir.exists());
    let tmp = dir.path().join("tmp").join("runtimes");
    if tmp.exists() {
        let leftovers: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert!(leftovers.is_empty(), "quedaron temporales: {leftovers:?}");
    }
}

#[test]
fn zip_slip_se_rechaza_y_no_escribe_fuera() {
    let mut zip_bytes = Vec::new();
    {
        use std::io::Cursor;
        let cursor = Cursor::new(&mut zip_bytes);
        let mut zip = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        // enclosed_name de la crate zip ya filtra .. ; forzamos nombre crudo.
        zip.start_file("ok.txt", options).unwrap();
        zip.write_all(b"safe").unwrap();
        zip.start_file("../escape.txt", options).unwrap();
        zip.write_all(b"evil").unwrap();
        zip.finish().unwrap();
    }
    let dest = tempdir().unwrap();
    let zip_path = dest.path().join("slip.zip");
    std::fs::write(&zip_path, &zip_bytes).unwrap();
    let extract_to = dest.path().join("out");
    std::fs::create_dir_all(&extract_to).unwrap();
    let result = extract_zip(&zip_path, &extract_to);
    assert!(result.is_err(), "debía rechazar zip slip");
    assert!(!extract_to.join("escape.txt").exists());
    assert!(!dest.path().join("escape.txt").exists());
}

#[test]
fn descarga_cortada_no_deja_directorio_final() {
    let zip = make_zip(&[("a.txt", b"contenido")]);
    let server = TestServer::spawn({
        let zip = zip.clone();
        move |path, hits| {
            hits.fetch_add(1, Ordering::SeqCst);
            if path == "/cut.zip" {
                // Cuerpo más corto que Content-Length → cliente ve EOF prematuro.
                let header_body = zip[..zip.len() / 2].to_vec();
                (200, header_body)
            } else {
                (404, Vec::new())
            }
        }
    });
    // Para simular corte real: servidor que cierra tras enviar bytes parciales
    // con Content-Length mentiroso. Nuestro TestServer envía Content-Length = body.len(),
    // así que el hash fallará respecto al esperado del ZIP completo.
    let full_hash = sha256_bytes(&zip);
    let dir = tempdir().unwrap();
    let entry = entry_with_urls(
        vec![format!("{}/cut.zip", server.base)],
        &full_hash,
        "cut.zip",
    );
    let service = RuntimeService::new(
        dir.path(),
        Catalog {
            entries: vec![entry.clone()],
        },
    );
    let err = service.install_entry_blocking(&entry).unwrap_err();
    assert!(
        err.message.contains("hash")
            || err.message.contains("interrump")
            || err.cause.contains("SHA-256")
            || err.cause.contains("ZIP")
    );
    assert!(!runtime_dir(dir.path(), &entry).exists());
}

#[test]
fn primera_url_caida_usa_la_siguiente() {
    let zip = make_zip(&[("bin/ok.txt", b"payload")]);
    let hash = sha256_bytes(&zip);
    let server = TestServer::spawn({
        let zip = zip.clone();
        move |path, hits| {
            hits.fetch_add(1, Ordering::SeqCst);
            match path {
                "/down.zip" => (500, b"fail".to_vec()),
                "/ok.zip" => (200, zip.clone()),
                _ => (404, Vec::new()),
            }
        }
    });
    let dir = tempdir().unwrap();
    let entry = entry_with_urls(
        vec![
            format!("{}/down.zip", server.base),
            format!("{}/ok.zip", server.base),
        ],
        &hash,
        "ok.zip",
    );
    let service = RuntimeService::new(
        dir.path(),
        Catalog {
            entries: vec![entry.clone()],
        },
    );
    let path = service.install_entry_blocking(&entry).unwrap();
    assert!(path.join(MANIFEST_NAME).is_file());
    assert!(path.join("bin/ok.txt").is_file());
    assert!(server.hits.load(Ordering::SeqCst) >= 2);
}

#[test]
fn dos_instalaciones_simultaneas_una_sola_descarga() {
    let zip = make_zip(&[("x.bin", b"shared")]);
    let hash = sha256_bytes(&zip);
    let server = TestServer::spawn({
        let zip = zip.clone();
        move |path, hits| {
            if path == "/once.zip" {
                hits.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(80));
                (200, zip.clone())
            } else {
                (404, Vec::new())
            }
        }
    });
    let dir = tempdir().unwrap();
    let entry = entry_with_urls(vec![format!("{}/once.zip", server.base)], &hash, "once.zip");
    let service = Arc::new(RuntimeService::new(
        dir.path(),
        Catalog {
            entries: vec![entry.clone()],
        },
    ));
    let a = Arc::clone(&service);
    let b = Arc::clone(&service);
    let e1 = entry.clone();
    let e2 = entry.clone();
    let t1 = thread::spawn(move || a.install_entry_blocking(&e1));
    let t2 = thread::spawn(move || b.install_entry_blocking(&e2));
    let p1 = t1.join().unwrap().unwrap();
    let p2 = t2.join().unwrap().unwrap();
    assert_eq!(p1, p2);
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn verify_detecta_archivo_alterado() {
    let zip = make_zip(&[("keep.txt", b"original")]);
    let hash = sha256_bytes(&zip);
    let server = TestServer::spawn({
        let zip = zip.clone();
        move |path, hits| {
            hits.fetch_add(1, Ordering::SeqCst);
            if path == "/v.zip" {
                (200, zip.clone())
            } else {
                (404, Vec::new())
            }
        }
    });
    let dir = tempdir().unwrap();
    let entry = entry_with_urls(vec![format!("{}/v.zip", server.base)], &hash, "v.zip");
    let service = RuntimeService::new(
        dir.path(),
        Catalog {
            entries: vec![entry.clone()],
        },
    );
    let path = service.install_entry_blocking(&entry).unwrap();
    std::fs::write(path.join("keep.txt"), b"alterado").unwrap();
    let rows = service.verify(Some("fixture"), Some("1.0.0")).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].ok);
}

#[test]
fn catalogo_embebido_tiene_windows_x64() {
    let catalog = Catalog::embedded().unwrap();
    for name in ["apache", "php", "mariadb"] {
        assert!(
            catalog
                .entries
                .iter()
                .any(|e| e.component == name && e.os == "windows" && e.arch == "x86_64"),
            "falta {name} windows x86_64"
        );
    }
}

#[test]
fn sha256_file_coincide_con_bytes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"abc").unwrap();
    assert_eq!(sha256_file(&path).unwrap(), sha256_bytes(b"abc"));
    let _ = PathBuf::from("unused");
}

/// Servidor que gotea el cuerpo (continuo o se atasca a mitad).
fn spawn_drip_server(
    body: Vec<u8>,
    chunk: usize,
    pause: Duration,
    stall_after: Option<usize>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let mut sent = 0usize;
            while sent < body.len() {
                if let Some(limit) = stall_after {
                    if sent >= limit {
                        thread::sleep(Duration::from_secs(3600));
                        break;
                    }
                }
                let end = (sent + chunk).min(body.len());
                if stream.write_all(&body[sent..end]).is_err() {
                    break;
                }
                let _ = stream.flush();
                sent = end;
                thread::sleep(pause);
            }
        }
    });
    format!("http://127.0.0.1:{port}/file.bin")
}

#[test]
fn descarga_lenta_continua_no_se_corta_por_inactividad() {
    // 40 trozos × 80 ms ≈ 3,2 s > idle 500 ms: si el timeout de lectura de
    // reqwest se interpretara como tope total, esta descarga fallaría.
    let body = vec![b'x'; 40];
    let url = spawn_drip_server(body.clone(), 1, Duration::from_millis(80), None);
    let dir = tempdir().unwrap();
    let dest = dir.path().join("slow.bin");
    let timeouts = DownloadTimeouts {
        idle: Duration::from_millis(500),
        total: Duration::from_secs(30),
    };
    let started = std::time::Instant::now();
    let hash = download_to_file_with(&url, &dest, timeouts).unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed > timeouts.idle,
        "la descarga debería durar más que el idle ({elapsed:?} ≤ {:?})",
        timeouts.idle
    );
    assert_eq!(hash, sha256_bytes(&body));
    assert_eq!(std::fs::read(&dest).unwrap(), body);
}

#[test]
fn descarga_detenida_corta_por_inactividad() {
    let body = vec![b'y'; 100];
    let url = spawn_drip_server(body, 10, Duration::from_millis(20), Some(20));
    let dir = tempdir().unwrap();
    let dest = dir.path().join("stall.bin");
    let timeouts = DownloadTimeouts {
        idle: Duration::from_millis(300),
        total: Duration::from_secs(30),
    };
    let started = std::time::Instant::now();
    let err = download_to_file_with(&url, &dest, timeouts).unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        err.message.contains("progreso")
            || err.cause.contains("no llegaron bytes")
            || err.message.contains("interrumpida"),
        "{err}"
    );
    assert!(
        !err.cause.contains("hilo lector no terminó"),
        "el hilo lector debería haber terminado tras el corte: {err}"
    );
    // `download_to_file_with` solo vuelve tras join del lector; con timeout=idle
    // en el cliente, el join acaba poco después del corte (no queda en read eterno).
    assert!(
        elapsed < timeouts.idle.saturating_mul(4) + Duration::from_millis(500),
        "el corte tardó {elapsed:?}; el hilo lector debería terminar pronto"
    );
}

#[test]
fn limpia_tmp_runtimes_de_restos() {
    let dir = tempdir().unwrap();
    let tmp = dir.path().join("tmp").join("runtimes");
    std::fs::create_dir_all(tmp.join("half-extract")).unwrap();
    std::fs::write(tmp.join("half.zip"), b"x").unwrap();
    assert_eq!(clean_tmp_runtimes(dir.path()).unwrap(), 2);
    assert!(std::fs::read_dir(&tmp).unwrap().next().is_none());
}

#[test]
fn solo_conserva_las_ultimas_50_tareas_terminadas() {
    let server = TestServer::spawn(|path, hits| {
        hits.fetch_add(1, Ordering::SeqCst);
        let _ = path;
        (404, b"no".to_vec())
    });
    let mut entries = Vec::new();
    for i in 0..(MAX_FINISHED_JOBS + 5) {
        entries.push(RuntimeEntry {
            component: format!("c{i}"),
            version: "1.0.0".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
            urls: vec![format!("{}/missing-{i}.zip", server.base)],
            filename: format!("missing-{i}.zip"),
            sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            license: "Apache-2.0".into(),
            notes: String::new(),
        });
    }
    let dir = tempdir().unwrap();
    let service = RuntimeService::new(dir.path(), Catalog { entries });
    for i in 0..(MAX_FINISHED_JOBS + 5) {
        let _ = service.install_blocking(&format!("c{i}"), "1.0.0");
    }
    assert_eq!(service.finished_job_count(), MAX_FINISHED_JOBS);
    assert!(service.tracked_job_count() <= MAX_FINISHED_JOBS);
}
