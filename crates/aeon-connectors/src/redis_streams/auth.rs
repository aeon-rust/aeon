//! S10 outbound-auth → redis `Client` translation.
//!
//! Shared by `RedisSource` and `RedisSink`. The operator supplies only the
//! address part of the Redis URL (`redis://host:6379/0` or
//! `rediss://host:6380/0`); credentials and cert/key material flow through
//! the signer so secrets are never embedded in YAML or logs.
//!
//! **One mode per connector**, matching the kafka translator:
//!
//! - `None` — URL passed through verbatim via `Client::open`.
//! - `BrokerNative` — `username` / `password` / `db` values from
//!   `signer.broker_native().values` override the URL-embedded equivalents
//!   on the parsed `ConnectionInfo`. URLs that already carry userinfo are
//!   accepted but the signer wins; a warn line is emitted so the conflict
//!   is visible.
//! - `Mtls` — uses `redis::Client::build_with_tls(...)` with the signer's
//!   `mtls_cert_pem` / `mtls_key_pem` fed into a `TlsCertificates` struct
//!   (inline PEM, no tempfiles, cert/key zeroised via `SecretBytes` when
//!   the signer drops). The scheme is auto-upgraded to `rediss://` when
//!   the operator provided a plain `redis://` URL, since the redis crate
//!   requires TLS schemes for mTLS client construction. Root certificate
//!   is inherited from the process trust store (webpki-roots via the
//!   `tls-rustls-webpki-roots` feature).
//! - `Bearer` / `Basic` / `ApiKey` / `HmacSign` — warn-and-skip (not
//!   meaningful to the Redis RESP protocol).

use aeon_types::{AeonError, OutboundAuthMode, OutboundAuthSigner};
use redis::{Client, ClientTlsConfig, ConnectionInfo, IntoConnectionInfo, TlsCertificates};
use std::sync::Arc;

/// Resolve the address string plus an optional outbound-auth signer into a
/// ready-to-use `redis::Client`. Non-TLS modes go through `Client::open`;
/// `Mtls` goes through `Client::build_with_tls` with an in-memory PEM cert
/// + key extracted from the signer.
pub(super) fn resolve_client(
    url: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<Client, AeonError> {
    let Some(s) = signer else {
        return open_non_tls(url);
    };

    match s.mode() {
        OutboundAuthMode::None => open_non_tls(url),
        OutboundAuthMode::BrokerNative => {
            let mut info = parse_info(url)?;
            apply_broker_native(&mut info, s)?;
            Client::open(info)
                .map_err(|e| AeonError::connection(format!("redis client create failed: {e}")))
        }
        OutboundAuthMode::Mtls => build_mtls_client(url, s),
        OutboundAuthMode::Bearer
        | OutboundAuthMode::Basic
        | OutboundAuthMode::ApiKey
        | OutboundAuthMode::HmacSign => {
            tracing::warn!(
                mode = ?s.mode(),
                "Redis connector: HTTP-style auth mode is not applicable to RESP; ignored. Use broker_native."
            );
            open_non_tls(url)
        }
    }
}

fn parse_info(url: &str) -> Result<ConnectionInfo, AeonError> {
    url.into_connection_info()
        .map_err(|e| AeonError::config(format!("redis url parse failed: {e}")))
}

fn open_non_tls(url: &str) -> Result<Client, AeonError> {
    let info = parse_info(url)?;
    Client::open(info)
        .map_err(|e| AeonError::connection(format!("redis client create failed: {e}")))
}

fn apply_broker_native(
    info: &mut ConnectionInfo,
    s: &Arc<OutboundAuthSigner>,
) -> Result<(), AeonError> {
    let Some(bn) = s.broker_native() else {
        return Ok(());
    };
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
        tracing::warn!("Redis URL embedded userinfo; outbound-auth signer credentials override it");
    }
    Ok(())
}

/// S10 mTLS path — construct a `Client` with inline PEM cert+key from the
/// signer. Forces `rediss://` scheme because `inner_build_with_tls` in the
/// redis crate refuses anything else.
fn build_mtls_client(url: &str, s: &Arc<OutboundAuthSigner>) -> Result<Client, AeonError> {
    let cert_pem = s.mtls_cert_pem().ok_or_else(|| {
        AeonError::config("redis mTLS: signer is in Mtls mode but missing cert PEM")
    })?;
    let key_pem = s.mtls_key_pem().ok_or_else(|| {
        AeonError::config("redis mTLS: signer is in Mtls mode but missing key PEM")
    })?;

    let rewritten = upgrade_to_rediss(url)?;
    let info = parse_info(&rewritten)?;

    let tls_certs = TlsCertificates {
        client_tls: Some(ClientTlsConfig {
            client_cert: cert_pem.to_vec(),
            client_key: key_pem.to_vec(),
        }),
        root_cert: None, // inherit webpki-roots via tls-rustls-webpki-roots feature
    };

    Client::build_with_tls(info, tls_certs)
        .map_err(|e| AeonError::connection(format!("redis mTLS client build failed: {e}")))
}

/// Auto-upgrade `redis://…` to `rediss://…` on mTLS. If the URL is already
/// `rediss://` or uses a Unix socket, it's returned unchanged.
fn upgrade_to_rediss(url: &str) -> Result<String, AeonError> {
    if let Some(rest) = url.strip_prefix("redis://") {
        tracing::warn!(
            "Redis mTLS: auto-upgrading scheme redis://…{suffix} → rediss://… — \
             use rediss:// explicitly to silence this warning.",
            suffix = if rest.len() > 16 { "" } else { rest }
        );
        return Ok(format!("rediss://{rest}"));
    }
    if url.starts_with("rediss://") || url.starts_with("redis+unix://") {
        return Ok(url.to_string());
    }
    Err(AeonError::config(format!(
        "redis mTLS: URL scheme must be redis:// or rediss://, got '{url}'"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{
        BrokerNativeConfig, OutboundAuthConfig, OutboundAuthMode, OutboundMtlsConfig,
    };
    use std::collections::BTreeMap;

    fn signer(config: OutboundAuthConfig) -> Arc<OutboundAuthSigner> {
        Arc::new(OutboundAuthSigner::build(config).unwrap())
    }

    /// rcgen-backed self-signed PEM pair for mTLS unit tests. The cert
    /// is never verified against a real peer — we only need it to parse
    /// cleanly in `rustls_pemfile`.
    fn test_cert_and_key() -> (Vec<u8>, Vec<u8>) {
        let cert = rcgen::generate_simple_self_signed(vec!["client.test".into()]).unwrap();
        (
            cert.cert.pem().into_bytes(),
            cert.signing_key.serialize_pem().into_bytes(),
        )
    }

    #[test]
    fn no_signer_uses_url_verbatim() {
        let c = resolve_client("redis://127.0.0.1:6379/2", None).unwrap();
        assert_eq!(c.get_connection_info().redis.db, 2);
        assert!(c.get_connection_info().redis.username.is_none());
    }

    #[test]
    fn broker_native_overrides_url_credentials() {
        let mut values = BTreeMap::new();
        values.insert("username".to_string(), "aeon-signer".to_string());
        values.insert("password".to_string(), "s3cret".to_string());
        values.insert("db".to_string(), "5".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let c = resolve_client("redis://url-user:url-pass@127.0.0.1:6379/0", Some(&s)).unwrap();
        assert_eq!(
            c.get_connection_info().redis.username.as_deref(),
            Some("aeon-signer")
        );
        assert_eq!(
            c.get_connection_info().redis.password.as_deref(),
            Some("s3cret")
        );
        assert_eq!(c.get_connection_info().redis.db, 5);
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
        let c = resolve_client("redis://127.0.0.1:6379", Some(&s)).unwrap();
        assert!(c.get_connection_info().redis.username.is_none());
        assert_eq!(c.get_connection_info().redis.password.as_deref(), Some("p"));
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
        let err = resolve_client("redis://127.0.0.1:6379", Some(&s)).unwrap_err();
        assert!(format!("{err}").contains("db"));
    }

    #[test]
    fn none_mode_passes_url_through() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        });
        let c = resolve_client("redis://127.0.0.1:6379/7", Some(&s)).unwrap();
        assert_eq!(c.get_connection_info().redis.db, 7);
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
        let c = resolve_client("redis://127.0.0.1:6379", Some(&s)).unwrap();
        assert!(c.get_connection_info().redis.username.is_none());
        assert!(c.get_connection_info().redis.password.is_none());
    }

    #[test]
    fn bad_url_is_rejected() {
        let err = resolve_client("not-a-url", None).unwrap_err();
        assert!(format!("{err}").contains("redis"));
    }

    // ── mTLS ────────────────────────────────────────────────────────────

    #[test]
    fn mtls_builds_client_with_rediss_url() {
        let (cert_pem, key_pem) = test_cert_and_key();
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(OutboundMtlsConfig {
                cert_pem: String::from_utf8(cert_pem).unwrap(),
                key_pem: String::from_utf8(key_pem).unwrap(),
            }),
            ..Default::default()
        });
        // rediss:// scheme — no rewrite needed.
        let c = resolve_client("rediss://127.0.0.1:6380", Some(&s)).unwrap();
        // Connection info remembers the TCP+TLS addr.
        match &c.get_connection_info().addr {
            redis::ConnectionAddr::TcpTls {
                host,
                port,
                tls_params,
                ..
            } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(*port, 6380);
                assert!(tls_params.is_some(), "TLS params must be attached for mTLS");
            }
            other => panic!("expected TcpTls connection addr, got {other:?}"),
        }
    }

    #[test]
    fn mtls_auto_upgrades_redis_scheme_to_rediss() {
        let (cert_pem, key_pem) = test_cert_and_key();
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(OutboundMtlsConfig {
                cert_pem: String::from_utf8(cert_pem).unwrap(),
                key_pem: String::from_utf8(key_pem).unwrap(),
            }),
            ..Default::default()
        });
        // Plain redis:// — the translator auto-upgrades to rediss://.
        let c = resolve_client("redis://127.0.0.1:6380", Some(&s)).unwrap();
        assert!(matches!(
            c.get_connection_info().addr,
            redis::ConnectionAddr::TcpTls { .. }
        ));
    }

    #[test]
    fn mtls_rejects_malformed_cert() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(OutboundMtlsConfig {
                cert_pem: "not a PEM".to_string(),
                key_pem: "not a key".to_string(),
            }),
            ..Default::default()
        });
        let err = resolve_client("rediss://127.0.0.1:6380", Some(&s)).unwrap_err();
        let msg = format!("{err}");
        // redis crate surfaces an underlying key-extraction or cert-parse error.
        assert!(
            msg.contains("mTLS")
                || msg.contains("redis")
                || msg.contains("key")
                || msg.contains("cert"),
            "expected mTLS failure message, got: {msg}"
        );
    }
}
