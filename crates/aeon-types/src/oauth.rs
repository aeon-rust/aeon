//! OAuth 2.0 Client Credentials configuration types.
//!
//! OAuth is an optional Layer 2.5 security mechanism for T3/T4 processors.
//! When enabled, processors must present a valid JWT in addition to ED25519
//! challenge-response authentication.

use serde::{Deserialize, Serialize};

/// OAuth 2.0 configuration for processor authentication.
///
/// When configured, Aeon validates JWTs presented by T3/T4 processors
/// during AWPP registration. The JWT must be obtained via Client Credentials
/// Grant from the configured IdP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// Whether OAuth is enabled (Layer 2.5).
    #[serde(default)]
    pub enabled: bool,
    /// JWKS URI for fetching the IdP's public keys (e.g., "https://idp.example.com/.well-known/jwks.json").
    pub jwks_uri: String,
    /// Expected JWT issuer (`iss` claim).
    pub issuer: String,
    /// Expected JWT audience (`aud` claim). Typically "aeon-processors".
    #[serde(default = "default_audience")]
    pub audience: String,
    /// Required scopes (must all be present in the JWT).
    #[serde(default = "default_required_scopes")]
    pub required_scopes: Vec<String>,
    /// JWT claim that maps to the processor name (e.g., "sub" or "client_id").
    #[serde(default = "default_processor_claim")]
    pub processor_name_claim: String,
    /// JWKS cache TTL in seconds (how long to cache the IdP's public keys).
    #[serde(default = "default_jwks_cache_ttl")]
    pub jwks_cache_ttl_secs: u64,
    /// Grace period in seconds for token expiry (accept tokens expired within this window).
    #[serde(default = "default_grace_period")]
    pub expiry_grace_secs: u64,
}

fn default_audience() -> String {
    "aeon-processors".into()
}

fn default_required_scopes() -> Vec<String> {
    vec!["aeon:process".into()]
}

fn default_processor_claim() -> String {
    "sub".into()
}

fn default_jwks_cache_ttl() -> u64 {
    3600 // 1 hour
}

fn default_grace_period() -> u64 {
    30 // 30 seconds
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            jwks_uri: String::new(),
            issuer: String::new(),
            audience: default_audience(),
            required_scopes: default_required_scopes(),
            processor_name_claim: default_processor_claim(),
            jwks_cache_ttl_secs: default_jwks_cache_ttl(),
            expiry_grace_secs: default_grace_period(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_disabled() {
        let config = OAuthConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.audience, "aeon-processors");
        assert_eq!(config.required_scopes, vec!["aeon:process"]);
        assert_eq!(config.processor_name_claim, "sub");
    }

    #[test]
    fn oauth_config_serde_roundtrip() {
        let config = OAuthConfig {
            enabled: true,
            jwks_uri: "https://idp.example.com/.well-known/jwks.json".into(),
            issuer: "https://idp.example.com".into(),
            audience: "aeon-processors".into(),
            required_scopes: vec!["aeon:process".into(), "aeon:admin".into()],
            processor_name_claim: "client_id".into(),
            jwks_cache_ttl_secs: 7200,
            expiry_grace_secs: 60,
        };

        let json = serde_json::to_string(&config).unwrap();
        let back: OAuthConfig = serde_json::from_str(&json).unwrap();
        assert!(back.enabled);
        assert_eq!(back.issuer, "https://idp.example.com");
        assert_eq!(back.required_scopes.len(), 2);
        assert_eq!(back.jwks_cache_ttl_secs, 7200);
    }

    #[test]
    fn oauth_config_serde_with_defaults() {
        // Minimal JSON — defaults should fill in
        let json = r#"{"enabled":true,"jwks_uri":"https://x/jwks","issuer":"https://x"}"#;
        let config: OAuthConfig = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.audience, "aeon-processors");
        assert_eq!(config.required_scopes, vec!["aeon:process"]);
        assert_eq!(config.processor_name_claim, "sub");
        assert_eq!(config.jwks_cache_ttl_secs, 3600);
        assert_eq!(config.expiry_grace_secs, 30);
    }
}
