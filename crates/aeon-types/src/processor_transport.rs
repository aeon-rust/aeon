//! Types supporting the `ProcessorTransport` trait (Phase 12b).
//!
//! These types describe processor health, metadata, tier classification,
//! binding mode, and per-pipeline connection configuration. They are used
//! by all four processor tiers (T1 Native, T2 Wasm, T3 WebTransport, T4 WebSocket).

use serde::{Deserialize, Serialize};

/// Health status of a processor, returned by `ProcessorTransport::health()`.
#[derive(Debug, Clone, Default)]
pub struct ProcessorHealth {
    /// Whether the processor is accepting batches.
    pub healthy: bool,
    /// Last observed batch latency in microseconds.
    pub latency_us: Option<u64>,
    /// Number of batches in-flight (T3/T4 only).
    pub pending_batches: Option<u32>,
    /// Seconds since the processor transport was created or connected.
    pub uptime_secs: Option<u64>,
}

/// Metadata about a processor, returned by `ProcessorTransport::info()`.
#[derive(Debug, Clone)]
pub struct ProcessorInfo {
    /// Processor name (matches registry).
    pub name: String,
    /// Version string.
    pub version: String,
    /// Transport tier.
    pub tier: ProcessorTier,
    /// Supported capabilities (e.g., "batch").
    pub capabilities: Vec<String>,
}

/// The four processor transport tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessorTier {
    /// T1 — Native shared library (.so/.dll), in-process, C-ABI.
    Native,
    /// T2 — WebAssembly module, in-process, Wasmtime.
    Wasm,
    /// T3 — WebTransport (HTTP/3 + QUIC), out-of-process.
    WebTransport,
    /// T4 — WebSocket (HTTP/2 + HTTP/1.1), out-of-process.
    WebSocket,
}

impl std::fmt::Display for ProcessorTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native => write!(f, "native"),
            Self::Wasm => write!(f, "wasm"),
            Self::WebTransport => write!(f, "web-transport"),
            Self::WebSocket => write!(f, "web-socket"),
        }
    }
}

/// Processor binding mode within a pipeline definition.
///
/// `Dedicated` (default): one processor instance serves exactly one pipeline.
/// `Shared`: one processor instance serves multiple pipelines in the same group,
/// with per-pipeline data stream isolation enforced by Aeon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "mode")]
pub enum ProcessorBinding {
    /// One processor instance per pipeline (physical isolation).
    #[default]
    Dedicated,
    /// Shared processor instance across pipelines in the same group.
    Shared {
        /// Group name — pipelines in the same group may share processor instances.
        group: String,
    },
}

/// Per-pipeline processor connection configuration (T3/T4 only).
///
/// All fields are optional — omitted fields use the processor's defaults
/// from the registry or Aeon's global defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessorConnectionConfig {
    /// Override endpoint URL for this pipeline's processor connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Override batch size for this pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<u32>,
    /// Timeout in milliseconds for a single `call_batch()` round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Minimum processor instances required before pipeline starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_instances: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processor_tier_display() {
        assert_eq!(ProcessorTier::Native.to_string(), "native");
        assert_eq!(ProcessorTier::Wasm.to_string(), "wasm");
        assert_eq!(ProcessorTier::WebTransport.to_string(), "web-transport");
        assert_eq!(ProcessorTier::WebSocket.to_string(), "web-socket");
    }

    #[test]
    fn processor_tier_serde_roundtrip() {
        let json = serde_json::to_string(&ProcessorTier::WebTransport).unwrap();
        assert_eq!(json, "\"web-transport\"");
        let back: ProcessorTier = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ProcessorTier::WebTransport);

        let json = serde_json::to_string(&ProcessorTier::WebSocket).unwrap();
        assert_eq!(json, "\"web-socket\"");
        let back: ProcessorTier = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ProcessorTier::WebSocket);
    }

    #[test]
    fn processor_binding_default_is_dedicated() {
        let binding = ProcessorBinding::default();
        assert!(matches!(binding, ProcessorBinding::Dedicated));
    }

    #[test]
    fn processor_binding_serde_roundtrip() {
        let dedicated = ProcessorBinding::Dedicated;
        let json = serde_json::to_string(&dedicated).unwrap();
        let back: ProcessorBinding = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ProcessorBinding::Dedicated));

        let shared = ProcessorBinding::Shared {
            group: "finance".into(),
        };
        let json = serde_json::to_string(&shared).unwrap();
        assert!(json.contains("finance"));
        let back: ProcessorBinding = serde_json::from_str(&json).unwrap();
        match back {
            ProcessorBinding::Shared { group } => assert_eq!(group, "finance"),
            _ => panic!("expected Shared"),
        }
    }

    #[test]
    fn processor_connection_config_default_all_none() {
        let config = ProcessorConnectionConfig::default();
        assert!(config.endpoint.is_none());
        assert!(config.batch_size.is_none());
        assert!(config.timeout_ms.is_none());
        assert!(config.min_instances.is_none());
    }

    #[test]
    fn processor_health_default() {
        let health = ProcessorHealth::default();
        assert!(!health.healthy);
        assert!(health.latency_us.is_none());
    }
}
