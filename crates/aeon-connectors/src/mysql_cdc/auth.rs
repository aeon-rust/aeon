//! S10 outbound-auth → mysql_async `Opts` translation.
//!
//! Used by `MysqlCdcSource::new`. The operator supplies the base URL
//! (`mysql://host:3306/db`); credentials flow through the signer.
//!
//! **One mode per connector**, matching the other translators:
//!
//! - `None` — URL passed through verbatim.
//! - `BrokerNative` — parse the URL into an `Opts`, convert to
//!   `OptsBuilder`, and overlay selected keys from
//!   `signer.broker_native().values`:
//!     - `user` → `OptsBuilder::user`
//!     - `password` → `OptsBuilder::pass`
//!     - `db_name` → `OptsBuilder::db_name`
//!
//!   Signer credentials win over URL userinfo; a warn line flags the
//!   conflict.
//! - `Mtls` — warn-and-skip. mysql_async's `SslOpts` requires either
//!   the `native-tls-tls` or `rustls-tls` cargo feature on the
//!   mysql_async dep; neither is enabled in this build. Follow-up can
//!   flip the feature and wire `SslOpts::with_client_identity` from
//!   inline PEM.
//! - `Bearer` / `Basic` / `ApiKey` / `HmacSign` — warn-and-skip.

use aeon_types::{AeonError, OutboundAuthMode, OutboundAuthSigner};
use mysql_async::{Opts, OptsBuilder};
use std::sync::Arc;

/// Parse `url` into an `Opts` and apply any signer-supplied overrides.
/// Returns the opts ready to hand to `mysql_async::Pool::new`.
pub(super) fn resolve_opts(
    url: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<Opts, AeonError> {
    let opts = Opts::from_url(url)
        .map_err(|e| AeonError::config(format!("mysql url parse failed: {e}")))?;

    let Some(s) = signer else { return Ok(opts) };
    match s.mode() {
        OutboundAuthMode::None => Ok(opts),
        OutboundAuthMode::BrokerNative => {
            let bn = match s.broker_native() {
                Some(bn) => bn,
                None => return Ok(opts),
            };

            let had_user = opts.user().is_some();
            let had_pass = opts.pass().is_some();

            let mut builder = OptsBuilder::from_opts(opts);
            let mut signer_had_creds = false;

            if let Some(u) = bn.values.get("user") {
                builder = builder.user(Some(u.clone()));
                signer_had_creds = true;
            }
            if let Some(p) = bn.values.get("password") {
                builder = builder.pass(Some(p.clone()));
                signer_had_creds = true;
            }
            if let Some(db) = bn.values.get("db_name") {
                builder = builder.db_name(Some(db.clone()));
            }

            if (had_user || had_pass) && signer_had_creds {
                tracing::warn!(
                    "MySQL URL embedded userinfo; outbound-auth signer credentials override it"
                );
            }

            Ok(builder.into())
        }
        OutboundAuthMode::Mtls => {
            tracing::warn!(
                "MySQL CDC connector: mTLS requires the mysql_async 'native-tls-tls' or \
                 'rustls-tls' cargo feature (not enabled in this build). Use broker_native."
            );
            Ok(opts)
        }
        OutboundAuthMode::Bearer
        | OutboundAuthMode::Basic
        | OutboundAuthMode::ApiKey
        | OutboundAuthMode::HmacSign => {
            tracing::warn!(
                mode = ?s.mode(),
                "MySQL CDC connector: HTTP-style auth mode is not applicable to the MySQL \
                 wire protocol; ignored. Use broker_native."
            );
            Ok(opts)
        }
    }
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
    fn no_signer_uses_url_verbatim() {
        let opts = resolve_opts("mysql://aeon:pw@127.0.0.1:3306/mydb", None).unwrap();
        assert_eq!(opts.user(), Some("aeon"));
        assert_eq!(opts.pass(), Some("pw"));
        assert_eq!(opts.db_name(), Some("mydb"));
    }

    #[test]
    fn broker_native_overrides_url_credentials() {
        let mut values = BTreeMap::new();
        values.insert("user".to_string(), "signer-user".to_string());
        values.insert("password".to_string(), "signer-pw".to_string());
        values.insert("db_name".to_string(), "signer-db".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let opts = resolve_opts(
            "mysql://url-user:url-pw@127.0.0.1:3306/url-db",
            Some(&s),
        )
        .unwrap();
        assert_eq!(opts.user(), Some("signer-user"));
        assert_eq!(opts.pass(), Some("signer-pw"));
        assert_eq!(opts.db_name(), Some("signer-db"));
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
        let opts = resolve_opts("mysql://u@127.0.0.1:3306/d", Some(&s)).unwrap();
        assert_eq!(opts.user(), Some("u"));
        assert_eq!(opts.pass(), Some("p"));
    }

    #[test]
    fn none_mode_passes_url_through() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        });
        let opts = resolve_opts("mysql://u:p@127.0.0.1:3306/d", Some(&s)).unwrap();
        assert_eq!(opts.user(), Some("u"));
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
        let opts = resolve_opts("mysql://u:p@127.0.0.1:3306/d", Some(&s)).unwrap();
        assert_eq!(opts.user(), Some("u"));
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
        let opts = resolve_opts("mysql://u:p@127.0.0.1:3306/d", Some(&s)).unwrap();
        assert_eq!(opts.user(), Some("u"));
    }

    #[test]
    fn bad_url_is_rejected() {
        let err = resolve_opts("not-a-url", None).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("mysql"));
    }
}
