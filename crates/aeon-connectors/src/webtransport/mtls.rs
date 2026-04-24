//! Shared mTLS plumbing for WebTransport Source and Sink.
//!
//! `wtransport::ClientConfig` and `wtransport::ServerConfig` are opaque by
//! the time they reach the sink/source constructors — so we expose public
//! builder helpers that callers can use to bake the S10 / S9 mTLS identity
//! into the config before construction.

use aeon_types::{AeonError, OutboundAuthMode, OutboundAuthSigner};
use std::sync::Arc;

/// Build a `wtransport::ClientConfig` whose TLS layer presents the signer's
/// mTLS cert/key (S10 Mtls mode) and verifies the remote server against the
/// public `webpki-roots` store.
///
/// Returns:
/// - `Err(AeonError::Config)` if the signer is not in mTLS mode or is
///   missing cert/key PEM bytes.
/// - `Err(AeonError::Crypto)` if the PEMs are malformed.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use aeon_connectors::webtransport::mtls_client_config_from_signer;
/// use aeon_types::{OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner, OutboundMtlsConfig};
///
/// # fn wire_wt(cert: String, key: String) -> Result<(), Box<dyn std::error::Error>> {
/// let signer = Arc::new(OutboundAuthSigner::build(OutboundAuthConfig {
///     mode: OutboundAuthMode::Mtls,
///     mtls: Some(OutboundMtlsConfig { cert_pem: cert, key_pem: key }),
///     ..Default::default()
/// })?);
/// let client_config = mtls_client_config_from_signer(&signer)?;
/// // pass into WebTransportSinkConfig::new(url, client_config)
/// # Ok(())
/// # }
/// ```
pub fn mtls_client_config_from_signer(
    signer: &Arc<OutboundAuthSigner>,
) -> Result<wtransport::ClientConfig, AeonError> {
    if signer.mode() != OutboundAuthMode::Mtls {
        return Err(AeonError::config(format!(
            "webtransport mTLS: signer is in mode {:?}, not Mtls",
            signer.mode()
        )));
    }
    let cert_pem = signer
        .mtls_cert_pem()
        .ok_or_else(|| AeonError::config("webtransport mTLS: signer missing cert pem"))?;
    let key_pem = signer
        .mtls_key_pem()
        .ok_or_else(|| AeonError::config("webtransport mTLS: signer missing key pem"))?;
    // Delegate PEM → rustls::ClientConfig to the shared helper. WebTransport
    // requires TLS 1.3 + `TLS13_AES_128_GCM_SHA256`, which is in the rustls
    // ring-provider default cipher set.
    let tls = crate::mtls_pem::build_mtls_client_config(cert_pem, key_pem)?;
    Ok(wtransport::ClientConfig::builder()
        .with_bind_default()
        .with_custom_tls(tls)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{
        BearerConfig, OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner, OutboundMtlsConfig,
    };

    fn fixture_cert_and_key() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn valid_mtls_signer_builds_client_config() {
        let (cert_pem, key_pem) = fixture_cert_and_key();
        let signer = Arc::new(
            OutboundAuthSigner::build(OutboundAuthConfig {
                mode: OutboundAuthMode::Mtls,
                mtls: Some(OutboundMtlsConfig { cert_pem, key_pem }),
                ..Default::default()
            })
            .unwrap(),
        );
        assert!(mtls_client_config_from_signer(&signer).is_ok());
    }

    #[test]
    fn non_mtls_signer_is_rejected_with_config_error() {
        let signer = Arc::new(
            OutboundAuthSigner::build(OutboundAuthConfig {
                mode: OutboundAuthMode::Bearer,
                bearer: Some(BearerConfig {
                    token: "t".to_string(),
                }),
                ..Default::default()
            })
            .unwrap(),
        );
        let err = mtls_client_config_from_signer(&signer).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.to_lowercase().contains("not mtls"), "got: {msg}");
    }

    #[test]
    fn empty_cert_pem_surfaces_config_error() {
        // No PEM envelope at all — certs parse yields an empty set and we
        // short-circuit before even looking at the key. Exercise via the
        // shared helper directly since `build_mtls_rustls_client_config`
        // was folded into `crate::mtls_pem::build_mtls_client_config`.
        let (_cert, key_pem) = fixture_cert_and_key();
        let err = crate::mtls_pem::build_mtls_client_config(b"", key_pem.as_bytes())
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.to_lowercase().contains("certificate") || msg.to_lowercase().contains("pem"),
            "got: {msg}"
        );
    }
}
