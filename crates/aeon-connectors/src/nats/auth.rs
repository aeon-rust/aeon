//! S10 outbound-auth → async-nats `ConnectOptions` translation.
//!
//! Shared by `NatsSource` and `NatsSink`. The operator supplies only the
//! server URL (`nats://host:4222` or `tls://host:4222`); credentials flow
//! through the signer.
//!
//! **One mode per connector**, matching the kafka / redis translators:
//!
//! - `None` — unauthenticated `async_nats::connect(url)`.
//! - `BrokerNative` — pick from the signer's `broker_native().values`
//!   map in this order of precedence (more specific first):
//!     1. `credentials` (inline `.creds` file contents, JWT + NKey pair —
//!        the canonical NATS decentralized-auth form)
//!     2. `nkey_seed` (NKey seed alone)
//!     3. `token`
//!     4. `username` + `password` (both required; either alone is a
//!        config error so misconfiguration is loud)
//!
//!   At most one of the first three is used; the translator does not
//!   attempt to combine them. Username/password may be combined with
//!   any of the first three as an additional credential.
//! - `Mtls` — async-nats 0.38 exposes
//!   `ConnectOptions::tls_client_config(rustls::ClientConfig)` which
//!   accepts an in-memory rustls config. The signer's cert/key PEMs
//!   are parsed via [`crate::mtls_pem::build_mtls_client_config`] (the
//!   same helper WebSocket + WebTransport use), and `require_tls(true)`
//!   is flipped so the connector refuses plaintext connections. No
//!   tempfiles, no secrets on disk.
//! - `Bearer` / `Basic` / `ApiKey` / `HmacSign` — warn-and-skip.

use aeon_types::{AeonError, OutboundAuthMode, OutboundAuthSigner};
use async_nats::{Client, ConnectOptions};
use std::sync::Arc;

/// Build a `ConnectOptions` from an optional signer and connect to `url`.
/// Returns the live `async_nats::Client`.
///
/// Kept separate from source/sink construction so both paths share one
/// translation layer — mirrors `kafka::auth::apply_outbound_auth` and
/// `redis_streams::auth::resolve_connection_info`.
pub(super) async fn connect_with_auth(
    url: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<Client, AeonError> {
    let options = build_connect_options(signer)?;
    options
        .connect(url)
        .await
        .map_err(|e| AeonError::connection(format!("nats connect failed: {e}")))
}

/// Translate the signer into a fully-configured `ConnectOptions` without
/// performing the network connect. Factored out so unit tests can assert
/// the translation logic without needing a live NATS server.
fn build_connect_options(
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<ConnectOptions, AeonError> {
    let mut options = ConnectOptions::new();
    let Some(s) = signer else { return Ok(options) };
    match s.mode() {
        OutboundAuthMode::None => { /* pass-through */ }
        OutboundAuthMode::BrokerNative => {
            if let Some(bn) = s.broker_native() {
                // Precedence: credentials (JWT+NKey) > nkey_seed > token > user/pass.
                // At most one of the top three wins; user/pass may still be
                // applied as an additional fallback credential so SERVER_INFO-
                // time re-auth can choose.
                if let Some(creds) = bn.values.get("credentials") {
                    options = options.credentials(creds).map_err(|e| {
                        AeonError::config(format!("nats broker_native 'credentials' parse failed: {e}"))
                    })?;
                } else if let Some(seed) = bn.values.get("nkey_seed") {
                    options = options.nkey(seed.clone());
                } else if let Some(tok) = bn.values.get("token") {
                    options = options.token(tok.clone());
                }

                let u = bn.values.get("username");
                let p = bn.values.get("password");
                match (u, p) {
                    (Some(u), Some(p)) => {
                        options = options.user_and_password(u.clone(), p.clone());
                    }
                    (Some(_), None) | (None, Some(_)) => {
                        return Err(AeonError::config(
                            "nats broker_native: 'username' and 'password' must be provided together",
                        ));
                    }
                    (None, None) => {}
                }
            }
        }
        OutboundAuthMode::Mtls => {
            let cert_pem = s.mtls_cert_pem().ok_or_else(|| {
                AeonError::config("nats mTLS: signer is in Mtls mode but missing cert PEM")
            })?;
            let key_pem = s.mtls_key_pem().ok_or_else(|| {
                AeonError::config("nats mTLS: signer is in Mtls mode but missing key PEM")
            })?;
            let tls = crate::mtls_pem::build_mtls_client_config(cert_pem, key_pem)?;
            options = options.require_tls(true).tls_client_config(tls);
        }
        OutboundAuthMode::Bearer
        | OutboundAuthMode::Basic
        | OutboundAuthMode::ApiKey
        | OutboundAuthMode::HmacSign => {
            tracing::warn!(
                mode = ?s.mode(),
                "NATS connector: HTTP-style auth mode is not applicable to NATS protocol; \
                 ignored. Use broker_native."
            );
        }
    }
    Ok(options)
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
    fn no_signer_returns_default_options() {
        // Smoke test — the call must not error and must return a usable options.
        assert!(build_connect_options(None).is_ok());
    }

    #[test]
    fn none_mode_returns_default_options() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        });
        assert!(build_connect_options(Some(&s)).is_ok());
    }

    #[test]
    fn broker_native_token_accepted() {
        let mut values = BTreeMap::new();
        values.insert("token".to_string(), "s3cret-token".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        assert!(build_connect_options(Some(&s)).is_ok());
    }

    #[test]
    fn broker_native_username_password_accepted() {
        let mut values = BTreeMap::new();
        values.insert("username".to_string(), "aeon".to_string());
        values.insert("password".to_string(), "pw".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        assert!(build_connect_options(Some(&s)).is_ok());
    }

    #[test]
    fn broker_native_username_without_password_rejected() {
        let mut values = BTreeMap::new();
        values.insert("username".to_string(), "aeon".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let err = build_connect_options(Some(&s)).unwrap_err();
        assert!(format!("{err}").contains("username"));
    }

    #[test]
    fn broker_native_password_without_username_rejected() {
        let mut values = BTreeMap::new();
        values.insert("password".to_string(), "pw".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let err = build_connect_options(Some(&s)).unwrap_err();
        assert!(format!("{err}").contains("username"));
    }

    #[test]
    fn broker_native_nkey_seed_accepted() {
        let mut values = BTreeMap::new();
        // A syntactically-plausible NKey seed. The NKey parser runs lazily
        // during connect, not during options construction, so any non-empty
        // string passes this translator.
        values.insert(
            "nkey_seed".to_string(),
            "SUAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
        );
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        assert!(build_connect_options(Some(&s)).is_ok());
    }

    #[test]
    fn broker_native_rejects_malformed_credentials() {
        // async_nats credentials() expects JWT+NKey format. A garbage blob
        // must surface as AeonError::config, not panic.
        let mut values = BTreeMap::new();
        values.insert("credentials".to_string(), "not a valid creds file".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let err = build_connect_options(Some(&s)).unwrap_err();
        assert!(format!("{err}").contains("credentials"));
    }

    fn fixture_cert_and_key() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn mtls_mode_wires_rustls_client_config_into_options() {
        let (cert_pem, key_pem) = fixture_cert_and_key();
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(aeon_types::OutboundMtlsConfig { cert_pem, key_pem }),
            ..Default::default()
        });
        let opts = build_connect_options(Some(&s)).unwrap();
        // async-nats ConnectOptions stores tls_client_config in a private
        // field. The public Debug impl prints "XXXXXXXX" when the config
        // is present; that's the tightest public-API-safe check we have.
        let dbg = format!("{:?}", opts);
        assert!(
            dbg.contains("XXXXXXXX"),
            "expected tls_client_config set on ConnectOptions: {dbg}"
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
        let err = build_connect_options(Some(&s)).unwrap_err();
        let msg = format!("{err}").to_lowercase();
        assert!(msg.contains("mtls") || msg.contains("pem") || msg.contains("certificate"));
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
        assert!(build_connect_options(Some(&s)).is_ok());
    }
}
