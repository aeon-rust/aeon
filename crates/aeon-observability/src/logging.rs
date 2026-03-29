//! Structured logging setup for Loki-compatible JSON output.
//!
//! Uses `tracing` + `tracing-subscriber` with JSON formatting.
//! Includes a PII/PHI masking utility for sensitive field values.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// Logging configuration.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Log level filter (e.g., "info", "aeon_engine=debug,aeon_connectors=trace").
    pub filter: String,
    /// Whether to use JSON format (for Loki). If false, uses human-readable format.
    pub json: bool,
    /// Whether to include file/line info in log output.
    pub with_file: bool,
    /// Whether to include target (module path) in log output.
    pub with_target: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: "info".to_string(),
            json: false,
            with_file: false,
            with_target: true,
        }
    }
}

/// Initialize the global tracing subscriber with the given config.
///
/// Call this once at startup. Returns an error if already initialized.
///
/// For JSON output (Loki integration), set `config.json = true`.
/// For human-readable output (development), set `config.json = false`.
pub fn init_logging(config: &LogConfig) -> Result<(), String> {
    let filter = EnvFilter::try_new(&config.filter)
        .map_err(|e| format!("invalid log filter '{}': {e}", config.filter))?;

    if config.json {
        let layer = fmt::layer()
            .json()
            .with_file(config.with_file)
            .with_target(config.with_target)
            .with_thread_ids(true);

        tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .try_init()
            .map_err(|e| format!("failed to init logging: {e}"))
    } else {
        let layer = fmt::layer()
            .with_file(config.with_file)
            .with_target(config.with_target);

        tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .try_init()
            .map_err(|e| format!("failed to init logging: {e}"))
    }
}

/// Mask a string value for PII/PHI protection.
///
/// Replaces all characters except the first and last with asterisks.
/// Strings shorter than 4 characters are fully masked.
///
/// # Examples
///
/// ```
/// # use aeon_observability::mask_pii;
/// assert_eq!(mask_pii("john.doe@example.com"), "j******************m");
/// assert_eq!(mask_pii("abc"), "***");
/// assert_eq!(mask_pii(""), "");
/// ```
pub fn mask_pii(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let len = chars.len();

    if len == 0 {
        return String::new();
    }

    if len < 4 {
        return "*".repeat(len);
    }

    let mut masked = String::with_capacity(len);
    masked.push(chars[0]);
    for _ in 1..len - 1 {
        masked.push('*');
    }
    masked.push(chars[len - 1]);
    masked
}

/// Mask an email address, preserving domain structure.
///
/// # Examples
///
/// ```
/// # use aeon_observability::mask_email;
/// assert_eq!(mask_email("user@example.com"), "u**r@e*********m");
/// ```
pub fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            format!("{}@{}", mask_pii(local), mask_pii(domain))
        }
        None => mask_pii(email),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_pii_normal() {
        assert_eq!(mask_pii("john"), "j**n");
        assert_eq!(mask_pii("hello"), "h***o");
    }

    #[test]
    fn mask_pii_short() {
        assert_eq!(mask_pii("ab"), "**");
        assert_eq!(mask_pii("abc"), "***");
    }

    #[test]
    fn mask_pii_empty() {
        assert_eq!(mask_pii(""), "");
    }

    #[test]
    fn mask_pii_single_char() {
        assert_eq!(mask_pii("x"), "*");
    }

    #[test]
    fn mask_pii_long_string() {
        let masked = mask_pii("john.doe@example.com");
        assert_eq!(masked.len(), 20);
        assert!(masked.starts_with('j'));
        assert!(masked.ends_with('m'));
        assert!(masked.contains('*'));
    }

    #[test]
    fn mask_email_basic() {
        let masked = mask_email("user@example.com");
        assert!(masked.contains('@'));
        assert!(masked.starts_with('u'));
    }

    #[test]
    fn mask_email_no_at_sign() {
        let masked = mask_email("noatsign");
        assert_eq!(masked, mask_pii("noatsign"));
    }

    #[test]
    fn log_config_defaults() {
        let config = LogConfig::default();
        assert_eq!(config.filter, "info");
        assert!(!config.json);
        assert!(!config.with_file);
        assert!(config.with_target);
    }

    #[test]
    fn invalid_filter_returns_error() {
        let config = LogConfig {
            filter: "[invalid".to_string(),
            ..Default::default()
        };
        assert!(init_logging(&config).is_err());
    }
}
