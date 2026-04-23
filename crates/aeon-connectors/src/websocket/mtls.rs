//! Shared mTLS plumbing for WebSocket Source and Sink.
//!
//! Both endpoints build a `rustls::ClientConfig` from the signer's cert/key
//! PEMs (S10) and drive the handshake via `Connector::Rustls`. The remote
//! server is verified against the public `webpki-roots` trust store —
//! private-CA server verification would need a separate
//! `ConnectorTlsConfig` (not in scope for #34).

use aeon_types::AeonError;
use std::sync::Arc;
use tokio_tungstenite::Connector;

/// Build a rustls `Connector` that presents `cert_pem` / `key_pem` as the
/// client identity. Server verification uses the `webpki-roots` store.
pub(crate) fn build_mtls_connector(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<Connector, AeonError> {
    Ok(Connector::Rustls(build_mtls_client_config_arc(
        cert_pem, key_pem,
    )?))
}

/// Variant that returns an `Arc<ClientConfig>` for callers that need to
/// reuse the config across multiple connect attempts (the source reconnect
/// loop) — cheap clones, no PEM re-parse per reconnect.
pub(crate) fn build_mtls_client_config_arc(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<rustls::ClientConfig>, AeonError> {
    Ok(Arc::new(build_mtls_client_config(cert_pem, key_pem)?))
}

/// Build a `rustls::ClientConfig` that presents the signer's mTLS
/// identity and verifies the remote server against the public
/// `webpki-roots` trust store. Returns a config error if the PEMs
/// don't parse — called at connect time, never on the hot path.
pub(crate) fn build_mtls_client_config(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<rustls::ClientConfig, AeonError> {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut std::io::Cursor::new(cert_pem))
            .collect::<Result<_, _>>()
            .map_err(|e| AeonError::config(format!("websocket mTLS cert parse: {e}")))?;
    if certs.is_empty() {
        return Err(AeonError::config(
            "websocket mTLS: no certificate in pem".to_string(),
        ));
    }

    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(key_pem))
        .map_err(|e| AeonError::config(format!("websocket mTLS key parse: {e}")))?
        .ok_or_else(|| AeonError::config("websocket mTLS: no private key in pem".to_string()))?;

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(certs, key)
        .map_err(|e| AeonError::Crypto {
            message: format!("websocket mTLS config build: {e}"),
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
    fn build_mtls_client_config_from_valid_pem_ok() {
        let (cert_pem, key_pem) = fixture_cert_and_key();
        let cfg = build_mtls_client_config(cert_pem.as_bytes(), key_pem.as_bytes());
        assert!(cfg.is_ok(), "expected rustls client config, got {cfg:?}");
    }

    #[test]
    fn build_mtls_client_config_rejects_empty_cert() {
        let (_cert_pem, key_pem) = fixture_cert_and_key();
        let err = build_mtls_client_config(b"", key_pem.as_bytes()).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.to_lowercase().contains("certificate") || msg.to_lowercase().contains("pem"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn build_mtls_client_config_rejects_missing_key() {
        let (cert_pem, _key_pem) = fixture_cert_and_key();
        let err = build_mtls_client_config(cert_pem.as_bytes(), b"").unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.to_lowercase().contains("key") || msg.to_lowercase().contains("private"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn build_mtls_connector_from_valid_pem_ok() {
        let (cert_pem, key_pem) = fixture_cert_and_key();
        let c = build_mtls_connector(cert_pem.as_bytes(), key_pem.as_bytes());
        assert!(matches!(c, Ok(Connector::Rustls(_))));
    }
}
