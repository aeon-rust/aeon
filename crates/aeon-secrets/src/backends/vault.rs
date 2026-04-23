//! HashiCorp Vault / OpenBao KV-v2 [`SecretProvider`] backend.
//!
//! OpenBao is API-compatible with Vault — one adapter covers both,
//! selected by pointing `endpoint` at whichever server you run.
//!
//! ## Sync vs async
//!
//! [`SecretProvider::resolve`] is a sync trait method. KV-v2 reads
//! happen at pipeline-start time (resolving auth tokens, KEK refs,
//! TLS keys), never on the hot event path, so this adapter uses
//! [`reqwest::blocking`] rather than dragging async into the resolve
//! signature.
//!
//! ## Auth methods
//!
//! - `Token` — static `client_token`, supplied via a secret ref
//!   (`${ENV:VAULT_TOKEN}` is the typical shape).
//! - `AppRole` — `role_id` + `secret_id` via secret refs; the adapter
//!   performs the initial `POST /v1/auth/approle/login` eagerly at
//!   construction so misconfiguration surfaces at startup, then caches
//!   the resulting `client_token` and re-logs-in on a `403` response.
//! - `Kubernetes` — declared in the config enum but not implemented in
//!   this commit; surfaces [`SecretsAdapterError::BackendNotImplemented`].
//!
//! ## Path syntax
//!
//! `resolve(path)` reads `GET /v1/{mount}/data/{path}`. KV-v2 returns a
//! map of fields — the adapter picks the field named after a `#`
//! suffix on the path (`secret/aeon/db#password`), or `value` by
//! default.

use std::sync::Mutex;

use aeon_types::{
    AeonError, SecretBytes, SecretError, SecretProvider, SecretRef, SecretRegistry,
    SecretScheme,
};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};

use crate::config::{VaultAuthConfig, VaultProviderConfig};
use crate::error::SecretsAdapterError;

/// Vault / OpenBao KV-v2 [`SecretProvider`].
pub struct VaultKvProvider {
    http: Client,
    endpoint: String,
    mount: String,
    namespace: Option<String>,
    auth: VaultAuth,
}

enum VaultAuth {
    Token(String),
    AppRole {
        role_id: String,
        secret_id: String,
        current_token: Mutex<Option<String>>,
    },
}

impl std::fmt::Debug for VaultKvProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let method = match &self.auth {
            VaultAuth::Token(_) => "token",
            VaultAuth::AppRole { .. } => "app_role",
        };
        f.debug_struct("VaultKvProvider")
            .field("endpoint", &self.endpoint)
            .field("mount", &self.mount)
            .field("namespace", &self.namespace)
            .field("auth_method", &method)
            .finish()
    }
}

impl VaultKvProvider {
    /// Construct a provider from config, resolving every `${...}` ref
    /// in the auth block via `bootstrap`. For AppRole, performs an
    /// eager login so config errors surface here, not at first read.
    pub fn from_config(
        cfg: &VaultProviderConfig,
        bootstrap: &SecretRegistry,
    ) -> Result<Self, SecretsAdapterError> {
        let http = Client::builder()
            .danger_accept_invalid_certs(!cfg.tls_verify)
            .build()
            .map_err(|e| http_err("build client", e))?;

        let endpoint = cfg.endpoint.trim_end_matches('/').to_string();
        let mount = cfg.mount.trim_matches('/').to_string();
        if mount.is_empty() {
            return Err(config_err("vault mount must not be empty".into()));
        }

        let auth = match &cfg.auth {
            VaultAuthConfig::Token { token_ref } => {
                let bytes = resolve_ref(bootstrap, token_ref)?;
                let token = bytes_to_string(&bytes, "token_ref")?;
                VaultAuth::Token(token)
            }
            VaultAuthConfig::AppRole {
                role_id_ref,
                secret_id_ref,
            } => {
                let role_bytes = resolve_ref(bootstrap, role_id_ref)?;
                let secret_bytes = resolve_ref(bootstrap, secret_id_ref)?;
                let role_id = bytes_to_string(&role_bytes, "role_id_ref")?;
                let secret_id = bytes_to_string(&secret_bytes, "secret_id_ref")?;
                VaultAuth::AppRole {
                    role_id,
                    secret_id,
                    current_token: Mutex::new(None),
                }
            }
            VaultAuthConfig::Kubernetes { .. } => {
                return Err(SecretsAdapterError::BackendNotImplemented {
                    backend: "vault_kubernetes_auth",
                });
            }
        };

        let provider = Self {
            http,
            endpoint,
            mount,
            namespace: cfg.namespace.clone(),
            auth,
        };

        if matches!(provider.auth, VaultAuth::AppRole { .. }) {
            provider.login_approle()?;
        }

        Ok(provider)
    }

    fn current_token(&self) -> Result<String, SecretsAdapterError> {
        match &self.auth {
            VaultAuth::Token(t) => Ok(t.clone()),
            VaultAuth::AppRole { current_token, .. } => {
                {
                    let guard = current_token.lock().map_err(|_| poisoned())?;
                    if let Some(t) = guard.as_ref() {
                        return Ok(t.clone());
                    }
                }
                self.login_approle()?;
                let guard = current_token.lock().map_err(|_| poisoned())?;
                guard
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| config_err("AppRole login produced no token".into()))
            }
        }
    }

    fn login_approle(&self) -> Result<(), SecretsAdapterError> {
        let (role_id, secret_id, slot) = match &self.auth {
            VaultAuth::AppRole {
                role_id,
                secret_id,
                current_token,
            } => (role_id.as_str(), secret_id.as_str(), current_token),
            _ => {
                return Err(config_err(
                    "internal: login_approle on non-AppRole auth".into(),
                ));
            }
        };

        #[derive(Serialize)]
        struct Body<'a> {
            role_id: &'a str,
            secret_id: &'a str,
        }
        #[derive(Deserialize)]
        struct AuthResp {
            auth: AuthInner,
        }
        #[derive(Deserialize)]
        struct AuthInner {
            client_token: String,
        }

        let url = format!("{}/v1/auth/approle/login", self.endpoint);
        let mut req = self.http.post(&url).json(&Body { role_id, secret_id });
        if let Some(ns) = &self.namespace {
            req = req.header("X-Vault-Namespace", ns);
        }
        let resp = req
            .send()
            .map_err(|e| http_err("approle login send", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(config_err(format!(
                "Vault AppRole login failed: HTTP {status} — {body}"
            )));
        }
        let parsed: AuthResp = resp
            .json()
            .map_err(|e| http_err("approle login parse", e))?;
        let mut guard = slot.lock().map_err(|_| poisoned())?;
        *guard = Some(parsed.auth.client_token);
        Ok(())
    }

    fn invalidate_token(&self) {
        if let VaultAuth::AppRole { current_token, .. } = &self.auth {
            if let Ok(mut guard) = current_token.lock() {
                *guard = None;
            }
        }
    }

    fn build_get(&self, url: &str, token: &str) -> reqwest::blocking::RequestBuilder {
        let mut req = self.http.get(url).header("X-Vault-Token", token);
        if let Some(ns) = &self.namespace {
            req = req.header("X-Vault-Namespace", ns);
        }
        req
    }

    fn fetch_kv(&self, path: &str, field: &str) -> Result<SecretBytes, SecretError> {
        let url = format!(
            "{}/v1/{}/data/{}",
            self.endpoint,
            self.mount,
            path.trim_start_matches('/'),
        );

        let token = self
            .current_token()
            .map_err(|e| provider_err(e.to_string()))?;
        let resp = self
            .build_get(&url, &token)
            .send()
            .map_err(|e| provider_err(format!("vault GET {url} send: {e}")))?;

        if resp.status() == StatusCode::FORBIDDEN
            && matches!(self.auth, VaultAuth::AppRole { .. })
        {
            self.invalidate_token();
            let token = self
                .current_token()
                .map_err(|e| provider_err(e.to_string()))?;
            let resp = self
                .build_get(&url, &token)
                .send()
                .map_err(|e| provider_err(format!("vault GET retry {url} send: {e}")))?;
            return decode_response(resp, field);
        }

        decode_response(resp, field)
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
    let value = parsed.data.data.get(field).ok_or_else(|| {
        provider_err(format!("vault KV-v2 path missing field '{field}'"))
    })?;
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
        let (kv_path, field) = match path.rsplit_once('#') {
            Some((p, f)) => (p, f),
            None => (path, "value"),
        };
        self.fetch_kv(kv_path, field)
    }
}

// ─── helpers ───────────────────────────────────────────────────────

fn resolve_ref(bootstrap: &SecretRegistry, raw: &str) -> Result<SecretBytes, SecretsAdapterError> {
    let parsed = SecretRef::parse(raw).map_err(|e| config_err(e.to_string()))?;
    let r = parsed.unwrap_or_else(|| SecretRef::literal(raw));
    bootstrap
        .resolve(&r)
        .map_err(|e| config_err(format!("resolve '{raw}': {e}")))
}

fn bytes_to_string(b: &SecretBytes, what: &str) -> Result<String, SecretsAdapterError> {
    let s = b
        .expose_str()
        .map_err(|_| config_err(format!("{what} resolved to non-UTF-8 bytes")))?;
    Ok(s.trim().to_string())
}

fn http_err(what: &'static str, e: reqwest::Error) -> SecretsAdapterError {
    SecretsAdapterError::Backend(AeonError::Config {
        message: format!("vault HTTP {what}: {e}"),
    })
}

fn config_err(message: String) -> SecretsAdapterError {
    SecretsAdapterError::Backend(AeonError::Config { message })
}

fn provider_err(message: String) -> SecretError {
    SecretError::Provider {
        scheme: SecretScheme::Vault,
        message,
    }
}

fn poisoned() -> SecretsAdapterError {
    SecretsAdapterError::Backend(AeonError::Config {
        message: "vault provider mutex poisoned".to_string(),
    })
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
            .with_body(
                json!({
                    "data": { "data": { "value": "shh-it-is-a-secret" } }
                })
                .to_string(),
            )
            .create();

        let provider =
            VaultKvProvider::from_config(&token_cfg(&server.url(), "secret", "literal-token"), &bootstrap())
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
            .with_body(
                json!({
                    "auth": { "client_token": "approle-issued-token" }
                })
                .to_string(),
            )
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
        login.assert(); // eager login at construction

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
        // Two logins expected: one eager at construction, one after 403.
        let login = server
            .mock("POST", "/v1/auth/approle/login")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({ "auth": { "client_token": "fresh-token" } }).to_string(),
            )
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
