//! YAML / serde-deserializable config enums naming every backend Aeon
//! supports, regardless of which feature flags are currently compiled.
//!
//! Keeping every variant declared unconditionally means:
//!
//! - Operator YAML schemas are stable — adding a new backend doesn't
//!   change the shape of `providers:` blocks in `main`.
//! - Enabling a previously-deferred backend is a Cargo-features change,
//!   not a manifest rewrite.
//! - A config naming an un-compiled backend fails loudly at pipeline
//!   start via [`crate::error::SecretsAdapterError::BackendFeatureDisabled`]
//!   rather than parsing into a "unknown" bucket.

use std::path::PathBuf;

use aeon_types::backoff::BackoffPolicy;
use serde::{Deserialize, Serialize};

// ─── SecretProvider configs ────────────────────────────────────────

/// YAML shape for a [`aeon_types::SecretProvider`] backend.
///
/// Discriminated by the `kind:` field. Each variant carries only the
/// backend-specific fields — endpoint URLs, auth blocks, etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretProviderConfig {
    /// Read from `std::env`. Always available.
    Env,

    /// Read from a key=value file at `AEON_DOTENV_PATH` (or the
    /// explicit path given here). Always available.
    DotEnv {
        #[serde(default)]
        path: Option<PathBuf>,
    },

    /// Dev-only pass-through. Always available; emits a warn-once log
    /// the first time it resolves.
    Literal,

    /// HashiCorp Vault KV-v2 backend. OpenBao is API-compatible and
    /// uses this same variant — no separate `OpenBao` kind.
    Vault(VaultProviderConfig),

    /// AWS Secrets Manager (GetSecretValue).
    AwsSm(AwsSmProviderConfig),

    /// GCP Secret Manager.
    GcpSm(GcpSmProviderConfig),

    /// Azure Key Vault (read side — `getSecret`).
    AzureKv(AzureKvProviderConfig),
}

/// Vault / OpenBao KV-v2 config. Both servers speak the same HTTP API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VaultProviderConfig {
    /// Base URL, e.g. `https://vault.example.com:8200`. For OpenBao,
    /// point at the OpenBao server — same path layout.
    pub endpoint: String,

    /// KV-v2 mount point. Default `"secret"`.
    #[serde(default = "default_kv_mount")]
    pub mount: String,

    /// Auth method. AppRole is the production default; `Token` is
    /// for CI / dev.
    pub auth: VaultAuthConfig,

    /// Optional namespace (Vault Enterprise / OpenBao multi-tenant).
    #[serde(default)]
    pub namespace: Option<String>,

    /// Whether to trust the server's TLS cert chain via the system
    /// roots (`true`, default) or require a specific CA bundle (via
    /// `ca_cert_ref`). `false` is dev-only.
    #[serde(default = "default_tls_verify")]
    pub tls_verify: bool,

    /// `${VAULT:...}` or `${ENV:...}` ref to a CA bundle if the server
    /// uses a private CA.
    #[serde(default)]
    pub ca_cert_ref: Option<String>,

    /// Retry policy applied to transient failures (429, 5xx, connect
    /// timeouts). Default is 4 attempts with 100ms → 30s jittered
    /// exponential backoff.
    #[serde(default)]
    pub retry: RetryPolicy,

    /// TTL for the in-memory resolve cache, in seconds. Defaults to
    /// 300 (5 min). Set to 0 to disable caching — every resolve hits
    /// Vault unconditionally.
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
}

fn default_kv_mount() -> String {
    "secret".to_string()
}

fn default_tls_verify() -> bool {
    true
}

fn default_cache_ttl_secs() -> u64 {
    300
}

/// Operator-tunable cap on attempts + backoff policy. `max_attempts`
/// of 1 disables retry entirely; default is 4 (1 initial + 3 retries).
/// Backoff delegates to [`aeon_types::backoff::BackoffPolicy`] so
/// every retryable Aeon call-site uses one set of defaults.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RetryPolicy {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default)]
    pub backoff: BackoffPolicy,
}

fn default_max_attempts() -> u32 {
    4
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff: BackoffPolicy::default(),
        }
    }
}

/// Auth methods supported by the Vault / OpenBao adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum VaultAuthConfig {
    /// Static token. The `token_ref` is a `${...}` secret reference
    /// (typically `${ENV:VAULT_TOKEN}` or `${DOTENV:VAULT_TOKEN}`),
    /// NOT the token itself.
    Token { token_ref: String },

    /// AppRole. Both refs are `${...}` tokens, resolved via the outer
    /// registry at pipeline start.
    AppRole {
        role_id_ref: String,
        secret_id_ref: String,
    },

    /// Kubernetes service-account JWT (auto-discovers the token from
    /// the projected volume mount).
    Kubernetes {
        role: String,
        #[serde(default = "default_k8s_token_path")]
        token_path: PathBuf,
    },
}

fn default_k8s_token_path() -> PathBuf {
    PathBuf::from("/var/run/secrets/kubernetes.io/serviceaccount/token")
}

/// AWS Secrets Manager backend config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AwsSmProviderConfig {
    /// AWS region, e.g. `"us-east-1"`.
    pub region: String,

    /// Optional fixed endpoint (for LocalStack / testing). Empty =
    /// default AWS resolution.
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// GCP Secret Manager backend config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GcpSmProviderConfig {
    /// GCP project id.
    pub project: String,
}

/// Azure Key Vault backend config (read side).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AzureKvProviderConfig {
    /// Vault URI, e.g. `https://my-vault.vault.azure.net/`.
    pub vault_uri: String,
}

// ─── KekProvider configs ───────────────────────────────────────────

/// YAML shape for a [`aeon_crypto::kek_provider::KekProvider`] backend.
///
/// Local AES-GCM and remote KMS backends are first-class siblings
/// under one enum — operators pick by `kind:` without touching the
/// rest of the pipeline manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KekProviderConfig {
    /// In-process AES-256-GCM, KEK bytes fetched via
    /// [`aeon_types::SecretProvider`]. Today's shipped path.
    Local(LocalKekConfig),

    /// AWS KMS `Encrypt` / `Decrypt` / `GenerateDataKey`.
    AwsKms(AwsKmsConfig),

    /// GCP Cloud KMS.
    GcpKms(GcpKmsConfig),

    /// Azure Key Vault crypto operations (wrapKey / unwrapKey).
    AzureKv(AzureKvKmsConfig),

    /// HashiCorp Vault Transit engine. Also covers OpenBao Transit
    /// (API-compatible).
    VaultTransit(VaultTransitConfig),

    /// PKCS#11 direct. Deferred behind task #33.
    Pkcs11(Pkcs11KmsConfig),
}

/// Config for the local AES-GCM path (today's `KekHandle`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalKekConfig {
    /// Which domain this provider serves — `log_context` or `data_context`.
    pub domain: String,

    /// Active KEK id and the `${...}` secret ref that yields 32 bytes.
    pub active_id: String,
    pub active_ref: String,

    /// Previous KEK id + ref, during a rotation window.
    #[serde(default)]
    pub previous_id: Option<String>,
    #[serde(default)]
    pub previous_ref: Option<String>,
}

/// AWS KMS backend config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AwsKmsConfig {
    pub domain: String,
    pub region: String,
    /// Full KMS key ARN or alias (e.g. `alias/aeon-kek`).
    pub active_key_id: String,
    #[serde(default)]
    pub previous_key_id: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GcpKmsConfig {
    pub domain: String,
    /// Fully-qualified key: `projects/P/locations/L/keyRings/R/cryptoKeys/K`.
    pub active_key: String,
    #[serde(default)]
    pub previous_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AzureKvKmsConfig {
    pub domain: String,
    pub vault_uri: String,
    pub active_key_name: String,
    pub active_key_version: String,
    #[serde(default)]
    pub previous_key_name: Option<String>,
    #[serde(default)]
    pub previous_key_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VaultTransitConfig {
    pub domain: String,
    /// Same Vault/OpenBao server the KV-v2 side points at, typically.
    pub endpoint: String,
    /// Transit mount point. Default `"transit"`.
    #[serde(default = "default_transit_mount")]
    pub mount: String,
    pub key_name: String,
    pub auth: VaultAuthConfig,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "default_tls_verify")]
    pub tls_verify: bool,

    /// Retry policy for transient wrap/unwrap failures. Unlike KV-v2
    /// there is no cache here — every call has a unique ciphertext.
    #[serde(default)]
    pub retry: RetryPolicy,
}

fn default_transit_mount() -> String {
    "transit".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pkcs11KmsConfig {
    pub domain: String,
    pub module_path: PathBuf,
    pub slot_id: u64,
    pub key_label: String,
    #[serde(default)]
    pub pin_ref: Option<String>,
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_provider_config_env_variant_roundtrips() {
        let yaml = "kind: env\n";
        let cfg: SecretProviderConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(cfg, SecretProviderConfig::Env));
    }

    #[test]
    fn secret_provider_config_vault_approle_roundtrips() {
        let yaml = r#"
kind: vault
endpoint: https://vault.example.com:8200
mount: kv
namespace: aeon-prod
auth:
  method: app_role
  role_id_ref: ${ENV:VAULT_ROLE_ID}
  secret_id_ref: ${ENV:VAULT_SECRET_ID}
"#;
        let cfg: SecretProviderConfig = serde_yaml::from_str(yaml).unwrap();
        match cfg {
            SecretProviderConfig::Vault(v) => {
                assert_eq!(v.endpoint, "https://vault.example.com:8200");
                assert_eq!(v.mount, "kv");
                assert_eq!(v.namespace.as_deref(), Some("aeon-prod"));
                assert!(v.tls_verify); // default
                match v.auth {
                    VaultAuthConfig::AppRole { role_id_ref, secret_id_ref } => {
                        assert_eq!(role_id_ref, "${ENV:VAULT_ROLE_ID}");
                        assert_eq!(secret_id_ref, "${ENV:VAULT_SECRET_ID}");
                    }
                    _ => panic!("wrong auth variant"),
                }
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn secret_provider_config_vault_default_mount() {
        let yaml = r#"
kind: vault
endpoint: https://v.example.com
auth:
  method: token
  token_ref: ${ENV:VAULT_TOKEN}
"#;
        let cfg: SecretProviderConfig = serde_yaml::from_str(yaml).unwrap();
        match cfg {
            SecretProviderConfig::Vault(v) => {
                assert_eq!(v.mount, "secret"); // default
                assert!(v.tls_verify); // default true
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn kek_provider_config_local_roundtrips() {
        let yaml = r#"
kind: local
domain: data_context
active_id: v2
active_ref: ${ENV:AEON_KEK_V2}
previous_id: v1
previous_ref: ${ENV:AEON_KEK_V1}
"#;
        let cfg: KekProviderConfig = serde_yaml::from_str(yaml).unwrap();
        match cfg {
            KekProviderConfig::Local(l) => {
                assert_eq!(l.domain, "data_context");
                assert_eq!(l.active_id, "v2");
                assert_eq!(l.previous_id.as_deref(), Some("v1"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn kek_provider_config_aws_kms_variant() {
        let yaml = r#"
kind: aws_kms
domain: data_context
region: us-east-1
active_key_id: alias/aeon-kek
"#;
        let cfg: KekProviderConfig = serde_yaml::from_str(yaml).unwrap();
        match cfg {
            KekProviderConfig::AwsKms(a) => {
                assert_eq!(a.region, "us-east-1");
                assert_eq!(a.active_key_id, "alias/aeon-kek");
                assert!(a.previous_key_id.is_none());
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn kek_provider_config_vault_transit_variant() {
        let yaml = r#"
kind: vault_transit
domain: data_context
endpoint: https://vault.example.com
key_name: aeon-dek-wrapper
auth:
  method: kubernetes
  role: aeon
"#;
        let cfg: KekProviderConfig = serde_yaml::from_str(yaml).unwrap();
        match cfg {
            KekProviderConfig::VaultTransit(v) => {
                assert_eq!(v.mount, "transit"); // default
                assert_eq!(v.key_name, "aeon-dek-wrapper");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn kek_provider_config_unknown_kind_errors() {
        let yaml = "kind: quantum_kms\n";
        let err = serde_yaml::from_str::<KekProviderConfig>(yaml).unwrap_err();
        assert!(err.to_string().contains("quantum_kms"), "got: {err}");
    }
}
