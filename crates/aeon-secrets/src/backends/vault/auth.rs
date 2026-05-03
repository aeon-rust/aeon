//! Shared HTTP + auth plumbing for the Vault / OpenBao adapters.
//!
//! Owns the `reqwest::blocking::Client`, the resolved auth state
//! (Token or AppRole), and the cached `client_token`. AppRole logins
//! are eager (run at construction so config errors surface at startup)
//! and re-runs are triggered by `invalidate_token()` — which the
//! engine adapters call after a `403` from a downstream request.
//!
//! Both [`crate::backends::vault::kv::VaultKvProvider`] and
//! [`crate::backends::vault::transit::VaultTransitKekProvider`] hold
//! one of these clients each.

use std::sync::Mutex;

use aeon_types::{AeonError, SecretBytes, SecretRef, SecretRegistry};
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};

use crate::config::VaultAuthConfig;
use crate::error::SecretsAdapterError;

/// HTTP + auth client shared by the KV-v2 and Transit adapters.
pub struct VaultAuthClient {
    http: Client,
    endpoint: String,
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

impl std::fmt::Debug for VaultAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let method = match &self.auth {
            VaultAuth::Token(_) => "token",
            VaultAuth::AppRole { .. } => "app_role",
        };
        f.debug_struct("VaultAuthClient")
            .field("endpoint", &self.endpoint)
            .field("namespace", &self.namespace)
            .field("auth_method", &method)
            .finish()
    }
}

impl VaultAuthClient {
    /// Construct from raw config pieces. `bootstrap` resolves any
    /// `${...}` refs in the auth block (typically `${ENV:VAULT_TOKEN}`).
    /// For AppRole, performs an eager login so misconfiguration
    /// surfaces here, not at first request.
    pub fn new(
        endpoint: &str,
        namespace: Option<&str>,
        tls_verify: bool,
        auth_cfg: &VaultAuthConfig,
        bootstrap: &SecretRegistry,
    ) -> Result<Self, SecretsAdapterError> {
        let http = Client::builder()
            .danger_accept_invalid_certs(!tls_verify)
            .build()
            .map_err(|e| http_err("build client", e))?;

        let auth = match resolve_auth(auth_cfg, bootstrap)? {
            ResolvedAuth::Token(t) => VaultAuth::Token(t),
            ResolvedAuth::AppRole { role_id, secret_id } => VaultAuth::AppRole {
                role_id,
                secret_id,
                current_token: Mutex::new(None),
            },
        };

        let client = Self {
            http,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            namespace: namespace.map(str::to_string),
            auth,
        };

        if matches!(client.auth, VaultAuth::AppRole { .. }) {
            client.login_approle()?;
        }
        Ok(client)
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    pub fn is_approle(&self) -> bool {
        matches!(self.auth, VaultAuth::AppRole { .. })
    }

    /// Build a GET against `{endpoint}{path}`, with the current Vault
    /// token + namespace header attached. `path` must start with `/`.
    pub fn get(&self, path: &str) -> Result<RequestBuilder, SecretsAdapterError> {
        let url = format!("{}{}", self.endpoint, path);
        self.attach(self.http.get(url))
    }

    /// Build a POST against `{endpoint}{path}`, with token + namespace.
    pub fn post(&self, path: &str) -> Result<RequestBuilder, SecretsAdapterError> {
        let url = format!("{}{}", self.endpoint, path);
        self.attach(self.http.post(url))
    }

    fn attach(&self, mut req: RequestBuilder) -> Result<RequestBuilder, SecretsAdapterError> {
        let token = self.current_token()?;
        req = req.header("X-Vault-Token", token);
        if let Some(ns) = &self.namespace {
            req = req.header("X-Vault-Namespace", ns);
        }
        Ok(req)
    }

    /// Drop the cached AppRole token so the next request triggers a
    /// fresh login. No-op for static-token auth.
    pub fn invalidate_token(&self) {
        if let VaultAuth::AppRole { current_token, .. } = &self.auth {
            if let Ok(mut guard) = current_token.lock() {
                *guard = None;
            }
        }
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
        let resp = req.send().map_err(|e| http_err("approle login send", e))?;
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
}

// ─── helpers shared by the engine adapters ──────────────────────────

/// Resolved (de-referenced) auth credentials, ready to use against
/// either a blocking or async HTTP client.
pub(crate) enum ResolvedAuth {
    Token(String),
    AppRole { role_id: String, secret_id: String },
}

pub(crate) fn resolve_auth(
    cfg: &VaultAuthConfig,
    bootstrap: &SecretRegistry,
) -> Result<ResolvedAuth, SecretsAdapterError> {
    match cfg {
        VaultAuthConfig::Token { token_ref } => {
            let bytes = resolve_ref(bootstrap, token_ref)?;
            Ok(ResolvedAuth::Token(bytes_to_string(&bytes, "token_ref")?))
        }
        VaultAuthConfig::AppRole {
            role_id_ref,
            secret_id_ref,
        } => {
            let role_id = bytes_to_string(&resolve_ref(bootstrap, role_id_ref)?, "role_id_ref")?;
            let secret_id =
                bytes_to_string(&resolve_ref(bootstrap, secret_id_ref)?, "secret_id_ref")?;
            Ok(ResolvedAuth::AppRole { role_id, secret_id })
        }
        VaultAuthConfig::Kubernetes { .. } => Err(SecretsAdapterError::BackendNotImplemented {
            backend: "vault_kubernetes_auth",
        }),
    }
}

pub(crate) fn resolve_ref(
    bootstrap: &SecretRegistry,
    raw: &str,
) -> Result<SecretBytes, SecretsAdapterError> {
    let parsed = SecretRef::parse(raw).map_err(|e| config_err(e.to_string()))?;
    let r = parsed.unwrap_or_else(|| SecretRef::literal(raw));
    bootstrap
        .resolve(&r)
        .map_err(|e| config_err(format!("resolve '{raw}': {e}")))
}

pub(crate) fn bytes_to_string(b: &SecretBytes, what: &str) -> Result<String, SecretsAdapterError> {
    let s = b
        .expose_str()
        .map_err(|_| config_err(format!("{what} resolved to non-UTF-8 bytes")))?;
    Ok(s.trim().to_string())
}

pub(crate) fn http_err(what: &'static str, e: reqwest::Error) -> SecretsAdapterError {
    SecretsAdapterError::Backend(AeonError::Config {
        message: format!("vault HTTP {what}: {e}"),
    })
}

pub(crate) fn config_err(message: String) -> SecretsAdapterError {
    SecretsAdapterError::Backend(AeonError::Config { message })
}

fn poisoned() -> SecretsAdapterError {
    SecretsAdapterError::Backend(AeonError::Config {
        message: "vault auth client mutex poisoned".to_string(),
    })
}
