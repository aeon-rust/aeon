//! TLS helpers for QUIC connectors.
//!
//! Generates self-signed certificates for development/testing.
//! Production deployments should use proper certificates.

use std::sync::Arc;

/// Generate development QUIC configs with self-signed certificates.
///
/// Returns (ServerConfig, ClientConfig) pair for testing.
/// NOT for production use — uses self-signed certs with no client auth.
///
/// FT-10: `.unwrap()` calls below are on startup-time crypto primitive
/// construction using hardcoded inputs. rcgen/rustls APIs return `Result`
/// for surface-area uniformity; with fixed inputs (`"localhost"`, freshly
/// generated keypair) these paths are infallible. A panic here would
/// indicate a fundamental rcgen/rustls bug, not a runtime condition.
#[allow(clippy::unwrap_used)]
pub fn dev_quic_configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
    // Pin the rustls crypto provider to ring. Required since multiple
    // rustls providers (ring + aws-lc-rs) are now linked via the
    // expanded S10 feature set — without an explicit default, rustls
    // panics on first `ClientConfig::builder()` / `ServerConfig::builder()`.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let key_pair = rcgen::KeyPair::generate().unwrap();
    let cert_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = cert_params.self_signed(&key_pair).unwrap();
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

    // Server config
    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der.clone_key())
        .unwrap();
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap(),
    ));

    // Client config — trust the self-signed cert
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add(cert_der).unwrap();
    let client_crypto = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto).unwrap(),
    ));

    (server_config, client_config)
}
