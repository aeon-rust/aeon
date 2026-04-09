//! TLS configuration for QUIC endpoints using rustls.

use std::sync::Arc;

use aeon_types::AeonError;

use crate::config::TlsConfig;

/// Build a rustls ServerConfig for QUIC from TLS file paths.
pub fn build_server_config(tls: &TlsConfig) -> Result<rustls::ServerConfig, AeonError> {
    let cert_pem = std::fs::read(&tls.cert).map_err(|e| AeonError::Config {
        message: format!("failed to read cert file {:?}: {e}", tls.cert),
    })?;
    let key_pem = std::fs::read(&tls.key).map_err(|e| AeonError::Config {
        message: format!("failed to read key file {:?}: {e}", tls.key),
    })?;
    let ca_pem = std::fs::read(&tls.ca).map_err(|e| AeonError::Config {
        message: format!("failed to read CA file {:?}: {e}", tls.ca),
    })?;

    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AeonError::Config {
            message: format!("failed to parse cert PEM: {e}"),
        })?;

    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| AeonError::Config {
            message: format!("failed to parse key PEM: {e}"),
        })?
        .ok_or_else(|| AeonError::Config {
            message: "no private key found in key file".to_string(),
        })?;

    // Build CA root store for client verification (mTLS)
    let mut root_store = rustls::RootCertStore::empty();
    let ca_certs = rustls_pemfile::certs(&mut ca_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AeonError::Config {
            message: format!("failed to parse CA PEM: {e}"),
        })?;
    for cert in ca_certs {
        root_store.add(cert).map_err(|e| AeonError::Config {
            message: format!("failed to add CA cert: {e}"),
        })?;
    }

    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()
        .map_err(|e| AeonError::Config {
            message: format!("failed to build client verifier: {e}"),
        })?;

    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, key)
        .map_err(|e| AeonError::Config {
            message: format!("failed to build server TLS config: {e}"),
        })?;

    Ok(config)
}

/// Build a rustls ClientConfig for QUIC from TLS file paths.
pub fn build_client_config(tls: &TlsConfig) -> Result<rustls::ClientConfig, AeonError> {
    let cert_pem = std::fs::read(&tls.cert).map_err(|e| AeonError::Config {
        message: format!("failed to read cert file {:?}: {e}", tls.cert),
    })?;
    let key_pem = std::fs::read(&tls.key).map_err(|e| AeonError::Config {
        message: format!("failed to read key file {:?}: {e}", tls.key),
    })?;
    let ca_pem = std::fs::read(&tls.ca).map_err(|e| AeonError::Config {
        message: format!("failed to read CA file {:?}: {e}", tls.ca),
    })?;

    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AeonError::Config {
            message: format!("failed to parse cert PEM: {e}"),
        })?;

    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| AeonError::Config {
            message: format!("failed to parse key PEM: {e}"),
        })?
        .ok_or_else(|| AeonError::Config {
            message: "no private key found in key file".to_string(),
        })?;

    let mut root_store = rustls::RootCertStore::empty();
    let ca_certs = rustls_pemfile::certs(&mut ca_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AeonError::Config {
            message: format!("failed to parse CA PEM: {e}"),
        })?;
    for cert in ca_certs {
        root_store.add(cert).map_err(|e| AeonError::Config {
            message: format!("failed to add CA cert: {e}"),
        })?;
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(certs, key)
        .map_err(|e| AeonError::Config {
            message: format!("failed to build client TLS config: {e}"),
        })?;

    Ok(config)
}

/// Generate ephemeral self-signed certs for development/testing.
/// Returns (cert_der, key_der) pair.
#[cfg(test)]
pub fn dev_self_signed_cert(
    subject_alt_names: Vec<String>,
) -> (
    rustls::pki_types::CertificateDer<'static>,
    rustls::pki_types::PrivateKeyDer<'static>,
) {
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let cert_params = rcgen::CertificateParams::new(subject_alt_names).unwrap();
    let cert = cert_params.self_signed(&key_pair).unwrap();
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();
    (cert_der, key_der)
}

/// Build insecure QUIC configs for development (no TLS file paths needed).
#[cfg(test)]
pub fn dev_quic_configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
    let (cert_der, key_der) = dev_self_signed_cert(vec!["localhost".to_string()]);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_self_signed_cert_generates() {
        let (cert, key) = dev_self_signed_cert(vec!["localhost".to_string()]);
        assert!(!cert.is_empty());
        assert!(!key.secret_der().is_empty());
    }

    #[test]
    fn dev_quic_configs_build() {
        let (server, client) = dev_quic_configs();
        // Just verify they constructed without panicking
        let _ = server;
        let _ = client;
    }
}
