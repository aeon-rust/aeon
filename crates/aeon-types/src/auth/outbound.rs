//! S10: outbound connector authentication.
//!
//! Scope: every client-side dial site — HTTP sink, HTTP polling source,
//! WebSocket source/sink, WebTransport sink, Kafka (SASL), Redis (AUTH),
//! NATS (creds), Postgres/MySQL CDC.
//!
//! ## No interlocking
//!
//! Exactly one [`OutboundAuthMode`] per connector. Stacking modes is
//! explicitly rejected — outbound is a single handshake, and composing
//! modes would break retry semantics (e.g. which credential caused a
//! 401? which do we rotate?). Stackable auth is the inbound world's
//! defence-in-depth job (see [`super::inbound`]).
//!
//! ## Modes
//!
//! - **[`None`](OutboundAuthMode::None)** — explicit no-credentials. The
//!   upstream authenticates Aeon via network-layer controls (egress IP
//!   allow-list, VPC peering, broker ACL on subnet). Legitimate default
//!   for in-VPC pull sources where layering app-level creds on top of
//!   network controls is redundant. Loud at pipeline start so operators
//!   see the deliberate choice.
//! - **[`Bearer`](OutboundAuthMode::Bearer)** — `Authorization: Bearer <token>`.
//! - **[`Basic`](OutboundAuthMode::Basic)** — `Authorization: Basic <b64(user:pass)>`.
//! - **[`ApiKey`](OutboundAuthMode::ApiKey)** — configurable header + key.
//! - **[`HmacSign`](OutboundAuthMode::HmacSign)** — client-side HMAC sign
//!   using the same canonical preimage as the inbound verifier.
//! - **[`Mtls`](OutboundAuthMode::Mtls)** — client certificate + key.
//! - **[`BrokerNative`](OutboundAuthMode::BrokerNative)** — delegate to
//!   the underlying client library's auth knobs (Kafka SASL/OAUTHBEARER,
//!   Redis AUTH/ACL, NATS creds, PG/MySQL native). Credentials flow
//!   through [`SecretBytes`]; the broker SDK sees plaintext only at
//!   connect time.

use crate::AeonError;
use crate::secrets::SecretBytes;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::hmac_sig::{HmacAlgorithm, HmacSignError, sign_request};

/// Default header name for API-key outbound auth (matches the inbound
/// default so the convention is symmetric).
pub const DEFAULT_OUTBOUND_API_KEY_HEADER: &str = "X-Aeon-Api-Key";
/// Default header name for HMAC signature on outbound requests.
pub const DEFAULT_OUTBOUND_HMAC_SIGNATURE_HEADER: &str = "X-Aeon-Signature";
/// Default header name for HMAC timestamp on outbound requests.
pub const DEFAULT_OUTBOUND_HMAC_TIMESTAMP_HEADER: &str = "X-Aeon-Timestamp";

/// The single outbound auth mode a connector uses. Declared at the top
/// of the connector's `auth:` block — never composed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundAuthMode {
    /// No credentials injected — upstream authenticates Aeon by egress
    /// IP / VPC / broker ACL. Must be an explicit choice.
    None,
    /// `Authorization: Bearer <token>`.
    Bearer,
    /// `Authorization: Basic <b64(user:pass)>`.
    Basic,
    /// Configurable header with a static key value.
    ApiKey,
    /// Per-request HMAC signature using the canonical preimage.
    HmacSign,
    /// Client certificate + private key for mTLS.
    Mtls,
    /// Delegates to the underlying client library's auth layer.
    BrokerNative,
}

impl OutboundAuthMode {
    /// Short tag for metric labels / log reasons (bounded cardinality).
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer => "bearer",
            Self::Basic => "basic",
            Self::ApiKey => "api_key",
            Self::HmacSign => "hmac_sign",
            Self::Mtls => "mtls",
            Self::BrokerNative => "broker_native",
        }
    }
}

/// Bearer-token config.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BearerConfig {
    /// Token value — plaintext at this point (S1.3 pre-parse resolved
    /// any `${VAULT:...}` / `${ENV:...}` references).
    pub token: String,
}

/// HTTP Basic config.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BasicConfig {
    pub username: String,
    pub password: String,
}

/// API-key header config.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboundApiKeyConfig {
    #[serde(default = "default_outbound_api_key_header")]
    pub header_name: String,
    pub key: String,
}

/// HMAC client-side signing config. Mirrors the inbound HMAC shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HmacSignConfig {
    #[serde(default = "default_outbound_hmac_sig_header")]
    pub signature_header: String,
    #[serde(default = "default_outbound_hmac_ts_header")]
    pub timestamp_header: String,
    pub secret: String,
    #[serde(default)]
    pub algorithm: HmacAlgorithm,
}

/// mTLS client credentials. Cert + key are PEM bytes held in
/// [`SecretBytes`] once compiled.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboundMtlsConfig {
    /// PEM-encoded client certificate chain.
    pub cert_pem: String,
    /// PEM-encoded private key.
    pub key_pem: String,
}

/// Broker-native credentials — protocol-specific blob handed to the
/// underlying client library. Kept as free-form key/value pairs so
/// Kafka (SASL mechanism, username, password, token), Redis (username,
/// password), NATS (creds-file path) all pass through the same type.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BrokerNativeConfig {
    /// Free-form credential values. Typical keys: `sasl_mechanism`,
    /// `username`, `password`, `oauth_token`, `creds_path`.
    /// Connector-specific parsers pluck what they need.
    #[serde(default)]
    pub values: std::collections::BTreeMap<String, String>,
}

fn default_outbound_api_key_header() -> String {
    DEFAULT_OUTBOUND_API_KEY_HEADER.to_string()
}
fn default_outbound_hmac_sig_header() -> String {
    DEFAULT_OUTBOUND_HMAC_SIGNATURE_HEADER.to_string()
}
fn default_outbound_hmac_ts_header() -> String {
    DEFAULT_OUTBOUND_HMAC_TIMESTAMP_HEADER.to_string()
}

/// Top-level outbound auth config. Exactly one block matching
/// [`OutboundAuthConfig::mode`] must be populated; others are ignored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboundAuthConfig {
    pub mode: OutboundAuthMode,
    #[serde(default)]
    pub bearer: Option<BearerConfig>,
    #[serde(default)]
    pub basic: Option<BasicConfig>,
    #[serde(default)]
    pub api_key: Option<OutboundApiKeyConfig>,
    #[serde(default)]
    pub hmac_sign: Option<HmacSignConfig>,
    #[serde(default)]
    pub mtls: Option<OutboundMtlsConfig>,
    #[serde(default)]
    pub broker_native: Option<BrokerNativeConfig>,
}

impl Default for OutboundAuthConfig {
    fn default() -> Self {
        Self {
            mode: OutboundAuthMode::None,
            bearer: None,
            basic: None,
            api_key: None,
            hmac_sign: None,
            mtls: None,
            broker_native: None,
        }
    }
}

/// Errors returned while building an [`OutboundAuthSigner`].
#[derive(Debug, thiserror::Error)]
pub enum OutboundAuthBuildError {
    #[error("outbound auth: mode `{mode:?}` selected but its config block is missing")]
    ModeConfigMissing { mode: OutboundAuthMode },
    #[error("outbound auth: bearer token is empty")]
    BearerEmpty,
    #[error("outbound auth: basic username is empty")]
    BasicUsernameEmpty,
    #[error("outbound auth: api_key key value is empty")]
    ApiKeyEmpty,
    #[error("outbound auth: hmac_sign secret is empty")]
    HmacSecretEmpty,
    #[error("outbound auth: mtls cert or key is empty")]
    MtlsEmpty,
}

impl From<OutboundAuthBuildError> for AeonError {
    fn from(e: OutboundAuthBuildError) -> Self {
        AeonError::config(e.to_string())
    }
}

/// Errors returned while applying an outbound signer to a request.
#[derive(Debug, thiserror::Error)]
pub enum OutboundSignError {
    #[error("outbound auth: hmac_sign failed: {0}")]
    HmacSign(#[from] HmacSignError),
}

impl From<OutboundSignError> for AeonError {
    fn from(e: OutboundSignError) -> Self {
        AeonError::config(e.to_string())
    }
}

/// Per-request context passed to [`OutboundAuthSigner::http_headers`]
/// for HMAC signing. Other modes ignore this and return a fixed header
/// set regardless.
#[derive(Debug, Clone)]
pub struct OutboundSignContext<'a> {
    /// Request method (`"GET"`, `"POST"`, …). Case-sensitive — stays
    /// as emitted by the client. Unused outside HMAC mode.
    pub method: &'a str,
    /// Request path (e.g. `/api/v1/events`). Unused outside HMAC mode.
    pub path: &'a str,
    /// Raw body bytes. Unused outside HMAC mode.
    pub body: &'a [u8],
    /// Unix-epoch seconds at send time — caller-supplied so tests
    /// stay deterministic. Used as the `X-Aeon-Timestamp` value.
    pub now_unix: i64,
}

/// Compiled internal state — one variant per mode, so every accessor
/// can pattern-match instead of unwrapping options. This makes it
/// impossible for `http_headers()` to observe a mode whose secret
/// material wasn't populated by `build()`, which is why there are no
/// `.unwrap()` / `.expect()` on the hot path (FT-10 policy).
enum CompiledSigner {
    None,
    Bearer(SecretBytes),
    Basic(SecretBytes),
    ApiKey {
        header_name: String,
        value: SecretBytes,
    },
    HmacSign(CompiledHmacSign),
    Mtls(CompiledMtls),
    BrokerNative(Option<BrokerNativeConfig>),
}

struct CompiledHmacSign {
    signature_header: String,
    timestamp_header: String,
    secret: SecretBytes,
    algorithm: HmacAlgorithm,
}

struct CompiledMtls {
    cert_pem: SecretBytes,
    key_pem: SecretBytes,
}

/// Compiled outbound signer. One instance per connector, held behind
/// an `Arc` for the connector's lifetime. Secret-bearing modes store
/// their material in [`SecretBytes`] so drop zeroizes.
pub struct OutboundAuthSigner {
    inner: CompiledSigner,
}

impl std::fmt::Debug for OutboundAuthSigner {
    /// Redacts secret material — surfaces the mode + presence of each
    /// block but never key bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mode = self.mode();
        let mut s = f.debug_struct("OutboundAuthSigner");
        s.field("mode", &mode);
        match &self.inner {
            CompiledSigner::None => {}
            CompiledSigner::Bearer(_) => {
                s.field("has_bearer", &true);
            }
            CompiledSigner::Basic(_) => {
                s.field("has_basic", &true);
            }
            CompiledSigner::ApiKey { header_name, .. } => {
                s.field("api_key_header", header_name)
                    .field("has_api_key", &true);
            }
            CompiledSigner::HmacSign(_) => {
                s.field("has_hmac", &true);
            }
            CompiledSigner::Mtls(_) => {
                s.field("has_mtls", &true);
            }
            CompiledSigner::BrokerNative(bn) => {
                s.field(
                    "broker_native_keys",
                    &bn.as_ref()
                        .map(|b| b.values.keys().cloned().collect::<Vec<_>>()),
                );
            }
        }
        s.finish()
    }
}

impl OutboundAuthSigner {
    /// Build a signer from its config, moving every secret into a
    /// zeroizing container. Rejects mode/block mismatches and empty
    /// credential values up-front so misconfiguration fails loud at
    /// pipeline start.
    pub fn build(config: OutboundAuthConfig) -> Result<Self, OutboundAuthBuildError> {
        let inner = match config.mode {
            OutboundAuthMode::None => CompiledSigner::None,
            OutboundAuthMode::Bearer => {
                let b = config
                    .bearer
                    .ok_or(OutboundAuthBuildError::ModeConfigMissing {
                        mode: OutboundAuthMode::Bearer,
                    })?;
                if b.token.is_empty() {
                    return Err(OutboundAuthBuildError::BearerEmpty);
                }
                CompiledSigner::Bearer(SecretBytes::new(b.token.into_bytes()))
            }
            OutboundAuthMode::Basic => {
                let b = config
                    .basic
                    .ok_or(OutboundAuthBuildError::ModeConfigMissing {
                        mode: OutboundAuthMode::Basic,
                    })?;
                if b.username.is_empty() {
                    return Err(OutboundAuthBuildError::BasicUsernameEmpty);
                }
                let raw = format!("{}:{}", b.username, b.password);
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
                CompiledSigner::Basic(SecretBytes::new(encoded.into_bytes()))
            }
            OutboundAuthMode::ApiKey => {
                let k = config
                    .api_key
                    .ok_or(OutboundAuthBuildError::ModeConfigMissing {
                        mode: OutboundAuthMode::ApiKey,
                    })?;
                if k.key.is_empty() {
                    return Err(OutboundAuthBuildError::ApiKeyEmpty);
                }
                CompiledSigner::ApiKey {
                    header_name: k.header_name,
                    value: SecretBytes::new(k.key.into_bytes()),
                }
            }
            OutboundAuthMode::HmacSign => {
                let h = config
                    .hmac_sign
                    .ok_or(OutboundAuthBuildError::ModeConfigMissing {
                        mode: OutboundAuthMode::HmacSign,
                    })?;
                if h.secret.is_empty() {
                    return Err(OutboundAuthBuildError::HmacSecretEmpty);
                }
                CompiledSigner::HmacSign(CompiledHmacSign {
                    signature_header: h.signature_header,
                    timestamp_header: h.timestamp_header,
                    secret: SecretBytes::new(h.secret.into_bytes()),
                    algorithm: h.algorithm,
                })
            }
            OutboundAuthMode::Mtls => {
                let m = config
                    .mtls
                    .ok_or(OutboundAuthBuildError::ModeConfigMissing {
                        mode: OutboundAuthMode::Mtls,
                    })?;
                if m.cert_pem.is_empty() || m.key_pem.is_empty() {
                    return Err(OutboundAuthBuildError::MtlsEmpty);
                }
                CompiledSigner::Mtls(CompiledMtls {
                    cert_pem: SecretBytes::new(m.cert_pem.into_bytes()),
                    key_pem: SecretBytes::new(m.key_pem.into_bytes()),
                })
            }
            OutboundAuthMode::BrokerNative => {
                // Broker-native blocks may legitimately be empty — e.g.
                // a Kafka broker that reads its auth from env vars the
                // SDK already picks up. The presence of the block is
                // optional; when absent, the signer just remembers the
                // mode so callers can branch on it.
                CompiledSigner::BrokerNative(config.broker_native)
            }
        };

        Ok(Self { inner })
    }

    /// The outbound mode this signer was built for.
    pub fn mode(&self) -> OutboundAuthMode {
        match &self.inner {
            CompiledSigner::None => OutboundAuthMode::None,
            CompiledSigner::Bearer(_) => OutboundAuthMode::Bearer,
            CompiledSigner::Basic(_) => OutboundAuthMode::Basic,
            CompiledSigner::ApiKey { .. } => OutboundAuthMode::ApiKey,
            CompiledSigner::HmacSign(_) => OutboundAuthMode::HmacSign,
            CompiledSigner::Mtls(_) => OutboundAuthMode::Mtls,
            CompiledSigner::BrokerNative(_) => OutboundAuthMode::BrokerNative,
        }
    }

    /// Compute the HTTP-style headers this signer contributes to a
    /// request. For HMAC mode the returned headers are per-request
    /// (timestamp + signature); for all other HTTP-applicable modes
    /// they are static. Returns an empty `Vec` for modes that don't
    /// touch HTTP headers (`None`, `Mtls`, `BrokerNative`).
    pub fn http_headers(
        &self,
        ctx: &OutboundSignContext<'_>,
    ) -> Result<Vec<(String, String)>, OutboundSignError> {
        match &self.inner {
            CompiledSigner::None | CompiledSigner::Mtls(_) | CompiledSigner::BrokerNative(_) => {
                Ok(Vec::new())
            }
            CompiledSigner::Bearer(token) => {
                let token_str = std::str::from_utf8(token.expose_bytes()).unwrap_or("");
                Ok(vec![(
                    "Authorization".to_string(),
                    format!("Bearer {token_str}"),
                )])
            }
            CompiledSigner::Basic(encoded) => {
                let encoded_str = std::str::from_utf8(encoded.expose_bytes()).unwrap_or("");
                Ok(vec![(
                    "Authorization".to_string(),
                    format!("Basic {encoded_str}"),
                )])
            }
            CompiledSigner::ApiKey { header_name, value } => {
                let value_str = std::str::from_utf8(value.expose_bytes()).unwrap_or("");
                Ok(vec![(header_name.clone(), value_str.to_string())])
            }
            CompiledSigner::HmacSign(h) => {
                let ts = ctx.now_unix.to_string();
                let sig = sign_request(
                    h.algorithm,
                    h.secret.expose_bytes(),
                    ctx.method.as_bytes(),
                    ctx.path.as_bytes(),
                    ts.as_bytes(),
                    ctx.body,
                )?;
                Ok(vec![
                    (h.timestamp_header.clone(), ts),
                    (h.signature_header.clone(), sig),
                ])
            }
        }
    }

    /// PEM-encoded client certificate bytes, when mode is `Mtls`.
    pub fn mtls_cert_pem(&self) -> Option<&[u8]> {
        match &self.inner {
            CompiledSigner::Mtls(m) => Some(m.cert_pem.expose_bytes()),
            _ => None,
        }
    }

    /// PEM-encoded private-key bytes, when mode is `Mtls`.
    pub fn mtls_key_pem(&self) -> Option<&[u8]> {
        match &self.inner {
            CompiledSigner::Mtls(m) => Some(m.key_pem.expose_bytes()),
            _ => None,
        }
    }

    /// Broker-native credential map, when mode is `BrokerNative`. The
    /// keys are whatever the connector/SDK expects — Aeon is just a
    /// pass-through.
    pub fn broker_native(&self) -> Option<&BrokerNativeConfig> {
        match &self.inner {
            CompiledSigner::BrokerNative(bn) => bn.as_ref(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign_ctx() -> OutboundSignContext<'static> {
        OutboundSignContext {
            method: "POST",
            path: "/events",
            body: b"hello",
            now_unix: 1_700_000_000,
        }
    }

    #[test]
    fn mode_none_emits_no_headers() {
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.mode(), OutboundAuthMode::None);
        assert!(s.http_headers(&sign_ctx()).unwrap().is_empty());
    }

    #[test]
    fn mode_bearer_emits_authorization_header() {
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            bearer: Some(BearerConfig {
                token: "abc123".to_string(),
            }),
            ..Default::default()
        })
        .unwrap();
        let h = s.http_headers(&sign_ctx()).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "Authorization");
        assert_eq!(h[0].1, "Bearer abc123");
    }

    #[test]
    fn mode_bearer_missing_block_errors() {
        let err = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(
            err,
            OutboundAuthBuildError::ModeConfigMissing {
                mode: OutboundAuthMode::Bearer
            }
        ));
    }

    #[test]
    fn mode_bearer_empty_token_errors() {
        let err = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            bearer: Some(BearerConfig {
                token: String::new(),
            }),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, OutboundAuthBuildError::BearerEmpty));
    }

    #[test]
    fn mode_basic_produces_base64_authorization() {
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::Basic,
            basic: Some(BasicConfig {
                username: "alice".to_string(),
                password: "s3cret".to_string(),
            }),
            ..Default::default()
        })
        .unwrap();
        let h = s.http_headers(&sign_ctx()).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "Authorization");
        // base64("alice:s3cret") = "YWxpY2U6czNjcmV0"
        assert_eq!(h[0].1, "Basic YWxpY2U6czNjcmV0");
    }

    #[test]
    fn mode_basic_empty_username_errors() {
        let err = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::Basic,
            basic: Some(BasicConfig {
                username: String::new(),
                password: "x".into(),
            }),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, OutboundAuthBuildError::BasicUsernameEmpty));
    }

    #[test]
    fn mode_api_key_default_header_name() {
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::ApiKey,
            api_key: Some(OutboundApiKeyConfig {
                header_name: DEFAULT_OUTBOUND_API_KEY_HEADER.to_string(),
                key: "k-123".to_string(),
            }),
            ..Default::default()
        })
        .unwrap();
        let h = s.http_headers(&sign_ctx()).unwrap();
        assert_eq!(h, vec![("X-Aeon-Api-Key".into(), "k-123".into())]);
    }

    #[test]
    fn mode_api_key_custom_header_name() {
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::ApiKey,
            api_key: Some(OutboundApiKeyConfig {
                header_name: "X-Upstream-Token".into(),
                key: "abc".into(),
            }),
            ..Default::default()
        })
        .unwrap();
        let h = s.http_headers(&sign_ctx()).unwrap();
        assert_eq!(h, vec![("X-Upstream-Token".into(), "abc".into())]);
    }

    #[test]
    fn mode_api_key_empty_errors() {
        let err = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::ApiKey,
            api_key: Some(OutboundApiKeyConfig {
                header_name: "H".into(),
                key: String::new(),
            }),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, OutboundAuthBuildError::ApiKeyEmpty));
    }

    #[test]
    fn mode_hmac_sign_produces_timestamp_and_signature_headers() {
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::HmacSign,
            hmac_sign: Some(HmacSignConfig {
                signature_header: DEFAULT_OUTBOUND_HMAC_SIGNATURE_HEADER.into(),
                timestamp_header: DEFAULT_OUTBOUND_HMAC_TIMESTAMP_HEADER.into(),
                secret: "shh".into(),
                algorithm: HmacAlgorithm::HmacSha256,
            }),
            ..Default::default()
        })
        .unwrap();
        let h = s.http_headers(&sign_ctx()).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].0, "X-Aeon-Timestamp");
        assert_eq!(h[0].1, "1700000000");
        assert_eq!(h[1].0, "X-Aeon-Signature");
        // Signature must be hex and non-empty; exact value verified
        // by the hmac_sig unit tests.
        assert!(!h[1].1.is_empty());
        assert!(h[1].1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn mode_hmac_sign_timestamp_varies_between_calls() {
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::HmacSign,
            hmac_sign: Some(HmacSignConfig {
                signature_header: DEFAULT_OUTBOUND_HMAC_SIGNATURE_HEADER.into(),
                timestamp_header: DEFAULT_OUTBOUND_HMAC_TIMESTAMP_HEADER.into(),
                secret: "shh".into(),
                algorithm: HmacAlgorithm::HmacSha256,
            }),
            ..Default::default()
        })
        .unwrap();

        let mut c1 = sign_ctx();
        c1.now_unix = 1;
        let mut c2 = sign_ctx();
        c2.now_unix = 2;
        let h1 = s.http_headers(&c1).unwrap();
        let h2 = s.http_headers(&c2).unwrap();
        // Different timestamps → different signatures (preimage differs).
        assert_ne!(h1[1].1, h2[1].1);
    }

    #[test]
    fn mode_hmac_sign_empty_secret_errors() {
        let err = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::HmacSign,
            hmac_sign: Some(HmacSignConfig {
                signature_header: "X-Sig".into(),
                timestamp_header: "X-Ts".into(),
                secret: String::new(),
                algorithm: HmacAlgorithm::HmacSha256,
            }),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, OutboundAuthBuildError::HmacSecretEmpty));
    }

    #[test]
    fn mode_mtls_emits_no_http_headers_but_surfaces_pem() {
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(OutboundMtlsConfig {
                cert_pem: "-----BEGIN CERT-----".into(),
                key_pem: "-----BEGIN KEY-----".into(),
            }),
            ..Default::default()
        })
        .unwrap();
        assert!(s.http_headers(&sign_ctx()).unwrap().is_empty());
        assert_eq!(s.mtls_cert_pem(), Some(b"-----BEGIN CERT-----" as &[u8]));
        assert_eq!(s.mtls_key_pem(), Some(b"-----BEGIN KEY-----" as &[u8]));
    }

    #[test]
    fn mode_mtls_empty_errors() {
        let err = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(OutboundMtlsConfig {
                cert_pem: String::new(),
                key_pem: "k".into(),
            }),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, OutboundAuthBuildError::MtlsEmpty));
    }

    #[test]
    fn mode_broker_native_emits_no_http_headers_and_surfaces_values() {
        let mut values = std::collections::BTreeMap::new();
        values.insert("sasl_mechanism".to_string(), "SCRAM-SHA-256".to_string());
        values.insert("username".to_string(), "svc".to_string());
        values.insert("password".to_string(), "p".to_string());
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig {
                values: values.clone(),
            }),
            ..Default::default()
        })
        .unwrap();
        assert!(s.http_headers(&sign_ctx()).unwrap().is_empty());
        let bn = s.broker_native().unwrap();
        assert_eq!(bn.values, values);
    }

    #[test]
    fn mode_broker_native_allows_empty_block() {
        // Some brokers read creds from ambient env vars the SDK
        // already picks up — the block may legitimately be absent.
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.mode(), OutboundAuthMode::BrokerNative);
        assert!(s.broker_native().is_none());
    }

    #[test]
    fn debug_redacts_secret_material() {
        let s = OutboundAuthSigner::build(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            bearer: Some(BearerConfig {
                token: "super-secret-token".into(),
            }),
            ..Default::default()
        })
        .unwrap();
        let dbg = format!("{s:?}");
        assert!(dbg.contains("Bearer"));
        assert!(dbg.contains("has_bearer"));
        // The raw token must not leak.
        assert!(!dbg.contains("super-secret-token"));
    }

    #[test]
    fn mode_tag_is_bounded_cardinality() {
        // Every mode has a distinct, short tag suitable for a metric
        // label. Exhaustive match so new modes can't slip in without
        // updating the tag table.
        let tags = [
            OutboundAuthMode::None.tag(),
            OutboundAuthMode::Bearer.tag(),
            OutboundAuthMode::Basic.tag(),
            OutboundAuthMode::ApiKey.tag(),
            OutboundAuthMode::HmacSign.tag(),
            OutboundAuthMode::Mtls.tag(),
            OutboundAuthMode::BrokerNative.tag(),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for t in tags {
            assert!(seen.insert(t), "duplicate tag: {t}");
            assert!(!t.is_empty());
            assert!(t.len() < 20);
        }
    }

    #[test]
    fn mode_serde_kebab_case() {
        let s = serde_json::to_string(&OutboundAuthMode::HmacSign).unwrap();
        assert_eq!(s, "\"hmac_sign\"");
        let back: OutboundAuthMode = serde_json::from_str("\"broker_native\"").unwrap();
        assert_eq!(back, OutboundAuthMode::BrokerNative);
    }

    #[test]
    fn outbound_auth_build_error_converts_to_aeon_error() {
        let err: AeonError = OutboundAuthBuildError::BearerEmpty.into();
        let msg = format!("{err}");
        assert!(msg.contains("bearer"));
    }
}
