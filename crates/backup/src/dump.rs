//! Ejecución de mariadb-dump, validación y retención.

use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::error::{BackupError, Result};
use crate::meta::{
    backup_dir, latest_good_meta, meta_path_for, needs_dump, now_unix, save_meta, BackupMeta,
    BackupSettings, ServerSnapshot, SYSTEM_SCHEMAS,
};
use crate::space;

/// Contexto para volcar una instancia en marcha.
#[derive(Debug, Clone)]
pub struct DumpContext {
    pub data_dir: PathBuf,
    pub instance_id: String,
    pub admin_cnf: PathBuf,
    pub port: u16,
    /// Directorio `bin` del runtime MariaDB (mariadb / mariadb-dump).
    pub bin_dir: PathBuf,
    pub settings: BackupSettings,
    /// Identificador de arranque estable (p. ej. creation time del proceso).
    /// Si falta, se deriva de `now − Uptime`.
    pub boot_id_unix: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BackupEntry {
    pub schema: String,
    pub path: PathBuf,
    pub created_unix: u64,
    pub size_bytes: u64,
}

fn exe_in(bin: &Path, name: &str) -> Option<PathBuf> {
    let with_exe = bin.join(format!("{name}.exe"));
    if with_exe.is_file() {
        return Some(with_exe);
    }
    let plain = bin.join(name);
    if plain.is_file() {
        return Some(plain);
    }
    None
}

fn defaults_extra_arg(admin_cnf: &Path) -> String {
    format!("--defaults-extra-file={}", admin_cnf.display())
}

fn run_sql(ctx: &DumpContext, sql: &str) -> Result<String> {
    let client = exe_in(&ctx.bin_dir, "mariadb").ok_or_else(|| {
        BackupError::new(
            "cliente mariadb ausente",
            ctx.bin_dir.display().to_string(),
            "reinstala MariaDB del catálogo",
        )
    })?;
    if !ctx.admin_cnf.is_file() {
        return Err(BackupError::new(
            "credenciales admin ausentes",
            ctx.admin_cnf.display().to_string(),
            "arranca la instancia al menos una vez",
        ));
    }
    let output = Command::new(&client)
        .arg(defaults_extra_arg(&ctx.admin_cnf))
        .arg("--protocol=tcp")
        .arg("-h")
        .arg("127.0.0.1")
        .arg("-P")
        .arg(ctx.port.to_string())
        .arg("-N")
        .arg("-B")
        .arg("-e")
        .arg(sql)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            BackupError::new(
                "no se pudo consultar MariaDB",
                error.to_string(),
                "comprueba que el servidor responde al ping",
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BackupError::new(
            "consulta SQL fallida",
            stderr.trim().to_string(),
            "comprueba que el servidor está en marcha y admin.cnf es válido",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Esquemas de usuario (excluye mysql/sys/information_schema/performance_schema).
pub fn list_user_schemas(ctx: &DumpContext) -> Result<Vec<String>> {
    let out = run_sql(ctx, "SHOW DATABASES")?;
    let mut schemas = Vec::new();
    for line in out.lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        if SYSTEM_SCHEMAS.iter().any(|s| s.eq_ignore_ascii_case(name)) {
            continue;
        }
        schemas.push(name.to_string());
    }
    schemas.sort();
    Ok(schemas)
}

fn parse_status_map(text: &str) -> std::collections::BTreeMap<String, u64> {
    let mut map = std::collections::BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        let Some(value) = parts.next() else {
            continue;
        };
        if let Ok(n) = value.parse::<u64>() {
            map.insert(name.to_string(), n);
        }
    }
    map
}

/// Instantánea de arranque + Com_* + motores no InnoDB.
pub fn collect_server_snapshot(ctx: &DumpContext) -> Result<ServerSnapshot> {
    let mut names = vec!["Uptime".to_string()];
    for v in crate::meta::write_status_vars() {
        names.push(v.to_string());
    }
    let in_list = names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(",");
    let status_sql = format!("SHOW GLOBAL STATUS WHERE Variable_name IN ({in_list})");
    let status_out = run_sql(ctx, &status_sql)?;
    let status = parse_status_map(&status_out);
    let uptime = status.get("Uptime").copied().unwrap_or(0);
    let now = now_unix();
    let boot_id_unix = ctx
        .boot_id_unix
        .unwrap_or_else(|| now.saturating_sub(uptime));

    // Solo contadores que esa versión expone; ausentes no se inventan a 0
    // (si desaparecen respecto a la copia previa, needs_dump vuelca por duda).
    let mut com_counters = std::collections::BTreeMap::new();
    for v in crate::meta::write_status_vars() {
        if let Some(n) = status.get(v).copied() {
            com_counters.insert(v.to_string(), n);
        }
    }

    let excluded = SYSTEM_SCHEMAS
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let engine_sql = format!(
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema NOT IN ({excluded}) \
         AND table_type = 'BASE TABLE' \
         AND engine IS NOT NULL \
         AND UPPER(engine) <> 'INNODB'"
    );
    let engine_out = run_sql(ctx, &engine_sql)?;
    let has_non_innodb = engine_out
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<u64>().ok())
        .unwrap_or(1)
        > 0;

    Ok(ServerSnapshot {
        boot_id_unix,
        uptime_secs: uptime,
        com_counters,
        has_non_innodb,
    })
}

fn estimate_schema_bytes(ctx: &DumpContext, schema: &str) -> u64 {
    let sql = format!(
        "SELECT COALESCE(SUM(data_length + index_length), 0) \
         FROM information_schema.tables WHERE table_schema = '{schema}'"
    );
    run_sql(ctx, &sql)
        .ok()
        .and_then(|o| o.lines().next().map(|l| l.trim().parse::<u64>().ok()))
        .flatten()
        .unwrap_or(0)
}

fn timestamp_name(now: SystemTime) -> String {
    let dur = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    // UTC YYYYMMDDTHHMMSS.mmmZ (millis evita colisión en el mismo segundo)
    let days = secs / 86400;
    let tod = secs % 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let hh = tod / 3600;
    let mm = (tod % 3600) / 60;
    let ss = tod % 60;
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}.{millis:03}Z")
}

/// Algoritmo civil_from_days (Howard Hinnant), días desde 1970-01-01.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

/// Comprueba cabecera y pie de mariadb-dump en el gzip.
pub fn validate_dump_gz(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    let mut decoder = GzDecoder::new(file);
    let mut text = String::new();
    // Leer completo: los volcados de test son pequeños; si falla, se descarta.
    decoder.read_to_string(&mut text).map_err(|error| {
        BackupError::new(
            "volcado gzip ilegible",
            error.to_string(),
            "borra el fichero parcial y reintenta el stop",
        )
    })?;
    let lower = text.to_ascii_lowercase();
    let has_header = lower.contains("mariadb dump") || lower.contains("mysql dump");
    let has_footer = lower.contains("dump completed");
    if !has_header || !has_footer {
        return Err(BackupError::new(
            "volcado incompleto o inválido",
            format!("cabecera={has_header} pie={has_footer}"),
            "reintenta el stop; si persiste, revisa el error de mariadb-dump",
        ));
    }
    Ok(())
}

fn dump_one_schema(ctx: &DumpContext, schema: &str, snap: &ServerSnapshot) -> Result<PathBuf> {
    let dump = exe_in(&ctx.bin_dir, "mariadb-dump").ok_or_else(|| {
        BackupError::new(
            "mariadb-dump ausente",
            ctx.bin_dir.display().to_string(),
            "reinstala MariaDB del catálogo",
        )
    })?;
    let dir = backup_dir(&ctx.data_dir, &ctx.instance_id, schema);
    fs::create_dir_all(&dir).map_err(|error| {
        disk_or_perm_error(
            &dir,
            estimate_schema_bytes(ctx, schema),
            error,
            "crear carpeta de copias",
        )
    })?;

    // Comprobar escritura anticipada.
    let probe = dir.join(".hostig-write-probe");
    if let Err(error) = fs::write(&probe, b"ok") {
        return Err(disk_or_perm_error(
            &dir,
            estimate_schema_bytes(ctx, schema),
            error,
            "escribir en la carpeta de copias",
        ));
    }
    let _ = fs::remove_file(&probe);

    let needed = estimate_schema_bytes(ctx, schema)
        .saturating_mul(2)
        .max(1024 * 1024);
    if let Some(free) = space::free_bytes(&dir) {
        if free < needed {
            return Err(BackupError::new(
                "disco lleno para el volcado",
                format!(
                    "hacen falta ~{} bytes libres en {} (hay {free})",
                    needed,
                    dir.display()
                ),
                format!(
                    "libera espacio o usa hostig stop --without-backup (confirma con {})",
                    crate::WITHOUT_BACKUP_CONFIRM
                ),
            ));
        }
    }

    // TMP propio: en Windows, --routines/--events crean tablas temporales y
    // fallan si varios mariadbd comparten %TEMP% (CI en paralelo).
    let dump_tmp = ctx.data_dir.join("tmp").join("dump").join(&ctx.instance_id);
    fs::create_dir_all(&dump_tmp)
        .map_err(|error| disk_or_perm_error(&dump_tmp, needed, error, "crear TMP de volcado"))?;

    let mut last_err = None;
    for attempt in 1..=3u32 {
        let stamp = timestamp_name(SystemTime::now());
        let final_path = dir.join(format!("{stamp}.sql.gz"));
        let tmp_path = dir.join(format!("{stamp}.sql.gz.partial"));
        let _ = fs::remove_file(&tmp_path);
        match run_mariadb_dump_once(ctx, &dump, schema, &tmp_path, &dump_tmp, needed, &dir) {
            Ok(()) => match validate_dump_gz(&tmp_path) {
                Ok(()) => {
                    fs::rename(&tmp_path, &final_path).map_err(|error| {
                        BackupError::new(
                            "no se pudo fijar el volcado",
                            error.to_string(),
                            "revisa permisos en la carpeta de copias",
                        )
                    })?;
                    let created = now_unix();
                    let meta = BackupMeta::from_snapshot(schema, snap, created);
                    let meta_path = meta_path_for(&final_path);
                    if let Err(error) = save_meta(&meta_path, &meta) {
                        let _ = fs::remove_file(&final_path);
                        return Err(error);
                    }
                    apply_retention(&dir, ctx.settings.retention)?;
                    return Ok(final_path);
                }
                Err(error) => {
                    let _ = fs::remove_file(&tmp_path);
                    last_err = Some(error);
                }
            },
            Err(error) => {
                let _ = fs::remove_file(&tmp_path);
                last_err = Some(error);
            }
        }
        if attempt < 3 {
            std::thread::sleep(std::time::Duration::from_millis(400 * u64::from(attempt)));
        }
    }
    Err(last_err.unwrap_or_else(|| {
        BackupError::new(
            "mariadb-dump falló",
            "sin detalle",
            format!(
                "reintenta o usa hostig stop --without-backup (confirma con {})",
                crate::WITHOUT_BACKUP_CONFIRM
            ),
        )
    }))
}

fn run_mariadb_dump_once(
    ctx: &DumpContext,
    dump: &Path,
    schema: &str,
    tmp_path: &Path,
    dump_tmp: &Path,
    needed: u64,
    dir: &Path,
) -> Result<()> {
    let mut child = Command::new(dump)
        .arg(defaults_extra_arg(&ctx.admin_cnf))
        .arg("--protocol=tcp")
        .arg("-h")
        .arg("127.0.0.1")
        .arg("-P")
        .arg(ctx.port.to_string())
        .arg("--single-transaction")
        .arg("--routines")
        .arg("--events")
        .arg("--triggers")
        .arg("--databases")
        .arg(schema)
        .env("TMP", dump_tmp)
        .env("TEMP", dump_tmp)
        .env("TMPDIR", dump_tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            BackupError::new(
                "no se pudo lanzar mariadb-dump",
                error.to_string(),
                "reinstala MariaDB o revisa permisos del binario",
            )
        })?;

    let stdout = child.stdout.take().ok_or_else(|| {
        BackupError::new(
            "mariadb-dump sin stdout",
            "pipe ausente".to_string(),
            "reintenta",
        )
    })?;

    let out_file = File::create(tmp_path)
        .map_err(|error| disk_or_perm_error(dir, needed, error, "crear fichero de volcado"))?;
    let mut encoder = GzEncoder::new(out_file, Compression::default());
    let mut reader = BufReader::new(stdout);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|error| {
            BackupError::new(
                "lectura de mariadb-dump fallida",
                error.to_string(),
                "comprueba que el servidor sigue en marcha",
            )
        })?;
        if n == 0 {
            break;
        }
        encoder.write_all(&buf[..n]).map_err(|error| {
            disk_or_perm_error(dir, needed, error, "escribir volcado comprimido")
        })?;
    }
    encoder
        .finish()
        .map_err(|error| disk_or_perm_error(dir, needed, error, "cerrar gzip del volcado"))?;

    let output = child.wait_with_output().map_err(|error| {
        BackupError::new(
            "mariadb-dump no terminó",
            error.to_string(),
            "revisa el error log de MariaDB",
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BackupError::new(
            "mariadb-dump falló",
            stderr.trim().to_string(),
            format!(
                "corrige el error o usa hostig stop --without-backup (confirma con {})",
                crate::WITHOUT_BACKUP_CONFIRM
            ),
        ));
    }
    Ok(())
}

fn disk_or_perm_error(dir: &Path, needed: u64, error: std::io::Error, what: &str) -> BackupError {
    let kind = error.kind();
    if kind == std::io::ErrorKind::PermissionDenied {
        return BackupError::new(
            "sin permiso para escribir la copia",
            format!("{what}: {} ({error})", dir.display()),
            format!(
                "corrige permisos de {} o usa hostig stop --without-backup (confirma con {})",
                dir.display(),
                crate::WITHOUT_BACKUP_CONFIRM
            ),
        );
    }
    let free = space::free_bytes(dir);
    let cause = match free {
        Some(f) => format!(
            "{what}: {error}; hacen falta ~{needed} bytes en {} (hay {f})",
            dir.display()
        ),
        None => format!("{what}: {error}; destino {}", dir.display()),
    };
    BackupError::new(
        "no se pudo escribir el volcado",
        cause,
        format!(
            "libera espacio o permisos, o usa hostig stop --without-backup (confirma con {})",
            crate::WITHOUT_BACKUP_CONFIRM
        ),
    )
}

/// Tras validar la nueva, borra las más antiguas dejando `retention` correctas.
pub fn apply_retention(schema_dir: &Path, retention: usize) -> Result<()> {
    if !schema_dir.is_dir() {
        return Ok(());
    }
    let keep = retention.max(1);
    let mut goods: Vec<(u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(schema_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".sql.gz") {
            continue;
        }
        let meta_path = meta_path_for(&path);
        if !meta_path.is_file() {
            continue;
        }
        let Ok(meta) = crate::meta::load_meta(&meta_path) else {
            continue;
        };
        goods.push((meta.created_unix, path));
    }
    goods.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_stamp, path) in goods.into_iter().skip(keep) {
        let meta = meta_path_for(&path);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&meta);
    }
    Ok(())
}

/// Lista copias correctas (con metadata) de una instancia.
pub fn list_backups(data_dir: &Path, instance_id: &str) -> Result<Vec<BackupEntry>> {
    let root = data_dir.join("backups").join(instance_id);
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    for schema_ent in fs::read_dir(&root)? {
        let schema_ent = schema_ent?;
        if !schema_ent.file_type()?.is_dir() {
            continue;
        }
        let schema = schema_ent.file_name().to_string_lossy().to_string();
        for entry in fs::read_dir(schema_ent.path())? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".sql.gz") {
                continue;
            }
            let meta_path = meta_path_for(&path);
            if !meta_path.is_file() {
                continue;
            }
            let Ok(meta) = crate::meta::load_meta(&meta_path) else {
                continue;
            };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(BackupEntry {
                schema: schema.clone(),
                path,
                created_unix: meta.created_unix,
                size_bytes: size,
            });
        }
    }
    out.sort_by(|a, b| {
        a.schema
            .cmp(&b.schema)
            .then(b.created_unix.cmp(&a.created_unix))
    });
    Ok(out)
}

/// Volca los esquemas que lo necesiten. Si hace falta y falla → Err (no parar).
pub fn ensure_backups_before_stop(ctx: &DumpContext) -> Result<Vec<PathBuf>> {
    if !ctx.admin_cnf.is_file() {
        return Err(BackupError::new(
            "no hay dump posible: credenciales ausentes",
            ctx.admin_cnf.display().to_string(),
            "si el servidor no responde, usa la recuperación del ADR 0003 o stop --without-backup",
        ));
    }

    let snap = match collect_server_snapshot(ctx) {
        Ok(s) => Some(s),
        Err(error) => {
            // Ante la duda se vuelca; si ni siquiera podemos consultar, no hay dump posible.
            return Err(BackupError::new(
                "no hay dump posible: el servidor no responde",
                error.cause,
                format!(
                    "usa la recuperación del ADR 0003 o hostig stop --without-backup (confirma con {})",
                    crate::WITHOUT_BACKUP_CONFIRM
                ),
            ));
        }
    };
    let snap = snap.expect("checked");

    let schemas = list_user_schemas(ctx)?;
    let mut created = Vec::new();
    for schema in schemas {
        let previous = latest_good_meta(&ctx.data_dir, &ctx.instance_id, &schema)?.map(|(_, m)| m);
        if !needs_dump(Some(&snap), previous.as_ref(), &ctx.settings, now_unix()) {
            continue;
        }
        let path = dump_one_schema(ctx, &schema, &snap)?;
        created.push(path);
    }
    Ok(created)
}

#[cfg(test)]
mod validate_tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn write_gz(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("t.sql.gz");
        let f = File::create(&path).unwrap();
        let mut enc = GzEncoder::new(f, Compression::default());
        enc.write_all(body.as_bytes()).unwrap();
        enc.finish().unwrap();
        path
    }

    #[test]
    fn acepta_cabecera_y_pie() {
        let dir = tempfile::tempdir().unwrap();
        let body =
            "-- MariaDB dump 10.0\nCREATE TABLE t(i INT);\n-- Dump completed on 2026-01-01\n";
        let path = write_gz(dir.path(), body);
        validate_dump_gz(&path).unwrap();
    }

    #[test]
    fn rechaza_sin_pie() {
        let dir = tempfile::tempdir().unwrap();
        let body = "-- MariaDB dump 10.0\nCREATE TABLE t(i INT);\n";
        let path = write_gz(dir.path(), body);
        assert!(validate_dump_gz(&path).is_err());
    }

    #[test]
    fn retencion_deja_las_n_mas_recientes() {
        let dir = tempfile::tempdir().unwrap();
        let schema_dir = dir.path().join("app");
        fs::create_dir_all(&schema_dir).unwrap();
        for i in 0..12u64 {
            let path = schema_dir.join(format!("20260101T00000{i}.sql.gz"));
            fs::write(&path, b"x").unwrap();
            let meta = BackupMeta {
                schema: "app".into(),
                created_unix: 1000 + i,
                boot_id_unix: 1,
                com_counters: Default::default(),
                has_non_innodb: false,
            };
            save_meta(&meta_path_for(&path), &meta).unwrap();
        }
        apply_retention(&schema_dir, 10).unwrap();
        let left: Vec<_> = fs::read_dir(&schema_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".sql.gz"))
            .collect();
        assert_eq!(left.len(), 10);
        let mut stamps: Vec<u64> = left
            .iter()
            .map(|e| {
                crate::meta::load_meta(&meta_path_for(&e.path()))
                    .unwrap()
                    .created_unix
            })
            .collect();
        stamps.sort();
        assert_eq!(stamps, (1002..1012).collect::<Vec<_>>());
    }
}
