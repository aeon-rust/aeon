//! TLS/mTLS certificate management for Aeon.
//!
//! Provides a `CertificateStore` that loads and caches TLS certificates
//! from PEM files. Used by cluster transport, connectors, and REST API.
//!
//! ## TLS Modes
//!
//! - `none` — no TLS (dev only, single-node)
//! - `auto` — auto-generate self-signed CA + node cert (single-node only)
//! - `pem` — load CA-signed certs from PEM files (production)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeon_types::AeonError;
use serde::{Deserialize, Serialize};

/// TLS mode for an Aeon endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// No TLS. Dev only. Rejected for multi-node clusters.
    None,
    /// Auto-generate self-signed CA + node cert. Single-node only.
    /// Certs persisted to `data_dir/tls/` for reuse across restarts.
    #[default]
    Auto,
    /// Load CA-signed certs from PEM files. Production.
    Pem,
}

/// TLS configuration block (used by cluster, HTTP, external QUIC).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsModeConfig {
    /// TLS mode: none, auto, or pem.
    #[serde(default)]
    pub mode: TlsMode,
    /// Path to node certificate (PEM). Required for `pem` mode.
    pub cert: Option<PathBuf>,
    /// Path to node private key (PEM). Required for `pem` mode.
    pub key: Option<PathBuf>,
    /// Path to CA certificate (PEM). Required for `pem` mode.
    pub ca: Option<PathBuf>,
}

impl TlsModeConfig {
    /// Validate the TLS config. Returns error if mode/paths are inconsistent.
    pub fn validate(&self, context: &str, allow_none: bool) -> Result<(), AeonError> {
        match self.mode {
            TlsMode::None => {
                if !allow_none {
                    return Err(AeonError::Config {
                        message: format!(
                            "{context}: tls.mode 'none' is not allowed (TLS is required)"
                        ),
                    });
                }
            }
            TlsMode::Auto => {
                // No file paths needed — certs are auto-generated
            }
            TlsMode::Pem => {
                if self.cert.is_none() || self.key.is_none() || self.ca.is_none() {
                    return Err(AeonError::Config {
                        message: format!(
                            "{context}: tls.mode 'pem' requires cert, key, and ca paths"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate that auto mode is not used with multi-node clusters.
    pub fn validate_not_auto_with_peers(
        &self,
        has_peers: bool,
        context: &str,
    ) -> Result<(), AeonError> {
        if self.mode == TlsMode::Auto && has_peers {
            return Err(AeonError::Config {
                message: format!(
                    "{context}: tls.mode 'auto' is not allowed for multi-node clusters \
                     (nodes cannot auto-trust each other without a shared CA). \
                     Use tls.mode 'pem' with a shared CA certificate."
                ),
            });
        }
        Ok(())
    }
}

/// TLS identity: a certificate chain + private key loaded from PEM files.
pub struct TlsIdentity {
    /// Certificate chain (leaf first, intermediates after).
    pub certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    /// Private key.
    pub key: rustls::pki_types::PrivateKeyDer<'static>,
}

/// A store of TLS certificates and CA roots.
///
/// Centralizes certificate loading for all Aeon components:
/// - Cluster inter-node mTLS (port 4470)
/// - HTTP management plane (port 4471)
/// - External QUIC connectors (port 4472)
/// - Per-connector TLS (e.g., Kafka SSL, Redis TLS)
pub struct CertificateStore {
    /// Node identity (cert + key) for mTLS.
    identity: Option<TlsIdentity>,
    /// Trusted CA certificates.
    root_store: rustls::RootCertStore,
    /// Paths for audit/reload.
    cert_path: Option<PathBuf>,
    key_path: Option<PathBuf>,
    ca_path: Option<PathBuf>,
}

impl CertificateStore {
    /// Create an empty store (no TLS — dev mode).
    pub fn new_insecure() -> Self {
        Self {
            identity: None,
            root_store: rustls::RootCertStore::empty(),
            cert_path: None,
            key_path: None,
            ca_path: None,
        }
    }

    /// Load from PEM file paths (cert chain, private key, CA bundle).
    pub fn from_pem_files(
        cert_path: impl AsRef<Path>,
        key_path: impl AsRef<Path>,
        ca_path: impl AsRef<Path>,
    ) -> Result<Self, AeonError> {
        let cert_path = cert_path.as_ref();
        let key_path = key_path.as_ref();
        let ca_path = ca_path.as_ref();

        let identity = load_identity(cert_path, key_path)?;
        let root_store = load_ca_roots(ca_path)?;

        Ok(Self {
            identity: Some(identity),
            root_store,
            cert_path: Some(cert_path.to_path_buf()),
            key_path: Some(key_path.to_path_buf()),
            ca_path: Some(ca_path.to_path_buf()),
        })
    }

    /// Load CA-only store (for verifying peers without presenting own identity).
    pub fn ca_only(ca_path: impl AsRef<Path>) -> Result<Self, AeonError> {
        let ca_path = ca_path.as_ref();
        let root_store = load_ca_roots(ca_path)?;
        Ok(Self {
            identity: None,
            root_store,
            cert_path: None,
            key_path: None,
            ca_path: Some(ca_path.to_path_buf()),
        })
    }

    /// Whether this store has a TLS identity (cert + key).
    pub fn has_identity(&self) -> bool {
        self.identity.is_some()
    }

    /// Whether this store has CA roots loaded.
    pub fn has_ca_roots(&self) -> bool {
        !self.root_store.is_empty()
    }

    /// Build a rustls ServerConfig with mTLS (client cert required).
    pub fn build_mtls_server_config(&self) -> Result<rustls::ServerConfig, AeonError> {
        let identity = self.identity.as_ref().ok_or_else(|| AeonError::Config {
            message: "mTLS server requires a TLS identity (cert + key)".into(),
        })?;

        if self.root_store.is_empty() {
            return Err(AeonError::Config {
                message: "mTLS server requires CA roots for client verification".into(),
            });
        }

        let client_verifier =
            rustls::server::WebPkiClientVerifier::builder(Arc::new(self.root_store.clone()))
                .build()
                .map_err(|e| AeonError::Crypto {
                    message: format!("failed to build client verifier: {e}"),
                    source: None,
                })?;

        rustls::ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(identity.certs.clone(), identity.key.clone_key())
            .map_err(|e| AeonError::Crypto {
                message: format!("failed to build mTLS server config: {e}"),
                source: None,
            })
    }

    /// Build a rustls ServerConfig with TLS only (no client cert).
    pub fn build_tls_server_config(&self) -> Result<rustls::ServerConfig, AeonError> {
        let identity = self.identity.as_ref().ok_or_else(|| AeonError::Config {
            message: "TLS server requires a TLS identity (cert + key)".into(),
        })?;

        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(identity.certs.clone(), identity.key.clone_key())
            .map_err(|e| AeonError::Crypto {
                message: format!("failed to build TLS server config: {e}"),
                source: None,
            })
    }

    /// Build a rustls ClientConfig with mTLS (present client cert).
    pub fn build_mtls_client_config(&self) -> Result<rustls::ClientConfig, AeonError> {
        let identity = self.identity.as_ref().ok_or_else(|| AeonError::Config {
            message: "mTLS client requires a TLS identity (cert + key)".into(),
        })?;

        if self.root_store.is_empty() {
            return Err(AeonError::Config {
                message: "mTLS client requires CA roots for server verification".into(),
            });
        }

        rustls::ClientConfig::builder()
            .with_root_certificates(self.root_store.clone())
            .with_client_auth_cert(identity.certs.clone(), identity.key.clone_key())
            .map_err(|e| AeonError::Crypto {
                message: format!("failed to build mTLS client config: {e}"),
                source: None,
            })
    }

    /// Build a rustls ClientConfig with TLS only (no client cert, verify server).
    pub fn build_tls_client_config(&self) -> Result<rustls::ClientConfig, AeonError> {
        if self.root_store.is_empty() {
            return Err(AeonError::Config {
                message: "TLS client requires CA roots for server verification".into(),
            });
        }

        Ok(rustls::ClientConfig::builder()
            .with_root_certificates(self.root_store.clone())
            .with_no_client_auth())
    }

    /// Get the leaf certificate's expiry as seconds since Unix epoch.
    ///
    /// Returns `None` if no identity is loaded or the cert can't be parsed.
    /// Useful for Prometheus metrics: `aeon_tls_cert_expiry_seconds`.
    pub fn leaf_cert_expiry_secs(&self) -> Option<i64> {
        let identity = self.identity.as_ref()?;
        let leaf = identity.certs.first()?;
        parse_x509_not_after(leaf.as_ref())
    }

    /// Seconds until the leaf certificate expires, or negative if already expired.
    /// Returns `None` if no identity is loaded or the cert can't be parsed.
    pub fn leaf_cert_days_until_expiry(&self) -> Option<i64> {
        let expiry_secs = self.leaf_cert_expiry_secs()?;
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        Some((expiry_secs - now_secs) / 86400)
    }

    /// Parse the expiry (notAfter) from a DER-encoded X.509 certificate.
    /// Returns seconds since Unix epoch, or `None` if parsing fails.
    pub fn parse_cert_expiry_secs(der: &[u8]) -> Option<i64> {
        parse_x509_not_after(der)
    }

    /// Parse Subject CN + SubjectAltNames from a DER-encoded X.509 cert.
    ///
    /// Returns subjects as strings using OpenSSL conventions so operator
    /// allow-lists are predictable across tooling:
    /// - `CN=<value>` — Common Name attribute of Subject
    /// - `DNS:<value>` — dNSName SAN
    /// - `URI:<value>` — uniformResourceIdentifier SAN
    /// - `IP:<value>` — iPAddress SAN (IPv4 dotted or IPv6 canonical)
    ///
    /// Used by WS / WT connector mTLS verification to populate
    /// `AuthContext::client_cert_subjects` against the S9 allow-list.
    pub fn parse_cert_subjects(der: &[u8]) -> Vec<String> {
        parse_x509_subjects(der)
    }

    /// Reload certificates from the original file paths.
    /// Useful for certificate rotation without restart.
    pub fn reload(&mut self) -> Result<(), AeonError> {
        if let (Some(cert_path), Some(key_path)) = (&self.cert_path, &self.key_path) {
            self.identity = Some(load_identity(cert_path, key_path)?);
        }
        if let Some(ca_path) = &self.ca_path {
            self.root_store = load_ca_roots(ca_path)?;
        }
        Ok(())
    }
}

fn load_identity(cert_path: &Path, key_path: &Path) -> Result<TlsIdentity, AeonError> {
    let cert_pem = std::fs::read(cert_path).map_err(|e| AeonError::Config {
        message: format!("failed to read cert file '{}': {e}", cert_path.display()),
    })?;
    let key_pem = std::fs::read(key_path).map_err(|e| AeonError::Config {
        message: format!("failed to read key file '{}': {e}", key_path.display()),
    })?;

    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AeonError::Config {
            message: format!("failed to parse cert PEM: {e}"),
        })?;

    if certs.is_empty() {
        return Err(AeonError::Config {
            message: format!("no certificates found in '{}'", cert_path.display()),
        });
    }

    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| AeonError::Config {
            message: format!("failed to parse key PEM: {e}"),
        })?
        .ok_or_else(|| AeonError::Config {
            message: format!("no private key found in '{}'", key_path.display()),
        })?;

    Ok(TlsIdentity { certs, key })
}

fn load_ca_roots(ca_path: &Path) -> Result<rustls::RootCertStore, AeonError> {
    let ca_pem = std::fs::read(ca_path).map_err(|e| AeonError::Config {
        message: format!("failed to read CA file '{}': {e}", ca_path.display()),
    })?;

    let ca_certs = rustls_pemfile::certs(&mut ca_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AeonError::Config {
            message: format!("failed to parse CA PEM: {e}"),
        })?;

    if ca_certs.is_empty() {
        return Err(AeonError::Config {
            message: format!("no CA certificates found in '{}'", ca_path.display()),
        });
    }

    let mut root_store = rustls::RootCertStore::empty();
    for cert in ca_certs {
        root_store.add(cert).map_err(|e| AeonError::Config {
            message: format!("failed to add CA cert: {e}"),
        })?;
    }

    Ok(root_store)
}

// ─── Auto-generated self-signed certificates ───────────────────────

/// Paths to auto-generated certificate files.
#[derive(Debug, Clone)]
pub struct AutoCertPaths {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
    pub node_cert: PathBuf,
    pub node_key: PathBuf,
}

impl AutoCertPaths {
    /// Standard paths under `data_dir/tls/`.
    pub fn from_data_dir(data_dir: &Path) -> Self {
        let tls_dir = data_dir.join("tls");
        Self {
            ca_cert: tls_dir.join("ca.pem"),
            ca_key: tls_dir.join("ca.key"),
            node_cert: tls_dir.join("node.pem"),
            node_key: tls_dir.join("node.key"),
        }
    }

    /// Whether all cert files already exist (reuse across restarts).
    pub fn all_exist(&self) -> bool {
        self.ca_cert.exists()
            && self.ca_key.exists()
            && self.node_cert.exists()
            && self.node_key.exists()
    }
}

/// Generate a self-signed CA + node cert and persist to `data_dir/tls/`.
///
/// SANs (Subject Alternative Names) are derived from:
/// - `localhost` (always)
/// - `127.0.0.1` (always)
/// - `::1` (always, IPv6 loopback)
/// - Machine hostname (from `hostname()`)
/// - Bind IP if not `0.0.0.0` or `::`
///
/// If certs already exist at the expected paths, they are reused (not regenerated).
/// Returns a `CertificateStore` ready for use.
#[cfg(feature = "auto-tls")]
pub fn auto_generate_certs(
    data_dir: &Path,
    bind_addr: &str,
) -> Result<(CertificateStore, AutoCertPaths), AeonError> {
    let paths = AutoCertPaths::from_data_dir(data_dir);

    if paths.all_exist() {
        // Reuse existing certs
        let store =
            CertificateStore::from_pem_files(&paths.node_cert, &paths.node_key, &paths.ca_cert)?;
        return Ok((store, paths));
    }

    // Ensure tls directory exists
    let tls_dir = data_dir.join("tls");
    std::fs::create_dir_all(&tls_dir).map_err(|e| AeonError::Config {
        message: format!(
            "failed to create tls directory '{}': {e}",
            tls_dir.display()
        ),
    })?;

    // Collect SANs
    let mut sans: Vec<rcgen::SanType> = vec![
        rcgen::SanType::DnsName("localhost".try_into().map_err(|e| AeonError::Crypto {
            message: format!("invalid SAN 'localhost': {e}"),
            source: None,
        })?),
        rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        rcgen::SanType::IpAddress(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
    ];

    // Add hostname
    if let Ok(hostname) = hostname::get() {
        if let Some(name) = hostname.to_str() {
            if name != "localhost" {
                if let Ok(ia5) = name.try_into() {
                    sans.push(rcgen::SanType::DnsName(ia5));
                }
            }
        }
    }

    // Add bind IP if specific (not wildcard)
    if let Ok(ip) = bind_addr.parse::<std::net::IpAddr>() {
        let is_wildcard = match ip {
            std::net::IpAddr::V4(v4) => v4.is_unspecified(),
            std::net::IpAddr::V6(v6) => v6.is_unspecified(),
        };
        if !is_wildcard {
            sans.push(rcgen::SanType::IpAddress(ip));
        }
    } else if let Some(host) = bind_addr.split(':').next() {
        // bind_addr might be "host:port"
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            let is_wildcard = match ip {
                std::net::IpAddr::V4(v4) => v4.is_unspecified(),
                std::net::IpAddr::V6(v6) => v6.is_unspecified(),
            };
            if !is_wildcard {
                sans.push(rcgen::SanType::IpAddress(ip));
            }
        }
    }

    // Generate CA
    let ca_key = rcgen::KeyPair::generate().map_err(|e| AeonError::Crypto {
        message: format!("failed to generate CA keypair: {e}"),
        source: None,
    })?;
    let mut ca_params = rcgen::CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("Aeon Auto-CA".into()),
    );
    ca_params.distinguished_name.push(
        rcgen::DnType::OrganizationName,
        rcgen::DnValue::Utf8String("Aeon".into()),
    );

    let ca_cert = ca_params
        .self_signed(&ca_key)
        .map_err(|e| AeonError::Crypto {
            message: format!("failed to self-sign CA cert: {e}"),
            source: None,
        })?;

    // Generate node cert signed by CA
    let node_key = rcgen::KeyPair::generate().map_err(|e| AeonError::Crypto {
        message: format!("failed to generate node keypair: {e}"),
        source: None,
    })?;
    let mut node_params = rcgen::CertificateParams::default();
    node_params.subject_alt_names = sans;
    node_params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("Aeon Node".into()),
    );

    let issuer = rcgen::Issuer::new(ca_params, &ca_key);
    let node_cert = node_params
        .signed_by(&node_key, &issuer)
        .map_err(|e| AeonError::Crypto {
            message: format!("failed to sign node cert: {e}"),
            source: None,
        })?;

    // Write PEM files
    write_file(&paths.ca_cert, ca_cert.pem().as_bytes())?;
    write_file(&paths.ca_key, ca_key.serialize_pem().as_bytes())?;
    write_file(&paths.node_cert, node_cert.pem().as_bytes())?;
    write_file(&paths.node_key, node_key.serialize_pem().as_bytes())?;

    // Load into CertificateStore
    let store =
        CertificateStore::from_pem_files(&paths.node_cert, &paths.node_key, &paths.ca_cert)?;

    Ok((store, paths))
}

fn write_file(path: &Path, data: &[u8]) -> Result<(), AeonError> {
    std::fs::write(path, data).map_err(|e| AeonError::Config {
        message: format!("failed to write '{}': {e}", path.display()),
    })
}

// ─── Per-connector TLS configuration ──────────────────────────────

/// TLS mode for individual source/sink connectors.
///
/// Each connector instance can independently configure its TLS settings.
/// This enables fan-in pipelines where sources use different CAs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectorTlsMode {
    /// No TLS — plaintext connection (default for connectors).
    #[default]
    None,
    /// Use system CA trust store (e.g., `/etc/ssl/certs`).
    /// Validates server cert but does not present a client cert.
    SystemCa,
    /// Explicit PEM files — CA cert, and optionally client cert + key for mTLS.
    Pem,
}

/// Per-connector TLS configuration block.
///
/// Embedded in each source/sink connector config:
/// ```yaml
/// source:
///   type: kafka
///   brokers: "broker:9092"
///   tls:
///     mode: pem
///     ca: /etc/aeon/certs/kafka-ca.pem
///     cert: /etc/aeon/certs/client.pem   # optional (mTLS)
///     key: /etc/aeon/certs/client.key    # optional (mTLS)
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorTlsConfig {
    /// TLS mode for this connector.
    #[serde(default)]
    pub mode: ConnectorTlsMode,
    /// Path to CA certificate (PEM). Required for `pem` mode.
    pub ca: Option<PathBuf>,
    /// Path to client certificate (PEM). Optional — enables mTLS.
    pub cert: Option<PathBuf>,
    /// Path to client private key (PEM). Required if `cert` is set.
    pub key: Option<PathBuf>,
}

impl Default for ConnectorTlsConfig {
    fn default() -> Self {
        Self {
            mode: ConnectorTlsMode::None,
            ca: None,
            cert: None,
            key: None,
        }
    }
}

impl ConnectorTlsConfig {
    /// Create a plaintext (no TLS) connector config.
    pub fn none() -> Self {
        Self::default()
    }

    /// Create a system-CA connector config.
    pub fn system_ca() -> Self {
        Self {
            mode: ConnectorTlsMode::SystemCa,
            ..Self::default()
        }
    }

    /// Create a PEM-based connector config.
    pub fn pem(ca: impl Into<PathBuf>) -> Self {
        Self {
            mode: ConnectorTlsMode::Pem,
            ca: Some(ca.into()),
            cert: None,
            key: None,
        }
    }

    /// Add client cert + key for mTLS.
    pub fn with_client_cert(mut self, cert: impl Into<PathBuf>, key: impl Into<PathBuf>) -> Self {
        self.cert = Some(cert.into());
        self.key = Some(key.into());
        self
    }

    /// Validate the connector TLS config.
    pub fn validate(&self, context: &str) -> Result<(), AeonError> {
        match self.mode {
            ConnectorTlsMode::None => {}
            ConnectorTlsMode::SystemCa => {
                // No file paths needed — uses OS trust store
            }
            ConnectorTlsMode::Pem => {
                if self.ca.is_none() {
                    return Err(AeonError::Config {
                        message: format!(
                            "{context}: connector tls.mode 'pem' requires a 'ca' path"
                        ),
                    });
                }
                // cert and key must be both present or both absent
                if self.cert.is_some() != self.key.is_some() {
                    return Err(AeonError::Config {
                        message: format!(
                            "{context}: connector tls 'cert' and 'key' must both be set for mTLS, \
                             or both omitted for server-only verification"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Whether this connector uses TLS (any mode other than none).
    pub fn is_tls_enabled(&self) -> bool {
        self.mode != ConnectorTlsMode::None
    }

    /// Whether this connector uses mTLS (has client cert + key).
    pub fn is_mtls(&self) -> bool {
        self.cert.is_some() && self.key.is_some()
    }

    /// Convert to rdkafka-compatible config pairs.
    ///
    /// Returns key-value pairs to be applied via `ClientConfig::set()`.
    /// For Kafka/Redpanda connectors: sets `security.protocol`, `ssl.ca.location`, etc.
    pub fn to_rdkafka_config_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        match self.mode {
            ConnectorTlsMode::None => {}
            ConnectorTlsMode::SystemCa => {
                pairs.push(("security.protocol".into(), "ssl".into()));
                // rdkafka uses system CA by default when ssl.ca.location is not set
            }
            ConnectorTlsMode::Pem => {
                pairs.push(("security.protocol".into(), "ssl".into()));
                if let Some(ca) = &self.ca {
                    pairs.push(("ssl.ca.location".into(), ca.display().to_string()));
                }
                if let Some(cert) = &self.cert {
                    pairs.push((
                        "ssl.certificate.location".into(),
                        cert.display().to_string(),
                    ));
                }
                if let Some(key) = &self.key {
                    pairs.push(("ssl.key.location".into(), key.display().to_string()));
                }
            }
        }
        pairs
    }

    /// Build a rustls `ClientConfig` from this connector TLS config.
    ///
    /// For TCP-based connectors (non-Kafka) that use tokio-rustls.
    /// Returns `None` if mode is `none`.
    pub fn build_rustls_client_config(&self) -> Result<Option<rustls::ClientConfig>, AeonError> {
        match self.mode {
            ConnectorTlsMode::None => Ok(None),
            ConnectorTlsMode::SystemCa => {
                // Build config with system roots
                let root_store = rustls::RootCertStore::empty();
                // Note: for system CA, users should add webpki-roots or
                // rustls-native-certs as a dependency. For now, empty store
                // is a placeholder — production connectors will integrate
                // platform-specific root loading.
                let config = rustls::ClientConfig::builder()
                    .with_root_certificates(root_store)
                    .with_no_client_auth();
                Ok(Some(config))
            }
            ConnectorTlsMode::Pem => {
                let ca_path = self.ca.as_ref().ok_or_else(|| AeonError::Config {
                    message: "connector pem mode requires ca path".into(),
                })?;
                if self.is_mtls() {
                    let store = CertificateStore::from_pem_files(
                        self.cert.as_ref().ok_or_else(|| AeonError::Config {
                            message: "mTLS requires cert path".into(),
                        })?,
                        self.key.as_ref().ok_or_else(|| AeonError::Config {
                            message: "mTLS requires key path".into(),
                        })?,
                        ca_path,
                    )?;
                    Ok(Some(store.build_mtls_client_config()?))
                } else {
                    let store = CertificateStore::ca_only(ca_path)?;
                    Ok(Some(store.build_tls_client_config()?))
                }
            }
        }
    }
}

// ── Minimal X.509 DER parser (notAfter extraction only) ────────────────

/// Parse the `notAfter` field from a DER-encoded X.509 certificate.
///
/// Returns seconds since Unix epoch, or `None` if the cert can't be parsed.
/// This is a minimal parser that only extracts the validity period — no
/// external X.509 crate needed.
fn parse_x509_not_after(der: &[u8]) -> Option<i64> {
    // X.509 structure:
    //   SEQUENCE {
    //     SEQUENCE (tbsCertificate) {
    //       [0] EXPLICIT version (optional)
    //       INTEGER serialNumber
    //       SEQUENCE signatureAlgorithm
    //       SEQUENCE issuer
    //       SEQUENCE validity { notBefore, notAfter }
    //       ...
    //     }
    //   }
    let (_, cert_body) = read_der_sequence(der)?;
    let (_, tbs_body) = read_der_sequence(cert_body)?;

    let mut pos = tbs_body;

    // Skip version [0] EXPLICIT if present (tag 0xA0)
    if pos.first() == Some(&0xA0) {
        let (rest, _) = read_der_tlv(pos)?;
        pos = rest;
    }

    // Skip serialNumber (INTEGER)
    let (rest, _) = read_der_tlv(pos)?;
    pos = rest;

    // Skip signatureAlgorithm (SEQUENCE)
    let (rest, _) = read_der_tlv(pos)?;
    pos = rest;

    // Skip issuer (SEQUENCE)
    let (rest, _) = read_der_tlv(pos)?;
    pos = rest;

    // Validity SEQUENCE { notBefore, notAfter }
    let (_, validity_body) = read_der_sequence(pos)?;

    // Skip notBefore
    let (rest, _) = read_der_tlv(validity_body)?;

    // Read notAfter (UTCTime tag=0x17 or GeneralizedTime tag=0x18)
    let (_rest, not_after_bytes) = read_der_tlv(rest)?;
    let tag = rest[0];

    parse_asn1_time(tag, not_after_bytes)
}

/// Read a DER SEQUENCE tag+length, returning (remaining, body).
fn read_der_sequence(data: &[u8]) -> Option<(&[u8], &[u8])> {
    if data.first()? != &0x30 {
        return None;
    }
    let (rest, body) = read_der_tlv(data)?;
    Some((rest, body))
}

/// Read a DER TLV (tag-length-value), returning (remaining_after, value).
fn read_der_tlv(data: &[u8]) -> Option<(&[u8], &[u8])> {
    if data.is_empty() {
        return None;
    }
    let (_tag, rest) = (data[0], &data[1..]);
    let (len, header_extra) = read_der_length(rest)?;
    let value_start = 1 + header_extra;
    let value_end = value_start + len;
    if value_end > data.len() {
        return None;
    }
    Some((&data[value_end..], &data[value_start..value_end]))
}

/// Read a DER length field, returning (length, bytes_consumed_for_length).
fn read_der_length(data: &[u8]) -> Option<(usize, usize)> {
    let first = *data.first()?;
    if first < 0x80 {
        Some((first as usize, 1))
    } else {
        let num_bytes = (first & 0x7F) as usize;
        if num_bytes == 0 || num_bytes > 4 || data.len() < 1 + num_bytes {
            return None;
        }
        let mut len: usize = 0;
        for &b in &data[1..1 + num_bytes] {
            len = (len << 8) | (b as usize);
        }
        Some((len, 1 + num_bytes))
    }
}

/// Parse ASN.1 UTCTime (YYMMDDHHMMSSZ) or GeneralizedTime (YYYYMMDDHHMMSSZ)
/// into seconds since Unix epoch.
fn parse_asn1_time(tag: u8, data: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(data).ok()?;
    let (year, rest) = if tag == 0x17 {
        // UTCTime: YY (≥50 → 19xx, <50 → 20xx)
        let yy: i64 = s.get(..2)?.parse().ok()?;
        let year = if yy >= 50 { 1900 + yy } else { 2000 + yy };
        (year, s.get(2..)?)
    } else if tag == 0x18 {
        // GeneralizedTime: YYYY
        let yyyy: i64 = s.get(..4)?.parse().ok()?;
        (yyyy, s.get(4..)?)
    } else {
        return None;
    };

    let month: i64 = rest.get(..2)?.parse().ok()?;
    let day: i64 = rest.get(2..4)?.parse().ok()?;
    let hour: i64 = rest.get(4..6)?.parse().ok()?;
    let min: i64 = rest.get(6..8)?.parse().ok()?;
    let sec: i64 = rest.get(8..10)?.parse().ok()?;

    // Simplified days-from-epoch (good enough for expiry gauge)
    Some(datetime_to_epoch(year, month, day, hour, min, sec))
}

/// Convert a UTC date/time to seconds since Unix epoch.
/// Only needs to be correct for dates in the range 1970–2099.
fn datetime_to_epoch(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64) -> i64 {
    // Days per month (non-leap)
    const DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    let mut days: i64 = 0;
    // Years since 1970
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    // Months in current year
    for m in 1..month {
        days += DAYS[(m - 1) as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    days * 86400 + hour * 3600 + min * 60 + sec
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ── Subject / SAN extraction for mTLS ────────────────────────────────

/// Walk tbsCertificate, extract CN from Subject + SANs from extensions.
/// Returns subjects formatted with OpenSSL-style prefixes (`CN=`, `DNS:`,
/// `URI:`, `IP:`). Any parse failure yields an empty vec rather than an
/// error — the caller (S9 verifier) will reject "no subjects" the same
/// way it rejects "no client cert presented".
fn parse_x509_subjects(der: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let Some((_, cert_body)) = read_der_sequence(der) else {
        return out;
    };
    let Some((_, tbs_body)) = read_der_sequence(cert_body) else {
        return out;
    };

    let mut pos = tbs_body;
    // Skip [0] EXPLICIT version if present.
    if pos.first() == Some(&0xA0) {
        let Some((rest, _)) = read_der_tlv(pos) else {
            return out;
        };
        pos = rest;
    }
    // Skip serialNumber, signatureAlgorithm, issuer, validity.
    for _ in 0..4 {
        let Some((rest, _)) = read_der_tlv(pos) else {
            return out;
        };
        pos = rest;
    }
    // Subject SEQUENCE.
    let Some((rest, subject_body)) = read_der_sequence(pos) else {
        return out;
    };
    extract_cn_from_subject(subject_body, &mut out);
    pos = rest;

    // Skip subjectPublicKeyInfo.
    let Some((rest, _)) = read_der_tlv(pos) else {
        return out;
    };
    pos = rest;

    // Look for [3] EXPLICIT extensions, skipping [1] IMPLICIT
    // issuerUniqueID (0x81) and [2] IMPLICIT subjectUniqueID (0x82) if
    // present.
    while !pos.is_empty() {
        let tag = pos[0];
        let Some((rest, body)) = read_der_tlv(pos) else {
            return out;
        };
        if tag == 0xA3 {
            let Some((_, ext_seq)) = read_der_sequence(body) else {
                return out;
            };
            extract_sans_from_extensions(ext_seq, &mut out);
            return out;
        }
        pos = rest;
    }
    out
}

// RDN OIDs (2.5.4.*). Encoded as tag 0x06 len 0x03 then these bytes.
const CN_OID: &[u8] = &[0x55, 0x04, 0x03]; // 2.5.4.3 — commonName
const SAN_OID: &[u8] = &[0x55, 0x1D, 0x11]; // 2.5.29.17 — subjectAltName

fn extract_cn_from_subject(mut subject_body: &[u8], out: &mut Vec<String>) {
    // Subject = SEQUENCE OF RelativeDistinguishedName
    // RDN = SET OF AttributeTypeAndValue
    // ATV = SEQUENCE { OID, Value }
    while !subject_body.is_empty() {
        let Some((rest, rdn_body)) = read_der_tlv(subject_body) else {
            return;
        };
        subject_body = rest;
        let mut p = rdn_body;
        while !p.is_empty() {
            let Some((r, atv_body)) = read_der_sequence(p) else {
                return;
            };
            p = r;
            let Some((after_oid, oid_body)) = read_der_tlv(atv_body) else {
                continue;
            };
            if oid_body != CN_OID {
                continue;
            }
            let Some((_, value_body)) = read_der_tlv(after_oid) else {
                continue;
            };
            // PrintableString (0x13), UTF8String (0x0C), T61 (0x14),
            // IA5String (0x16), BMPString (0x1E) — we accept any of
            // these as UTF-8 best-effort. BMPString is UTF-16 but rare;
            // treat it as opaque if non-UTF-8.
            if let Ok(s) = std::str::from_utf8(value_body) {
                out.push(format!("CN={s}"));
            }
        }
    }
}

fn extract_sans_from_extensions(mut ext_seq: &[u8], out: &mut Vec<String>) {
    while !ext_seq.is_empty() {
        let Some((rest, ext_body)) = read_der_sequence(ext_seq) else {
            return;
        };
        ext_seq = rest;

        let Some((after_oid, oid_body)) = read_der_tlv(ext_body) else {
            continue;
        };
        if oid_body != SAN_OID {
            continue;
        }

        // Optional critical BOOLEAN (tag 0x01).
        let mut p = after_oid;
        if p.first() == Some(&0x01) {
            let Some((r, _)) = read_der_tlv(p) else {
                continue;
            };
            p = r;
        }

        // extnValue OCTET STRING wrapping SEQUENCE OF GeneralName.
        let Some((_, octet_body)) = read_der_tlv(p) else {
            continue;
        };
        let Some((_, san_seq_body)) = read_der_sequence(octet_body) else {
            continue;
        };

        let mut q = san_seq_body;
        while !q.is_empty() {
            let tag = q[0];
            let Some((r, value_body)) = read_der_tlv(q) else {
                return;
            };
            q = r;
            match tag {
                // [2] IMPLICIT IA5String — dNSName
                0x82 => {
                    if let Ok(s) = std::str::from_utf8(value_body) {
                        out.push(format!("DNS:{s}"));
                    }
                }
                // [6] IMPLICIT IA5String — URI
                0x86 => {
                    if let Ok(s) = std::str::from_utf8(value_body) {
                        out.push(format!("URI:{s}"));
                    }
                }
                // [7] IMPLICIT OCTET STRING — iPAddress (4 or 16 bytes)
                0x87 => match value_body.len() {
                    4 => out.push(format!(
                        "IP:{}.{}.{}.{}",
                        value_body[0], value_body[1], value_body[2], value_body[3]
                    )),
                    16 => {
                        let mut segs = [0u16; 8];
                        for (i, seg) in segs.iter_mut().enumerate() {
                            *seg = u16::from_be_bytes([
                                value_body[2 * i],
                                value_body[2 * i + 1],
                            ]);
                        }
                        let ip = std::net::Ipv6Addr::new(
                            segs[0], segs[1], segs[2], segs[3], segs[4], segs[5], segs[6],
                            segs[7],
                        );
                        out.push(format!("IP:{ip}"));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Generate a self-signed CA + node cert for testing.
    fn generate_test_pem_files(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        // Generate CA
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        // Build issuer from params before consuming them with self_signed
        let issuer = rcgen::Issuer::new(ca_params.clone(), &ca_key);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        // Generate node cert signed by CA
        let node_key = rcgen::KeyPair::generate().unwrap();
        let node_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let node_cert = node_params.signed_by(&node_key, &issuer).unwrap();

        // Write PEM files
        let cert_path = dir.join("node.pem");
        let key_path = dir.join("node.key");
        let ca_path = dir.join("ca.pem");

        let mut f = std::fs::File::create(&cert_path).unwrap();
        f.write_all(node_cert.pem().as_bytes()).unwrap();

        let mut f = std::fs::File::create(&key_path).unwrap();
        f.write_all(node_key.serialize_pem().as_bytes()).unwrap();

        let mut f = std::fs::File::create(&ca_path).unwrap();
        f.write_all(ca_cert.pem().as_bytes()).unwrap();

        (cert_path, key_path, ca_path)
    }

    #[test]
    fn load_from_pem_files() {
        let dir = std::env::temp_dir().join("aeon_tls_test_load");
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key, ca) = generate_test_pem_files(&dir);

        let store = CertificateStore::from_pem_files(&cert, &key, &ca).unwrap();
        assert!(store.has_identity());
        assert!(store.has_ca_roots());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_mtls_server_config() {
        let dir = std::env::temp_dir().join("aeon_tls_test_mtls_srv");
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key, ca) = generate_test_pem_files(&dir);

        let store = CertificateStore::from_pem_files(&cert, &key, &ca).unwrap();
        let _config = store.build_mtls_server_config().unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_tls_server_config() {
        let dir = std::env::temp_dir().join("aeon_tls_test_tls_srv");
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key, ca) = generate_test_pem_files(&dir);

        let store = CertificateStore::from_pem_files(&cert, &key, &ca).unwrap();
        let _config = store.build_tls_server_config().unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_mtls_client_config() {
        let dir = std::env::temp_dir().join("aeon_tls_test_mtls_cli");
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key, ca) = generate_test_pem_files(&dir);

        let store = CertificateStore::from_pem_files(&cert, &key, &ca).unwrap();
        let _config = store.build_mtls_client_config().unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_tls_client_config_ca_only() {
        let dir = std::env::temp_dir().join("aeon_tls_test_ca_only");
        std::fs::create_dir_all(&dir).unwrap();
        let (_, _, ca) = generate_test_pem_files(&dir);

        let store = CertificateStore::ca_only(&ca).unwrap();
        assert!(!store.has_identity());
        assert!(store.has_ca_roots());
        let _config = store.build_tls_client_config().unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn insecure_store_has_nothing() {
        let store = CertificateStore::new_insecure();
        assert!(!store.has_identity());
        assert!(!store.has_ca_roots());
    }

    #[test]
    fn mtls_server_fails_without_identity() {
        let store = CertificateStore::new_insecure();
        assert!(store.build_mtls_server_config().is_err());
    }

    #[test]
    fn mtls_client_fails_without_ca() {
        let dir = std::env::temp_dir().join("aeon_tls_test_no_ca");
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key, _) = generate_test_pem_files(&dir);

        // Load identity but build store without CA
        let identity = load_identity(&cert, &key).unwrap();
        let store = CertificateStore {
            identity: Some(identity),
            root_store: rustls::RootCertStore::empty(),
            cert_path: Some(cert),
            key_path: Some(key),
            ca_path: None,
        };
        assert!(store.build_mtls_client_config().is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reload_certificates() {
        let dir = std::env::temp_dir().join("aeon_tls_test_reload");
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key, ca) = generate_test_pem_files(&dir);

        let mut store = CertificateStore::from_pem_files(&cert, &key, &ca).unwrap();
        // Reload should succeed (same files)
        store.reload().unwrap();
        assert!(store.has_identity());

        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── Auto-cert tests ─────────────────────────────────────────

    #[test]
    fn auto_generate_creates_certs() {
        let dir = std::env::temp_dir().join("aeon_tls_test_auto_gen");
        // Clean up any previous run
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let (store, paths) = auto_generate_certs(&dir, "0.0.0.0:4470").unwrap();
        assert!(store.has_identity());
        assert!(store.has_ca_roots());
        assert!(paths.all_exist());
        assert!(paths.ca_cert.exists());
        assert!(paths.ca_key.exists());
        assert!(paths.node_cert.exists());
        assert!(paths.node_key.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_generate_reuses_existing_certs() {
        let dir = std::env::temp_dir().join("aeon_tls_test_auto_reuse");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        // First generation
        let (_, paths) = auto_generate_certs(&dir, "0.0.0.0").unwrap();
        let ca_bytes_first = std::fs::read(&paths.ca_cert).unwrap();

        // Second call should reuse (same bytes)
        let (store2, _) = auto_generate_certs(&dir, "0.0.0.0").unwrap();
        let ca_bytes_second = std::fs::read(&paths.ca_cert).unwrap();
        assert_eq!(ca_bytes_first, ca_bytes_second);
        assert!(store2.has_identity());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_generate_builds_mtls_config() {
        let dir = std::env::temp_dir().join("aeon_tls_test_auto_mtls");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let (store, _) = auto_generate_certs(&dir, "127.0.0.1:4470").unwrap();
        // Should be able to build both server and client mTLS configs
        let _srv = store.build_mtls_server_config().unwrap();
        let _cli = store.build_mtls_client_config().unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_generate_with_specific_ip() {
        let dir = std::env::temp_dir().join("aeon_tls_test_auto_ip");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        // Specific IP (not wildcard) should be added to SANs
        let (store, _) = auto_generate_certs(&dir, "192.168.1.100:4470").unwrap();
        assert!(store.has_identity());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tls_mode_config_validate_pem_requires_paths() {
        let config = TlsModeConfig {
            mode: TlsMode::Pem,
            cert: None,
            key: None,
            ca: None,
        };
        assert!(config.validate("test", true).is_err());
    }

    #[test]
    fn tls_mode_config_validate_pem_with_paths() {
        let config = TlsModeConfig {
            mode: TlsMode::Pem,
            cert: Some(PathBuf::from("/tmp/cert.pem")),
            key: Some(PathBuf::from("/tmp/key.pem")),
            ca: Some(PathBuf::from("/tmp/ca.pem")),
        };
        assert!(config.validate("test", true).is_ok());
    }

    #[test]
    fn tls_mode_config_validate_none_rejected_when_required() {
        let config = TlsModeConfig {
            mode: TlsMode::None,
            cert: None,
            key: None,
            ca: None,
        };
        assert!(config.validate("test", false).is_err());
        assert!(config.validate("test", true).is_ok());
    }

    #[test]
    fn tls_mode_config_validate_auto_with_peers_rejected() {
        let config = TlsModeConfig {
            mode: TlsMode::Auto,
            cert: None,
            key: None,
            ca: None,
        };
        assert!(config.validate_not_auto_with_peers(true, "test").is_err());
        assert!(config.validate_not_auto_with_peers(false, "test").is_ok());
    }

    #[test]
    fn tls_mode_serde_roundtrip() {
        // Use bincode (available in workspace) for roundtrip
        let modes = vec![TlsMode::None, TlsMode::Auto, TlsMode::Pem];
        for mode in &modes {
            let encoded = bincode::serialize(mode).unwrap();
            let decoded: TlsMode = bincode::deserialize(&encoded).unwrap();
            assert_eq!(*mode, decoded);
        }
    }

    #[test]
    fn tls_mode_default_is_auto() {
        assert_eq!(TlsMode::default(), TlsMode::Auto);
    }

    #[test]
    fn auto_cert_paths_from_data_dir() {
        let paths = AutoCertPaths::from_data_dir(Path::new("/data/aeon"));
        assert_eq!(paths.ca_cert, PathBuf::from("/data/aeon/tls/ca.pem"));
        assert_eq!(paths.ca_key, PathBuf::from("/data/aeon/tls/ca.key"));
        assert_eq!(paths.node_cert, PathBuf::from("/data/aeon/tls/node.pem"));
        assert_eq!(paths.node_key, PathBuf::from("/data/aeon/tls/node.key"));
    }

    // ─── Per-connector TLS config tests ──────────────────────────

    #[test]
    fn connector_tls_none_default() {
        let config = ConnectorTlsConfig::default();
        assert_eq!(config.mode, ConnectorTlsMode::None);
        assert!(!config.is_tls_enabled());
        assert!(!config.is_mtls());
        assert!(config.validate("test").is_ok());
    }

    #[test]
    fn connector_tls_system_ca() {
        let config = ConnectorTlsConfig::system_ca();
        assert_eq!(config.mode, ConnectorTlsMode::SystemCa);
        assert!(config.is_tls_enabled());
        assert!(!config.is_mtls());
        assert!(config.validate("test").is_ok());
    }

    #[test]
    fn connector_tls_pem_requires_ca() {
        let config = ConnectorTlsConfig {
            mode: ConnectorTlsMode::Pem,
            ca: None,
            cert: None,
            key: None,
        };
        assert!(config.validate("test").is_err());
    }

    #[test]
    fn connector_tls_pem_cert_without_key_rejected() {
        let config = ConnectorTlsConfig {
            mode: ConnectorTlsMode::Pem,
            ca: Some(PathBuf::from("/ca.pem")),
            cert: Some(PathBuf::from("/cert.pem")),
            key: None,
        };
        assert!(config.validate("test").is_err());
    }

    #[test]
    fn connector_tls_pem_valid_server_only() {
        let config = ConnectorTlsConfig::pem("/ca.pem");
        assert!(config.is_tls_enabled());
        assert!(!config.is_mtls());
        assert!(config.validate("test").is_ok());
    }

    #[test]
    fn connector_tls_pem_valid_mtls() {
        let config = ConnectorTlsConfig::pem("/ca.pem").with_client_cert("/cert.pem", "/key.pem");
        assert!(config.is_tls_enabled());
        assert!(config.is_mtls());
        assert!(config.validate("test").is_ok());
    }

    #[test]
    fn connector_tls_rdkafka_pairs_none() {
        let config = ConnectorTlsConfig::none();
        assert!(config.to_rdkafka_config_pairs().is_empty());
    }

    #[test]
    fn connector_tls_rdkafka_pairs_system_ca() {
        let config = ConnectorTlsConfig::system_ca();
        let pairs = config.to_rdkafka_config_pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("security.protocol".into(), "ssl".into()));
    }

    #[test]
    fn connector_tls_rdkafka_pairs_pem_mtls() {
        let config = ConnectorTlsConfig::pem("/ca.pem").with_client_cert("/cert.pem", "/key.pem");
        let pairs = config.to_rdkafka_config_pairs();
        assert_eq!(pairs.len(), 4); // protocol, ca, cert, key
        assert!(pairs.iter().any(|(k, _)| k == "security.protocol"));
        assert!(pairs.iter().any(|(k, _)| k == "ssl.ca.location"));
        assert!(pairs.iter().any(|(k, _)| k == "ssl.certificate.location"));
        assert!(pairs.iter().any(|(k, _)| k == "ssl.key.location"));
    }

    #[test]
    fn connector_tls_rustls_none_returns_none() {
        let config = ConnectorTlsConfig::none();
        assert!(config.build_rustls_client_config().unwrap().is_none());
    }

    #[test]
    fn connector_tls_rustls_pem_server_only() {
        let dir = std::env::temp_dir().join("aeon_tls_test_conn_pem");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let (_, _, ca) = generate_test_pem_files(&dir);

        let config = ConnectorTlsConfig::pem(&ca);
        let rustls_config = config.build_rustls_client_config().unwrap();
        assert!(rustls_config.is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn connector_tls_rustls_pem_mtls() {
        let dir = std::env::temp_dir().join("aeon_tls_test_conn_mtls");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key, ca) = generate_test_pem_files(&dir);

        let config = ConnectorTlsConfig::pem(&ca).with_client_cert(&cert, &key);
        let rustls_config = config.build_rustls_client_config().unwrap();
        assert!(rustls_config.is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn connector_tls_serde_roundtrip() {
        let configs = vec![
            ConnectorTlsConfig::none(),
            ConnectorTlsConfig::system_ca(),
            ConnectorTlsConfig::pem("/ca.pem").with_client_cert("/cert.pem", "/key.pem"),
        ];
        for config in &configs {
            let encoded = bincode::serialize(config).unwrap();
            let decoded: ConnectorTlsConfig = bincode::deserialize(&encoded).unwrap();
            assert_eq!(config.mode, decoded.mode);
            assert_eq!(config.ca, decoded.ca);
            assert_eq!(config.cert, decoded.cert);
            assert_eq!(config.key, decoded.key);
        }
    }

    // ── Certificate expiry tests ─────────────────────────────────────

    #[test]
    fn datetime_to_epoch_known_values() {
        // 2024-01-01 00:00:00 UTC
        let epoch = super::datetime_to_epoch(2024, 1, 1, 0, 0, 0);
        assert_eq!(epoch, 1704067200);

        // 1970-01-01 00:00:00 UTC
        assert_eq!(super::datetime_to_epoch(1970, 1, 1, 0, 0, 0), 0);

        // 2000-01-01 00:00:00 UTC
        assert_eq!(super::datetime_to_epoch(2000, 1, 1, 0, 0, 0), 946684800);
    }

    #[test]
    fn parse_asn1_utc_time() {
        // UTCTime "250101120000Z" → 2025-01-01 12:00:00 UTC
        let time = super::parse_asn1_time(0x17, b"250101120000Z");
        assert!(time.is_some());
        let expected = super::datetime_to_epoch(2025, 1, 1, 12, 0, 0);
        assert_eq!(time.unwrap(), expected);
    }

    #[test]
    fn parse_asn1_generalized_time() {
        // GeneralizedTime "20250601000000Z" → 2025-06-01 00:00:00 UTC
        let time = super::parse_asn1_time(0x18, b"20250601000000Z");
        assert!(time.is_some());
        let expected = super::datetime_to_epoch(2025, 6, 1, 0, 0, 0);
        assert_eq!(time.unwrap(), expected);
    }

    #[test]
    fn leaf_cert_expiry_from_pem() {
        let dir = std::env::temp_dir().join("aeon_tls_test_expiry");
        std::fs::create_dir_all(&dir).unwrap();
        let (cert_path, key_path, ca_path) = generate_test_pem_files(&dir);
        let store = CertificateStore::from_pem_files(cert_path, key_path, ca_path).unwrap();

        // rcgen default validity is ~4096 days from now
        let expiry_secs = store.leaf_cert_expiry_secs();
        assert!(expiry_secs.is_some(), "should parse cert expiry");

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Expiry should be in the future (at least 1 year from now)
        let secs = expiry_secs.unwrap();
        assert!(
            secs > now_secs + 365 * 86400,
            "cert should expire > 1 year from now, got {} days",
            (secs - now_secs) / 86400
        );

        let days = store.leaf_cert_days_until_expiry();
        assert!(days.is_some());
        assert!(
            days.unwrap() > 365,
            "expected > 365 days, got {}",
            days.unwrap()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn insecure_store_has_no_expiry() {
        let store = CertificateStore::new_insecure();
        assert!(store.leaf_cert_expiry_secs().is_none());
        assert!(store.leaf_cert_days_until_expiry().is_none());
    }

    // ── Subject / SAN extraction tests ──────────────────────────────

    fn cert_with_cn_and_sans(cn: &str, sans: Vec<rcgen::SanType>) -> Vec<u8> {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String(cn.to_string()),
        );
        params.subject_alt_names = sans;
        let cert = params.self_signed(&key).unwrap();
        cert.der().to_vec()
    }

    #[test]
    fn subjects_extracts_cn() {
        let der = cert_with_cn_and_sans("aeon-ingest", vec![]);
        let subjects = CertificateStore::parse_cert_subjects(&der);
        assert!(
            subjects.contains(&"CN=aeon-ingest".to_string()),
            "got: {subjects:?}"
        );
    }

    #[test]
    fn subjects_extracts_dns_san() {
        let der = cert_with_cn_and_sans(
            "svc",
            vec![rcgen::SanType::DnsName(
                "ingest.example.com".try_into().unwrap(),
            )],
        );
        let subjects = CertificateStore::parse_cert_subjects(&der);
        assert!(
            subjects.contains(&"DNS:ingest.example.com".to_string()),
            "got: {subjects:?}"
        );
    }

    #[test]
    fn subjects_extracts_ipv4_san() {
        let der = cert_with_cn_and_sans(
            "svc",
            vec![rcgen::SanType::IpAddress(std::net::IpAddr::V4(
                std::net::Ipv4Addr::new(10, 0, 0, 5),
            ))],
        );
        let subjects = CertificateStore::parse_cert_subjects(&der);
        assert!(
            subjects.contains(&"IP:10.0.0.5".to_string()),
            "got: {subjects:?}"
        );
    }

    #[test]
    fn subjects_extracts_ipv6_san() {
        let der = cert_with_cn_and_sans(
            "svc",
            vec![rcgen::SanType::IpAddress(std::net::IpAddr::V6(
                std::net::Ipv6Addr::LOCALHOST,
            ))],
        );
        let subjects = CertificateStore::parse_cert_subjects(&der);
        assert!(
            subjects.iter().any(|s| s.starts_with("IP:")),
            "expected IPv6 SAN, got: {subjects:?}"
        );
    }

    #[test]
    fn subjects_extracts_uri_san() {
        let der = cert_with_cn_and_sans(
            "svc",
            vec![rcgen::SanType::URI(
                "spiffe://cluster/ns/default/sa/aeon".try_into().unwrap(),
            )],
        );
        let subjects = CertificateStore::parse_cert_subjects(&der);
        assert!(
            subjects.contains(&"URI:spiffe://cluster/ns/default/sa/aeon".to_string()),
            "got: {subjects:?}"
        );
    }

    #[test]
    fn subjects_multiple_values_all_captured() {
        let der = cert_with_cn_and_sans(
            "multi-svc",
            vec![
                rcgen::SanType::DnsName("a.example.com".try_into().unwrap()),
                rcgen::SanType::DnsName("b.example.com".try_into().unwrap()),
                rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    192, 168, 1, 1,
                ))),
            ],
        );
        let subjects = CertificateStore::parse_cert_subjects(&der);
        assert!(subjects.contains(&"CN=multi-svc".to_string()));
        assert!(subjects.contains(&"DNS:a.example.com".to_string()));
        assert!(subjects.contains(&"DNS:b.example.com".to_string()));
        assert!(subjects.contains(&"IP:192.168.1.1".to_string()));
    }

    #[test]
    fn subjects_garbage_input_returns_empty() {
        let junk = b"not a cert".to_vec();
        assert!(CertificateStore::parse_cert_subjects(&junk).is_empty());
    }

    #[test]
    fn subjects_empty_input_returns_empty() {
        assert!(CertificateStore::parse_cert_subjects(&[]).is_empty());
    }
}
