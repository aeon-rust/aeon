//! Vault / OpenBao KV-v2 read-side [`SecretProvider`].
//!
//! ## Sync vs async
//!
//! [`SecretProvider::resolve`] is a sync trait method. KV-v2 reads
//! happen at pipeline-start time (resolving auth tokens, KEK refs,
//! TLS keys), never on the hot event path, so this adapter uses
//! `reqwest::blocking` rather than dragging async into the resolve
//! signature.
//!
//! ## Path syntax
//!
//! `resolve(path)` reads `GET /v1/{mount}/data/{path}`. KV-v2 returns
//! a map of fields — the adapter picks the field named after a `#`
//! suffix on the path (`secret/aeon/db#password`), or `value` by
//! default.

use aeon_types::{SecretBytes, SecretError, SecretProvider, SecretRegistry, SecretScheme};
use reqwest::blocking::Response;
use serde::Deserialize;
use std::time::Duration;

use crate::backends::vault::auth::{VaultAuthClient, config_err};
use crate::backends::vault::cache::SecretCache;
use crate::backends::vault::retry::{
    Classification, RetryPolicyExt, classify_status, is_transient_network_error,
};
use crate::config::{RetryPolicy, VaultProviderConfig};
use crate::error::SecretsAdapterError;

/// Vault / OpenBao KV-v2 read-side provider.
pub struct VaultKvProvider {
    auth: VaultAuthClient,
    mount: String,
    retry: RetryPolicy,
    cache: SecretCache,
}

impl std::fmt::Debug for VaultKvProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultKvProvider")
            .field("mount", &self.mount)
            .field("auth", &self.auth)
            .field("retry_attempts", &self.retry.max_attempts)
            .field("cache", &self.cache)
            .finish()
    }
}

impl VaultKvProvider {
    pub fn from_config(
        cfg: &VaultProviderConfig,
        bootstrap: &SecretRegistry,
    ) -> Result<Self, SecretsAdapterError> {
        let mount = cfg.mount.trim_matches('/').to_string();
        if mount.is_empty() {
            return Err(config_err("vault mount must not be empty".into()));
        }

        let auth = VaultAuthClient::new(
            &cfg.endpoint,
            cfg.namespace.as_deref(),
            cfg.tls_verify,
            &cfg.auth,
            bootstrap,
        )?;

        Ok(Self {
            auth,
            mount,
            retry: cfg.retry,
            cache: SecretCache::new(Duration::from_secs(cfg.cache_ttl_secs)),
        })
    }

    fn fetch_kv(&self, path: &str, field: &str) -> Result<SecretBytes, SecretError> {
        let url_path = format!("/v1/{}/data/{}", self.mount, path.trim_start_matches('/'),);

        let mut attempt: u32 = 0;
        let mut auth_retried = false;
        let mut backoff = self.retry.backoff.iter();

        loop {
            let req = self
                .auth
                .get(&url_path)
                .map_err(|e| provider_err(e.to_string()))?;
            let send_result = req.send();
            let resp = match send_result {
                Ok(r) => r,
                Err(e) => {
                    if is_transient_network_error(&e) && self.retry.can_retry(attempt) {
                        std::thread::sleep(backoff.next_delay());
                        attempt += 1;
                        continue;
                    }
                    return Err(provider_err(format!("vault GET {url_path} send: {e}")));
                }
            };

            match classify_status(resp.status()) {
                Classification::Success | Classification::Permanent => {
                    return decode_response(resp, field);
                }
                Classification::Auth if self.auth.is_approle() && !auth_retried => {
                    self.auth.invalidate_token();
                    auth_retried = true;
                    continue;
                }
                Classification::Auth => {
                    // Static-token 403 or already retried — surface the
                    // error response through the normal decode path.
                    return decode_response(resp, field);
                }
                Classification::Transient => {
                    if self.retry.can_retry(attempt) {
                        std::thread::sleep(backoff.next_delay());
                        attempt += 1;
                        continue;
                    }
                    return decode_response(resp, field);
                }
            }
        }
    }
}

fn decode_response(resp: Response, field: &str) -> Result<SecretBytes, SecretError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(provider_err(format!(
            "vault read failed: HTTP {status} — {body}"
        )));
    }

    #[derive(Deserialize)]
    struct ReadResp {
        data: ReadData,
    }
    #[derive(Deserialize)]
    struct ReadData {
        data: serde_json::Map<String, serde_json::Value>,
    }

    let parsed: ReadResp = resp
        .json()
        .map_err(|e| provider_err(format!("vault response parse: {e}")))?;
    let value = parsed
        .data
        .data
        .get(field)
        .ok_or_else(|| provider_err(format!("vault KV-v2 path missing field '{field}'")))?;
    let bytes = match value {
        serde_json::Value::String(s) => s.as_bytes().to_vec(),
        other => other.to_string().into_bytes(),
    };
    Ok(SecretBytes::new(bytes))
}

impl SecretProvider for VaultKvProvider {
    fn scheme(&self) -> SecretScheme {
        SecretScheme::Vault
    }

    fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
        if let Some(cached) = self.cache.get(path) {
            return Ok(cached);
        }
        let (kv_path, field) = match path.rsplit_once('#') {
            Some((p, f)) => (p, f),
            None => (path, "value"),
        };
        let bytes = self.fetch_kv(kv_path, field)?;
        // Store a fresh copy under the original key so the next resolve
        // short-circuits. SecretBytes is !Clone on purpose, so we re-
        // allocate rather than share — acceptable on the startup path.
        self.cache.put(
            path.to_string(),
            SecretBytes::new(bytes.expose_bytes().to_vec()),
        );
        Ok(bytes)
    }
}

fn provider_err(message: String) -> SecretError {
    SecretError::Provider {
        scheme: SecretScheme::Vault,
        message,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{VaultAuthConfig, VaultProviderConfig};
    use serde_json::json;

    fn bootstrap() -> SecretRegistry {
        SecretRegistry::default_local()
    }

    fn token_cfg(endpoint: &str, mount: &str, token: &str) -> VaultProviderConfig {
        VaultProviderConfig {
            endpoint: endpoint.to_string(),
            mount: mount.to_string(),
            auth: VaultAuthConfig::Token {
                token_ref: token.to_string(),
            },
            namespace: None,
            tls_verify: false,
            ca_cert_ref: None,
            retry: fast_retry(),
            cache_ttl_secs: 0, // disable cache by default in tests
        }
    }

    fn approle_cfg(endpoint: &str, mount: &str, role: &str, secret: &str) -> VaultProviderConfig {
        VaultProviderConfig {
            endpoint: endpoint.to_string(),
            mount: mount.to_string(),
            auth: VaultAuthConfig::AppRole {
                role_id_ref: role.to_string(),
                secret_id_ref: secret.to_string(),
            },
            namespace: None,
            tls_verify: false,
            ca_cert_ref: None,
            retry: fast_retry(),
            cache_ttl_secs: 0,
        }
    }

    /// Tight backoff so the retry tests finish in milliseconds without
    /// papering over the policy logic.
    fn fast_retry() -> crate::config::RetryPolicy {
        crate::config::RetryPolicy {
            max_attempts: 3,
            backoff: aeon_types::backoff::BackoffPolicy {
                initial_ms: 1,
                max_ms: 5,
                multiplier: 2.0,
                jitter_pct: 0.0,
            },
        }
    }

    #[test]
    fn token_auth_resolves_default_value_field() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/v1/secret/data/aeon/db")
            .match_header("X-Vault-Token", "literal-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": { "data": { "value": "shh-it-is-a-secret" } } }).to_string())
            .create();

        let provider = VaultKvProvider::from_config(
            &token_cfg(&server.url(), "secret", "literal-token"),
            &bootstrap(),
        )
        .unwrap();

        let bytes = provider.resolve("aeon/db").unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "shh-it-is-a-secret");
        m.assert();
    }

    #[test]
    fn token_auth_picks_named_field_after_hash() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/v1/secret/data/aeon/db")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "data": { "data": { "value": "v", "password": "hunter2" } }
                })
                .to_string(),
            )
            .create();

        let provider =
            VaultKvProvider::from_config(&token_cfg(&server.url(), "secret", "t"), &bootstrap())
                .unwrap();
        let bytes = provider.resolve("aeon/db#password").unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "hunter2");
        m.assert();
    }

    #[test]
    fn token_auth_missing_field_errors() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/v1/secret/data/aeon/db")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": { "data": { "value": "x" } } }).to_string())
            .create();

        let provider =
            VaultKvProvider::from_config(&token_cfg(&server.url(), "secret", "t"), &bootstrap())
                .unwrap();
        let err = provider.resolve("aeon/db#missing").unwrap_err();
        assert!(matches!(err, SecretError::Provider { .. }));
    }

    #[test]
    fn custom_mount_is_honored() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/v1/kv-prod/data/api/keys")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": { "data": { "value": "k1" } } }).to_string())
            .create();

        let provider =
            VaultKvProvider::from_config(&token_cfg(&server.url(), "kv-prod", "t"), &bootstrap())
                .unwrap();
        provider.resolve("api/keys").unwrap();
        m.assert();
    }

    #[test]
    fn namespace_header_is_propagated() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/v1/secret/data/x")
            .match_header("X-Vault-Namespace", "tenant-a")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": { "data": { "value": "v" } } }).to_string())
            .create();

        let mut cfg = token_cfg(&server.url(), "secret", "t");
        cfg.namespace = Some("tenant-a".to_string());
        let provider = VaultKvProvider::from_config(&cfg, &bootstrap()).unwrap();
        provider.resolve("x").unwrap();
        m.assert();
    }

    #[test]
    fn http_404_is_provider_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/v1/secret/data/missing")
            .with_status(404)
            .with_body("{}")
            .create();
        let provider =
            VaultKvProvider::from_config(&token_cfg(&server.url(), "secret", "t"), &bootstrap())
                .unwrap();
        let err = provider.resolve("missing").unwrap_err();
        assert!(matches!(err, SecretError::Provider { .. }));
    }

    #[test]
    fn approle_logs_in_eagerly_then_reads() {
        let mut server = mockito::Server::new();
        let login = server
            .mock("POST", "/v1/auth/approle/login")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "auth": { "client_token": "approle-issued-token" } }).to_string())
            .create();
        let read = server
            .mock("GET", "/v1/secret/data/x")
            .match_header("X-Vault-Token", "approle-issued-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": { "data": { "value": "ok" } } }).to_string())
            .create();

        let provider = VaultKvProvider::from_config(
            &approle_cfg(&server.url(), "secret", "role-uuid", "secret-uuid"),
            &bootstrap(),
        )
        .unwrap();
        login.assert();

        let bytes = provider.resolve("x").unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "ok");
        read.assert();
    }

    #[test]
    fn approle_login_failure_returns_config_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/v1/auth/approle/login")
            .with_status(400)
            .with_body(r#"{"errors":["invalid role_id"]}"#)
            .create();

        let err = VaultKvProvider::from_config(
            &approle_cfg(&server.url(), "secret", "bad", "bad"),
            &bootstrap(),
        )
        .unwrap_err();
        assert!(matches!(err, SecretsAdapterError::Backend(_)));
    }

    #[test]
    fn approle_re_authenticates_on_403() {
        let mut server = mockito::Server::new();
        let login = server
            .mock("POST", "/v1/auth/approle/login")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "auth": { "client_token": "fresh-token" } }).to_string())
            .expect(2)
            .create();
        let _denied = server
            .mock("GET", "/v1/secret/data/x")
            .with_status(403)
            .with_body("{}")
            .expect(1)
            .create();
        let _granted = server
            .mock("GET", "/v1/secret/data/x")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": { "data": { "value": "ok" } } }).to_string())
            .expect(1)
            .create();

        let provider = VaultKvProvider::from_config(
            &approle_cfg(&server.url(), "secret", "role", "secret"),
            &bootstrap(),
        )
        .unwrap();
        let bytes = provider.resolve("x").unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "ok");
        login.assert();
    }

    #[test]
    fn kubernetes_auth_returns_not_implemented() {
        let cfg = VaultProviderConfig {
            endpoint: "https://v.example.com".to_string(),
            mount: "secret".to_string(),
            auth: VaultAuthConfig::Kubernetes {
                role: "aeon".to_string(),
                token_path: "/var/run/secrets/kubernetes.io/serviceaccount/token".into(),
            },
            namespace: None,
            tls_verify: true,
            ca_cert_ref: None,
            retry: fast_retry(),
            cache_ttl_secs: 0,
        };
        let err = VaultKvProvider::from_config(&cfg, &bootstrap()).unwrap_err();
        assert!(matches!(
            err,
            SecretsAdapterError::BackendNotImplemented {
                backend: "vault_kubernetes_auth"
            }
        ));
    }

    #[test]
    fn empty_mount_is_rejected() {
        let cfg = token_cfg("https://v.example.com", "/", "t");
        let err = VaultKvProvider::from_config(&cfg, &bootstrap()).unwrap_err();
        assert!(matches!(err, SecretsAdapterError::Backend(_)));
    }

    #[test]
    fn cache_hit_short_circuits_network_call() {
        let mut server = mockito::Server::new();
        // .expect(1) — a second network hit would fail the assertion.
        let m = server
            .mock("GET", "/v1/secret/data/aeon/db")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": { "data": { "value": "cached-v" } } }).to_string())
            .expect(1)
            .create();

        let mut cfg = token_cfg(&server.url(), "secret", "t");
        cfg.cache_ttl_secs = 60;
        let provider = VaultKvProvider::from_config(&cfg, &bootstrap()).unwrap();

        let first = provider.resolve("aeon/db").unwrap();
        let second = provider.resolve("aeon/db").unwrap();
        assert_eq!(first.expose_str().unwrap(), "cached-v");
        assert_eq!(second.expose_str().unwrap(), "cached-v");
        m.assert();
    }

    #[test]
    fn retry_succeeds_on_transient_503_then_200() {
        let mut server = mockito::Server::new();
        let fail = server
            .mock("GET", "/v1/secret/data/aeon/db")
            .with_status(503)
            .with_body("{}")
            .expect(1)
            .create();
        let ok = server
            .mock("GET", "/v1/secret/data/aeon/db")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": { "data": { "value": "after-retry" } } }).to_string())
            .expect(1)
            .create();

        let provider =
            VaultKvProvider::from_config(&token_cfg(&server.url(), "secret", "t"), &bootstrap())
                .unwrap();
        let bytes = provider.resolve("aeon/db").unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "after-retry");
        fail.assert();
        ok.assert();
    }

    #[test]
    fn retry_exhausted_surfaces_last_error() {
        let mut server = mockito::Server::new();
        // fast_retry() sets max_attempts=3 → one initial + two retries.
        let m = server
            .mock("GET", "/v1/secret/data/x")
            .with_status(503)
            .with_body("{}")
            .expect(3)
            .create();

        let provider =
            VaultKvProvider::from_config(&token_cfg(&server.url(), "secret", "t"), &bootstrap())
                .unwrap();
        let err = provider.resolve("x").unwrap_err();
        assert!(matches!(err, SecretError::Provider { .. }));
        m.assert();
    }

    #[test]
    fn debug_does_not_leak_token() {
        let server = mockito::Server::new();
        let provider = VaultKvProvider::from_config(
            &token_cfg(&server.url(), "secret", "ultra-secret-token-xyz"),
            &bootstrap(),
        )
        .unwrap();
        let dbg = format!("{provider:?}");
        assert!(!dbg.contains("ultra-secret-token-xyz"), "got: {dbg}");
        assert!(dbg.contains("token"), "expected method label, got: {dbg}");
    }
}
