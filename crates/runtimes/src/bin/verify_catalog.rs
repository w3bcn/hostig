//! Comprueba en red las URLs del catálogo. Uso: CI semanal, no el PR.

fn main() {
    let catalog = match hostig_runtimes::Catalog::embedded() {
        Ok(c) => c,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    match hostig_runtimes::verify_catalog_urls(&catalog) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
        }
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "level": "error",
                    "module": "runtimes",
                    "cause": error.cause,
                    "action": error.action,
                    "error": error.message,
                })
            );
            std::process::exit(1);
        }
    }
}
