//! WebSocket mTLS plumbing.
//!
//! Both endpoints build a `rustls::ClientConfig` from the signer's cert/key
//! PEMs (S10) and drive the handshake via `Connector::Rustls`. The PEM
//! parsing + webpki-roots trust-store assembly lives in the shared
//! [`crate::mtls_pem`] module; this file only wraps the result in the
//! tokio-tungstenite-specific `Connector` shape.

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
    Ok(Arc::new(crate::mtls_pem::build_mtls_client_config(
        cert_pem, key_pem,
    )?))
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
    fn build_mtls_connector_from_valid_pem_ok() {
        let (cert_pem, key_pem) = fixture_cert_and_key();
        let c = build_mtls_connector(cert_pem.as_bytes(), key_pem.as_bytes());
        assert!(matches!(c, Ok(Connector::Rustls(_))));
    }
}
