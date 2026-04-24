//! S10 outbound-auth → tokio-postgres `Config` translation.
//!
//! Used by `PostgresCdcSource::establish`. The operator supplies the
//! base `connection_string` (libpq keyword=value or `postgres://` URL);
//! credentials flow through the signer.
//!
//! **One mode per connector**, matching the other translators:
//!
//! - `None` — string passed through verbatim.
//! - `BrokerNative` — parse the string into a `tokio_postgres::Config`
//!   and then overlay selected keys from `signer.broker_native().values`
//!   onto the parsed config:
//!     - `user` → `Config::user`
//!     - `password` → `Config::password` (bytes, moves out of signer)
//!     - `dbname` → `Config::dbname`
//!     - `application_name` → `Config::application_name`
//!
//!   Signer credentials win over any embedded in the connection string;
//!   a warn line is emitted on conflict so the override is visible.
//! - `Mtls` — parse the signer's cert/key PEMs into a `rustls::ClientConfig`
//!   via [`crate::mtls_pem::build_mtls_client_config`] and return it alongside
//!   the `Config`. The source then hands the rustls config to
//!   `tokio_postgres_rustls::MakeRustlsConnect::new(tls)` and uses that as the
//!   TLS connector for `Config::connect`. Inline PEM only; no tempfiles.
//! - `Bearer` / `Basic` / `ApiKey` / `HmacSign` — warn-and-skip.

use aeon_types::{AeonError, OutboundAuthMode, OutboundAuthSigner};
use std::str::FromStr;
use std::sync::Arc;
use tokio_postgres::Config;

/// Parse `conn_string` into a `tokio_postgres::Config`, apply any
/// signer-supplied overrides, and — when the signer is in `Mtls` mode —
/// return a ready-to-use `rustls::ClientConfig` carrying the client
/// identity. The caller wraps the rustls config in a
/// `tokio_postgres_rustls::MakeRustlsConnect` and passes it to
/// `Config::connect(...)`; the `None` branch means "use `NoTls`".
pub(super) fn resolve_config(
    conn_string: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<(Config, Option<rustls::ClientConfig>), AeonError> {
    let mut config = Config::from_str(conn_string).map_err(|e| {
        AeonError::config(format!("postgres connection string parse failed: {e}"))
    })?;

    let Some(s) = signer else { return Ok((config, None)) };
    let mut tls: Option<rustls::ClientConfig> = None;
    match s.mode() {
        OutboundAuthMode::None => { /* pass-through */ }
        OutboundAuthMode::BrokerNative => {
            if let Some(bn) = s.broker_native() {
                let had_user = config.get_user().is_some();
                let had_password = config.get_password().is_some();
                let mut signer_had_creds = false;

                if let Some(u) = bn.values.get("user") {
                    config.user(u);
                    signer_had_creds = true;
                }
                if let Some(p) = bn.values.get("password") {
                    config.password(p);
                    signer_had_creds = true;
                }
                if let Some(db) = bn.values.get("dbname") {
                    config.dbname(db);
                }
                if let Some(app) = bn.values.get("application_name") {
                    config.application_name(app);
                }

                if (had_user || had_password) && signer_had_creds {
                    tracing::warn!(
                        "Postgres connection string embedded user/password; outbound-auth \
                         signer credentials override them"
                    );
                }
            }
        }
        OutboundAuthMode::Mtls => {
            let cert_pem = s.mtls_cert_pem().ok_or_else(|| {
                AeonError::config(
                    "postgres CDC mTLS: signer is in Mtls mode but missing cert PEM",
                )
            })?;
            let key_pem = s.mtls_key_pem().ok_or_else(|| {
                AeonError::config(
                    "postgres CDC mTLS: signer is in Mtls mode but missing key PEM",
                )
            })?;
            tls = Some(crate::mtls_pem::build_mtls_client_config(cert_pem, key_pem)?);
        }
        OutboundAuthMode::Bearer
        | OutboundAuthMode::Basic
        | OutboundAuthMode::ApiKey
        | OutboundAuthMode::HmacSign => {
            tracing::warn!(
                mode = ?s.mode(),
                "Postgres CDC connector: HTTP-style auth mode is not applicable to the \
                 Postgres wire protocol; ignored. Use broker_native."
            );
        }
    }
    Ok((config, tls))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{BrokerNativeConfig, OutboundAuthConfig};
    use std::collections::BTreeMap;

    fn signer(config: OutboundAuthConfig) -> Arc<OutboundAuthSigner> {
        Arc::new(OutboundAuthSigner::build(config).unwrap())
    }

    fn fixture_cert_and_key() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn no_signer_uses_string_verbatim() {
        let (cfg, tls) = resolve_config(
            "host=localhost user=aeon password=pw dbname=a",
            None,
        )
        .unwrap();
        assert_eq!(cfg.get_user(), Some("aeon"));
        assert_eq!(cfg.get_dbname(), Some("a"));
        assert!(tls.is_none(), "no signer must not produce a TLS config");
    }

    #[test]
    fn broker_native_overrides_user_and_password() {
        let mut values = BTreeMap::new();
        values.insert("user".to_string(), "signer-user".to_string());
        values.insert("password".to_string(), "signer-pw".to_string());
        values.insert("dbname".to_string(), "signer-db".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let (cfg, tls) = resolve_config(
            "host=localhost user=conn-user password=conn-pw dbname=conn-db",
            Some(&s),
        )
        .unwrap();
        assert_eq!(cfg.get_user(), Some("signer-user"));
        assert_eq!(cfg.get_password(), Some(b"signer-pw".as_slice()));
        assert_eq!(cfg.get_dbname(), Some("signer-db"));
        assert!(tls.is_none());
    }

    #[test]
    fn broker_native_password_only_sets_password() {
        let mut values = BTreeMap::new();
        values.insert("password".to_string(), "p".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let (cfg, _tls) = resolve_config("host=localhost user=u", Some(&s)).unwrap();
        assert_eq!(cfg.get_user(), Some("u"));
        assert_eq!(cfg.get_password(), Some(b"p".as_slice()));
    }

    #[test]
    fn broker_native_application_name_applied() {
        let mut values = BTreeMap::new();
        values.insert("application_name".to_string(), "aeon-cdc".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let (cfg, _tls) = resolve_config("host=localhost user=u", Some(&s)).unwrap();
        assert_eq!(cfg.get_application_name(), Some("aeon-cdc"));
    }

    #[test]
    fn none_mode_passes_string_through() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        });
        let (cfg, tls) = resolve_config("host=localhost user=u dbname=d", Some(&s)).unwrap();
        assert_eq!(cfg.get_user(), Some("u"));
        assert_eq!(cfg.get_dbname(), Some("d"));
        assert!(tls.is_none());
    }

    #[test]
    fn mtls_mode_produces_rustls_client_config() {
        let (cert_pem, key_pem) = fixture_cert_and_key();
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(aeon_types::OutboundMtlsConfig { cert_pem, key_pem }),
            ..Default::default()
        });
        let (cfg, tls) = resolve_config("host=localhost user=u", Some(&s)).unwrap();
        assert_eq!(cfg.get_user(), Some("u"));
        assert!(
            tls.is_some(),
            "mTLS signer must produce a rustls::ClientConfig"
        );
    }

    #[test]
    fn mtls_mode_rejects_malformed_pem() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(aeon_types::OutboundMtlsConfig {
                cert_pem: "not a PEM".to_string(),
                key_pem: "not a key".to_string(),
            }),
            ..Default::default()
        });
        let err = resolve_config("host=localhost user=u", Some(&s)).unwrap_err();
        let msg = format!("{err}").to_lowercase();
        assert!(msg.contains("mtls") || msg.contains("certificate") || msg.contains("pem"));
    }

    #[test]
    fn bearer_mode_is_warn_and_skip() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            bearer: Some(aeon_types::BearerConfig {
                token: "x".to_string(),
            }),
            ..Default::default()
        });
        let (cfg, tls) = resolve_config("host=localhost user=u", Some(&s)).unwrap();
        assert_eq!(cfg.get_user(), Some("u"));
        assert!(tls.is_none());
    }

    #[test]
    fn bad_connection_string_is_rejected() {
        let err = resolve_config("this is not a valid conn string =====", None).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("postgres"));
    }
}
