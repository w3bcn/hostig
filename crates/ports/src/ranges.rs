use std::ops::RangeInclusive;

/// HTTP (Apache) — ADR 0004.
pub const HTTP: RangeInclusive<u16> = 8080..=8099;
/// HTTPS — ADR 0004.
pub const HTTPS: RangeInclusive<u16> = 8443..=8459;
/// MariaDB — ADR 0004.
pub const MARIADB: RangeInclusive<u16> = 3306..=3399;
/// PHP-CGI en Windows — ADR 0004.
pub const PHP_WINDOWS: RangeInclusive<u16> = 9000..=9099;

/// 80, 443 y 53: rechazados en modo sin privilegios (ADR 0004 / 0006).
pub fn is_privileged(port: u16) -> bool {
    matches!(port, 80 | 443 | 53)
}

/// Rango convencional que contiene `port`, si encaja en alguno del ADR.
pub fn for_port(port: u16) -> Option<RangeInclusive<u16>> {
    for range in [HTTP, HTTPS, MARIADB, PHP_WINDOWS] {
        if range.contains(&port) {
            return Some(range);
        }
    }
    None
}
