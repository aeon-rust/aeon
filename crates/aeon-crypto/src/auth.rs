//! REST API authentication for Aeon management plane (port 4471).
//!
//! ## Auth Modes
//!
//! - `none` — no authentication (dev only)
//! - `api-key` — Bearer token in `Authorization` header
//! - `mtls` — client certificate required (handled at TLS layer)
//!
//! Phase 10 wires basic auth; Phase 13 adds full RBAC.

use aeon_types::AeonError;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Authentication mode for the HTTP management API.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// No authentication. Dev/single-node only.
    #[default]
    None,
    /// API key authentication via `Authorization: Bearer <key>` header.
    ApiKey,
    /// Mutual TLS — client must present a valid certificate.
    /// Auth is enforced at the TLS layer, not the application layer.
    Mtls,
}

/// HTTP authentication configuration block.
///
/// ```yaml
/// http:
///   auth:
///     mode: api-key
///     api_keys:
///       - name: admin
///         key_env: AEON_API_KEY
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Authentication mode.
    #[serde(default)]
    pub mode: AuthMode,
    /// API key entries (used when mode is `api-key`).
    #[serde(default)]
    pub api_keys: Vec<ApiKeyEntry>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            mode: AuthMode::None,
            api_keys: Vec::new(),
        }
    }
}

/// A named API key entry. The actual key value is loaded from an environment variable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyEntry {
    /// Human-readable name for this key (e.g., "admin", "ci-bot").
    pub name: String,
    /// Environment variable that holds the key value.
    pub key_env: String,
}

/// Resolved API key (loaded from env). Zeroized on Drop.
pub struct ResolvedApiKey {
    pub name: String,
    key: String,
}

impl Drop for ResolvedApiKey {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl std::fmt::Debug for ResolvedApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedApiKey")
            .field("name", &self.name)
            .field("key", &"[REDACTED]")
            .finish()
    }
}

/// API key authenticator. Validates Bearer tokens against resolved keys.
pub struct ApiKeyAuthenticator {
    keys: Vec<ResolvedApiKey>,
}

impl ApiKeyAuthenticator {
    /// Resolve API keys from environment variables.
    pub fn from_config(entries: &[ApiKeyEntry]) -> Result<Self, AeonError> {
        let mut keys = Vec::with_capacity(entries.len());
        for entry in entries {
            let value = std::env::var(&entry.key_env).map_err(|_| AeonError::Config {
                message: format!(
                    "API key '{}': environment variable '{}' not set",
                    entry.name, entry.key_env
                ),
            })?;
            if value.is_empty() {
                return Err(AeonError::Config {
                    message: format!(
                        "API key '{}': environment variable '{}' is empty",
                        entry.name, entry.key_env
                    ),
                });
            }
            keys.push(ResolvedApiKey {
                name: entry.name.clone(),
                key: value,
            });
        }
        if keys.is_empty() {
            return Err(AeonError::Config {
                message: "api-key auth mode requires at least one API key entry".into(),
            });
        }
        Ok(Self { keys })
    }

    /// Validate a Bearer token. Returns the key name if valid.
    ///
    /// Uses constant-time comparison to prevent timing attacks.
    pub fn validate_bearer(&self, token: &str) -> Option<&str> {
        for resolved in &self.keys {
            if constant_time_eq(resolved.key.as_bytes(), token.as_bytes()) {
                return Some(&resolved.name);
            }
        }
        None
    }

    /// Extract and validate from an `Authorization` header value.
    ///
    /// Expects format: `Bearer <token>`
    pub fn validate_header(&self, header_value: &str) -> Result<&str, AeonError> {
        let token = header_value
            .strip_prefix("Bearer ")
            .ok_or_else(|| AeonError::Crypto {
                message: "Authorization header must use 'Bearer <token>' format".into(),
                source: None,
            })?;
        self.validate_bearer(token)
            .ok_or_else(|| AeonError::Crypto {
                message: "invalid API key".into(),
                source: None,
            })
    }
}

impl AuthConfig {
    /// Validate the auth configuration.
    pub fn validate(&self, context: &str) -> Result<(), AeonError> {
        match self.mode {
            AuthMode::None => {}
            AuthMode::ApiKey => {
                if self.api_keys.is_empty() {
                    return Err(AeonError::Config {
                        message: format!(
                            "{context}: auth.mode 'api-key' requires at least one api_keys entry"
                        ),
                    });
                }
                for entry in &self.api_keys {
                    if entry.name.is_empty() {
                        return Err(AeonError::Config {
                            message: format!("{context}: api_keys entry has empty name"),
                        });
                    }
                    if entry.key_env.is_empty() {
                        return Err(AeonError::Config {
                            message: format!(
                                "{context}: api_keys entry '{}' has empty key_env",
                                entry.name
                            ),
                        });
                    }
                }
            }
            AuthMode::Mtls => {
                // mTLS auth is enforced at the TLS layer — no additional config needed here.
            }
        }
        Ok(())
    }
}

/// Constant-time byte comparison to prevent timing attacks on key validation.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_mode_default_is_none() {
        assert_eq!(AuthMode::default(), AuthMode::None);
    }

    #[test]
    fn auth_mode_serde_roundtrip() {
        let modes = vec![AuthMode::None, AuthMode::ApiKey, AuthMode::Mtls];
        for mode in &modes {
            let encoded = bincode::serialize(mode).unwrap();
            let decoded: AuthMode = bincode::deserialize(&encoded).unwrap();
            assert_eq!(*mode, decoded);
        }
    }

    #[test]
    fn auth_config_validate_none() {
        let config = AuthConfig::default();
        assert!(config.validate("test").is_ok());
    }

    #[test]
    fn auth_config_validate_api_key_no_entries() {
        let config = AuthConfig {
            mode: AuthMode::ApiKey,
            api_keys: Vec::new(),
        };
        assert!(config.validate("test").is_err());
    }

    #[test]
    fn auth_config_validate_api_key_empty_name() {
        let config = AuthConfig {
            mode: AuthMode::ApiKey,
            api_keys: vec![ApiKeyEntry {
                name: String::new(),
                key_env: "AEON_KEY".into(),
            }],
        };
        assert!(config.validate("test").is_err());
    }

    #[test]
    fn auth_config_validate_api_key_empty_env() {
        let config = AuthConfig {
            mode: AuthMode::ApiKey,
            api_keys: vec![ApiKeyEntry {
                name: "admin".into(),
                key_env: String::new(),
            }],
        };
        assert!(config.validate("test").is_err());
    }

    #[test]
    fn auth_config_validate_api_key_valid() {
        let config = AuthConfig {
            mode: AuthMode::ApiKey,
            api_keys: vec![ApiKeyEntry {
                name: "admin".into(),
                key_env: "AEON_ADMIN_KEY".into(),
            }],
        };
        assert!(config.validate("test").is_ok());
    }

    #[test]
    fn auth_config_validate_mtls() {
        let config = AuthConfig {
            mode: AuthMode::Mtls,
            api_keys: Vec::new(),
        };
        assert!(config.validate("test").is_ok());
    }

    #[test]
    fn authenticator_from_config() {
        // SAFETY: test-only env var manipulation, single-threaded test
        unsafe { std::env::set_var("AEON_TEST_AUTH_KEY_1", "secret-key-12345") };
        let entries = vec![ApiKeyEntry {
            name: "test-key".into(),
            key_env: "AEON_TEST_AUTH_KEY_1".into(),
        }];
        let auth = ApiKeyAuthenticator::from_config(&entries).unwrap();
        assert!(auth.validate_bearer("secret-key-12345").is_some());
        assert!(auth.validate_bearer("wrong-key").is_none());
        unsafe { std::env::remove_var("AEON_TEST_AUTH_KEY_1") };
    }

    #[test]
    fn authenticator_missing_env_var() {
        let entries = vec![ApiKeyEntry {
            name: "missing".into(),
            key_env: "AEON_TEST_AUTH_NONEXISTENT_VAR_XYZ".into(),
        }];
        assert!(ApiKeyAuthenticator::from_config(&entries).is_err());
    }

    #[test]
    fn authenticator_empty_env_var() {
        unsafe { std::env::set_var("AEON_TEST_AUTH_KEY_EMPTY", "") };
        let entries = vec![ApiKeyEntry {
            name: "empty".into(),
            key_env: "AEON_TEST_AUTH_KEY_EMPTY".into(),
        }];
        assert!(ApiKeyAuthenticator::from_config(&entries).is_err());
        unsafe { std::env::remove_var("AEON_TEST_AUTH_KEY_EMPTY") };
    }

    #[test]
    fn authenticator_validate_header() {
        unsafe { std::env::set_var("AEON_TEST_AUTH_KEY_2", "my-secret-token") };
        let entries = vec![ApiKeyEntry {
            name: "ci".into(),
            key_env: "AEON_TEST_AUTH_KEY_2".into(),
        }];
        let auth = ApiKeyAuthenticator::from_config(&entries).unwrap();

        // Valid header
        let name = auth.validate_header("Bearer my-secret-token").unwrap();
        assert_eq!(name, "ci");

        // Wrong token
        assert!(auth.validate_header("Bearer wrong-token").is_err());

        // Missing Bearer prefix
        assert!(auth.validate_header("my-secret-token").is_err());

        // Empty
        assert!(auth.validate_header("Bearer ").is_err());

        unsafe { std::env::remove_var("AEON_TEST_AUTH_KEY_2") };
    }

    #[test]
    fn authenticator_multiple_keys() {
        unsafe {
            std::env::set_var("AEON_TEST_AUTH_MULTI_A", "key-alpha");
            std::env::set_var("AEON_TEST_AUTH_MULTI_B", "key-beta");
        }
        let entries = vec![
            ApiKeyEntry {
                name: "alpha".into(),
                key_env: "AEON_TEST_AUTH_MULTI_A".into(),
            },
            ApiKeyEntry {
                name: "beta".into(),
                key_env: "AEON_TEST_AUTH_MULTI_B".into(),
            },
        ];
        let auth = ApiKeyAuthenticator::from_config(&entries).unwrap();

        assert_eq!(auth.validate_bearer("key-alpha"), Some("alpha"));
        assert_eq!(auth.validate_bearer("key-beta"), Some("beta"));
        assert!(auth.validate_bearer("key-gamma").is_none());

        unsafe {
            std::env::remove_var("AEON_TEST_AUTH_MULTI_A");
            std::env::remove_var("AEON_TEST_AUTH_MULTI_B");
        }
    }

    #[test]
    fn resolved_key_debug_redacted() {
        let key = ResolvedApiKey {
            name: "test".into(),
            key: "secret".into(),
        };
        let debug = format!("{key:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
