//! TLS configuration for QUIC endpoints using rustls.

use std::sync::Arc;
use std::time::Duration;

use aeon_types::AeonError;

use crate::config::{ClusterConfig, RaftTiming, TlsConfig};

/// Install `ring` as the process-wide rustls default crypto provider if
/// nothing is installed yet. Idempotent — safe to call from multiple
/// threads and multiple call sites.
///
/// Required since the S10 feature expansion pulled `aws-lc-rs` into the
/// workspace graph alongside the pre-existing `ring` (aeon-crypto /
/// quinn / tokio-rustls). Without an explicit default, rustls panics
/// on the first `ClientConfig::builder()` / `ServerConfig::builder()`.
/// Mirrors `aeon_crypto::tls::ensure_rustls_default_provider`.
fn ensure_rustls_default_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Build a shared `quinn::TransportConfig` sized from the cluster's Raft
/// timing. Sets two knobs that quinn leaves disabled by default:
///
/// - `keep_alive_interval = heartbeat_ms`: forces PING frames at the same
///   cadence as Raft heartbeats so a peer that silently disappears is
///   detected at the transport layer, not only via application-level
///   heartbeat-miss in openraft.
/// - `max_idle_timeout = 2 × election_max_ms`: hard upper bound on
///   orphaned connections after a peer crashes. Sized well above the
///   Raft election window so normal traffic never trips it, but low
///   enough that a genuinely dead peer's connection is reaped within one
///   election cycle's worth of slack.
///
/// Session 0 (2026-04-18) surfaced that the Raft QUIC transport was
/// using all-quinn-defaults with no idle-timeout and no keep-alive,
/// leaving peer-death detection entirely to openraft's heartbeat-miss
/// logic. See `docs/GATE2-ACCEPTANCE-PLAN.md § 11.5` Row 4.
fn build_transport_config(raft_timing: &RaftTiming) -> Arc<quinn::TransportConfig> {
    let mut tc = quinn::TransportConfig::default();
    tc.keep_alive_interval(Some(Duration::from_millis(raft_timing.heartbeat_ms)));
    let idle_ms = raft_timing.election_max_ms.saturating_mul(2);
    if let Ok(idle) = quinn::IdleTimeout::try_from(Duration::from_millis(idle_ms)) {
        tc.max_idle_timeout(Some(idle));
    }
    Arc::new(tc)
}

/// Build a rustls ServerConfig for QUIC from TLS file paths.
pub fn build_server_config(tls: &TlsConfig) -> Result<rustls::ServerConfig, AeonError> {
    ensure_rustls_default_provider();
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
    ensure_rustls_default_provider();
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

/// Wrap a rustls `ServerConfig` into a `quinn::ServerConfig`.
fn quic_server_config(rustls_config: rustls::ServerConfig) -> Result<quinn::ServerConfig, AeonError> {
    let quic_crypto =
        quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config).map_err(|e| {
            AeonError::Config {
                message: format!("failed to build QUIC server config: {e}"),
            }
        })?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic_crypto)))
}

/// Wrap a rustls `ClientConfig` into a `quinn::ClientConfig`.
fn quic_client_config(rustls_config: rustls::ClientConfig) -> Result<quinn::ClientConfig, AeonError> {
    let quic_crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config).map_err(|e| {
            AeonError::Config {
                message: format!("failed to build QUIC client config: {e}"),
            }
        })?;
    Ok(quinn::ClientConfig::new(Arc::new(quic_crypto)))
}

/// Build the `(ServerConfig, ClientConfig)` pair a cluster node should use,
/// selecting the safest TLS mode the cluster config permits (FT-8).
///
/// Selection order:
/// 1. `config.tls = Some(...)` → **production mTLS** via file-based certs
///    (peer verification enforced on both sides).
/// 2. `config.auto_tls = true` → **insecure dev mode**: self-signed cert +
///    accept-any-cert verifier. Emits a loud runtime warning so operators
///    notice when this path activates unexpectedly in production.
/// 3. Neither set → error. Single-node clusters don't call this function;
///    the caller should short-circuit before reaching here.
///
/// `auto-tls` is a build-time feature; if the binary was compiled without it
/// and falls into case 2, this returns a config error instead of silently
/// starting insecurely.
pub fn quic_configs_for_cluster(
    config: &ClusterConfig,
) -> Result<(quinn::ServerConfig, quinn::ClientConfig), AeonError> {
    let transport = build_transport_config(&config.raft_timing);

    if let Some(tls) = &config.tls {
        let server_rustls = build_server_config(tls)?;
        let client_rustls = build_client_config(tls)?;
        tracing::info!(
            cert = ?tls.cert,
            ca = ?tls.ca,
            "cluster TLS: production mTLS enabled (file-based certs, peer verification active)"
        );
        let mut server_cfg = quic_server_config(server_rustls)?;
        let mut client_cfg = quic_client_config(client_rustls)?;
        server_cfg.transport_config(transport.clone());
        client_cfg.transport_config(transport);
        return Ok((server_cfg, client_cfg));
    }

    if config.auto_tls {
        #[cfg(feature = "auto-tls")]
        {
            tracing::warn!(
                node_id = config.node_id,
                "⚠ cluster TLS: auto_tls=true — using SELF-SIGNED certs and ACCEPT-ANY-CERT \
                 peer verifier. This is INSECURE and intended for development/testing only. \
                 For production, set cluster.tls.{{cert,key,ca}} to file paths."
            );
            let (mut server_cfg, mut client_cfg) = dev_quic_configs_insecure();
            server_cfg.transport_config(transport.clone());
            client_cfg.transport_config(transport);
            return Ok((server_cfg, client_cfg));
        }
        #[cfg(not(feature = "auto-tls"))]
        {
            return Err(AeonError::Config {
                message: "cluster.auto_tls=true but this binary was built without the \
                          'auto-tls' feature. Either enable the feature or provide \
                          cluster.tls.{cert,key,ca} file paths."
                    .to_string(),
            });
        }
    }

    Err(AeonError::Config {
        message: "cluster TLS not configured: set cluster.tls.{cert,key,ca} for production \
                  mTLS, or cluster.auto_tls=true for development self-signed mode"
            .to_string(),
    })
}

/// Generate ephemeral self-signed certs for development/testing.
/// Returns (cert_der, key_der) pair.
/// FT-10: `.unwrap()` calls below are on rcgen/rustls crypto primitive
/// construction with hardcoded or caller-supplied SAN inputs. With valid
/// strings these paths are infallible; a panic here would indicate a
/// fundamental rcgen/rustls bug. Helper is `#[cfg(any(test, feature = "auto-tls"))]`
/// so it never ships in production builds without the dev feature.
#[cfg(any(test, feature = "auto-tls"))]
#[allow(clippy::unwrap_used)]
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
///
/// FT-10: dev helper; unwraps are on infallible crypto-construction paths.
#[cfg(any(test, feature = "auto-tls"))]
#[allow(clippy::unwrap_used)]
pub fn dev_quic_configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
    ensure_rustls_default_provider();
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

/// Build QUIC configs for multi-node development clusters.
///
/// Each node generates its own self-signed cert. The client uses a custom
/// verifier that accepts any server certificate — suitable for development
/// and testing only, NOT for production deployments.
///
/// FT-10: dev helper; unwraps are on infallible crypto-construction paths.
#[cfg(any(test, feature = "auto-tls"))]
#[allow(clippy::unwrap_used)]
pub fn dev_quic_configs_insecure() -> (quinn::ServerConfig, quinn::ClientConfig) {
    ensure_rustls_default_provider();
    let (cert_der, key_der) = dev_self_signed_cert(vec!["localhost".to_string()]);

    // Server config — no client auth required
    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.clone_key())
        .unwrap();
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap(),
    ));

    // Client config — skip server certificate verification (dev only)
    let client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    let client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto).unwrap(),
    ));

    (server_config, client_config)
}

/// A certificate verifier that accepts any server certificate.
/// For development/testing only.
#[cfg(any(test, feature = "auto-tls"))]
#[derive(Debug)]
struct AcceptAnyCert;

#[cfg(any(test, feature = "auto-tls"))]
impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
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

    // ── quic_configs_for_cluster selector (FT-8) ────────────────────

    fn base_cfg() -> ClusterConfig {
        use crate::types::NodeAddress;
        ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4470".parse().unwrap(),
            num_partitions: 16,
            peers: vec![NodeAddress::new("10.0.0.2", 4470)],
            seed_nodes: Vec::new(),
            tls: None,
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: crate::config::RaftTiming::default(),
            poh_verify_mode: "verify".to_string(),
            rest_api_port: None,
            rest_scheme: None,
        }
    }

    #[test]
    fn selector_errors_without_tls_or_auto_tls() {
        let cfg = base_cfg();
        let err = quic_configs_for_cluster(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("cluster TLS not configured"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn selector_accepts_auto_tls_when_feature_enabled() {
        let cfg = ClusterConfig {
            auto_tls: true,
            ..base_cfg()
        };
        // This test runs in `cfg(test)` which implies auto-tls helpers are
        // compiled, but the `cfg(feature = "auto-tls")` gate in the selector
        // depends on the actual feature. With feature enabled we expect Ok;
        // without, Err.
        let result = quic_configs_for_cluster(&cfg);
        #[cfg(feature = "auto-tls")]
        {
            assert!(result.is_ok(), "expected Ok with auto-tls feature, got {result:?}");
        }
        #[cfg(not(feature = "auto-tls"))]
        {
            assert!(result.is_err(), "expected Err without auto-tls feature");
        }
    }

    #[test]
    fn selector_production_path_builds_configs_from_files() {
        // Write a self-signed cert + key + self-CA to temp files, then verify
        // that the production path (config.tls = Some(...)) actually builds
        // a valid quinn ServerConfig + ClientConfig pair without errors.
        use rcgen::{CertificateParams, KeyPair};

        let key_pair = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        let tmp = std::env::temp_dir().join(format!("aeon-tls-ft8-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cert_path = tmp.join("node.pem");
        let key_path = tmp.join("node.key");
        let ca_path = tmp.join("ca.pem");
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();
        // Self-signed → the same cert serves as its own CA for this test.
        std::fs::write(&ca_path, &cert_pem).unwrap();

        let cfg = ClusterConfig {
            tls: Some(TlsConfig {
                cert: cert_path,
                key: key_path,
                ca: ca_path,
            }),
            ..base_cfg()
        };

        let result = quic_configs_for_cluster(&cfg);
        assert!(result.is_ok(), "production mTLS path failed: {result:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn transport_config_built_for_all_raft_profiles() {
        // Smoke: build_transport_config must not panic or overflow for any of
        // the three shipped RaftTiming presets (default / prod / flaky).
        // flaky_network has election_max_ms = 12000 → idle = 24s, well inside
        // quinn's IdleTimeout range; default is 6s; prod is 12s.
        for timing in [
            RaftTiming::default(),
            RaftTiming::prod_recommended(),
            RaftTiming::flaky_network(),
        ] {
            let tc = build_transport_config(&timing);
            // The Debug output of quinn::TransportConfig includes
            // `max_idle_timeout: Some(...)` and `keep_alive_interval: Some(...)`
            // — assert both were flipped from the quinn default of None.
            let dbg = format!("{tc:?}");
            assert!(
                dbg.contains("max_idle_timeout: Some"),
                "max_idle_timeout not set for {timing:?}: {dbg}"
            );
            assert!(
                dbg.contains("keep_alive_interval: Some"),
                "keep_alive_interval not set for {timing:?}: {dbg}"
            );
        }
    }

    #[cfg(feature = "auto-tls")]
    #[test]
    fn auto_tls_path_applies_transport_config() {
        // Round-trip: auto_tls=true + custom RaftTiming → configs are built
        // and the transport tweaks are applied to *both* server and client.
        // We can't inspect quinn::{Server,Client}Config.transport_config
        // directly (no public getter), so this test only asserts the call
        // succeeds; build_transport_config is covered above.
        let cfg = ClusterConfig {
            auto_tls: true,
            raft_timing: RaftTiming::prod_recommended(),
            ..base_cfg()
        };
        let result = quic_configs_for_cluster(&cfg);
        assert!(result.is_ok(), "auto-tls path failed: {result:?}");
    }

    #[test]
    fn selector_errors_on_missing_cert_files() {
        // tls: Some(...) with non-existent paths → build_server_config fails
        let cfg = ClusterConfig {
            tls: Some(TlsConfig {
                cert: std::path::PathBuf::from("/nonexistent/cert.pem"),
                key: std::path::PathBuf::from("/nonexistent/key.pem"),
                ca: std::path::PathBuf::from("/nonexistent/ca.pem"),
            }),
            ..base_cfg()
        };
        let err = quic_configs_for_cluster(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("cert") || err.to_string().contains("read"),
            "expected file-read error, got: {err}"
        );
    }
}
