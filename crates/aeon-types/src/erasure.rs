//! S6.2 — Erasure request + tombstone types.
//!
//! These are the **schema** for the erasure API. The REST handler
//! accepts an [`ErasureRequest`], the erasure store persists one
//! [`ErasureTombstone`] per pending subject, and a later compaction
//! sweep converts each tombstone into a physical delete + PoH
//! null-receipt (see S6.3 / S6.4).
//!
//! The types live in `aeon-types` (not `aeon-engine`) because the
//! REST layer, the erasure store, the compactor, and any future
//! cross-node replicator all need to agree on the on-wire shape.
//!
//! # Scope boundaries
//!
//! - **Request**: what the caller asks for (subject, wildcard,
//!   optional soft-delete window).
//! - **Tombstone**: what the store persists (immutable record of the
//!   request, with `accepted_at` / `hard_delete_after` timestamps the
//!   caller cannot set directly).
//!
//! Storage (`ErasureStore` trait + `L3ErasureStore` impl) is S6.3.
//! PoH null-receipt builder is S6.4. Both are separate modules so a
//! dependent crate can take just the types without pulling in the
//! storage backend.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::error::AeonError;
use crate::subject_id::{SubjectId, validate_namespace_for_wildcard};

/// Serde shim for `Arc<str>` — the workspace does not enable serde's
/// `rc` feature, so we round-trip through `String`. Used via
/// `#[serde(with = "arc_str")]` on individual fields.
mod arc_str {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::sync::Arc;

    pub fn serialize<S: Serializer>(value: &Arc<str>, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(value.as_ref())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Arc<str>, D::Error> {
        let s = String::deserialize(de)?;
        Ok(Arc::from(s))
    }
}

/// What the caller wants erased. Either a single subject
/// (`namespace/id`) or every subject in a namespace (wildcard form).
/// The wildcard form is the GDPR "tenant-wide forget" path — it does
/// not enumerate subjects; compaction drops any record whose
/// `aeon.subject_id` matches the namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ErasureSelector {
    /// Erase exactly one subject.
    Subject(SubjectId),
    /// Erase every subject under a namespace. The string is the
    /// namespace component only, validated with the same character
    /// rules as [`SubjectId`] namespaces.
    Namespace(Arc<str>),
}

/// On-wire representation of [`ErasureSelector`]. Uses the default
/// externally-tagged form (`{"Subject": {...}}` / `{"Namespace": "ns"}`)
/// because bincode does not support adjacently-tagged enums (it can't
/// call `deserialize_identifier`). JSON / MessagePack handle both
/// forms; the default form is the common denominator.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum ErasureSelectorWire {
    Subject(SubjectId),
    Namespace(String),
}

impl Serialize for ErasureSelector {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Self::Subject(s) => ErasureSelectorWire::Subject(s.clone()),
            Self::Namespace(ns) => ErasureSelectorWire::Namespace(ns.as_ref().to_string()),
        };
        wire.serialize(ser)
    }
}

impl<'de> Deserialize<'de> for ErasureSelector {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let wire = ErasureSelectorWire::deserialize(de)?;
        Ok(match wire {
            ErasureSelectorWire::Subject(s) => Self::Subject(s),
            ErasureSelectorWire::Namespace(ns) => Self::Namespace(Arc::from(ns)),
        })
    }
}

impl ErasureSelector {
    /// Parse the REST form. Accepts either `"namespace/id"` or
    /// `"namespace/*"`.
    pub fn parse(s: &str) -> Result<Self, AeonError> {
        match s.split_once('/') {
            Some((ns, "*")) => {
                validate_namespace_for_wildcard(ns)?;
                Ok(Self::Namespace(Arc::from(ns)))
            }
            Some(_) => Ok(Self::Subject(SubjectId::parse(s)?)),
            None => Err(AeonError::config(
                "erasure selector: expected 'namespace/id' or 'namespace/*', no '/' found"
                    .to_string(),
            )),
        }
    }

    /// Canonical string form — used for tombstone keys and metric
    /// labels (after salted-hashing). Wildcard renders as `ns/*`.
    pub fn to_canonical(&self) -> String {
        match self {
            Self::Subject(s) => s.to_canonical(),
            Self::Namespace(ns) => format!("{ns}/*"),
        }
    }

    /// Does this selector match the given [`SubjectId`]? Used by the
    /// compactor to decide whether to drop an event at a segment-scan
    /// time.
    pub fn matches(&self, subject: &SubjectId) -> bool {
        match self {
            Self::Subject(s) => s == subject,
            Self::Namespace(ns) => subject.matches_namespace(ns.as_ref()),
        }
    }

    /// `true` if this selector covers every subject under a namespace.
    pub fn is_wildcard(&self) -> bool {
        matches!(self, Self::Namespace(_))
    }
}

/// Incoming erasure request as received by the REST layer. The
/// caller supplies the selector and an optional soft-delete window.
/// The handler timestamps the request and converts it to an
/// [`ErasureTombstone`] before persisting — callers cannot backdate
/// or forge tombstone timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErasureRequest {
    /// Target pipeline. Erasure is pipeline-scoped — a subject may
    /// exist in many pipelines, but each erasure request applies to
    /// exactly one.
    #[serde(with = "arc_str")]
    pub pipeline: Arc<str>,

    /// Who / what to erase.
    pub selector: ErasureSelector,

    /// Human-readable reason (GDPR request reference, ticket id,
    /// etc.). Kept out of logs (PII-adjacent); persisted as-is in
    /// the tombstone so an auditor can correlate.
    #[serde(default)]
    pub reason: Option<String>,

    /// Soft-delete window: delay physical deletion by this duration.
    /// `None` and `Some(Duration::ZERO)` both mean immediate (purist
    /// GDPR). Operators who need rollback set a non-zero window.
    /// The value is **advisory** — compaction will not run earlier,
    /// but may run later (e.g. a quiet partition).
    #[serde(default)]
    pub soft_delete: Option<Duration>,
}

impl ErasureRequest {
    /// Convert into a tombstone stamped with the given `accepted_at`
    /// (Unix nanos). A fresh UUIDv7 is assigned. The caller (the
    /// REST handler) is the only code that should invoke this — it
    /// owns the trusted clock.
    pub fn into_tombstone(self, accepted_at: i64, id: Uuid) -> ErasureTombstone {
        let hard_delete_after = self
            .soft_delete
            .filter(|d| !d.is_zero())
            .and_then(|d| i64::try_from(d.as_nanos()).ok())
            .map(|nanos| accepted_at.saturating_add(nanos));
        ErasureTombstone {
            id,
            pipeline: self.pipeline,
            selector: self.selector,
            reason: self.reason,
            accepted_at_nanos: accepted_at,
            hard_delete_after_nanos: hard_delete_after,
            state: TombstoneState::Pending,
        }
    }
}

/// Lifecycle of a tombstone. `Pending` is the initial state;
/// compaction flips it to `Completed` after a successful physical
/// delete + PoH null-receipt append. `Failed` records a permanent
/// error (bad config, corrupted segment) so operators can triage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TombstoneState {
    Pending,
    Completed,
    Failed,
}

/// Persisted erasure tombstone. Written once by the REST handler,
/// read by the compactor, updated to `Completed` / `Failed` in place
/// when the erasure has been processed. The record itself is kept
/// for the audit window — auditors need proof that a request was
/// honored, independent of the (now-deleted) event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErasureTombstone {
    /// UUIDv7 — sortable by accept time, useful as a storage key.
    pub id: Uuid,

    /// Pipeline this tombstone belongs to.
    #[serde(with = "arc_str")]
    pub pipeline: Arc<str>,

    /// The original selector. Stored verbatim so an auditor can
    /// reconstruct exactly what was asked for.
    pub selector: ErasureSelector,

    /// Original request reason.
    ///
    /// Serialized unconditionally — `skip_serializing_if` would
    /// corrupt bincode, which requires symmetric encode/decode for
    /// `Option<T>`. JSON callers simply see `"reason": null` on
    /// absent values.
    #[serde(default)]
    pub reason: Option<String>,

    /// Unix-epoch nanoseconds when the REST handler accepted the
    /// request. The GDPR 30-day SLA is measured from here.
    pub accepted_at_nanos: i64,

    /// Earliest Unix-epoch nanoseconds at which the compactor may
    /// physically delete. `None` = immediate. Non-zero soft-delete
    /// windows move this forward.
    #[serde(default)]
    pub hard_delete_after_nanos: Option<i64>,

    /// Processing state.
    pub state: TombstoneState,
}

impl ErasureTombstone {
    /// Is this tombstone eligible for physical erasure at `now_nanos`?
    /// Returns `true` only if `Pending` and past any soft-delete
    /// window. The compactor calls this on every pass.
    pub fn is_ripe(&self, now_nanos: i64) -> bool {
        if self.state != TombstoneState::Pending {
            return false;
        }
        match self.hard_delete_after_nanos {
            Some(t) => now_nanos >= t,
            None => true,
        }
    }

    /// Does this tombstone match the given subject? The compactor
    /// uses this inside its per-event decision on a segment scan.
    pub fn matches(&self, subject: &SubjectId) -> bool {
        self.selector.matches(subject)
    }

    /// Canonical selector form — for tombstone keys and audit logs.
    pub fn selector_canonical(&self) -> String {
        self.selector.to_canonical()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_parses_subject_form() {
        let sel = ErasureSelector::parse("tenant-a/user-1").unwrap();
        match sel {
            ErasureSelector::Subject(s) => assert_eq!(s.to_canonical(), "tenant-a/user-1"),
            _ => panic!("expected Subject"),
        }
    }

    #[test]
    fn selector_parses_wildcard_form() {
        let sel = ErasureSelector::parse("tenant-a/*").unwrap();
        assert!(sel.is_wildcard());
        assert_eq!(sel.to_canonical(), "tenant-a/*");
    }

    #[test]
    fn selector_rejects_missing_slash() {
        assert!(ErasureSelector::parse("no-slash").is_err());
    }

    #[test]
    fn selector_rejects_empty_namespace_in_wildcard() {
        assert!(ErasureSelector::parse("/*").is_err());
    }

    #[test]
    fn selector_rejects_disallowed_chars_in_wildcard_namespace() {
        assert!(ErasureSelector::parse("ns space/*").is_err());
        assert!(ErasureSelector::parse("ns!/*").is_err());
    }

    #[test]
    fn selector_matches_exact_subject() {
        let sel = ErasureSelector::parse("tenant-a/user-1").unwrap();
        let s1 = SubjectId::parse("tenant-a/user-1").unwrap();
        let s2 = SubjectId::parse("tenant-a/user-2").unwrap();
        assert!(sel.matches(&s1));
        assert!(!sel.matches(&s2));
    }

    #[test]
    fn selector_matches_namespace_wildcard() {
        let sel = ErasureSelector::parse("tenant-a/*").unwrap();
        let hit = SubjectId::parse("tenant-a/anything").unwrap();
        let miss = SubjectId::parse("tenant-b/anything").unwrap();
        assert!(sel.matches(&hit));
        assert!(!sel.matches(&miss));
    }

    #[test]
    fn request_into_tombstone_immediate() {
        let req = ErasureRequest {
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/user-1").unwrap(),
            reason: Some("ticket-42".to_string()),
            soft_delete: None,
        };
        let id = uuid::Uuid::now_v7();
        let t = req.into_tombstone(1_700_000_000_000_000_000, id);
        assert_eq!(t.id, id);
        assert_eq!(t.accepted_at_nanos, 1_700_000_000_000_000_000);
        assert_eq!(t.hard_delete_after_nanos, None);
        assert_eq!(t.state, TombstoneState::Pending);
        assert!(t.is_ripe(1_700_000_000_000_000_000));
    }

    #[test]
    fn request_into_tombstone_zero_soft_delete_is_immediate() {
        let req = ErasureRequest {
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/user-1").unwrap(),
            reason: None,
            soft_delete: Some(Duration::ZERO),
        };
        let id = uuid::Uuid::now_v7();
        let t = req.into_tombstone(0, id);
        assert_eq!(t.hard_delete_after_nanos, None);
    }

    #[test]
    fn request_into_tombstone_with_soft_delete() {
        let req = ErasureRequest {
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/user-1").unwrap(),
            reason: None,
            soft_delete: Some(Duration::from_secs(3600)),
        };
        let id = uuid::Uuid::now_v7();
        let t = req.into_tombstone(0, id);
        assert_eq!(t.hard_delete_after_nanos, Some(3_600_000_000_000_i64));
        assert!(!t.is_ripe(0));
        assert!(!t.is_ripe(3_600_000_000_000 - 1));
        assert!(t.is_ripe(3_600_000_000_000));
    }

    #[test]
    fn completed_tombstone_is_never_ripe() {
        let t = ErasureTombstone {
            id: uuid::Uuid::now_v7(),
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/user-1").unwrap(),
            reason: None,
            accepted_at_nanos: 0,
            hard_delete_after_nanos: None,
            state: TombstoneState::Completed,
        };
        assert!(!t.is_ripe(i64::MAX));
    }

    #[test]
    fn failed_tombstone_is_never_ripe() {
        let t = ErasureTombstone {
            id: uuid::Uuid::now_v7(),
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/user-1").unwrap(),
            reason: None,
            accepted_at_nanos: 0,
            hard_delete_after_nanos: None,
            state: TombstoneState::Failed,
        };
        assert!(!t.is_ripe(i64::MAX));
    }

    #[test]
    fn tombstone_bincode_roundtrip() {
        let req = ErasureRequest {
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/user-1").unwrap(),
            reason: Some("ticket-99".to_string()),
            soft_delete: Some(Duration::from_secs(120)),
        };
        let t = req.into_tombstone(1_000_000_000, uuid::Uuid::now_v7());
        let bytes = bincode::serialize(&t).expect("serialize");
        let back: ErasureTombstone = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back, t);
    }

    #[test]
    fn tombstone_json_roundtrip() {
        let req = ErasureRequest {
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/*").unwrap(),
            reason: Some("ticket-99".to_string()),
            soft_delete: Some(Duration::from_secs(120)),
        };
        let t = req.into_tombstone(1_000_000_000, uuid::Uuid::now_v7());
        let j = serde_json::to_string(&t).unwrap();
        let back: ErasureTombstone = serde_json::from_str(&j).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn tombstone_json_emits_null_for_none() {
        // Intentional — `skip_serializing_if` would corrupt bincode, so
        // we keep the field present with `null` on the wire.
        let req = ErasureRequest {
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/user-1").unwrap(),
            reason: None,
            soft_delete: None,
        };
        let t = req.into_tombstone(0, uuid::Uuid::now_v7());
        let j = serde_json::to_string(&t).unwrap();
        assert!(j.contains("\"reason\":null"));
        assert!(j.contains("\"hard_delete_after_nanos\":null"));
    }

    #[test]
    fn tombstone_matches_delegates_to_selector() {
        let t = ErasureRequest {
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/*").unwrap(),
            reason: None,
            soft_delete: None,
        }
        .into_tombstone(0, uuid::Uuid::now_v7());
        assert!(t.matches(&SubjectId::parse("tenant-a/x").unwrap()));
        assert!(!t.matches(&SubjectId::parse("tenant-b/x").unwrap()));
    }

    #[test]
    fn selector_canonical_roundtrip_exact() {
        let sel = ErasureSelector::parse("tenant-a/user-1").unwrap();
        assert_eq!(sel.to_canonical(), "tenant-a/user-1");
    }

    #[test]
    fn selector_canonical_roundtrip_wildcard() {
        let sel = ErasureSelector::parse("tenant-a/*").unwrap();
        assert_eq!(sel.to_canonical(), "tenant-a/*");
    }
}
