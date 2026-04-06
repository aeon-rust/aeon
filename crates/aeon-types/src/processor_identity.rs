//! ED25519 processor identity types for T3/T4 authentication.
//!
//! Each T3/T4 processor instance holds an ED25519 private key; the
//! corresponding public key is registered in Aeon. Authentication uses
//! challenge-response — the processor signs a server-provided nonce to
//! prove key ownership. Batch responses are also signed for non-repudiation.

use serde::{Deserialize, Serialize};

/// ED25519 identity for a T3/T4 processor.
///
/// Each processor instance holds a private key; the corresponding public key
/// is registered in Aeon. Authentication uses challenge-response — the
/// processor signs a server-provided nonce to prove key ownership.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorIdentity {
    /// ED25519 public key (base64-encoded, 32 bytes raw).
    pub public_key: String,
    /// Key fingerprint: SHA-256 of the raw public key bytes (hex, for display/audit).
    pub fingerprint: String,
    /// Processor name this key authenticates as.
    pub processor_name: String,
    /// Pipelines this key is authorized to serve.
    pub allowed_pipelines: PipelineScope,
    /// Maximum concurrent connections from this key.
    pub max_instances: u32,
    /// Registered at (Unix epoch millis).
    pub registered_at: i64,
    /// Registered by (admin identity).
    pub registered_by: String,
    /// Revoked at (None = active).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<i64>,
}

impl ProcessorIdentity {
    /// Whether this identity is currently active (not revoked).
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

impl std::fmt::Display for ProcessorIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{} ({})",
            self.processor_name,
            &self.fingerprint[..core::cmp::min(20, self.fingerprint.len())],
            if self.is_active() {
                "active"
            } else {
                "revoked"
            }
        )
    }
}

/// Which pipelines a processor identity is authorized to serve.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PipelineScope {
    /// Can serve any pipeline that references this processor name.
    #[default]
    AllMatchingPipelines,
    /// Can only serve specifically named pipelines.
    Named(Vec<String>),
}

impl PipelineScope {
    /// Check whether a given pipeline name is within this scope.
    pub fn allows(&self, pipeline_name: &str) -> bool {
        match self {
            Self::AllMatchingPipelines => true,
            Self::Named(names) => names.iter().any(|n| n == pipeline_name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_identity() -> ProcessorIdentity {
        ProcessorIdentity {
            public_key: "ed25519:MCowBQYDK2VwAyEA".into(),
            fingerprint: "SHA256:abc123def456".into(),
            processor_name: "my-enricher".into(),
            allowed_pipelines: PipelineScope::Named(vec![
                "orders-pipeline".into(),
                "payments-pipeline".into(),
            ]),
            max_instances: 4,
            registered_at: 1712345678000,
            registered_by: "admin".into(),
            revoked_at: None,
        }
    }

    #[test]
    fn identity_is_active_when_not_revoked() {
        let id = make_identity();
        assert!(id.is_active());
    }

    #[test]
    fn identity_is_not_active_when_revoked() {
        let mut id = make_identity();
        id.revoked_at = Some(1712345999000);
        assert!(!id.is_active());
    }

    #[test]
    fn pipeline_scope_all_allows_any() {
        let scope = PipelineScope::AllMatchingPipelines;
        assert!(scope.allows("anything"));
        assert!(scope.allows("orders-pipeline"));
    }

    #[test]
    fn pipeline_scope_named_allows_listed() {
        let scope = PipelineScope::Named(vec!["orders".into(), "payments".into()]);
        assert!(scope.allows("orders"));
        assert!(scope.allows("payments"));
        assert!(!scope.allows("other"));
    }

    #[test]
    fn identity_serde_roundtrip() {
        let id = make_identity();
        let json = serde_json::to_string(&id).unwrap();
        let back: ProcessorIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(back.processor_name, "my-enricher");
        assert_eq!(back.fingerprint, "SHA256:abc123def456");
        assert!(back.is_active());
        assert!(back.allowed_pipelines.allows("orders-pipeline"));
        assert!(!back.allowed_pipelines.allows("other-pipeline"));
    }

    #[test]
    fn identity_display() {
        let id = make_identity();
        let s = id.to_string();
        assert!(s.contains("my-enricher"));
        assert!(s.contains("active"));
    }
}
