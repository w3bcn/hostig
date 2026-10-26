use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::RuntimeEntry;
use crate::download::{download_verified, sha256_file};
use crate::error::{Result, RuntimeError};
use crate::extract::extract_zip;

pub const MANIFEST_NAME: &str = "hostig-install-manifest.json";

/// Borra restos de instalaciones interrumpidas en `{datos}/tmp/runtimes`.
/// Devuelve cuántas entradas se eliminaron.
pub fn clean_tmp_runtimes(data_dir: &Path) -> std::io::Result<usize> {
    let tmp_root = data_dir.join("tmp").join("runtimes");
    if !tmp_root.exists() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in fs::read_dir(&tmp_root)? {
        let entry = entry?;
        let path = entry.path();
        let outcome = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if outcome.is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallManifest {
    pub component: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub package_sha256: String,
    pub license: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub sha256: String,
}

/// Instala un runtime de forma atómica bajo `data_dir/runtimes/...`.
pub fn install_entry(data_dir: &Path, entry: &RuntimeEntry) -> Result<PathBuf> {
    let final_dir = runtime_dir(data_dir, entry);
    if final_dir.join(MANIFEST_NAME).is_file() {
        return Ok(final_dir);
    }

    let tmp_root = data_dir.join("tmp").join("runtimes");
    fs::create_dir_all(&tmp_root)?;
    let stamp = uuid::Uuid::new_v4().to_string();
    let zip_path = tmp_root.join(format!("{stamp}-{}", entry.filename));
    let extract_dir = tmp_root.join(format!("{stamp}-extract"));

    let cleanup = |zip: &Path, extract: &Path| {
        let _ = fs::remove_file(zip);
        let _ = fs::remove_dir_all(extract);
    };

    if let Err(error) = download_verified(&entry.urls, &entry.normalized_sha256(), &zip_path) {
        cleanup(&zip_path, &extract_dir);
        return Err(error);
    }

    let files = match extract_zip(&zip_path, &extract_dir) {
        Ok(files) => files,
        Err(error) => {
            cleanup(&zip_path, &extract_dir);
            return Err(error);
        }
    };

    ensure_license_marker(&extract_dir, entry)?;

    let manifest = InstallManifest {
        component: entry.component.clone(),
        version: entry.version.clone(),
        os: entry.os.clone(),
        arch: entry.arch.clone(),
        package_sha256: entry.normalized_sha256(),
        license: entry.license.clone(),
        files: files
            .into_iter()
            .map(|(path, sha256)| ManifestFile { path, sha256 })
            .collect(),
    };
    write_manifest(&extract_dir.join(MANIFEST_NAME), &manifest)?;

    if let Some(parent) = final_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    if final_dir.exists() {
        // Carrera: otra instalación terminó antes. Descartamos la nuestra.
        cleanup(&zip_path, &extract_dir);
        if final_dir.join(MANIFEST_NAME).is_file() {
            return Ok(final_dir);
        }
        return Err(RuntimeError::new(
            "destino ocupado",
            format!("{} existe sin manifiesto", final_dir.display()),
            "borra a mano ese directorio incompleto y reintenta",
        ));
    }

    atomic_rename(&extract_dir, &final_dir).inspect_err(|_| {
        cleanup(&zip_path, &extract_dir);
        let _ = fs::remove_dir_all(&final_dir);
    })?;
    let _ = fs::remove_file(&zip_path);
    Ok(final_dir)
}

pub fn runtime_dir(data_dir: &Path, entry: &RuntimeEntry) -> PathBuf {
    data_dir.join("runtimes").join(entry.relative_dir())
}

pub fn verify_installed(data_dir: &Path, entry: &RuntimeEntry) -> Result<()> {
    let dir = runtime_dir(data_dir, entry);
    let manifest_path = dir.join(MANIFEST_NAME);
    if !manifest_path.is_file() {
        return Err(RuntimeError::new(
            "runtime no instalado",
            format!("falta {}", manifest_path.display()),
            "ejecuta hostig runtimes install",
        ));
    }
    let text = fs::read_to_string(&manifest_path)?;
    let manifest: InstallManifest = serde_json::from_str(&text).map_err(|error| {
        RuntimeError::new(
            "manifiesto ilegible",
            error.to_string(),
            "reinstala el runtime",
        )
    })?;
    if manifest.package_sha256 != entry.normalized_sha256() {
        return Err(RuntimeError::new(
            "manifiesto desfasado",
            format!(
                "instalado {} y catálogo {}",
                manifest.package_sha256,
                entry.normalized_sha256()
            ),
            "reinstala el runtime tras actualizar el catálogo",
        ));
    }
    for file in &manifest.files {
        let path = dir.join(&file.path);
        if !path.is_file() {
            return Err(RuntimeError::new(
                "archivo ausente",
                format!("{} no está en la instalación", file.path),
                "reinstala el runtime",
            ));
        }
        let got = sha256_file(&path)?;
        if got != file.sha256 {
            return Err(RuntimeError::new(
                "hash de archivo incorrecto",
                format!(
                    "{} tiene {got} y el manifiesto espera {}",
                    file.path, file.sha256
                ),
                "reinstala el runtime; el árbol pudo corromperse",
            ));
        }
    }
    Ok(())
}

fn ensure_license_marker(extract_dir: &Path, entry: &RuntimeEntry) -> Result<()> {
    let has_license = walk_has_license(extract_dir);
    if has_license {
        return Ok(());
    }
    let path = extract_dir.join("LICENSE.upstream");
    let body = format!(
        "SPDX: {}\nOrigen: {}\nComponente: {} {}\n",
        entry.license,
        entry.urls.first().map(String::as_str).unwrap_or(""),
        entry.component,
        entry.version
    );
    fs::write(path, body)?;
    Ok(())
}

fn walk_has_license(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if path.is_file()
            && (name == "license"
                || name == "license.txt"
                || name == "license.md"
                || name == "copying"
                || name == "copying.txt"
                || name.starts_with("license"))
        {
            return true;
        }
        if path.is_dir() && walk_has_license(&path) {
            return true;
        }
    }
    false
}

fn write_manifest(path: &Path, manifest: &InstallManifest) -> Result<()> {
    let text = serde_json::to_string_pretty(manifest).map_err(|error| {
        RuntimeError::new(
            "no se pudo serializar el manifiesto",
            error.to_string(),
            "reintenta la instalación",
        )
    })?;
    let mut file = File::create(path)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    Ok(())
}

fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) => {
            // En Windows, rename entre volúmenes falla; copiar no es atómico.
            // Misma carpeta de datos: debe funcionar. Si no, error claro.
            Err(RuntimeError::new(
                "no se pudo publicar la instalación",
                format!("{} -> {}: {error}", from.display(), to.display()),
                "libera el destino y reintenta; no debe quedar un directorio final a medias",
            ))
        }
    }
}
