use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::error::{Result, RuntimeError};

/// Descomprime `zip_path` en `dest_dir`, rechazando zip slip y enlaces fuera del destino.
/// Devuelve la lista (ruta relativa, sha256) de cada archivo regular escrito.
pub fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<Vec<(String, String)>> {
    fs::create_dir_all(dest_dir)?;
    let file = File::open(zip_path)?;
    let mut archive = ZipArchive::new(file).map_err(|error| {
        RuntimeError::new(
            "ZIP inválido",
            error.to_string(),
            "borra el temporal y vuelve a descargar el runtime",
        )
    })?;

    let mut files = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            RuntimeError::new(
                "no se pudo leer una entrada del ZIP",
                error.to_string(),
                "vuelve a descargar el paquete",
            )
        })?;
        let raw_name = entry
            .enclosed_name()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                RuntimeError::new(
                    "entrada ZIP rechazada",
                    format!("ruta insegura en el ZIP: {}", entry.name()),
                    "el catálogo no debe apuntar a un ZIP con rutas absolutas o ..",
                )
            })?;

        let target = safe_join(dest_dir, &raw_name)?;

        if entry.is_dir() || entry.name().ends_with('/') {
            fs::create_dir_all(&target)?;
            continue;
        }

        if entry.is_symlink() {
            let mut link = String::new();
            entry.read_to_string(&mut link).map_err(|error| {
                RuntimeError::new(
                    "enlace simbólico ilegible",
                    error.to_string(),
                    "revisa el ZIP del publicador",
                )
            })?;
            let link_path = Path::new(link.trim());
            let resolved = if link_path.is_absolute() {
                link_path.to_path_buf()
            } else {
                target.parent().unwrap_or(dest_dir).join(link_path)
            };
            ensure_within(dest_dir, &resolved)?;
            // En Windows crear symlinks puede pedir privilegios; se materializa como archivo
            // de texto solo si el destino queda dentro. Preferimos rechazar la creación
            // y escribir el contenido del enlace no: copiamos como archivo vacío no.
            // Política: rechazar symlinks (ADR estabilidad). El check de "fuera" ya pasó.
            return Err(RuntimeError::new(
                "ZIP con enlace simbólico",
                format!("{} -> {link}", entry.name()),
                "usa un paquete sin symlinks o un runtime de otra fuente del catálogo",
            ));
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&target)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = entry.read(&mut buffer).map_err(|error| {
                RuntimeError::new(
                    "error al extraer",
                    error.to_string(),
                    "borra el temporal y reintenta la instalación",
                )
            })?;
            if read == 0 {
                break;
            }
            out.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
        }
        out.flush()?;
        let rel = raw_name.to_string_lossy().replace('\\', "/");
        files.push((rel, hex::encode(hasher.finalize())));
    }
    Ok(files)
}

fn safe_join(base: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute() {
        return Err(RuntimeError::new(
            "entrada ZIP rechazada",
            format!("ruta absoluta: {}", relative.display()),
            "el ZIP no puede contener rutas absolutas",
        ));
    }
    let mut out = base.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(RuntimeError::new(
                    "entrada ZIP rechazada",
                    format!("componente .. en {}", relative.display()),
                    "el ZIP no puede usar .. (zip slip)",
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(RuntimeError::new(
                    "entrada ZIP rechazada",
                    format!("prefijo o raíz en {}", relative.display()),
                    "el ZIP no puede contener rutas absolutas",
                ));
            }
        }
    }
    ensure_within(base, &out)?;
    Ok(out)
}

fn ensure_within(base: &Path, candidate: &Path) -> Result<()> {
    let base_abs = fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    // candidate puede no existir aún: normalizamos con el padre.
    let check = if candidate.exists() {
        fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf())
    } else if let Some(parent) = candidate.parent() {
        let parent_abs = if parent.exists() {
            fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf())
        } else {
            parent.to_path_buf()
        };
        parent_abs.join(candidate.file_name().unwrap_or_default())
    } else {
        candidate.to_path_buf()
    };
    if !check.starts_with(&base_abs) && !candidate.starts_with(base) {
        return Err(RuntimeError::new(
            "entrada ZIP rechazada",
            format!("{} sale de {}", candidate.display(), base.display()),
            "el ZIP no puede escribir fuera del destino",
        ));
    }
    Ok(())
}
