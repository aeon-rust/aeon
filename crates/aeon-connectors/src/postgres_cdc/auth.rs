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
//! - `Mtls` — warn-and-skip. tokio-postgres takes a separate TLS
//!   connector argument (`tokio_postgres::connect(cfg, NoTls)`); wiring
//!   a rustls-based client-auth connector requires the
//!   `tokio-postgres-rustls` crate, which is not in the workspace. For
//!   server-auth TLS use `sslmode=require` in the base connection
//!   string with a TLS connector crate (follow-up).
//! - `Bearer` / `Basic` / `ApiKey` / `HmacSign` — warn-and-skip.

use aeon_types::{AeonError, OutboundAuthMode, OutboundAuthSigner};
use std::str::FromStr;
use std::sync::Arc;
use tokio_postgres::Config;

/// Parse `conn_string` into a `tokio_postgres::Config` and apply any
/// signer-supplied overrides. Returns the config ready to hand to
/// `Config::connect(NoTls)`.
pub(super) fn resolve_config(
    conn_string: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<Config, AeonError> {
    let mut config = Config::from_str(conn_string).map_err(|e| {
        AeonError::config(format!("postgres connection string parse failed: {e}"))
    })?;

    let Some(s) = signer else { return Ok(config) };
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
            tracing::warn!(
                "Postgres CDC connector: mTLS requires the tokio-postgres-rustls crate \
                 (not in this build). Inline cert/key PEM can't be wired through tokio-postgres' \
                 NoTls connector. Use broker_native + sslmode=require for server-auth TLS."
            );
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
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{BrokerNativeConfig, OutboundAuthConfig};
    use std::collections::BTreeMap;

    fn signer(config: OutboundAuthConfig) -> Arc<OutboundAuthSigner> {
        Arc::new(OutboundAuthSigner::build(config).unwrap())
    }

    #[test]
    fn no_signer_uses_string_verbatim() {
        let cfg = resolve_config(
            "host=localhost user=aeon password=pw dbname=a",
            None,
        )
        .unwrap();
        assert_eq!(cfg.get_user(), Some("aeon"));
        assert_eq!(cfg.get_dbname(), Some("a"));
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
        let cfg = resolve_config(
            "host=localhost user=conn-user password=conn-pw dbname=conn-db",
            Some(&s),
        )
        .unwrap();
        assert_eq!(cfg.get_user(), Some("signer-user"));
        assert_eq!(cfg.get_password(), Some(b"signer-pw".as_slice()));
        assert_eq!(cfg.get_dbname(), Some("signer-db"));
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
        let cfg = resolve_config("host=localhost user=u", Some(&s)).unwrap();
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
        let cfg = resolve_config("host=localhost user=u", Some(&s)).unwrap();
        assert_eq!(cfg.get_application_name(), Some("aeon-cdc"));
    }

    #[test]
    fn none_mode_passes_string_through() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        });
        let cfg = resolve_config("host=localhost user=u dbname=d", Some(&s)).unwrap();
        assert_eq!(cfg.get_user(), Some("u"));
        assert_eq!(cfg.get_dbname(), Some("d"));
    }

    #[test]
    fn mtls_mode_is_warn_and_skip() {
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
        let cfg = resolve_config("host=localhost user=u", Some(&s)).unwrap();
        assert_eq!(cfg.get_user(), Some("u"));
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
        let cfg = resolve_config("host=localhost user=u", Some(&s)).unwrap();
        assert_eq!(cfg.get_user(), Some("u"));
    }

    #[test]
    fn bad_connection_string_is_rejected() {
        let err = resolve_config("this is not a valid conn string =====", None).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("postgres"));
    }
}
