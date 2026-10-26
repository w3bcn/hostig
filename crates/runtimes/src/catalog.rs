use serde::Deserialize;

use crate::error::{Result, RuntimeError};

/// Catálogo embebido desde `catalog/runtimes.toml` en la raíz del workspace.
pub const EMBEDDED_CATALOG: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../catalog/runtimes.toml"
));

#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    #[serde(rename = "runtime", default)]
    pub entries: Vec<RuntimeEntry>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RuntimeEntry {
    pub component: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub urls: Vec<String>,
    pub filename: String,
    pub sha256: String,
    pub license: String,
    #[serde(default)]
    pub notes: String,
}

impl RuntimeEntry {
    pub fn install_key(&self) -> String {
        format!(
            "{}@{}-{}-{}",
            self.component, self.version, self.os, self.arch
        )
    }

    pub fn relative_dir(&self) -> String {
        format!(
            "{}/{}-{}-{}",
            self.component, self.version, self.os, self.arch
        )
    }

    pub fn normalized_sha256(&self) -> String {
        self.sha256.trim().to_ascii_lowercase()
    }
}

impl Catalog {
    pub fn parse(text: &str) -> Result<Self> {
        let catalog: Catalog = toml::from_str(text).map_err(|error| {
            RuntimeError::new(
                "catálogo inválido",
                error.to_string(),
                "corrige catalog/runtimes.toml y vuelve a compilar",
            )
        })?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn embedded() -> Result<Self> {
        Self::parse(EMBEDDED_CATALOG)
    }

    pub fn validate(&self) -> Result<()> {
        if self.entries.is_empty() {
            return Err(RuntimeError::new(
                "catálogo vacío",
                "no hay entradas [[runtime]]",
                "añade al menos Apache, PHP y MariaDB para Windows",
            ));
        }
        for entry in &self.entries {
            if entry.urls.is_empty() {
                return Err(RuntimeError::new(
                    "entrada sin URL",
                    format!("{}@{} no declara urls", entry.component, entry.version),
                    "añade al menos una URL HTTPS en el catálogo",
                ));
            }
            if entry.normalized_sha256().len() != 64
                || !entry
                    .normalized_sha256()
                    .chars()
                    .all(|c| c.is_ascii_hexdigit())
            {
                return Err(RuntimeError::new(
                    "hash inválido",
                    format!(
                        "{}@{} no tiene un SHA-256 de 64 hex",
                        entry.component, entry.version
                    ),
                    "fija el SHA-256 del archivo tras contrastarlo con el publicador",
                ));
            }
            for url in &entry.urls {
                if !url.starts_with("https://") {
                    return Err(RuntimeError::new(
                        "URL no segura",
                        format!("{url} no usa HTTPS"),
                        "usa solo URLs https:// en el catálogo",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn find(&self, component: &str, version: &str) -> Option<&RuntimeEntry> {
        self.entries
            .iter()
            .find(|e| e.component == component && e.version == version)
    }

    pub fn for_host(&self) -> Vec<&RuntimeEntry> {
        let os = host_os();
        let arch = host_arch();
        self.entries
            .iter()
            .filter(|e| e.os == os && e.arch == arch)
            .collect()
    }
}

pub fn host_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

pub fn host_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_catalogo_embebido_parsea_y_valida() {
        let catalog = Catalog::embedded().unwrap();
        assert!(catalog.entries.len() >= 3);
        let apache = catalog.find("apache", "2.4.69").unwrap();
        assert_eq!(apache.normalized_sha256().len(), 64);
        assert!(!apache.urls.is_empty());
        assert!(catalog.find("php", "8.4.27").is_some());
        assert!(catalog.find("mariadb", "11.4.13").is_some());
    }

    #[test]
    fn rechaza_http_en_claro() {
        let text = r#"
[[runtime]]
component = "php"
version = "1.0.0"
os = "windows"
arch = "x86_64"
urls = ["http://example.com/a.zip"]
filename = "a.zip"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
license = "MIT"
"#;
        let err = Catalog::parse(text).unwrap_err();
        assert!(err.cause.contains("HTTPS") || err.message.contains("segura"));
    }
}
