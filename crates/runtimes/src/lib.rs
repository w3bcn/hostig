//! Descarga, verificación y almacenamiento de runtimes (ADR 0002).

mod catalog;
mod download;
mod error;
mod extract;
mod install;
mod jobs;

pub use catalog::{host_arch, host_os, Catalog, RuntimeEntry, EMBEDDED_CATALOG};
pub use download::DownloadTimeouts;
pub use error::{Result, RuntimeError};
pub use install::{
    clean_tmp_runtimes, runtime_dir, verify_installed, InstallManifest, MANIFEST_NAME,
};
pub use jobs::{JobState, JobStatus, ListedRuntime, RuntimeService, VerifyRow, MAX_FINISHED_JOBS};

/// Comprueba en red que cada URL del catálogo responde y que el SHA-256 coincide.
/// Pensado para el workflow semanal, no para el CI de cada PR.
pub fn verify_catalog_urls(catalog: &Catalog) -> Result<Vec<String>> {
    let tmp = std::env::temp_dir().join(format!("hostig-catalog-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp)?;
    let mut ok_lines = Vec::new();
    for entry in &catalog.entries {
        let dest = tmp.join(&entry.filename);
        download::download_verified(&entry.urls, &entry.normalized_sha256(), &dest)?;
        let _ = std::fs::remove_file(&dest);
        ok_lines.push(format!(
            "{}@{}-{}-{} OK",
            entry.component, entry.version, entry.os, entry.arch
        ));
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(ok_lines)
}

#[cfg(test)]
mod tests;
