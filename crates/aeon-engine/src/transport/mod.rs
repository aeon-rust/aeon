//! Processor transport implementations.
//!
//! This module provides concrete `ProcessorTransport` implementations for
//! each processor tier (T1–T4). Phase 12b-1 provides `InProcessTransport`
//! which wraps any sync `Processor` into the async `ProcessorTransport` trait.
//! Phase 12b-3/4 adds `session` (shared AWPP lifecycle), `webtransport_host`
//! (T3), and `websocket_host` (T4).

pub mod in_process;
pub mod session;

#[cfg(feature = "webtransport-host")]
pub mod webtransport_host;

#[cfg(feature = "websocket-host")]
pub mod websocket_host;

pub use in_process::InProcessTransport;
pub use session::{
    AwppSession, BatchInflight, ControlChannel, HandshakeConfig, PipelineResolver, SessionState,
};

#[cfg(feature = "webtransport-host")]
pub use webtransport_host::WebTransportProcessorHost;

#[cfg(feature = "websocket-host")]
pub use websocket_host::WebSocketProcessorHost;
