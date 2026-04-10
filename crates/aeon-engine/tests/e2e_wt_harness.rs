//! T3 WebTransport Test Harness
//!
//! Provides reusable infrastructure for T3 E2E tests:
//! - Starts a `WebTransportProcessorHost` on a random UDP port with a
//!   self-signed TLS cert (via `wtransport::Identity::self_signed`).
//! - Registers ED25519 processor identities using the same identity store
//!   as the WS harness, so the rest of the auth flow is shared.
//! - Drives events through the T3 transport and collects the resulting
//!   outputs.
//!
//! Mirrors the structure of `e2e_ws_harness.rs` but targets the T3
//! (QUIC/WebTransport/HTTP3) path. Self-signed certs mean clients must
//! opt in to insecure validation — the test binary does so via the
//! `aeon-processor-client` `webtransport-insecure` feature.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aeon_engine::identity_store::ProcessorIdentityStore;
use aeon_engine::transport::session::{DEFAULT_MAX_INFLIGHT_BATCHES, PipelineResolver};
use aeon_engine::transport::webtransport_host::{
    WebTransportHostConfig, WebTransportProcessorHost,
};
use aeon_types::awpp::PipelineAssignment;
use aeon_types::error::AeonError;
use aeon_types::event::{Event, Output};
use aeon_types::processor_identity::{PipelineScope, ProcessorIdentity};
use aeon_types::traits::ProcessorTransport;

/// Pipeline resolver that hands a 16-partition assignment to any requested pipeline.
struct TestPipelineResolver {
    pipeline_name: String,
}

impl PipelineResolver for TestPipelineResolver {
    fn resolve(
        &self,
        requested_pipelines: &[String],
        _processor_name: &str,
    ) -> Result<Vec<PipelineAssignment>, AeonError> {
        let mut assignments = Vec::new();
        for name in requested_pipelines {
            if name == &self.pipeline_name || self.pipeline_name.is_empty() {
                assignments.push(PipelineAssignment {
                    name: name.clone(),
                    partitions: (0..16).collect(),
                    batch_size: 128,
                });
            }
        }
        if assignments.is_empty() {
            for name in requested_pipelines {
                assignments.push(PipelineAssignment {
                    name: name.clone(),
                    partitions: (0..16).collect(),
                    batch_size: 128,
                });
            }
        }
        Ok(assignments)
    }
}

/// Running WebTransport test server with all state needed to drive E2E tests.
pub struct WtTestServer {
    /// Actual bound `SocketAddr` (ephemeral port assigned when binding to :0).
    pub local_addr: SocketAddr,
    /// `https://localhost:<port>` URL the processor client should connect to.
    pub url: String,
    pub wt_host: Arc<WebTransportProcessorHost>,
    pub identity_store: Arc<ProcessorIdentityStore>,
    pub pipeline_name: String,
}

/// ED25519 identity registered for a test processor (shared with the WS harness shape).
pub struct TestIdentity {
    pub signing_key: ed25519_dalek::SigningKey,
    pub public_key: String,
    pub fingerprint: String,
}

/// Start a WebTransport test server bound to `127.0.0.1:0` with a self-signed cert.
///
/// The returned `WtTestServer.url` points at the ephemeral port — use it
/// directly as the `ProcessorConfig.url` for the Rust WT client.
pub async fn start_wt_test_server(pipeline_name: &str) -> WtTestServer {
    let identity_store = Arc::new(ProcessorIdentityStore::new());
    let pipeline_resolver: Arc<dyn PipelineResolver> = Arc::new(TestPipelineResolver {
        pipeline_name: pipeline_name.to_string(),
    });

    // Self-signed cert covering `localhost` — the WT client trusts it via
    // the `webtransport-insecure` feature (no-cert-validation).
    let identity = wtransport::Identity::self_signed(["localhost"])
        .expect("wtransport self-signed identity");

    let server_config = wtransport::ServerConfig::builder()
        .with_bind_address(SocketAddr::from(([127, 0, 0, 1], 0)))
        .with_identity(identity)
        .build();

    let host_config = WebTransportHostConfig {
        bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        identity_store: Arc::clone(&identity_store),
        pipeline_resolver,
        oauth_required: false,
        heartbeat_interval: Duration::from_secs(30),
        handshake_timeout: Duration::from_secs(10),
        batch_timeout: Duration::from_secs(30),
        processor_name: "test-wt-processor".into(),
        processor_version: "1.0.0".into(),
        pipeline_name: pipeline_name.to_string(),
        pipeline_codec: None,
        max_inflight_batches: DEFAULT_MAX_INFLIGHT_BATCHES,
    };

    let wt_host = Arc::new(
        WebTransportProcessorHost::start(host_config, server_config)
            .await
            .expect("start WebTransportProcessorHost"),
    );

    let local_addr = wt_host.local_addr();
    // Use the explicit loopback IP: on Windows `localhost` may resolve to `::1`
    // first and the server is bound to `127.0.0.1`. With
    // `webtransport-insecure` the client disables cert validation entirely,
    // so the cert's `localhost` SAN isn't enforced against the SNI.
    let url = format!("https://127.0.0.1:{}", local_addr.port());

    // Tiny settle so the endpoint's background accept loop is definitely running
    // before the test spawns the client.
    tokio::time::sleep(Duration::from_millis(50)).await;

    WtTestServer {
        local_addr,
        url,
        wt_host,
        identity_store,
        pipeline_name: pipeline_name.to_string(),
    }
}

/// Register an ED25519 keypair for a test processor.
///
/// Identical to the WS harness version; duplicated here so each test binary
/// can include only one harness module.
pub fn register_test_identity(server: &WtTestServer, processor_name: &str) -> TestIdentity {
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};

    let mut rng = rand::thread_rng();
    let signing_key = SigningKey::generate(&mut rng);
    let verifying_key = signing_key.verifying_key();

    let pk_b64 = base64::engine::general_purpose::STANDARD.encode(verifying_key.as_bytes());
    let public_key = format!("ed25519:{pk_b64}");

    let hash = Sha256::digest(verifying_key.as_bytes());
    let fingerprint = format!("SHA256:{}", hex::encode(hash));

    let identity = ProcessorIdentity {
        public_key: public_key.clone(),
        fingerprint: fingerprint.clone(),
        processor_name: processor_name.to_string(),
        allowed_pipelines: PipelineScope::AllMatchingPipelines,
        max_instances: 4,
        registered_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
        registered_by: "e2e-test".into(),
        revoked_at: None,
    };

    server
        .identity_store
        .register(identity)
        .expect("register test identity");

    TestIdentity {
        signing_key,
        public_key,
        fingerprint,
    }
}

/// Wait for a T3 processor to connect (polls `session_count`).
pub async fn wait_for_connection(server: &WtTestServer, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if server.wt_host.session_count() > 0 {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Drive events through the T3 WebTransport host's `call_batch` API and
/// collect the resulting outputs in order.
///
/// Matches the shape of `e2e_ws_harness::drive_events_through_transport`
/// but takes a `&WebTransportProcessorHost`.
pub async fn drive_events_through_transport(
    wt_host: &WebTransportProcessorHost,
    events: Vec<Event>,
    batch_size: usize,
) -> Result<Vec<Output>, AeonError> {
    let mut all_outputs = Vec::new();
    for chunk in events.chunks(batch_size) {
        let outputs = wt_host.call_batch(chunk.to_vec()).await?;
        all_outputs.extend(outputs);
    }
    Ok(all_outputs)
}

/// Write the 32-byte ED25519 seed to a temp file (for external SDK processes).
///
/// Mirrors `e2e_ws_harness::write_seed_file` so Tier D tests can hand the seed
/// to a Python / Go / Node.js / Java subprocess running the per-language WT
/// client SDK.
pub fn write_seed_file(identity: &TestIdentity) -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("aeon-e2e-wt-key-{}.bin", &identity.fingerprint[7..23]));
    std::fs::write(&path, identity.signing_key.as_bytes()).expect("write WT seed file");
    path
}

/// Check if a runtime command exists (returns true if executable is found).
///
/// Mirrors `e2e_ws_harness::runtime_available` so Tier D tests can skip
/// cleanly when a per-language SDK runtime isn't installed on the host.
pub fn runtime_available(command: &str) -> bool {
    // Go uses `go version` not `go --version`.
    let args = if command == "go" {
        vec!["version"]
    } else {
        vec!["--version"]
    };
    std::process::Command::new(command)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
