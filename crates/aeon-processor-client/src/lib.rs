//! Rust SDK for building standalone Aeon processors (T3 WebTransport + T4 WebSocket).
//!
//! This crate enables Rust developers to write processors as standalone binaries
//! that connect to Aeon via T3 (WebTransport/HTTP3) or T4 (WebSocket) transports.
//! No Aeon recompilation, no Wasm overhead, full async Rust ecosystem access.
//!
//! # Quick Start (T4 WebSocket)
//!
//! ```rust,no_run
//! use aeon_processor_client::{ProcessorConfig, ProcessorClient, ProcessEvent, ProcessOutput};
//!
//! fn my_processor(event: ProcessEvent) -> Vec<ProcessOutput> {
//!     vec![ProcessOutput {
//!         destination: "output-topic".into(),
//!         key: None,
//!         payload: event.payload,
//!         headers: vec![],
//!     }]
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let config = ProcessorConfig::new("my-processor", "ws://localhost:4471/api/v1/processors/connect");
//!     ProcessorClient::run(config, my_processor).await.unwrap();
//! }
//! ```

// FT-10: no-panic policy. Production code in this crate must not use
// `.unwrap()` or `.expect()` except for explicitly-documented startup-time
// invariants, which must carry an `#[allow(...)]` attribute with rationale.
// Test modules and benches are exempt (`cfg(not(test))`).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod auth;
pub mod wire;

#[cfg(feature = "websocket")]
pub mod websocket;

#[cfg(feature = "webtransport")]
pub mod webtransport;

use serde::{Deserialize, Serialize};

/// Configuration for connecting a processor to Aeon.
#[derive(Debug, Clone)]
pub struct ProcessorConfig {
    /// Processor name (must be registered in Aeon identity store).
    pub name: String,
    /// Processor version (semantic version string).
    pub version: String,
    /// Aeon endpoint URL.
    /// T4: `ws://host:4471/api/v1/processors/connect` or `wss://...`
    /// T3: `https://host:4472` (WebTransport)
    pub url: String,
    /// Pipeline names this processor wants to serve.
    pub pipelines: Vec<String>,
    /// Preferred transport codec: "msgpack" (default) or "json".
    pub codec: String,
    /// Processor binding mode: "dedicated" (default) or "shared".
    pub binding: String,
    /// ED25519 signing keypair (seed bytes).
    /// Generated via `auth::generate_keypair()` or loaded from file.
    pub signing_key: ed25519_dalek::SigningKey,
}

impl ProcessorConfig {
    /// Create a config with a new random keypair. Useful for development.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: "0.1.0".into(),
            url: url.into(),
            pipelines: vec![],
            codec: "msgpack".into(),
            binding: "dedicated".into(),
            signing_key: auth::generate_keypair(),
        }
    }

    /// Set the processor version.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Add a pipeline to serve.
    pub fn pipeline(mut self, pipeline: impl Into<String>) -> Self {
        self.pipelines.push(pipeline.into());
        self
    }

    /// Set the transport codec.
    pub fn codec(mut self, codec: impl Into<String>) -> Self {
        self.codec = codec.into();
        self
    }

    /// Set the signing key from raw seed bytes (32 bytes).
    pub fn signing_key_from_seed(mut self, seed: &[u8; 32]) -> Self {
        self.signing_key = ed25519_dalek::SigningKey::from_bytes(seed);
        self
    }
}

/// An event received from Aeon for processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessEvent {
    /// Event UUID (v7).
    ///
    /// Must match the engine's `WireEvent.id` type exactly. `uuid::Uuid`'s
    /// default serde impl branches on `is_human_readable()` — 16-byte array
    /// in msgpack, string in JSON — so a `String` field here would fail to
    /// decode msgpack frames emitted by the engine.
    pub id: uuid::Uuid,
    /// Unix epoch nanoseconds.
    pub timestamp: i64,
    /// Source identifier.
    pub source: String,
    /// Partition ID.
    pub partition: u16,
    /// Metadata key-value pairs.
    pub metadata: Vec<(String, String)>,
    /// Raw event payload.
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    /// Source offset for checkpointing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_offset: Option<i64>,
}

/// An output produced by the processor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessOutput {
    /// Destination sink/topic name.
    pub destination: String,
    /// Optional partition key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<Vec<u8>>,
    /// Output payload.
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    /// Output headers.
    pub headers: Vec<(String, String)>,
}

/// Information about an accepted session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Unique session ID assigned by Aeon.
    pub session_id: String,
    /// Confirmed transport codec.
    pub codec: String,
    /// Heartbeat interval in milliseconds.
    pub heartbeat_interval_ms: u64,
    /// Whether batch signing is required.
    pub batch_signing: bool,
}

/// Type alias for a processor function that handles one event at a time.
pub type ProcessFn = fn(ProcessEvent) -> Vec<ProcessOutput>;

/// Type alias for a batch processor function.
pub type BatchProcessFn = fn(Vec<ProcessEvent>) -> Vec<Vec<ProcessOutput>>;

/// Processor client for connecting to Aeon.
///
/// Handles AWPP handshake, heartbeat, batch wire encode/decode, and
/// ED25519 signing transparently.
pub struct ProcessorClient;

impl ProcessorClient {
    /// Run the processor using T4 WebSocket transport.
    ///
    /// Connects to the Aeon endpoint, performs AWPP handshake, and enters
    /// the processing loop. Blocks until the connection is closed or an
    /// error occurs.
    #[cfg(feature = "websocket")]
    pub async fn run(
        config: ProcessorConfig,
        process_fn: ProcessFn,
    ) -> Result<(), aeon_types::AeonError> {
        websocket::run_websocket(config, process_fn).await
    }

    /// Run the processor in batch mode using T4 WebSocket transport.
    #[cfg(feature = "websocket")]
    pub async fn run_batch(
        config: ProcessorConfig,
        process_fn: BatchProcessFn,
    ) -> Result<(), aeon_types::AeonError> {
        websocket::run_websocket_batch(config, process_fn).await
    }
}
