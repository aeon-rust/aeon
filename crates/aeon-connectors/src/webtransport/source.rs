//! WebTransport Streams source — reliable, ordered delivery.
//!
//! Accepts WebTransport sessions via an HTTP/3 endpoint.
//! Each bidirectional stream carries length-prefixed messages.
//! Push-source with three-phase backpressure.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_crypto::tls::CertificateStore;
use aeon_types::{
    AeonError, AuthContext, CoreLocalUuidGenerator, Event, InboundAuthMode, InboundAuthVerifier,
    PartitionId, Source, SourceKind,
};
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wtransport::ServerConfig;

/// Configuration for `WebTransportSource`.
pub struct WebTransportSourceConfig {
    /// Address to bind the WebTransport endpoint to.
    pub bind_addr: SocketAddr,
    /// wtransport server configuration.
    pub server_config: ServerConfig,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Push buffer configuration.
    pub buffer_config: PushBufferConfig,
    /// Timeout waiting for first event in `next_batch()`.
    pub poll_timeout: Duration,
    /// S9: optional inbound auth verifier.
    /// - `IpAllowlist` is enforced pre-handshake (cheap early reject).
    /// - `Mtls` is enforced post-handshake — the peer's certificate chain
    ///   is pulled from the QUIC/TLS layer and subjects (CN + SANs) are
    ///   surfaced into the verifier's allow-list check.
    /// - `ApiKey` / `Hmac` require HTTP headers or a request body, neither
    ///   of which are present on raw WT bidi streams; verifiers configured
    ///   with those modes will reject post-handshake with `AuthMissing`.
    pub auth: Option<Arc<InboundAuthVerifier>>,
}

impl WebTransportSourceConfig {
    /// Create a config for a WebTransport streams source.
    pub fn new(bind_addr: SocketAddr, server_config: ServerConfig) -> Self {
        Self {
            bind_addr,
            server_config,
            source_name: Arc::from("webtransport"),
            buffer_config: PushBufferConfig::default(),
            poll_timeout: Duration::from_secs(1),
            auth: None,
        }
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set the poll timeout.
    pub fn with_poll_timeout(mut self, timeout: Duration) -> Self {
        self.poll_timeout = timeout;
        self
    }

    /// Attach an inbound auth verifier (S9). Only `ip_allowlist` mode is
    /// enforceable on raw WebTransport streams.
    pub fn with_auth(mut self, verifier: Arc<InboundAuthVerifier>) -> Self {
        self.auth = Some(verifier);
        self
    }
}

/// WebTransport Streams event source.
///
/// Binds an HTTP/3 WebTransport endpoint and accepts sessions.
/// Each bidirectional stream reads length-prefixed binary messages.
/// Reliable and ordered — no data loss under normal operation.
pub struct WebTransportSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    _accept_handle: tokio::task::JoinHandle<()>,
}

impl WebTransportSource {
    /// Bind and start the WebTransport listener.
    pub async fn new(config: WebTransportSourceConfig) -> Result<Self, AeonError> {
        let endpoint = wtransport::Endpoint::server(config.server_config).map_err(|e| {
            AeonError::connection(format!(
                "webtransport bind failed on {}: {e}",
                config.bind_addr
            ))
        })?;

        tracing::info!(addr = %config.bind_addr, "WebTransportSource listening");

        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = config.source_name;
        let auth = config.auth;

        let handle = tokio::spawn(wt_accept_loop(endpoint, tx, source_name, auth));

        Ok(Self {
            rx,
            poll_timeout: config.poll_timeout,
            _accept_handle: handle,
        })
    }
}

async fn wt_accept_loop(
    endpoint: wtransport::Endpoint<wtransport::endpoint::endpoint_side::Server>,
    tx: crate::push_buffer::PushBufferTx,
    source_name: Arc<str>,
    auth: Option<Arc<InboundAuthVerifier>>,
) {
    // Classify the verifier's mode set once — decides whether we run the
    // pre-handshake IP check (cheap early reject for IpAllowlist-only
    // verifiers) and whether we need the post-handshake mTLS subject
    // extraction (any verifier that includes Mtls).
    let (only_ip_allowlist, needs_post_handshake) = classify_auth_modes(auth.as_deref());

    loop {
        let incoming = endpoint.accept().await;
        let peer_ip = incoming.remote_address().ip();

        // S9 pre-handshake: only when the verifier is IpAllowlist-only.
        // Refusing IP-denied peers before the TLS handshake saves the
        // crypto cost — correctness-wise any later subject-check still
        // runs after handshake.
        if only_ip_allowlist {
            if let Some(verifier) = &auth {
                let now_unix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let ctx = AuthContext {
                    peer_ip,
                    method: "WT",
                    path: "",
                    body: b"",
                    headers: &[],
                    now_unix,
                    client_cert_subjects: None,
                };
                if let Err(rejection) = verifier.verify(&ctx) {
                    tracing::warn!(
                        source = %source_name,
                        reason = rejection.reason_tag(),
                        peer_ip = ?rejection.redacted_peer_ip(),
                        "webtransport auth rejected (pre-handshake)"
                    );
                    aeon_observability::emit_auth_rejected(
                        &format!("webtransport/{source_name}"),
                        rejection.reason_tag(),
                        &peer_ip.to_string(),
                    );
                    incoming.refuse();
                    continue;
                }
            }
        }

        let session_request = match incoming.await {
            Ok(req) => req,
            Err(e) => {
                tracing::warn!(error = %e, "webtransport session request failed");
                continue;
            }
        };

        let session = match session_request.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "webtransport session accept failed");
                continue;
            }
        };

        // S9 post-handshake: now that TLS is complete, pull the peer's
        // cert chain and re-run the verifier with subjects populated.
        // This is where `Mtls` mode enforces its allow-list.
        if needs_post_handshake {
            if let Some(verifier) = &auth {
                // Extract subjects from the leaf certificate (index 0 is
                // the peer's own cert; intermediates follow).
                let cert_subjects: Vec<String> = session
                    .peer_identity()
                    .and_then(|chain| {
                        chain
                            .as_slice()
                            .first()
                            .map(|c| CertificateStore::parse_cert_subjects(c.der()))
                    })
                    .unwrap_or_default();
                let subjects_refs: Vec<&str> =
                    cert_subjects.iter().map(|s| s.as_str()).collect();

                let now_unix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let ctx = AuthContext {
                    peer_ip,
                    method: "WT",
                    path: "",
                    body: b"",
                    headers: &[],
                    now_unix,
                    client_cert_subjects: Some(&subjects_refs),
                };
                if let Err(rejection) = verifier.verify(&ctx) {
                    tracing::warn!(
                        source = %source_name,
                        reason = rejection.reason_tag(),
                        peer_ip = ?rejection.redacted_peer_ip(),
                        "webtransport auth rejected (post-handshake)"
                    );
                    aeon_observability::emit_auth_rejected(
                        &format!("webtransport-mtls/{source_name}"),
                        rejection.reason_tag(),
                        &peer_ip.to_string(),
                    );
                    // Close the QUIC connection carrying the session with a
                    // generic application error code. The client sees
                    // ConnectionClosed and no bidi streams are opened.
                    session.close(wtransport::VarInt::from_u32(0x100), b"auth");
                    continue;
                }
            }
        }

        let tx = tx.clone();
        let source_name = Arc::clone(&source_name);

        tokio::spawn(async move {
            loop {
                let stream = match session.accept_bi().await {
                    Ok(stream) => stream,
                    Err(e) => {
                        tracing::debug!(error = %e, "webtransport bi-stream accept ended");
                        break;
                    }
                };

                let tx = tx.clone();
                let source_name = Arc::clone(&source_name);

                tokio::spawn(async move {
                    // Per-stream UUID generator — each task owns its own.
                    let id_gen = CoreLocalUuidGenerator::new(0);
                    if let Err(e) = handle_wt_stream(stream, tx, source_name, id_gen).await {
                        tracing::debug!(error = %e, "webtransport stream error");
                    }
                });
            }
        });
    }
}

async fn handle_wt_stream(
    (mut send, mut recv): (wtransport::SendStream, wtransport::RecvStream),
    tx: crate::push_buffer::PushBufferTx,
    source_name: Arc<str>,
    mut id_gen: CoreLocalUuidGenerator,
) -> Result<(), AeonError> {
    loop {
        // Phase 3: backpressure signal
        if tx.is_overloaded() {
            let _ = send.write_all(&0u32.to_le_bytes()).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        // Read length prefix (4 bytes LE)
        let mut len_buf = [0u8; 4];
        match recv.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(_) => break, // Stream closed
        }

        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 {
            continue;
        }

        let mut payload_buf = vec![0u8; len];
        recv.read_exact(&mut payload_buf)
            .await
            .map_err(|e| AeonError::connection(format!("webtransport read payload failed: {e}")))?;

        let event = Event::new(
            id_gen.next_uuid(),
            0,
            Arc::clone(&source_name),
            PartitionId::new(0),
            Bytes::from(payload_buf),
        );

        if tx.send(event).await.is_err() {
            break;
        }
    }

    Ok(())
}

/// Classify a verifier's mode set. Returns `(only_ip_allowlist,
/// needs_post_handshake)`:
/// - `only_ip_allowlist = true` when the verifier exists and is configured
///   with `IpAllowlist` as the sole mode — allows the pre-handshake
///   refuse() path to skip the TLS handshake for denied peers.
/// - `needs_post_handshake = true` when the verifier exists and has any
///   mode other than `IpAllowlist` — forces a post-accept re-verify with
///   the peer cert subjects populated.
///
/// No verifier ⇒ both `false`. Both flags can be false only if the
/// verifier is configured with `IpAllowlist` AND something else — in
/// that case the pre-handshake check is skipped (we can't reject IP
/// without also running the other modes) and the post-handshake check
/// covers everything.
fn classify_auth_modes(verifier: Option<&InboundAuthVerifier>) -> (bool, bool) {
    let Some(v) = verifier else {
        return (false, false);
    };
    let modes = v.modes();
    if modes.is_empty() {
        return (false, false);
    }
    let only_ip = modes.iter().all(|m| *m == InboundAuthMode::IpAllowlist);
    let has_non_ip = modes.iter().any(|m| *m != InboundAuthMode::IpAllowlist);
    (only_ip, has_non_ip)
}

impl Source for WebTransportSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }

    fn source_kind(&self) -> SourceKind {
        SourceKind::Push
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{InboundAuthConfig, IpAllowlistConfig, MtlsConfig};

    fn build_verifier(cfg: InboundAuthConfig) -> InboundAuthVerifier {
        InboundAuthVerifier::build(cfg).expect("verifier build")
    }

    #[test]
    fn classify_no_verifier_returns_both_false() {
        let (pre, post) = classify_auth_modes(None);
        assert!(!pre);
        assert!(!post);
    }

    #[test]
    fn classify_ip_allowlist_only_enables_pre_handshake() {
        let v = build_verifier(InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist],
            ip_allowlist: Some(IpAllowlistConfig {
                cidrs: vec!["10.0.0.0/8".parse().unwrap()],
            }),
            ..Default::default()
        });
        let (pre, post) = classify_auth_modes(Some(&v));
        assert!(pre, "IpAllowlist-only should enable pre-handshake");
        assert!(!post, "IpAllowlist-only does not need post-handshake");
    }

    #[test]
    fn classify_mtls_only_enables_post_handshake() {
        let v = build_verifier(InboundAuthConfig {
            modes: vec![InboundAuthMode::Mtls],
            mtls: Some(MtlsConfig {
                subject_allowlist: vec!["CN=client".to_string()],
            }),
            ..Default::default()
        });
        let (pre, post) = classify_auth_modes(Some(&v));
        assert!(!pre, "Mtls-only cannot reject pre-handshake");
        assert!(post, "Mtls-only must run post-handshake");
    }

    #[test]
    fn classify_ip_plus_mtls_disables_pre_handshake() {
        // Mixed modes: we cannot refuse at the IP layer because the mTLS
        // allow-list also has to match — rejection is only legal after
        // both modes have been evaluated post-handshake.
        let v = build_verifier(InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist, InboundAuthMode::Mtls],
            ip_allowlist: Some(IpAllowlistConfig {
                cidrs: vec!["10.0.0.0/8".parse().unwrap()],
            }),
            mtls: Some(MtlsConfig {
                subject_allowlist: vec!["CN=client".to_string()],
            }),
            ..Default::default()
        });
        let (pre, post) = classify_auth_modes(Some(&v));
        assert!(!pre);
        assert!(post);
    }
}
