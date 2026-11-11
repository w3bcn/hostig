//! Generación de httpd.conf, health y vhost de prueba (ADR 0005).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{ProcessError, Result};

pub const DEFAULT_APACHE_ID: &str = "apache-default";

const HTTPD_TEMPLATE: &str = include_str!("../../../../templates/apache/httpd.conf.template");
const HEALTH_TEMPLATE: &str = include_str!("../../../../templates/apache/health.php.template");
const INDEX_TEMPLATE: &str = include_str!("../../../../templates/apache/index.php.template");

#[derive(Debug, Clone)]
pub struct ApachePaths {
    pub id: String,
    pub root: PathBuf,
    pub httpd_conf: PathBuf,
    pub error_log: PathBuf,
    pub access_log: PathBuf,
    pub pid_file: PathBuf,
    pub document_root: PathBuf,
    pub hostig_dir: PathBuf,
    pub health_php: PathBuf,
}

impl ApachePaths {
    pub fn new(data_dir: &Path, id: &str) -> Self {
        let root = data_dir.join("apache").join(id);
        let hostig_dir = root.join("hostig");
        Self {
            id: id.to_string(),
            httpd_conf: root.join("httpd.conf"),
            error_log: root.join("logs").join("error.log"),
            access_log: root.join("logs").join("access.log"),
            pid_file: root.join("httpd.pid"),
            document_root: data_dir.join("www").join("default"),
            hostig_dir: hostig_dir.clone(),
            health_php: hostig_dir.join("health.php"),
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.root.join("logs"))?;
        fs::create_dir_all(&self.document_root)?;
        fs::create_dir_all(&self.hostig_dir)?;
        Ok(())
    }
}

/// Localiza ServerRoot (carpeta con `bin/httpd` y `modules/`).
pub fn find_server_root(runtime_root: &Path) -> Result<PathBuf> {
    if httpd_in(&runtime_root.join("bin")).is_some() {
        return Ok(runtime_root.to_path_buf());
    }
    if let Ok(entries) = fs::read_dir(runtime_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && httpd_in(&path.join("bin")).is_some() {
                return Ok(path);
            }
        }
    }
    Err(ProcessError::new(
        "binarios Apache no encontrados",
        format!("no hay httpd bajo {}", runtime_root.display()),
        "instala apache del catálogo con hostig runtimes install",
    ))
}

pub fn httpd_in(bin: &Path) -> Option<PathBuf> {
    let with_exe = bin.join("httpd.exe");
    if with_exe.is_file() {
        return Some(with_exe);
    }
    let plain = bin.join("httpd");
    if plain.is_file() {
        return Some(plain);
    }
    None
}

pub struct HealthDbParams<'a> {
    pub host: &'a str,
    pub port: u16,
    pub user: &'a str,
    pub password: &'a str,
}

/// Regenera conf, health.php e index.php de bienvenida.
///
/// PHP: `<FilesMatch "(?i)\.php$">` + `SetHandler proxy:balancer://php/` y
/// `ProxyFCGIBackendType GENERIC` (sin router ni ProxyPassMatch). En Windows,
/// un `RewriteRule` guarda `%{REQUEST_FILENAME}` en `HOSTIG_SCRIPT` (también a
/// nivel VirtualHost para redirects internos de `.htaccess`) y
/// `ProxyFCGISetEnvIf` lo restaura como `SCRIPT_FILENAME`. El health usa
/// `Alias` fuera del DocumentRoot + ruta fija.
pub fn regenerate_apache_config(
    paths: &ApachePaths,
    server_root: &Path,
    http_port: u16,
    php_ports: &[u16],
    db: &HealthDbParams<'_>,
) -> Result<()> {
    paths.ensure_dirs()?;
    let members = php_ports
        .iter()
        .map(|p| {
            // disablereuse: evita reutilizar un socket FastCGI tras el reciclado de php-cgi.
            format!("    BalancerMember fcgi://127.0.0.1:{p}/ retry=1 disablereuse=On")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let conf = HTTPD_TEMPLATE
        .replace("{{SERVER_ROOT}}", &path_conf(server_root))
        .replace("{{HTTP_PORT}}", &http_port.to_string())
        .replace("{{PID_FILE}}", &path_conf(&paths.pid_file))
        .replace("{{ERROR_LOG}}", &path_conf(&paths.error_log))
        .replace("{{ACCESS_LOG}}", &path_conf(&paths.access_log))
        .replace("{{DOCUMENT_ROOT}}", &path_conf(&paths.document_root))
        .replace("{{HOSTIG_DIR}}", &path_conf(&paths.hostig_dir))
        .replace("{{HEALTH_PHP}}", &path_conf(&paths.health_php))
        .replace("{{BALANCER_MEMBERS}}", &members);
    fs::write(&paths.httpd_conf, conf.as_bytes())?;

    let health = HEALTH_TEMPLATE
        .replace("{{DB_HOST}}", db.host)
        .replace("{{DB_PORT}}", &db.port.to_string())
        .replace("{{DB_USER}}", &escape_php_single(db.user))
        .replace("{{DB_PASSWORD}}", &escape_php_single(db.password));
    fs::write(&paths.health_php, health.as_bytes())?;
    // El health embebe la contraseña de hostig_health: solo el usuario.
    hostig_ipc::restrict_to_user_and_system(&paths.health_php).map_err(|error| {
        ProcessError::new(
            "no se pudieron fijar permisos de health.php",
            error.to_string(),
            "revisa la ACL del usuario actual",
        )
    })?;

    // Eliminar router legado si quedó de una versión anterior.
    let legacy_router = paths.hostig_dir.join("fcgi-router.php");
    if legacy_router.is_file() {
        let _ = fs::remove_file(&legacy_router);
    }

    let index_path = paths.document_root.join("index.php");
    if !index_path.is_file() {
        fs::write(&index_path, INDEX_TEMPLATE.as_bytes())?;
    }
    Ok(())
}

fn path_conf(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn escape_php_single(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genera_balancer_filesmatch_y_health_sin_router() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ApachePaths::new(dir.path(), DEFAULT_APACHE_ID);
        let server = dir.path().join("Apache24");
        fs::create_dir_all(server.join("bin")).unwrap();
        regenerate_apache_config(
            &paths,
            &server,
            8080,
            &[9000, 9001],
            &HealthDbParams {
                host: "127.0.0.1",
                port: 3306,
                user: "hostig_health",
                password: "sec'ret",
            },
        )
        .unwrap();
        let conf = fs::read_to_string(&paths.httpd_conf).unwrap();
        assert!(
            conf.contains("BalancerMember fcgi://127.0.0.1:9000/")
                && conf.contains("retry=1")
                && conf.contains("disablereuse=On"),
            "{conf}"
        );
        assert!(conf.contains("SetHandler \"proxy:balancer://php/\""));
        assert!(conf.contains("FilesMatch \"(?i)\\.php$\""));
        assert!(conf.contains("ProxyFCGIBackendType GENERIC"));
        assert!(conf.contains("RewriteRule \"(?i)\\.php[\\. ]$\" \"-\" [F,L]"));
        assert!(!conf.contains("ProxyPassMatch \""));
        assert!(!conf.contains("AddType application/x-httpd-php"));
        assert!(!conf.contains("fcgi-router"));
        assert!(conf.contains("Alias /__hostig/health"));
        assert!(conf.contains("E=HOSTIG_SCRIPT:%{REQUEST_FILENAME}"));
        assert!(conf.contains("Listen 127.0.0.1:8080"));
        assert!(!conf.contains("{{"));
        let health = fs::read_to_string(&paths.health_php).unwrap();
        assert!(health.contains("sec\\'ret"));
        assert!(health.contains("hostig_health"));
        assert!(paths.document_root.join("index.php").is_file());
        assert!(!paths.hostig_dir.join("fcgi-router.php").is_file());
    }
}
