//! AWPP (Aeon Wire Processor Protocol) control stream message types.
//!
//! These types define the JSON messages exchanged on the control stream
//! between Aeon and T3/T4 processors during connection lifecycle:
//! challenge, registration, acceptance/rejection, heartbeat, drain, error.

use serde::{Deserialize, Serialize};

use crate::transport_codec::TransportCodec;

/// Protocol version identifier.
pub const PROTOCOL_VERSION: &str = "awpp/1";

// ── Aeon → Processor ─────────────────────────────────────────────────

/// Challenge message sent by Aeon to initiate authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Challenge {
    /// Message type (always "challenge").
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Protocol version.
    pub protocol: String,
    /// Cryptographically random 32-byte nonce (hex-encoded).
    pub nonce: String,
    /// Whether the processor must include an OAuth token.
    #[serde(default)]
    pub oauth_required: bool,
}

impl Challenge {
    /// Create a new challenge with the given nonce.
    pub fn new(nonce: String, oauth_required: bool) -> Self {
        Self {
            msg_type: "challenge".into(),
            protocol: PROTOCOL_VERSION.into(),
            nonce,
            oauth_required,
        }
    }
}

// ── Processor → Aeon ─────────────────────────────────────────────────

/// Registration message sent by processor after signing the challenge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    /// Message type (always "register").
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Protocol version.
    pub protocol: String,
    /// Transport type ("webtransport" or "websocket").
    pub transport: String,
    /// Processor name (must match registry).
    pub name: String,
    /// Processor version string.
    pub version: String,
    /// ED25519 public key ("ed25519:<base64>").
    pub public_key: String,
    /// ED25519 signature of the challenge nonce bytes (hex-encoded).
    pub challenge_signature: String,
    /// OAuth 2.0 JWT token (included when OAuth is required, null/absent otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_token: Option<String>,
    /// Supported capabilities (e.g., ["batch"]).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Maximum batch size the processor can handle.
    #[serde(default)]
    pub max_batch_size: Option<u32>,
    /// Preferred transport codec for data stream serialization.
    #[serde(default)]
    pub transport_codec: TransportCodec,
    /// Pipelines the processor wants to serve.
    #[serde(default)]
    pub requested_pipelines: Vec<String>,
    /// Binding mode ("dedicated" or "shared").
    #[serde(default = "default_binding")]
    pub binding: String,
}

fn default_binding() -> String {
    "dedicated".into()
}

// ── Aeon → Processor (response) ──────────────────────────────────────

/// Pipeline assignment in an acceptance message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineAssignment {
    /// Pipeline name.
    pub name: String,
    /// Assigned partition IDs.
    pub partitions: Vec<u16>,
    /// Batch size for this pipeline.
    pub batch_size: u32,
}

/// Acceptance message sent by Aeon after successful authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Accepted {
    /// Message type (always "accepted").
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Session identifier (unique per connection).
    pub session_id: String,
    /// Pipeline assignments.
    pub pipelines: Vec<PipelineAssignment>,
    /// Wire format for data streams.
    pub wire_format: String,
    /// Confirmed transport codec for data stream serialization.
    /// Pipeline config takes precedence over processor preference.
    #[serde(default)]
    pub transport_codec: TransportCodec,
    /// Heartbeat interval in milliseconds.
    pub heartbeat_interval_ms: u64,
    /// Whether per-batch ED25519 signing is required.
    #[serde(default = "default_true")]
    pub batch_signing: bool,
}

fn default_true() -> bool {
    true
}

/// Rejection error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RejectCode {
    AuthFailed,
    KeyRevoked,
    KeyNotFound,
    PipelineNotAuthorized,
    MaxInstancesReached,
    VersionNotFound,
    ProcessorNotRegistered,
    OauthRequired,
    OauthInvalid,
}

impl std::fmt::Display for RejectCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthFailed => write!(f, "AUTH_FAILED"),
            Self::KeyRevoked => write!(f, "KEY_REVOKED"),
            Self::KeyNotFound => write!(f, "KEY_NOT_FOUND"),
            Self::PipelineNotAuthorized => write!(f, "PIPELINE_NOT_AUTHORIZED"),
            Self::MaxInstancesReached => write!(f, "MAX_INSTANCES_REACHED"),
            Self::VersionNotFound => write!(f, "VERSION_NOT_FOUND"),
            Self::ProcessorNotRegistered => write!(f, "PROCESSOR_NOT_REGISTERED"),
            Self::OauthRequired => write!(f, "OAUTH_REQUIRED"),
            Self::OauthInvalid => write!(f, "OAUTH_INVALID"),
        }
    }
}

/// Rejection message sent by Aeon when authentication/authorization fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rejected {
    /// Message type (always "rejected").
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Error code.
    pub code: RejectCode,
    /// Human-readable error message.
    pub message: String,
}

impl Rejected {
    pub fn new(code: RejectCode, message: impl Into<String>) -> Self {
        Self {
            msg_type: "rejected".into(),
            code,
            message: message.into(),
        }
    }
}

// ── Bidirectional ────────────────────────────────────────────────────

/// Heartbeat message (bidirectional).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Sender's timestamp in milliseconds.
    pub timestamp_ms: i64,
}

impl Heartbeat {
    pub fn new(timestamp_ms: i64) -> Self {
        Self {
            msg_type: "heartbeat".into(),
            timestamp_ms,
        }
    }
}

/// Drain message (Aeon → Processor).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drain {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Reason for drain (e.g., "upgrade", "shutdown").
    pub reason: String,
    /// Deadline in milliseconds to complete in-flight work.
    pub deadline_ms: u64,
}

impl Drain {
    pub fn new(reason: impl Into<String>, deadline_ms: u64) -> Self {
        Self {
            msg_type: "drain".into(),
            reason: reason.into(),
            deadline_ms,
        }
    }
}

/// Error message (either direction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwppError {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Error code.
    pub code: String,
    /// Human-readable error message.
    pub message: String,
    /// Batch ID if the error relates to a specific batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<u64>,
}

impl AwppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            msg_type: "error".into(),
            code: code.into(),
            message: message.into(),
            batch_id: None,
        }
    }

    pub fn with_batch_id(mut self, batch_id: u64) -> Self {
        self.batch_id = Some(batch_id);
        self
    }
}

/// Token refresh message (Processor → Aeon, when OAuth enabled).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRefresh {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// New OAuth JWT token.
    pub oauth_token: String,
}

impl TokenRefresh {
    pub fn new(oauth_token: impl Into<String>) -> Self {
        Self {
            msg_type: "token_refresh".into(),
            oauth_token: oauth_token.into(),
        }
    }
}

/// Envelope for all AWPP control stream messages.
///
/// Deserialized from JSON using the "type" field as discriminant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    Challenge(ChallengePayload),
    Register(RegisterPayload),
    Accepted(AcceptedPayload),
    Rejected(RejectedPayload),
    Heartbeat(HeartbeatPayload),
    Drain(DrainPayload),
    Error(ErrorPayload),
    TokenRefresh(TokenRefreshPayload),
}

/// Payload for `ControlMessage::Challenge`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengePayload {
    pub protocol: String,
    pub nonce: String,
    #[serde(default)]
    pub oauth_required: bool,
}

/// Payload for `ControlMessage::Register`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterPayload {
    pub protocol: String,
    pub transport: String,
    pub name: String,
    pub version: String,
    pub public_key: String,
    pub challenge_signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_token: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub max_batch_size: Option<u32>,
    #[serde(default)]
    pub transport_codec: TransportCodec,
    #[serde(default)]
    pub requested_pipelines: Vec<String>,
    #[serde(default = "default_binding")]
    pub binding: String,
}

/// Payload for `ControlMessage::Accepted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedPayload {
    pub session_id: String,
    pub pipelines: Vec<PipelineAssignment>,
    pub wire_format: String,
    #[serde(default)]
    pub transport_codec: TransportCodec,
    pub heartbeat_interval_ms: u64,
    #[serde(default = "default_true")]
    pub batch_signing: bool,
}

/// Payload for `ControlMessage::Rejected`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedPayload {
    pub code: RejectCode,
    pub message: String,
}

/// Payload for `ControlMessage::Heartbeat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    pub timestamp_ms: i64,
}

/// Payload for `ControlMessage::Drain`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainPayload {
    pub reason: String,
    pub deadline_ms: u64,
}

/// Payload for `ControlMessage::Error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<u64>,
}

/// Payload for `ControlMessage::TokenRefresh`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRefreshPayload {
    pub oauth_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_new() {
        let c = Challenge::new("abcdef".into(), true);
        assert_eq!(c.msg_type, "challenge");
        assert_eq!(c.protocol, "awpp/1");
        assert_eq!(c.nonce, "abcdef");
        assert!(c.oauth_required);
    }

    #[test]
    fn rejected_new() {
        let r = Rejected::new(RejectCode::AuthFailed, "bad signature");
        assert_eq!(r.code, RejectCode::AuthFailed);
        assert_eq!(r.code.to_string(), "AUTH_FAILED");
    }

    #[test]
    fn reject_code_serde_roundtrip() {
        let json = serde_json::to_string(&RejectCode::MaxInstancesReached).unwrap();
        assert_eq!(json, "\"MAX_INSTANCES_REACHED\"");
        let back: RejectCode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RejectCode::MaxInstancesReached);
    }

    #[test]
    fn heartbeat_serde() {
        let h = Heartbeat::new(1712345678000);
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains("heartbeat"));
        assert!(json.contains("1712345678000"));
    }

    #[test]
    fn drain_new() {
        let d = Drain::new("upgrade", 30000);
        assert_eq!(d.reason, "upgrade");
        assert_eq!(d.deadline_ms, 30000);
    }

    #[test]
    fn awpp_error_with_batch_id() {
        let e = AwppError::new("BATCH_FAILED", "timeout").with_batch_id(42);
        assert_eq!(e.batch_id, Some(42));
    }

    #[test]
    fn token_refresh_serde() {
        let tr = TokenRefresh::new("eyJhbG...");
        let json = serde_json::to_string(&tr).unwrap();
        let back: TokenRefresh = serde_json::from_str(&json).unwrap();
        assert_eq!(back.oauth_token, "eyJhbG...");
    }

    #[test]
    fn control_message_challenge_serde() {
        let msg = ControlMessage::Challenge(ChallengePayload {
            protocol: "awpp/1".into(),
            nonce: "deadbeef".into(),
            oauth_required: false,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"challenge\""));
        let back: ControlMessage = serde_json::from_str(&json).unwrap();
        match back {
            ControlMessage::Challenge(p) => assert_eq!(p.nonce, "deadbeef"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn control_message_rejected_serde() {
        let msg = ControlMessage::Rejected(RejectedPayload {
            code: RejectCode::KeyRevoked,
            message: "key was revoked".into(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let back: ControlMessage = serde_json::from_str(&json).unwrap();
        match back {
            ControlMessage::Rejected(p) => assert_eq!(p.code, RejectCode::KeyRevoked),
            _ => panic!("wrong variant"),
        }
    }
}
