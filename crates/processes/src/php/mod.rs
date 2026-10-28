//! Pool PHP FastCGI (Windows: N × php-cgi; ADR 0005).

mod config;
mod service;
mod settings;

pub use config::{find_php_root, php_cgi_in, regenerate_php_ini, PhpPaths, DEFAULT_POOL_ID};
pub use service::{
    PhpJobStatus, PhpService, PhpStartRequest, PhpState, PhpStatus, PhpWorkerStatus,
    TEST_PHPCGI_ENV,
};
pub use settings::{
    PhpSettings, DEFAULT_FCGI_MAX_REQUESTS, DEFAULT_PHP_VERSION, DEFAULT_POOL_SIZE,
};
