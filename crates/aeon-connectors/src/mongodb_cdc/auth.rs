//! S10 outbound-auth → mongodb `ClientOptions` translation.
//!
//! Used by `MongoDbCdcSource::new`. The operator supplies the base URI
//! (`mongodb://host:27017` or `mongodb+srv://...`); credentials flow
//! through the signer.
//!
//! **One mode per connector**, matching the other translators:
//!
//! - `None` — URI parsed verbatim via `ClientOptions::parse`.
//! - `BrokerNative` — parse the URI into `ClientOptions`, overlay the
//!   `credential` field from `signer.broker_native().values`:
//!     - `username` → `Credential::username`
//!     - `password` → `Credential::password`
//!     - `auth_source` → `Credential::source`
//!
//!   Signer credentials win over URI userinfo; a warn line flags the
//!   conflict.
//! - `Mtls` — warn-and-skip. `mongodb::options::TlsOptions` takes a
//!   `cert_key_file_path: PathBuf`, not inline PEM. S1 policy forbids
//!   writing signer-supplied PEM to a temp file outside the deploy dir,
//!   so wiring this mode requires a follow-up that either teaches the
//!   mongodb driver to accept inline bytes or holds the PEM in a
//!   process-scoped tempfile with an explicit signer opt-in.
//! - `Bearer` / `Basic` / `ApiKey` / `HmacSign` — warn-and-skip. HTTP
//!   header auth is not applicable to the MongoDB wire protocol.

use aeon_types::{AeonError, OutboundAuthMode, OutboundAuthSigner};
use mongodb::options::ClientOptions;
use std::sync::Arc;

/// Parse `uri` into `ClientOptions` and apply any signer-supplied overrides.
/// Returns the options ready to hand to `mongodb::Client::with_options`.
pub(super) async fn resolve_options(
    uri: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<ClientOptions, AeonError> {
    let mut options = ClientOptions::parse(uri)
        .await
        .map_err(|e| AeonError::config(format!("mongodb uri parse failed: {e}")))?;

    let Some(s) = signer else { return Ok(options) };
    match s.mode() {
        OutboundAuthMode::None => Ok(options),
        OutboundAuthMode::BrokerNative => {
            let Some(bn) = s.broker_native() else {
                return Ok(options);
            };

            let had_credential = options.credential.is_some();
            let mut cred = options.credential.take().unwrap_or_default();
            let mut signer_had_creds = false;

            if let Some(u) = bn.values.get("username") {
                cred.username = Some(u.clone());
                signer_had_creds = true;
            }
            if let Some(p) = bn.values.get("password") {
                cred.password = Some(p.clone());
                signer_had_creds = true;
            }
            if let Some(src) = bn.values.get("auth_source") {
                cred.source = Some(src.clone());
            }

            if had_credential && signer_had_creds {
                tracing::warn!(
                    "MongoDB URI embedded credentials; outbound-auth signer credentials override them"
                );
            }

            options.credential = Some(cred);
            Ok(options)
        }
        OutboundAuthMode::Mtls => {
            tracing::warn!(
                "MongoDB CDC connector: mTLS requires a filesystem PEM path \
                 (TlsOptions::cert_key_file_path); inline PEM from the signer \
                 is not wired in this build. Use broker_native."
            );
            Ok(options)
        }
        OutboundAuthMode::Bearer
        | OutboundAuthMode::Basic
        | OutboundAuthMode::ApiKey
        | OutboundAuthMode::HmacSign => {
            tracing::warn!(
                mode = ?s.mode(),
                "MongoDB CDC connector: HTTP-style auth mode is not applicable to the MongoDB \
                 wire protocol; ignored. Use broker_native."
            );
            Ok(options)
        }
    }
}

/// Build a `mongodb::Client` from `uri` and optional outbound-auth signer.
pub(super) async fn resolve_client(
    uri: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<mongodb::Client, AeonError> {
    let options = resolve_options(uri, signer).await?;
    mongodb::Client::with_options(options)
        .map_err(|e| AeonError::connection(format!("mongodb connect failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{BrokerNativeConfig, OutboundAuthConfig};
    use mongodb::options::Credential;
    use std::collections::BTreeMap;

    fn signer(config: OutboundAuthConfig) -> Arc<OutboundAuthSigner> {
        Arc::new(OutboundAuthSigner::build(config).unwrap())
    }

    fn cred(opts: &ClientOptions) -> &Credential {
        opts.credential.as_ref().expect("credential present")
    }

    #[tokio::test]
    async fn no_signer_uses_uri_verbatim() {
        let opts = resolve_options("mongodb://aeon:pw@127.0.0.1:27017/mydb", None)
            .await
            .unwrap();
        let c = cred(&opts);
        assert_eq!(c.username.as_deref(), Some("aeon"));
        assert_eq!(c.password.as_deref(), Some("pw"));
    }

    #[tokio::test]
    async fn broker_native_overrides_uri_credentials() {
        let mut values = BTreeMap::new();
        values.insert("username".to_string(), "signer-user".to_string());
        values.insert("password".to_string(), "signer-pw".to_string());
        values.insert("auth_source".to_string(), "signer-src".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let opts = resolve_options(
            "mongodb://url-user:url-pw@127.0.0.1:27017/url-db",
            Some(&s),
        )
        .await
        .unwrap();
        let c = cred(&opts);
        assert_eq!(c.username.as_deref(), Some("signer-user"));
        assert_eq!(c.password.as_deref(), Some("signer-pw"));
        assert_eq!(c.source.as_deref(), Some("signer-src"));
    }

    #[tokio::test]
    async fn broker_native_sets_credentials_on_uri_without_userinfo() {
        let mut values = BTreeMap::new();
        values.insert("username".to_string(), "u".to_string());
        values.insert("password".to_string(), "p".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let opts = resolve_options("mongodb://127.0.0.1:27017/d", Some(&s))
            .await
            .unwrap();
        let c = cred(&opts);
        assert_eq!(c.username.as_deref(), Some("u"));
        assert_eq!(c.password.as_deref(), Some("p"));
    }

    #[tokio::test]
    async fn broker_native_password_only_preserves_uri_username() {
        let mut values = BTreeMap::new();
        values.insert("password".to_string(), "new-pw".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let opts = resolve_options("mongodb://u:old-pw@127.0.0.1:27017/d", Some(&s))
            .await
            .unwrap();
        let c = cred(&opts);
        assert_eq!(c.username.as_deref(), Some("u"));
        assert_eq!(c.password.as_deref(), Some("new-pw"));
    }

    #[tokio::test]
    async fn none_mode_passes_uri_through() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        });
        let opts = resolve_options("mongodb://u:p@127.0.0.1:27017/d", Some(&s))
            .await
            .unwrap();
        assert_eq!(cred(&opts).username.as_deref(), Some("u"));
    }

    #[tokio::test]
    async fn mtls_mode_is_warn_and_skip() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(aeon_types::OutboundMtlsConfig {
                cert_pem: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
                    .to_string(),
                key_pem: "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n"
                    .to_string(),
            }),
            ..Default::default()
        });
        let opts = resolve_options("mongodb://u:p@127.0.0.1:27017/d", Some(&s))
            .await
            .unwrap();
        // URI userinfo preserved; no mutation applied.
        assert_eq!(cred(&opts).username.as_deref(), Some("u"));
    }

    #[tokio::test]
    async fn bearer_mode_is_warn_and_skip() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            bearer: Some(aeon_types::BearerConfig {
                token: "x".to_string(),
            }),
            ..Default::default()
        });
        let opts = resolve_options("mongodb://u:p@127.0.0.1:27017/d", Some(&s))
            .await
            .unwrap();
        assert_eq!(cred(&opts).username.as_deref(), Some("u"));
    }

    #[tokio::test]
    async fn bad_uri_is_rejected() {
        let err = resolve_options("not-a-mongodb-uri", None).await.unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("mongodb"));
    }
}
