//! Vault / OpenBao Transit-engine [`KekProvider`].
//!
//! Wrap and unwrap operations are performed by the Transit engine —
//! KEK bytes never enter the Aeon process. The trait surface stays
//! identical to the in-process `KekHandle` path; `KekRegistry`
//! dispatches by domain and the Transit provider handles its own
//! request shape.
//!
//! ## Why async (vs. KV-v2's blocking)
//!
//! [`KekProvider`] is async — `wrap_new_dek`/`unwrap_dek` are called
//! from the engine runtime on every wrap/unwrap, not just at startup.
//! Driving them through `reqwest::blocking` would panic
//! ("Cannot start a runtime from within a runtime") because the
//! blocking client spins up its own internal runtime. So Transit uses
//! `reqwest::Client` (async) directly.
//!
//! ## Wire format
//!
//! Transit ciphertext is a UTF-8 string of the form
//! `vault:v{N}:{base64-payload}`. We store those bytes verbatim in
//! [`WrappedDek::ciphertext`] and leave `nonce` empty — Transit
//! manages nonces internally.
//!
//! ## Versioning
//!
//! Transit auto-versions keys; the version is encoded in the
//! ciphertext (`vault:v3:...`). Decryption picks the right version
//! automatically, so we don't need a separate "previous_id" — we
//! always report the configured `key_name` as the active id and
//! return `None` for previous_id.
//!
//! ## Endpoints used
//!
//! - `POST /v1/{mount}/datakey/plaintext/{key_name}` — generate a
//!   fresh DEK; returns `{ data: { plaintext: <b64>, ciphertext: <str> } }`.
//! - `POST /v1/{mount}/encrypt/{key_name}` — wrap an existing DEK.
//! - `POST /v1/{mount}/decrypt/{key_name}` — unwrap.

use aeon_crypto::kek::{DekBytes, KekDomain, WrappedDek};
use aeon_crypto::kek_provider::{KekFuture, KekProvider};
use aeon_types::{AeonError, SecretRegistry};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use crate::backends::vault::auth::{ResolvedAuth, config_err, resolve_auth};
use crate::backends::vault::retry::{
    Classification, RetryPolicyExt, classify_status, is_transient_network_error,
};
use crate::config::{RetryPolicy, VaultTransitConfig};
use crate::error::SecretsAdapterError;

/// Vault / OpenBao Transit-engine KEK provider.
pub struct VaultTransitKekProvider {
    http: Client,
    endpoint: String,
    namespace: Option<String>,
    mount: String,
    key_name: String,
    domain: KekDomain,
    auth: AsyncAuth,
    retry: RetryPolicy,
}

enum AsyncAuth {
    Token(String),
    AppRole {
        role_id: String,
        secret_id: String,
        current_token: Mutex<Option<String>>,
    },
}

impl AsyncAuth {
    fn is_approle(&self) -> bool {
        matches!(self, AsyncAuth::AppRole { .. })
    }
}

impl std::fmt::Debug for VaultTransitKekProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let method = match &self.auth {
            AsyncAuth::Token(_) => "token",
            AsyncAuth::AppRole { .. } => "app_role",
        };
        f.debug_struct("VaultTransitKekProvider")
            .field("endpoint", &self.endpoint)
            .field("namespace", &self.namespace)
            .field("mount", &self.mount)
            .field("key_name", &self.key_name)
            .field("domain", &self.domain)
            .field("auth_method", &method)
            .finish()
    }
}

impl VaultTransitKekProvider {
    /// Construct from config. Resolves auth refs synchronously through
    /// the bootstrap registry; for AppRole, performs an eager async
    /// login by spawning a single-request task on the current runtime.
    pub async fn from_config(
        cfg: &VaultTransitConfig,
        bootstrap: &SecretRegistry,
    ) -> Result<Self, SecretsAdapterError> {
        let mount = cfg.mount.trim_matches('/').to_string();
        if mount.is_empty() {
            return Err(config_err("vault transit mount must not be empty".into()));
        }
        if cfg.key_name.is_empty() {
            return Err(config_err("vault transit key_name must not be empty".into()));
        }
        let domain = parse_domain(&cfg.domain)?;

        let http = Client::builder()
            .danger_accept_invalid_certs(!cfg.tls_verify)
            .build()
            .map_err(|e| http_err("build client", e))?;

        let auth = match resolve_auth(&cfg.auth, bootstrap)? {
            ResolvedAuth::Token(t) => AsyncAuth::Token(t),
            ResolvedAuth::AppRole { role_id, secret_id } => AsyncAuth::AppRole {
                role_id,
                secret_id,
                current_token: Mutex::new(None),
            },
        };

        let provider = Self {
            http,
            endpoint: cfg.endpoint.trim_end_matches('/').to_string(),
            namespace: cfg.namespace.clone(),
            mount,
            key_name: cfg.key_name.clone(),
            domain,
            auth,
            retry: cfg.retry,
        };

        if provider.auth.is_approle() {
            provider.login_approle().await?;
        }
        Ok(provider)
    }

    fn datakey_path(&self) -> String {
        format!("/v1/{}/datakey/plaintext/{}", self.mount, self.key_name)
    }

    fn encrypt_path(&self) -> String {
        format!("/v1/{}/encrypt/{}", self.mount, self.key_name)
    }

    fn decrypt_path(&self) -> String {
        format!("/v1/{}/decrypt/{}", self.mount, self.key_name)
    }

    async fn current_token(&self) -> Result<String, SecretsAdapterError> {
        match &self.auth {
            AsyncAuth::Token(t) => Ok(t.clone()),
            AsyncAuth::AppRole { current_token, .. } => {
                {
                    let guard = current_token.lock().map_err(|_| poisoned())?;
                    if let Some(t) = guard.as_ref() {
                        return Ok(t.clone());
                    }
                }
                self.login_approle().await?;
                let guard = current_token.lock().map_err(|_| poisoned())?;
                guard
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| config_err("AppRole login produced no token".into()))
            }
        }
    }

    fn invalidate_token(&self) {
        if let AsyncAuth::AppRole { current_token, .. } = &self.auth {
            if let Ok(mut guard) = current_token.lock() {
                *guard = None;
            }
        }
    }

    async fn login_approle(&self) -> Result<(), SecretsAdapterError> {
        let (role_id, secret_id, slot) = match &self.auth {
            AsyncAuth::AppRole {
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
            .await
            .map_err(|e| http_err("approle login send", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(config_err(format!(
                "Vault AppRole login failed: HTTP {status} — {body}"
            )));
        }
        let parsed: AuthResp = resp
            .json()
            .await
            .map_err(|e| http_err("approle login parse", e))?;
        let mut guard = slot.lock().map_err(|_| poisoned())?;
        *guard = Some(parsed.auth.client_token);
        Ok(())
    }

    async fn build_post<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<RequestBuilder, SecretsAdapterError> {
        let token = self.current_token().await?;
        let url = format!("{}{}", self.endpoint, path);
        let mut req = self
            .http
            .post(&url)
            .header("X-Vault-Token", token)
            .json(body);
        if let Some(ns) = &self.namespace {
            req = req.header("X-Vault-Namespace", ns);
        }
        Ok(req)
    }

    async fn post_with_reauth<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<Response, AeonError> {
        let mut attempt: u32 = 0;
        let mut auth_retried = false;
        let mut backoff = self.retry.backoff.iter();

        loop {
            let req = self.build_post(path, body).await.map_err(secrets_to_aeon)?;
            let send_result = req.send().await;
            let resp = match send_result {
                Ok(r) => r,
                Err(e) => {
                    if is_transient_network_error(&e) && self.retry.can_retry(attempt) {
                        tokio::time::sleep(backoff.next_delay()).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(crypto_err(format!(
                        "vault transit POST {path} send: {e}"
                    )));
                }
            };

            match classify_status(resp.status()) {
                Classification::Success | Classification::Permanent => return Ok(resp),
                Classification::Auth if self.auth.is_approle() && !auth_retried => {
                    self.invalidate_token();
                    auth_retried = true;
                    continue;
                }
                Classification::Auth => return Ok(resp),
                Classification::Transient => {
                    if self.retry.can_retry(attempt) {
                        tokio::time::sleep(backoff.next_delay()).await;
                        attempt += 1;
                        continue;
                    }
                    return Ok(resp);
                }
            }
        }
    }

    async fn wrap_new_dek_async(&self) -> Result<(DekBytes, WrappedDek), AeonError> {
        // Vault Transit's `datakey` endpoint with `bits: 256` returns
        // both plaintext (base64) and ciphertext (vault:v1:...) in one
        // round-trip. Saves a wrap call vs. generate-then-encrypt.
        #[derive(Serialize)]
        struct Body {
            bits: u32,
        }
        #[derive(Deserialize)]
        struct DataResp {
            data: DataInner,
        }
        #[derive(Deserialize)]
        struct DataInner {
            plaintext: String,
            ciphertext: String,
        }

        let path = self.datakey_path();
        let resp = self.post_with_reauth(&path, &Body { bits: 256 }).await?;
        let parsed: DataResp = parse_json(resp, "datakey").await?;

        let plaintext = BASE64
            .decode(parsed.data.plaintext.as_bytes())
            .map_err(|e| crypto_err(format!("vault transit datakey base64: {e}")))?;
        if plaintext.len() != 32 {
            return Err(crypto_err(format!(
                "vault transit datakey returned {} bytes, expected 32",
                plaintext.len()
            )));
        }
        let mut dek_bytes = [0u8; 32];
        dek_bytes.copy_from_slice(&plaintext);
        let dek = DekBytes::from_bytes(dek_bytes);

        let wrapped = WrappedDek {
            kek_domain: self.domain,
            kek_id: self.key_name.clone(),
            nonce: Vec::new(),
            ciphertext: parsed.data.ciphertext.into_bytes(),
        };
        Ok((dek, wrapped))
    }

    async fn wrap_dek_async(&self, dek: &DekBytes) -> Result<WrappedDek, AeonError> {
        #[derive(Serialize)]
        struct Body {
            plaintext: String,
        }
        #[derive(Deserialize)]
        struct EncResp {
            data: EncInner,
        }
        #[derive(Deserialize)]
        struct EncInner {
            ciphertext: String,
        }

        let body = Body {
            plaintext: BASE64.encode(dek.expose_bytes()),
        };
        let path = self.encrypt_path();
        let resp = self.post_with_reauth(&path, &body).await?;
        let parsed: EncResp = parse_json(resp, "encrypt").await?;

        Ok(WrappedDek {
            kek_domain: self.domain,
            kek_id: self.key_name.clone(),
            nonce: Vec::new(),
            ciphertext: parsed.data.ciphertext.into_bytes(),
        })
    }

    async fn unwrap_dek_async(&self, wrapped: &WrappedDek) -> Result<DekBytes, AeonError> {
        if wrapped.kek_domain != self.domain {
            return Err(crypto_err(format!(
                "wrong kek_domain: wrapped={:?} provider={:?}",
                wrapped.kek_domain, self.domain
            )));
        }

        let ciphertext = std::str::from_utf8(&wrapped.ciphertext).map_err(|_| {
            crypto_err("vault transit ciphertext is not valid UTF-8".into())
        })?;

        #[derive(Serialize)]
        struct Body<'a> {
            ciphertext: &'a str,
        }
        #[derive(Deserialize)]
        struct DecResp {
            data: DecInner,
        }
        #[derive(Deserialize)]
        struct DecInner {
            plaintext: String,
        }

        let path = self.decrypt_path();
        let resp = self.post_with_reauth(&path, &Body { ciphertext }).await?;
        let parsed: DecResp = parse_json(resp, "decrypt").await?;

        let plaintext = BASE64
            .decode(parsed.data.plaintext.as_bytes())
            .map_err(|e| crypto_err(format!("vault transit decrypt base64: {e}")))?;
        if plaintext.len() != 32 {
            return Err(crypto_err(format!(
                "vault transit decrypt returned {} bytes, expected 32",
                plaintext.len()
            )));
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&plaintext);
        Ok(DekBytes::from_bytes(bytes))
    }
}

impl KekProvider for VaultTransitKekProvider {
    fn domain(&self) -> KekDomain {
        self.domain
    }

    fn active_id(&self) -> &str {
        &self.key_name
    }

    fn previous_id(&self) -> Option<&str> {
        // Transit ciphertext encodes its own key version, so unwrap
        // dispatches automatically. There's nothing to surface here.
        None
    }

    fn wrap_new_dek<'a>(&'a self) -> KekFuture<'a, (DekBytes, WrappedDek)> {
        Box::pin(self.wrap_new_dek_async())
    }

    fn wrap_dek<'a>(&'a self, dek: &'a DekBytes) -> KekFuture<'a, WrappedDek> {
        Box::pin(self.wrap_dek_async(dek))
    }

    fn unwrap_dek<'a>(&'a self, wrapped: &'a WrappedDek) -> KekFuture<'a, DekBytes> {
        Box::pin(self.unwrap_dek_async(wrapped))
    }
}

// ─── helpers ───────────────────────────────────────────────────────

fn parse_domain(s: &str) -> Result<KekDomain, SecretsAdapterError> {
    match s {
        "log_context" | "log-context" => Ok(KekDomain::LogContext),
        "data_context" | "data-context" => Ok(KekDomain::DataContext),
        other => Err(config_err(format!(
            "unknown KEK domain '{other}' — expected 'log_context' or 'data_context'"
        ))),
    }
}

async fn parse_json<T: for<'de> Deserialize<'de>>(
    resp: Response,
    op: &'static str,
) -> Result<T, AeonError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(crypto_err(format!(
            "vault transit {op} failed: HTTP {status} — {body}"
        )));
    }
    resp.json::<T>()
        .await
        .map_err(|e| crypto_err(format!("vault transit {op} parse: {e}")))
}

fn crypto_err(message: String) -> AeonError {
    AeonError::Crypto {
        message,
        source: None,
    }
}

fn http_err(what: &'static str, e: reqwest::Error) -> SecretsAdapterError {
    SecretsAdapterError::Backend(AeonError::Config {
        message: format!("vault transit HTTP {what}: {e}"),
    })
}

fn poisoned() -> SecretsAdapterError {
    SecretsAdapterError::Backend(AeonError::Config {
        message: "vault transit auth mutex poisoned".to_string(),
    })
}

fn secrets_to_aeon(e: SecretsAdapterError) -> AeonError {
    e.into()
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{VaultAuthConfig, VaultTransitConfig};
    use serde_json::json;

    fn bootstrap() -> SecretRegistry {
        SecretRegistry::default_local()
    }

    fn token_cfg(
        endpoint: &str,
        mount: &str,
        key_name: &str,
        domain: &str,
        token: &str,
    ) -> VaultTransitConfig {
        VaultTransitConfig {
            domain: domain.to_string(),
            endpoint: endpoint.to_string(),
            mount: mount.to_string(),
            key_name: key_name.to_string(),
            auth: VaultAuthConfig::Token {
                token_ref: token.to_string(),
            },
            namespace: None,
            tls_verify: false,
            retry: fast_retry(),
        }
    }

    /// Tight backoff so retry tests finish in milliseconds without
    /// hiding the policy under `thread::sleep`.
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

    #[tokio::test]
    async fn wrap_new_dek_returns_plaintext_and_ciphertext() {
        let mut server = mockito::Server::new_async().await;
        let dek_b64 = BASE64.encode([0xAAu8; 32]);
        let m = server
            .mock("POST", "/v1/transit/datakey/plaintext/aeon-dek")
            .match_header("X-Vault-Token", "tok")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "data": {
                        "plaintext": dek_b64,
                        "ciphertext": "vault:v1:opaque-blob"
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let provider = VaultTransitKekProvider::from_config(
            &token_cfg(&server.url(), "transit", "aeon-dek", "data_context", "tok"),
            &bootstrap(),
        )
        .await
        .unwrap();

        let (dek, wrapped) = provider.wrap_new_dek().await.unwrap();
        assert_eq!(dek.expose_bytes(), &[0xAAu8; 32]);
        assert_eq!(wrapped.kek_domain, KekDomain::DataContext);
        assert_eq!(wrapped.kek_id, "aeon-dek");
        assert_eq!(
            std::str::from_utf8(&wrapped.ciphertext).unwrap(),
            "vault:v1:opaque-blob"
        );
        assert!(wrapped.nonce.is_empty(), "transit doesn't use the nonce field");
        m.assert_async().await;
    }

    #[tokio::test]
    async fn wrap_existing_dek_calls_encrypt_endpoint() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/v1/transit/encrypt/aeon-dek")
            .match_body(mockito::Matcher::PartialJson(json!({
                "plaintext": BASE64.encode([0x55u8; 32])
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({ "data": { "ciphertext": "vault:v2:wrapped-existing" } })
                    .to_string(),
            )
            .create_async()
            .await;

        let provider = VaultTransitKekProvider::from_config(
            &token_cfg(&server.url(), "transit", "aeon-dek", "log_context", "tok"),
            &bootstrap(),
        )
        .await
        .unwrap();

        let dek = DekBytes::from_bytes([0x55u8; 32]);
        let wrapped = provider.wrap_dek(&dek).await.unwrap();
        assert_eq!(wrapped.kek_domain, KekDomain::LogContext);
        assert_eq!(
            std::str::from_utf8(&wrapped.ciphertext).unwrap(),
            "vault:v2:wrapped-existing"
        );
        m.assert_async().await;
    }

    #[tokio::test]
    async fn unwrap_dek_decrypts_and_returns_plaintext() {
        let mut server = mockito::Server::new_async().await;
        let dek_b64 = BASE64.encode([0x33u8; 32]);
        let m = server
            .mock("POST", "/v1/transit/decrypt/aeon-dek")
            .match_body(mockito::Matcher::PartialJson(json!({
                "ciphertext": "vault:v1:to-decrypt"
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json!({ "data": { "plaintext": dek_b64 } }).to_string())
            .create_async()
            .await;

        let provider = VaultTransitKekProvider::from_config(
            &token_cfg(&server.url(), "transit", "aeon-dek", "data_context", "tok"),
            &bootstrap(),
        )
        .await
        .unwrap();

        let wrapped = WrappedDek {
            kek_domain: KekDomain::DataContext,
            kek_id: "aeon-dek".to_string(),
            nonce: Vec::new(),
            ciphertext: b"vault:v1:to-decrypt".to_vec(),
        };
        let dek = provider.unwrap_dek(&wrapped).await.unwrap();
        assert_eq!(dek.expose_bytes(), &[0x33u8; 32]);
        m.assert_async().await;
    }

    #[tokio::test]
    async fn unwrap_rejects_wrong_domain() {
        let server = mockito::Server::new_async().await;
        let provider = VaultTransitKekProvider::from_config(
            &token_cfg(&server.url(), "transit", "aeon-dek", "data_context", "tok"),
            &bootstrap(),
        )
        .await
        .unwrap();

        let wrapped = WrappedDek {
            kek_domain: KekDomain::LogContext,
            kek_id: "aeon-dek".to_string(),
            nonce: Vec::new(),
            ciphertext: b"vault:v1:x".to_vec(),
        };
        let err = provider.unwrap_dek(&wrapped).await.unwrap_err();
        assert!(matches!(err, AeonError::Crypto { .. }));
    }

    #[tokio::test]
    async fn approle_re_authenticates_on_403_during_wrap() {
        let mut server = mockito::Server::new_async().await;
        let dek_b64 = BASE64.encode([0xCCu8; 32]);
        let login = server
            .mock("POST", "/v1/auth/approle/login")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({ "auth": { "client_token": "fresh-token" } }).to_string(),
            )
            .expect(2)
            .create_async()
            .await;
        let _denied = server
            .mock("POST", "/v1/transit/datakey/plaintext/aeon-dek")
            .with_status(403)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;
        let _granted = server
            .mock("POST", "/v1/transit/datakey/plaintext/aeon-dek")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "data": {
                        "plaintext": dek_b64,
                        "ciphertext": "vault:v1:retry"
                    }
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;

        let cfg = VaultTransitConfig {
            domain: "data_context".to_string(),
            endpoint: server.url(),
            mount: "transit".to_string(),
            key_name: "aeon-dek".to_string(),
            auth: VaultAuthConfig::AppRole {
                role_id_ref: "role".to_string(),
                secret_id_ref: "secret".to_string(),
            },
            namespace: None,
            tls_verify: false,
            retry: fast_retry(),
        };
        let provider = VaultTransitKekProvider::from_config(&cfg, &bootstrap())
            .await
            .unwrap();
        let (dek, wrapped) = provider.wrap_new_dek().await.unwrap();
        assert_eq!(dek.expose_bytes(), &[0xCCu8; 32]);
        assert_eq!(
            std::str::from_utf8(&wrapped.ciphertext).unwrap(),
            "vault:v1:retry"
        );
        login.assert_async().await;
    }

    #[tokio::test]
    async fn datakey_short_plaintext_is_rejected() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/v1/transit/datakey/plaintext/aeon-dek")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "data": {
                        "plaintext": BASE64.encode([0u8; 16]),
                        "ciphertext": "vault:v1:short"
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let provider = VaultTransitKekProvider::from_config(
            &token_cfg(&server.url(), "transit", "aeon-dek", "data_context", "tok"),
            &bootstrap(),
        )
        .await
        .unwrap();
        let err = provider.wrap_new_dek().await.unwrap_err();
        assert!(matches!(err, AeonError::Crypto { .. }));
    }

    #[tokio::test]
    async fn http_error_during_wrap_is_crypto_error() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/v1/transit/datakey/plaintext/aeon-dek")
            .with_status(500)
            .with_body(r#"{"errors":["transit unavailable"]}"#)
            .create_async()
            .await;
        let provider = VaultTransitKekProvider::from_config(
            &token_cfg(&server.url(), "transit", "aeon-dek", "data_context", "tok"),
            &bootstrap(),
        )
        .await
        .unwrap();
        let err = provider.wrap_new_dek().await.unwrap_err();
        assert!(matches!(err, AeonError::Crypto { .. }));
    }

    #[tokio::test]
    async fn empty_mount_rejected() {
        let cfg = token_cfg("https://v.example.com", "/", "k", "data_context", "t");
        let err = VaultTransitKekProvider::from_config(&cfg, &bootstrap())
            .await
            .unwrap_err();
        assert!(matches!(err, SecretsAdapterError::Backend(_)));
    }

    #[tokio::test]
    async fn empty_key_name_rejected() {
        let cfg = token_cfg("https://v.example.com", "transit", "", "data_context", "t");
        let err = VaultTransitKekProvider::from_config(&cfg, &bootstrap())
            .await
            .unwrap_err();
        assert!(matches!(err, SecretsAdapterError::Backend(_)));
    }

    #[tokio::test]
    async fn unknown_domain_rejected() {
        let cfg = token_cfg("https://v.example.com", "transit", "k", "weird", "t");
        let err = VaultTransitKekProvider::from_config(&cfg, &bootstrap())
            .await
            .unwrap_err();
        assert!(matches!(err, SecretsAdapterError::Backend(_)));
    }

    #[tokio::test]
    async fn retry_succeeds_on_transient_503_then_200_on_wrap() {
        let mut server = mockito::Server::new_async().await;
        let dek_b64 = BASE64.encode([0x77u8; 32]);
        let fail = server
            .mock("POST", "/v1/transit/datakey/plaintext/aeon-dek")
            .with_status(503)
            .with_body(r#"{"errors":["sealed"]}"#)
            .expect(1)
            .create_async()
            .await;
        let ok = server
            .mock("POST", "/v1/transit/datakey/plaintext/aeon-dek")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "data": {
                        "plaintext": dek_b64,
                        "ciphertext": "vault:v1:after-retry"
                    }
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;

        let provider = VaultTransitKekProvider::from_config(
            &token_cfg(&server.url(), "transit", "aeon-dek", "data_context", "tok"),
            &bootstrap(),
        )
        .await
        .unwrap();
        let (dek, wrapped) = provider.wrap_new_dek().await.unwrap();
        assert_eq!(dek.expose_bytes(), &[0x77u8; 32]);
        assert_eq!(
            std::str::from_utf8(&wrapped.ciphertext).unwrap(),
            "vault:v1:after-retry"
        );
        fail.assert_async().await;
        ok.assert_async().await;
    }

    #[tokio::test]
    async fn debug_does_not_leak_token() {
        let server = mockito::Server::new_async().await;
        let provider = VaultTransitKekProvider::from_config(
            &token_cfg(
                &server.url(),
                "transit",
                "aeon-dek",
                "data_context",
                "very-secret-static-token",
            ),
            &bootstrap(),
        )
        .await
        .unwrap();
        let dbg = format!("{provider:?}");
        assert!(
            !dbg.contains("very-secret-static-token"),
            "debug leaked token: {dbg}"
        );
    }
}
