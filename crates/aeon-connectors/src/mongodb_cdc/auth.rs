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
//! - `Mtls` — uses [`crate::mongodb_cdc::mtls_tempfile::SecureMtlsTempFile`]
//!   to materialise the signer's cert+key PEMs as a process-owned tempfile
//!   (0600 perms on Unix, owner-only ACL on Windows), then sets
//!   `TlsOptions::cert_key_file_path` to that tempfile. The tempfile is
//!   returned alongside the parsed options so the caller can hold it for
//!   the `Client`'s lifetime — it's deleted when dropped, with a best-
//!   effort zeroise pass before removal. MongoDB's `TlsOptions` API is
//!   the bottleneck here (no inline-PEM accessor in the driver); per
//!   ROADMAP §S10 this is the agreed short-term path.
//! - `Bearer` / `Basic` / `ApiKey` / `HmacSign` — warn-and-skip. HTTP
//!   header auth is not applicable to the MongoDB wire protocol.

use aeon_types::{AeonError, OutboundAuthMode, OutboundAuthSigner};
use mongodb::options::{ClientOptions, TlsOptions};
use std::sync::Arc;

use super::mtls_tempfile::SecureMtlsTempFile;

/// Parse `uri` into `ClientOptions` and apply any signer-supplied overrides.
/// Returns the options alongside an optional `SecureMtlsTempFile` that the
/// caller must keep alive for the lifetime of the resulting
/// `mongodb::Client` (see module doc for why).
pub(super) async fn resolve_options(
    uri: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<(ClientOptions, Option<SecureMtlsTempFile>), AeonError> {
    let mut options = ClientOptions::parse(uri)
        .await
        .map_err(|e| AeonError::config(format!("mongodb uri parse failed: {e}")))?;

    let Some(s) = signer else { return Ok((options, None)) };
    match s.mode() {
        OutboundAuthMode::None => Ok((options, None)),
        OutboundAuthMode::BrokerNative => {
            let Some(bn) = s.broker_native() else {
                return Ok((options, None));
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
            Ok((options, None))
        }
        OutboundAuthMode::Mtls => {
            let cert_pem = s.mtls_cert_pem().ok_or_else(|| {
                AeonError::config(
                    "mongodb CDC mTLS: signer is in Mtls mode but missing cert PEM",
                )
            })?;
            let key_pem = s.mtls_key_pem().ok_or_else(|| {
                AeonError::config(
                    "mongodb CDC mTLS: signer is in Mtls mode but missing key PEM",
                )
            })?;
            let tempfile = SecureMtlsTempFile::write_pem(cert_pem, key_pem)?;
            let mut tls = TlsOptions::default();
            tls.cert_key_file_path = Some(tempfile.path().to_path_buf());
            options.tls = Some(mongodb::options::Tls::Enabled(tls));
            Ok((options, Some(tempfile)))
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
            Ok((options, None))
        }
    }
}

/// Build a `mongodb::Client` from `uri` and optional outbound-auth signer.
/// Returns the client plus any mTLS tempfile the caller must hold alive —
/// the tempfile carries the signer's cert/key and is deleted on drop.
pub(super) async fn resolve_client(
    uri: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<(mongodb::Client, Option<SecureMtlsTempFile>), AeonError> {
    let (options, tempfile) = resolve_options(uri, signer).await?;
    let client = mongodb::Client::with_options(options)
        .map_err(|e| AeonError::connection(format!("mongodb connect failed: {e}")))?;
    Ok((client, tempfile))
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
        let (opts, tmp) =
            resolve_options("mongodb://aeon:pw@127.0.0.1:27017/mydb", None)
                .await
                .unwrap();
        let c = cred(&opts);
        assert_eq!(c.username.as_deref(), Some("aeon"));
        assert_eq!(c.password.as_deref(), Some("pw"));
        assert!(tmp.is_none(), "no signer must not produce a tempfile");
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
        let (opts, tmp) = resolve_options(
            "mongodb://url-user:url-pw@127.0.0.1:27017/url-db",
            Some(&s),
        )
        .await
        .unwrap();
        let c = cred(&opts);
        assert_eq!(c.username.as_deref(), Some("signer-user"));
        assert_eq!(c.password.as_deref(), Some("signer-pw"));
        assert_eq!(c.source.as_deref(), Some("signer-src"));
        assert!(tmp.is_none());
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
        let (opts, _) = resolve_options("mongodb://127.0.0.1:27017/d", Some(&s))
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
        let (opts, _) =
            resolve_options("mongodb://u:old-pw@127.0.0.1:27017/d", Some(&s))
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
        let (opts, tmp) = resolve_options("mongodb://u:p@127.0.0.1:27017/d", Some(&s))
            .await
            .unwrap();
        assert_eq!(cred(&opts).username.as_deref(), Some("u"));
        assert!(tmp.is_none());
    }

    #[tokio::test]
    async fn mtls_mode_attaches_tempfile_and_tls_options() {
        let (cert_pem, key_pem) = fixture_cert_and_key();
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(aeon_types::OutboundMtlsConfig { cert_pem, key_pem }),
            ..Default::default()
        });
        let (opts, tmp) = resolve_options("mongodb://u:p@127.0.0.1:27017/d", Some(&s))
            .await
            .unwrap();
        let tmp = tmp.expect("mTLS must produce a tempfile");
        // URI userinfo still preserved.
        assert_eq!(cred(&opts).username.as_deref(), Some("u"));
        // TLS enabled with cert_key_file_path pointing at our tempfile.
        match &opts.tls {
            Some(mongodb::options::Tls::Enabled(tls_opts)) => {
                let path = tls_opts
                    .cert_key_file_path
                    .as_ref()
                    .expect("cert_key_file_path set");
                assert_eq!(path, tmp.path(), "path must match the tempfile handle");
                assert!(path.exists(), "tempfile must exist while held");
            }
            other => panic!("expected TLS Enabled with cert path, got {other:?}"),
        }
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
        let (opts, tmp) = resolve_options("mongodb://u:p@127.0.0.1:27017/d", Some(&s))
            .await
            .unwrap();
        assert_eq!(cred(&opts).username.as_deref(), Some("u"));
        assert!(tmp.is_none());
    }

    #[tokio::test]
    async fn bad_uri_is_rejected() {
        let err = resolve_options("not-a-mongodb-uri", None).await.unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("mongodb"));
    }

    fn fixture_cert_and_key() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }
}
