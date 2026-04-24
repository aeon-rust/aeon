//! S10 shared helper — parse PEM cert + key into a `rustls::ClientConfig`
//! suitable for mTLS outbound on any TLS-capable connector.
//!
//! Originally lived in `websocket::mtls`; lifted here because NATS
//! (`tls_client_config`), Postgres (`tokio-postgres-rustls::MakeRustlsConnect`),
//! MySQL (`mysql_async::RustlsConnector`), and the WebSocket / WebTransport
//! outbound paths all consume the same `rustls::ClientConfig` shape. Kept
//! deliberately thin: one function, no connector-specific types, no
//! assumptions about how the resulting config is plugged into the driver.
//!
//! Server verification uses the public `webpki-roots` trust store. Private
//! CA verification would need an additional config surface (not yet in
//! scope; tracked as a future follow-up if a connector asks for it).
//!
//! This module is only compiled when at least one mTLS-capable feature
//! is enabled (`websocket`, `nats`, `postgres-cdc`, `mysql-cdc`), which is
//! gated at `lib.rs` via a re-export cfg.

use aeon_types::AeonError;

/// Install `ring` as the process-wide rustls default crypto provider if
/// nothing is installed yet. Idempotent — safe to call from multiple
/// threads; losing the race just means another caller already installed
/// one (which we then respect).
///
/// The workspace pulls rustls transitively through multiple crates
/// (redis `tls-rustls`, mysql_async `rustls-tls`, tokio-rustls, etc.),
/// some of which default to `aws-lc-rs` while aeon-crypto and the
/// rest of the Aeon stack use `ring`. Without an explicit selection
/// rustls panics on first `ClientConfig::builder()` call when more
/// than one provider is linked. This helper pins `ring` so every
/// mTLS bring-up is deterministic.
fn ensure_rustls_default_provider() {
    // CryptoProvider::install_default returns Err if one is already
    // installed; ignore because idempotence is the whole point.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Build a `rustls::ClientConfig` that presents the supplied PEM cert
/// chain + private key as the client identity, verifying the remote server
/// against the public `webpki-roots` trust store. Returns a config error
/// if either PEM fails to parse.
pub(crate) fn build_mtls_client_config(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<rustls::ClientConfig, AeonError> {
    ensure_rustls_default_provider();
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut std::io::Cursor::new(cert_pem))
            .collect::<Result<_, _>>()
            .map_err(|e| AeonError::config(format!("mTLS cert parse: {e}")))?;
    if certs.is_empty() {
        return Err(AeonError::config("mTLS: no certificate in pem".to_string()));
    }

    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(key_pem))
        .map_err(|e| AeonError::config(format!("mTLS key parse: {e}")))?
        .ok_or_else(|| AeonError::config("mTLS: no private key in pem".to_string()))?;

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(certs, key)
        .map_err(|e| AeonError::Crypto {
            message: format!("mTLS config build: {e}"),
            source: None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_cert_and_key() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn builds_client_config_from_valid_pem() {
        let (cert_pem, key_pem) = fixture_cert_and_key();
        assert!(build_mtls_client_config(cert_pem.as_bytes(), key_pem.as_bytes()).is_ok());
    }

    #[test]
    fn rejects_empty_cert() {
        let (_, key_pem) = fixture_cert_and_key();
        let err = build_mtls_client_config(b"", key_pem.as_bytes()).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("certificate"));
    }

    #[test]
    fn rejects_missing_key() {
        let (cert_pem, _) = fixture_cert_and_key();
        let err = build_mtls_client_config(cert_pem.as_bytes(), b"").unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("key"));
    }
}
