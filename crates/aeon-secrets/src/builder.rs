//! Build [`aeon_types::SecretRegistry`] and [`crate::KekRegistry`]
//! from serde-deserialized config values.
//!
//! The builders are the feature-flag gate: a config value naming a
//! backend whose feature is not compiled returns
//! [`SecretsAdapterError::BackendFeatureDisabled`]. Operators see the
//! mismatch at pipeline start, never silently.

use std::sync::Arc;

use aeon_crypto::kek::{KekDomain, KekHandle};
use aeon_types::{
    DotEnvProvider, EnvProvider, LiteralProvider, SecretProvider, SecretRef, SecretRegistry,
};

use crate::config::{
    KekProviderConfig, LocalKekConfig, SecretProviderConfig, VaultProviderConfig,
    VaultTransitConfig,
};
use crate::error::SecretsAdapterError;
use crate::kek_registry::KekRegistry;

// ─── SecretRegistryBuilder ──────────────────────────────────────────

/// Accumulates [`SecretProviderConfig`] values and turns them into a
/// fully-populated [`SecretRegistry`].
///
/// `Env` / `DotEnv` / `Literal` always register (they're always
/// compiled). Backend variants are gated on their Cargo feature flags
/// and return a typed error if asked for while disabled.
pub struct SecretRegistryBuilder {
    configs: Vec<SecretProviderConfig>,
    include_defaults: bool,
}

impl SecretRegistryBuilder {
    pub fn new() -> Self {
        Self {
            configs: Vec::new(),
            include_defaults: true,
        }
    }

    /// Skip auto-registering `Env` / `DotEnv` / `Literal`. Callers who
    /// want a locked-down registry with only explicit configs use this.
    pub fn without_defaults(mut self) -> Self {
        self.include_defaults = false;
        self
    }

    pub fn with_config(mut self, cfg: SecretProviderConfig) -> Self {
        self.configs.push(cfg);
        self
    }

    pub fn extend<I: IntoIterator<Item = SecretProviderConfig>>(mut self, cfgs: I) -> Self {
        self.configs.extend(cfgs);
        self
    }

    /// Two-pass build:
    ///
    /// 1. Defaults + simple schemes (`Env` / `DotEnv` / `Literal`) register
    ///    first, populating a bootstrap registry that knows how to resolve
    ///    `${ENV:...}` / `${DOTENV:...}` references.
    /// 2. Backend providers that need to resolve their auth blocks via
    ///    secret refs (Vault, AWS SM, …) register second, with the
    ///    pass-1 registry passed in as their bootstrap.
    pub fn build(self) -> Result<SecretRegistry, SecretsAdapterError> {
        let mut reg = SecretRegistry::empty();

        if self.include_defaults {
            reg.register(Arc::new(EnvProvider));
            reg.register(Arc::new(DotEnvProvider::from_env()));
            reg.register(Arc::new(LiteralProvider::new()));
        }

        let mut deferred = Vec::new();
        for cfg in self.configs {
            match cfg {
                SecretProviderConfig::Env => {
                    reg.register(Arc::new(EnvProvider));
                }
                SecretProviderConfig::DotEnv { path } => {
                    let provider = match path {
                        Some(p) => DotEnvProvider::with_path(p),
                        None => DotEnvProvider::from_env(),
                    };
                    reg.register(Arc::new(provider));
                }
                SecretProviderConfig::Literal => {
                    reg.register(Arc::new(LiteralProvider::new()));
                }
                other => deferred.push(other),
            }
        }

        for cfg in deferred {
            match cfg {
                SecretProviderConfig::Vault(v) => {
                    let provider = build_vault(&v, &reg)?;
                    reg.register(provider);
                }
                SecretProviderConfig::AwsSm(_) => {
                    return Err(feature_disabled("aws_secrets_manager", "aws-sm"));
                }
                SecretProviderConfig::GcpSm(_) => {
                    return Err(feature_disabled("gcp_secret_manager", "gcp-sm"));
                }
                SecretProviderConfig::AzureKv(_) => {
                    return Err(feature_disabled("azure_key_vault", "azure-kv"));
                }
                SecretProviderConfig::Env
                | SecretProviderConfig::DotEnv { .. }
                | SecretProviderConfig::Literal => {
                    // Already handled in pass 1; deferred only collects
                    // backends that need bootstrap resolution.
                    unreachable!()
                }
            }
        }

        Ok(reg)
    }
}

#[cfg(feature = "vault")]
fn build_vault(
    cfg: &VaultProviderConfig,
    bootstrap: &SecretRegistry,
) -> Result<Arc<dyn SecretProvider>, SecretsAdapterError> {
    let provider = crate::backends::vault::VaultKvProvider::from_config(cfg, bootstrap)?;
    Ok(Arc::new(provider))
}

#[cfg(not(feature = "vault"))]
fn build_vault(
    _cfg: &VaultProviderConfig,
    _bootstrap: &SecretRegistry,
) -> Result<Arc<dyn SecretProvider>, SecretsAdapterError> {
    Err(feature_disabled("vault", "vault"))
}

impl Default for SecretRegistryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ─── KekRegistryBuilder ─────────────────────────────────────────────

/// Accumulates [`KekProviderConfig`] values and turns them into a
/// fully-populated [`KekRegistry`].
///
/// Requires a [`SecretRegistry`] because the `Local` variant fetches
/// KEK bytes via a secret reference.
pub struct KekRegistryBuilder {
    configs: Vec<KekProviderConfig>,
    secrets: Arc<SecretRegistry>,
}

impl KekRegistryBuilder {
    pub fn new(secrets: Arc<SecretRegistry>) -> Self {
        Self {
            configs: Vec::new(),
            secrets,
        }
    }

    pub fn with_config(mut self, cfg: KekProviderConfig) -> Self {
        self.configs.push(cfg);
        self
    }

    pub fn extend<I: IntoIterator<Item = KekProviderConfig>>(mut self, cfgs: I) -> Self {
        self.configs.extend(cfgs);
        self
    }

    /// Async because the KEK backends (Vault Transit today, cloud KMS
    /// later) speak async HTTP and perform eager login at registration
    /// so config errors surface at startup. `KekProvider` itself is
    /// async, so the builder is naturally called from an async context.
    pub async fn build(self) -> Result<KekRegistry, SecretsAdapterError> {
        let mut reg = KekRegistry::empty();
        for cfg in self.configs {
            match cfg {
                KekProviderConfig::Local(l) => {
                    let handle = build_local(l, Arc::clone(&self.secrets))?;
                    reg.register(Arc::new(handle))?;
                }
                KekProviderConfig::AwsKms(_) => {
                    return Err(feature_disabled("aws_kms", "aws-kms"));
                }
                KekProviderConfig::GcpKms(_) => {
                    return Err(feature_disabled("gcp_kms", "gcp-kms"));
                }
                KekProviderConfig::AzureKv(_) => {
                    return Err(feature_disabled("azure_key_vault_crypto", "azure-kv"));
                }
                KekProviderConfig::VaultTransit(v) => {
                    let provider = build_vault_transit(&v, &self.secrets).await?;
                    reg.register(provider)?;
                }
                KekProviderConfig::Pkcs11(_) => {
                    return Err(SecretsAdapterError::BackendNotImplemented {
                        backend: "pkcs11",
                    });
                }
            }
        }
        Ok(reg)
    }
}

#[cfg(feature = "vault")]
async fn build_vault_transit(
    cfg: &VaultTransitConfig,
    bootstrap: &SecretRegistry,
) -> Result<Arc<dyn aeon_crypto::kek_provider::KekProvider>, SecretsAdapterError> {
    let provider =
        crate::backends::vault::VaultTransitKekProvider::from_config(cfg, bootstrap).await?;
    Ok(Arc::new(provider))
}

#[cfg(not(feature = "vault"))]
async fn build_vault_transit(
    _cfg: &VaultTransitConfig,
    _bootstrap: &SecretRegistry,
) -> Result<Arc<dyn aeon_crypto::kek_provider::KekProvider>, SecretsAdapterError> {
    Err(feature_disabled("vault_transit", "vault"))
}

// ─── helpers ───────────────────────────────────────────────────────

fn feature_disabled(backend: &'static str, feature: &'static str) -> SecretsAdapterError {
    SecretsAdapterError::BackendFeatureDisabled { backend, feature }
}

fn parse_domain(s: &str) -> Result<KekDomain, SecretsAdapterError> {
    match s {
        "log_context" | "log-context" => Ok(KekDomain::LogContext),
        "data_context" | "data-context" => Ok(KekDomain::DataContext),
        other => Err(SecretsAdapterError::Backend(aeon_types::AeonError::Config {
            message: format!(
                "unknown KEK domain '{other}' — expected 'log_context' or 'data_context'"
            ),
        })),
    }
}

fn parse_ref(s: &str) -> Result<SecretRef, SecretsAdapterError> {
    match SecretRef::parse(s) {
        Ok(Some(r)) => Ok(r),
        Ok(None) => Ok(SecretRef::literal(s)),
        Err(e) => Err(SecretsAdapterError::Backend(aeon_types::AeonError::Config {
            message: e.to_string(),
        })),
    }
}

fn build_local(
    cfg: LocalKekConfig,
    secrets: Arc<SecretRegistry>,
) -> Result<KekHandle, SecretsAdapterError> {
    let domain = parse_domain(&cfg.domain)?;
    let active_ref = parse_ref(&cfg.active_ref)?;
    let mut handle = KekHandle::new(domain, cfg.active_id, active_ref, secrets);
    if let (Some(pid), Some(pref)) = (cfg.previous_id, cfg.previous_ref) {
        let pref = parse_ref(&pref)?;
        handle = handle.with_previous(pid, pref);
    }
    Ok(handle)
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AwsKmsConfig, AwsSmProviderConfig, VaultAuthConfig, VaultProviderConfig,
        VaultTransitConfig,
    };
    use aeon_crypto::kek::KekDomain;

    fn hex_encode(bytes: &[u8]) -> String {
        const HEX: &[u8] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    struct HexEnvProvider;
    impl aeon_types::SecretProvider for HexEnvProvider {
        fn scheme(&self) -> aeon_types::SecretScheme {
            aeon_types::SecretScheme::Env
        }
        fn resolve(
            &self,
            path: &str,
        ) -> Result<aeon_types::SecretBytes, aeon_types::SecretError> {
            let val = std::env::var(path).map_err(|_| {
                aeon_types::SecretError::EnvNotSet(path.to_string())
            })?;
            let bytes: Vec<u8> = (0..val.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&val[i..i + 2], 16).unwrap())
                .collect();
            Ok(aeon_types::SecretBytes::new(bytes))
        }
    }

    #[test]
    fn secret_registry_builder_default_registers_env_dotenv_literal() {
        let reg = SecretRegistryBuilder::new().build().unwrap();
        assert!(reg.has_scheme(aeon_types::SecretScheme::Env));
        assert!(reg.has_scheme(aeon_types::SecretScheme::DotEnv));
        assert!(reg.has_scheme(aeon_types::SecretScheme::Literal));
    }

    #[test]
    fn secret_registry_builder_without_defaults_is_empty() {
        let reg = SecretRegistryBuilder::new().without_defaults().build().unwrap();
        assert!(!reg.has_scheme(aeon_types::SecretScheme::Env));
        assert!(!reg.has_scheme(aeon_types::SecretScheme::DotEnv));
    }

    #[cfg(not(feature = "vault"))]
    #[test]
    fn secret_registry_builder_vault_returns_feature_disabled() {
        let cfg = SecretProviderConfig::Vault(VaultProviderConfig {
            endpoint: "https://v".to_string(),
            mount: "secret".to_string(),
            auth: VaultAuthConfig::Token {
                token_ref: "${ENV:X}".to_string(),
            },
            namespace: None,
            tls_verify: true,
            ca_cert_ref: None,
            retry: Default::default(),
            cache_ttl_secs: 0,
        });
        let err = SecretRegistryBuilder::new().with_config(cfg).build().unwrap_err();
        assert!(matches!(
            err,
            SecretsAdapterError::BackendFeatureDisabled {
                backend: "vault",
                feature: "vault"
            }
        ));
    }

    /// With the `vault` feature compiled, the builder must actually try
    /// to construct the provider against the configured endpoint. We
    /// can't bring up a real Vault here, but the construction path
    /// itself should be exercised — the auth-token resolution + the
    /// AppRole eager login are what matters. Use a known-good mockito
    /// server so we test the happy registration path end-to-end.
    #[cfg(feature = "vault")]
    #[test]
    fn secret_registry_builder_vault_registers_via_real_provider() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/v1/secret/data/probe")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data":{"data":{"value":"ok"}}}"#)
            .create();

        let cfg = SecretProviderConfig::Vault(VaultProviderConfig {
            endpoint: server.url(),
            mount: "secret".to_string(),
            auth: VaultAuthConfig::Token {
                token_ref: "literal-token".to_string(),
            },
            namespace: None,
            tls_verify: false,
            ca_cert_ref: None,
            retry: Default::default(),
            cache_ttl_secs: 0,
        });
        let reg = SecretRegistryBuilder::new().with_config(cfg).build().unwrap();
        assert!(reg.has_scheme(aeon_types::SecretScheme::Vault));

        let bytes = reg.resolve(&SecretRef::vault("probe")).unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "ok");
    }

    #[test]
    fn secret_registry_builder_aws_sm_returns_feature_disabled() {
        let cfg = SecretProviderConfig::AwsSm(AwsSmProviderConfig {
            region: "us-east-1".to_string(),
            endpoint: None,
        });
        let err = SecretRegistryBuilder::new().with_config(cfg).build().unwrap_err();
        assert!(matches!(
            err,
            SecretsAdapterError::BackendFeatureDisabled {
                backend: "aws_secrets_manager",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn kek_registry_builder_local_roundtrips() {
        const VAR: &str = "AEON_TEST_SECRETS_BUILDER_LOCAL";
        unsafe { std::env::set_var(VAR, hex_encode(&[0x44u8; 32])) };

        // Use HexEnvProvider so the env-stored hex string becomes raw
        // 32 bytes — the shape the local KEK path needs.
        let mut secrets = aeon_types::SecretRegistry::empty();
        secrets.register(Arc::new(HexEnvProvider));
        let secrets = Arc::new(secrets);

        let cfg = KekProviderConfig::Local(LocalKekConfig {
            domain: "data_context".to_string(),
            active_id: "v1".to_string(),
            active_ref: format!("${{ENV:{VAR}}}"),
            previous_id: None,
            previous_ref: None,
        });
        let reg = KekRegistryBuilder::new(secrets)
            .with_config(cfg)
            .build()
            .await
            .unwrap();
        assert!(reg.has_domain(KekDomain::DataContext));

        let (dek, wrapped) = reg.wrap_new_dek(KekDomain::DataContext).await.unwrap();
        let unwrapped = reg.unwrap_dek(&wrapped).await.unwrap();
        assert_eq!(dek.expose_bytes(), unwrapped.expose_bytes());

        unsafe { std::env::remove_var(VAR) };
    }

    #[tokio::test]
    async fn kek_registry_builder_aws_kms_returns_feature_disabled() {
        let secrets = Arc::new(aeon_types::SecretRegistry::empty());
        let cfg = KekProviderConfig::AwsKms(AwsKmsConfig {
            domain: "data_context".to_string(),
            region: "us-east-1".to_string(),
            active_key_id: "alias/aeon".to_string(),
            previous_key_id: None,
            endpoint: None,
        });
        let err = KekRegistryBuilder::new(secrets)
            .with_config(cfg)
            .build()
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SecretsAdapterError::BackendFeatureDisabled {
                backend: "aws_kms",
                ..
            }
        ));
    }

    #[cfg(not(feature = "vault"))]
    #[tokio::test]
    async fn kek_registry_builder_vault_transit_returns_feature_disabled() {
        let secrets = Arc::new(aeon_types::SecretRegistry::empty());
        let cfg = KekProviderConfig::VaultTransit(VaultTransitConfig {
            domain: "data_context".to_string(),
            endpoint: "https://v".to_string(),
            mount: "transit".to_string(),
            key_name: "aeon".to_string(),
            auth: VaultAuthConfig::Token {
                token_ref: "${ENV:X}".to_string(),
            },
            namespace: None,
            tls_verify: true,
            retry: Default::default(),
        });
        let err = KekRegistryBuilder::new(secrets)
            .with_config(cfg)
            .build()
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SecretsAdapterError::BackendFeatureDisabled {
                backend: "vault_transit",
                ..
            }
        ));
    }

    /// With the `vault` feature compiled, the Transit registration
    /// path runs the real construction. Use a mockito server so the
    /// AppRole-less Token path succeeds without a live Vault.
    #[cfg(feature = "vault")]
    #[tokio::test]
    async fn kek_registry_builder_vault_transit_registers_via_real_provider() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let mut server = mockito::Server::new_async().await;
        let dek_b64 = BASE64.encode([0x77u8; 32]);
        let _m = server
            .mock("POST", "/v1/transit/datakey/plaintext/aeon-dek")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"data":{{"plaintext":"{dek_b64}","ciphertext":"vault:v1:builder-test"}}}}"#
            ))
            .create_async()
            .await;

        let secrets = Arc::new(aeon_types::SecretRegistry::default_local());
        let cfg = KekProviderConfig::VaultTransit(VaultTransitConfig {
            domain: "data_context".to_string(),
            endpoint: server.url(),
            mount: "transit".to_string(),
            key_name: "aeon-dek".to_string(),
            auth: VaultAuthConfig::Token {
                token_ref: "literal-token".to_string(),
            },
            namespace: None,
            tls_verify: false,
            retry: Default::default(),
        });
        let reg = KekRegistryBuilder::new(secrets)
            .with_config(cfg)
            .build()
            .await
            .unwrap();
        assert!(reg.has_domain(KekDomain::DataContext));

        // Smoke-check the registered provider actually serves wraps.
        let (dek, _wrapped) = reg.wrap_new_dek(KekDomain::DataContext).await.unwrap();
        assert_eq!(dek.expose_bytes(), &[0x77u8; 32]);
    }

    #[test]
    fn parse_domain_accepts_both_spellings() {
        assert_eq!(parse_domain("log_context").unwrap(), KekDomain::LogContext);
        assert_eq!(parse_domain("log-context").unwrap(), KekDomain::LogContext);
        assert_eq!(parse_domain("data_context").unwrap(), KekDomain::DataContext);
        assert_eq!(parse_domain("data-context").unwrap(), KekDomain::DataContext);
        assert!(parse_domain("unknown").is_err());
    }
}
