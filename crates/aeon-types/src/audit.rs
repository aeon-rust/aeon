//! S2.5 — Audit log channel, separated from data-path tracing.
//!
//! Aeon's operational logs (via the `tracing` crate) are stripped of
//! payloads, subject-ids, and secrets under S2. That same pipeline is
//! **not** where compliance-sensitive events (secret resolved, key
//! rotated, pipeline refused to start, subject erasure request) should
//! land: conflating them dilutes incident forensics, and tracing
//! subscribers can be reconfigured at runtime in ways that drop lines.
//!
//! This module defines the audit event contract + sink trait. The
//! transport (stderr writer, eventually PoH-signed chain) lives in
//! `aeon-observability::audit` so that consumers choose the destination
//! without pulling tracing stack deps into leaf crates.
//!
//! # Properties
//!
//! - **Structured.** No free-text message; every field is typed. A
//!   future PoH signer can serialise each event deterministically
//!   without regex-splitting a human string.
//! - **Redaction-safe by construction.** `AuditEvent` carries no
//!   `Bytes` / payload / subject-id fields. `detail_fields` is a
//!   `Vec<(String, String)>` so callers are forced to redact at emit
//!   time — the sink has no escape hatch.
//! - **Stable action tags.** `action` is `&'static str` — lookups like
//!   `pipeline.start.refused` are greppable across releases.
//! - **Outcome is mandatory.** Audit systems care about
//!   Success / Failure / Denied more than the event itself; making
//!   outcome a separate field lets dashboards pivot on it without
//!   string-matching.
//!
//! # Not in scope here
//!
//! - PoH signing: future atom, landed in a follow-up that can depend
//!   on `aeon-crypto::poh`.
//! - Call-site wiring: emitting audit events from the engine,
//!   supervisor, and connectors proceeds incrementally. This module
//!   only defines the *contract*.

use serde::{Deserialize, Serialize};

/// Broad taxonomy for an audit event. Used as the primary filter key
/// in downstream stores (Loki label, SIEM index, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditCategory {
    /// Secret resolution (Vault / KMS / env / dotenv).
    Secret,
    /// KEK lifecycle — resolve, rotate, wrap/unwrap failure.
    Key,
    /// Pipeline lifecycle — start, stop, refuse-to-start.
    Pipeline,
    /// Compliance validator outcome.
    Compliance,
    /// GDPR erasure / export requests.
    Erasure,
    /// Inbound / outbound connector authentication.
    Auth,
    /// Anything else — use sparingly, prefer adding a dedicated variant.
    Other,
}

/// Outcome of the audited action. Audit consumers pivot on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Success,
    /// Action attempted but failed for a system reason (not a policy
    /// decision). Example: secret backend unreachable.
    Failure,
    /// Action refused because a policy said no. Example:
    /// compliance-validator refused to start pipeline, inbound auth
    /// rejected a caller.
    Denied,
}

/// A single structured audit record. Cheap to emit — fields are
/// owned strings so the caller does not need to hold references
/// alive past the emission point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Unix epoch nanoseconds at emission time. The caller stamps this
    /// so out-of-order writes retain original ordering.
    pub ts_nanos: i64,

    pub category: AuditCategory,

    /// Short, stable identifier for the action, in dotted form.
    /// Examples: `"pipeline.start.refused"`, `"kek.rotate.success"`,
    /// `"erasure.request.accepted"`. Stable across releases. Typed as
    /// `String` so the event deserialises cleanly from a stored audit
    /// log — call sites pass `&'static str` literals through
    /// `into()` at the constructor and pay one allocation per emit.
    pub action: String,

    pub outcome: AuditOutcome,

    /// Who initiated the action. `None` for system-initiated events.
    /// Examples: `Some("api://user/raghu")`, `Some("system/poh-sweeper")`,
    /// `None` for startup-time events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,

    /// The target of the action. Typically a pipeline name, a KEK id,
    /// a subject id prefix (never the full subject id — that's
    /// redacted at emit time by the caller).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,

    /// Redaction-safe key-value details. Callers are responsible for
    /// ensuring no secret / payload material ends up here. The sink
    /// has no escape hatch to strip anything at write time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detail_fields: Vec<(String, String)>,
}

impl AuditEvent {
    /// Minimal constructor. The caller stamps `ts_nanos` to preserve
    /// original ordering across async emission.
    pub fn new(
        ts_nanos: i64,
        category: AuditCategory,
        action: impl Into<String>,
        outcome: AuditOutcome,
    ) -> Self {
        Self {
            ts_nanos,
            category,
            action: action.into(),
            outcome,
            actor: None,
            resource: None,
            detail_fields: Vec::new(),
        }
    }

    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn with_resource(mut self, resource: impl Into<String>) -> Self {
        self.resource = Some(resource.into());
        self
    }

    /// Append a detail key-value. Use `k` as a stable snake_case
    /// identifier (`"pipeline_name"`, `"reason_tag"`). Callers must
    /// redact `v` before calling — values are written verbatim.
    pub fn with_detail(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.detail_fields.push((k.into(), v.into()));
        self
    }
}

/// Contract implemented by audit transports (stderr JSON writer,
/// PoH-signed chain, future SIEM pushers).
///
/// Implementations must be `Send + Sync`. Emission runs on the hot
/// path for some call sites (pipeline refuse-to-start), so implementors
/// should make the common-case emit cheap — typically serialise +
/// non-blocking write.
pub trait AuditSink: Send + Sync {
    /// Emit one audit event. The sink owns the serialisation format
    /// and the transport. Returning `()` is intentional: a failed
    /// audit write must not abort the caller's operation (the point
    /// of audit is "tell me what happened even on error paths") —
    /// sinks should log failures internally via some other channel,
    /// never via the audit sink itself.
    fn emit(&self, event: &AuditEvent);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_defaults_are_none_empty() {
        let e = AuditEvent::new(
            1_700_000_000_000_000_000,
            AuditCategory::Pipeline,
            "pipeline.start.refused",
            AuditOutcome::Denied,
        );
        assert!(e.actor.is_none());
        assert!(e.resource.is_none());
        assert!(e.detail_fields.is_empty());
    }

    #[test]
    fn builder_methods_populate_fields() {
        let e = AuditEvent::new(
            0,
            AuditCategory::Compliance,
            "compliance.validate.denied",
            AuditOutcome::Denied,
        )
        .with_actor("system/supervisor")
        .with_resource("pipeline/orders-kafka")
        .with_detail("regime", "gdpr")
        .with_detail("concern", "encryption");
        assert_eq!(e.actor.as_deref(), Some("system/supervisor"));
        assert_eq!(e.resource.as_deref(), Some("pipeline/orders-kafka"));
        assert_eq!(e.detail_fields.len(), 2);
        assert_eq!(e.detail_fields[0], ("regime".into(), "gdpr".into()));
    }

    #[test]
    fn serialises_as_snake_case_enums() {
        let e = AuditEvent::new(
            42,
            AuditCategory::Key,
            "kek.rotate.success",
            AuditOutcome::Success,
        );
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains(r#""category":"key""#), "got: {j}");
        assert!(j.contains(r#""outcome":"success""#), "got: {j}");
        assert!(j.contains(r#""action":"kek.rotate.success""#), "got: {j}");
    }

    #[test]
    fn empty_optional_fields_are_omitted_from_json() {
        let e = AuditEvent::new(
            0,
            AuditCategory::Secret,
            "secret.resolve.success",
            AuditOutcome::Success,
        );
        let j = serde_json::to_string(&e).unwrap();
        assert!(!j.contains("actor"), "empty actor must be skipped: {j}");
        assert!(
            !j.contains("resource"),
            "empty resource must be skipped: {j}"
        );
        assert!(
            !j.contains("detail_fields"),
            "empty details must be skipped: {j}"
        );
    }

    #[test]
    fn roundtrip_through_json_preserves_all_fields() {
        let e = AuditEvent::new(
            1_700_000_000,
            AuditCategory::Erasure,
            "erasure.request.accepted",
            AuditOutcome::Success,
        )
        .with_actor("api://user/raghu")
        .with_resource("pipeline/orders-kafka")
        .with_detail("subject_ns", "user");
        let j = serde_json::to_string(&e).unwrap();
        let back: AuditEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn sink_trait_is_object_safe() {
        struct Noop;
        impl AuditSink for Noop {
            fn emit(&self, _event: &AuditEvent) {}
        }
        let _b: Box<dyn AuditSink> = Box::new(Noop);
    }

    #[test]
    fn outcome_denied_is_distinct_from_failure() {
        // Contract test: audit consumers rely on the three-way split.
        assert_ne!(AuditOutcome::Denied, AuditOutcome::Failure);
        assert_ne!(AuditOutcome::Success, AuditOutcome::Denied);
    }
}
