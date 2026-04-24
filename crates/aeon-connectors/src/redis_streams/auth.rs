//! S10 outbound-auth → redis `ConnectionInfo` translation.
//!
//! Shared by `RedisSource` and `RedisSink`. The operator supplies only the
//! address part of the Redis URL (`redis://host:6379/0`); credentials flow
//! through the signer so secrets are never embedded in YAML or logs.
//!
//! **One mode per connector**, matching the kafka translator:
//!
//! - `None` — URL passed through verbatim.
//! - `BrokerNative` — `username` / `password` / `db` values from
//!   `signer.broker_native().values` override the URL-embedded equivalents
//!   on the parsed `ConnectionInfo`. URLs that already carry userinfo are
//!   accepted but the signer wins; a warn line is emitted so the conflict
//!   is visible.
//! - `Mtls` — warn-and-skip. The `redis` crate in this build is linked
//!   without its `tls-rustls` feature; a follow-up can enable it and wire
//!   client certs through `redis::TlsCertificates`.
//! - `Bearer` / `Basic` / `ApiKey` / `HmacSign` — warn-and-skip (not
//!   meaningful to the Redis RESP protocol).

use aeon_types::{AeonError, OutboundAuthMode, OutboundAuthSigner};
use redis::{ConnectionInfo, IntoConnectionInfo};
use std::sync::Arc;

/// Resolve the address string plus an optional outbound-auth signer into a
/// `redis::ConnectionInfo` ready to hand to `redis::Client::open`.
pub(super) fn resolve_connection_info(
    url: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<ConnectionInfo, AeonError> {
    let mut info = url
        .into_connection_info()
        .map_err(|e| AeonError::config(format!("redis url parse failed: {e}")))?;

    let Some(s) = signer else { return Ok(info) };
    match s.mode() {
        OutboundAuthMode::None => { /* pass-through */ }
        OutboundAuthMode::BrokerNative => {
            if let Some(bn) = s.broker_native() {
                let had_userinfo = info.redis.username.is_some() || info.redis.password.is_some();
                let mut signer_had_creds = false;
                if let Some(u) = bn.values.get("username") {
                    info.redis.username = Some(u.clone());
                    signer_had_creds = true;
                }
                if let Some(p) = bn.values.get("password") {
                    info.redis.password = Some(p.clone());
                    signer_had_creds = true;
                }
                if let Some(db) = bn.values.get("db") {
                    info.redis.db = db.parse::<i64>().map_err(|e| {
                        AeonError::config(format!("redis broker_native 'db' not an integer: {e}"))
                    })?;
                }
                if had_userinfo && signer_had_creds {
                    tracing::warn!(
                        "Redis URL embedded userinfo; outbound-auth signer credentials override it"
                    );
                }
            }
        }
        OutboundAuthMode::Mtls => {
            tracing::warn!(
                "Redis connector: mTLS requires the redis crate's tls-rustls feature — not enabled in this build; ignored. \
                 Use broker_native with a rediss:// URL for server-auth TLS."
            );
        }
        OutboundAuthMode::Bearer
        | OutboundAuthMode::Basic
        | OutboundAuthMode::ApiKey
        | OutboundAuthMode::HmacSign => {
            tracing::warn!(
                mode = ?s.mode(),
                "Redis connector: HTTP-style auth mode is not applicable to RESP; ignored. Use broker_native."
            );
        }
    }
    Ok(info)
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
        let info = resolve_connection_info("redis://127.0.0.1:6379/2", None).unwrap();
        assert_eq!(info.redis.db, 2);
        assert!(info.redis.username.is_none());
        assert!(info.redis.password.is_none());
    }

    #[test]
    fn broker_native_overrides_url_credentials() {
        // URL carries one pair, signer carries another — signer must win.
        let mut values = BTreeMap::new();
        values.insert("username".to_string(), "aeon-signer".to_string());
        values.insert("password".to_string(), "s3cret".to_string());
        values.insert("db".to_string(), "5".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let info =
            resolve_connection_info("redis://url-user:url-pass@127.0.0.1:6379/0", Some(&s))
                .unwrap();
        assert_eq!(info.redis.username.as_deref(), Some("aeon-signer"));
        assert_eq!(info.redis.password.as_deref(), Some("s3cret"));
        assert_eq!(info.redis.db, 5);
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
        let info = resolve_connection_info("redis://127.0.0.1:6379", Some(&s)).unwrap();
        assert!(info.redis.username.is_none());
        assert_eq!(info.redis.password.as_deref(), Some("p"));
    }

    #[test]
    fn broker_native_rejects_non_integer_db() {
        let mut values = BTreeMap::new();
        values.insert("db".to_string(), "not-a-number".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let err = resolve_connection_info("redis://127.0.0.1:6379", Some(&s)).unwrap_err();
        assert!(format!("{err}").contains("db"));
    }

    #[test]
    fn none_mode_passes_url_through() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        });
        let info = resolve_connection_info("redis://127.0.0.1:6379/7", Some(&s)).unwrap();
        assert_eq!(info.redis.db, 7);
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
        let info = resolve_connection_info("redis://127.0.0.1:6379", Some(&s)).unwrap();
        // URL had no credentials and we didn't invent any.
        assert!(info.redis.username.is_none());
        assert!(info.redis.password.is_none());
    }

    #[test]
    fn bad_url_is_rejected() {
        let err = resolve_connection_info("not-a-url", None).unwrap_err();
        assert!(format!("{err}").contains("redis"));
    }
}
