//! S6 — GDPR subject-id convention on `Event.metadata`.
//!
//! The envelope is not changed. A pipeline that needs GDPR erasure
//! (i.e. declares `compliance.regime = gdpr` or `mixed`) attaches
//! one or more `aeon.subject_id` entries to each event's metadata.
//! The value follows a `namespace/id` convention — the namespace
//! isolates erasure requests across tenants so a single "forget
//! user-123" cannot accidentally wipe another tenant's data.
//!
//! # Why metadata rather than a first-class envelope field
//!
//! Most pipelines are not GDPR-regulated. Every envelope byte costs
//! hot-path cycles (false-sharing budget, clone cost, serialization
//! bandwidth). The metadata `SmallVec` already allocates space for 4
//! inline headers, so adding `aeon.subject_id` to it costs zero
//! additional bytes for the common case of zero subjects and a single
//! cache-line pointer for events that carry one. See S6.1.a in the
//! design discussion at `docs/aeon-dev-notes.txt`.
//!
//! # Multi-subject events
//!
//! Payment receipts, chat transcripts, shared documents — many
//! real-world events touch more than one data subject. The metadata
//! list may contain **repeated** `aeon.subject_id` entries; the
//! erasure path walks every match. See S6.1.b.
//!
//! # Format
//!
//! `namespace/id` — both components match `[A-Za-z0-9._:-]+`. The
//! slash is the canonical separator. Whitespace around either
//! component is rejected so values round-trip cleanly through JSON
//! and YAML. See S6.1.c.
//!
//! # Logging / metrics
//!
//! Raw subject-ids are pseudonymous PII under GDPR Art. 4(5). The S2
//! redaction layer (`aeon_types::redact`) denies them by default;
//! metric labels must use a salted hash, never the raw value. See
//! S6.1.i — this module does not log or format the raw id into any
//! tracing span.
//!
//! This module is a **schema + parser** only. The erasure store,
//! tombstone writer, and PoH null-receipt builder consume these
//! types from their own modules.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

use crate::error::AeonError;

/// Well-known metadata key for subject-id annotations. Any number of
/// entries with this key may appear on a single `Event`.
pub const METADATA_KEY_SUBJECT_ID: &str = "aeon.subject_id";

/// Maximum length of any single component (namespace or id). 128 is
/// well above realistic ids (UUIDs, ULIDs, email addresses) and
/// bounds the cost of every `contains_subject` scan.
pub const MAX_COMPONENT_LEN: usize = 128;

/// A parsed, validated subject identifier. Components are interned as
/// `Arc<str>` so a pipeline processing millions of events for the
/// same subject does not re-allocate the string on every event.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubjectId {
    pub namespace: Arc<str>,
    pub id: Arc<str>,
}

/// On-wire form. The `serde` crate's `rc` feature (which teaches it to
/// serialize `Arc<str>` directly) is not enabled workspace-wide, so we
/// round-trip through `String` manually.
#[derive(Serialize, Deserialize)]
struct SubjectIdWire {
    namespace: String,
    id: String,
}

impl Serialize for SubjectId {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        SubjectIdWire {
            namespace: self.namespace.as_ref().to_string(),
            id: self.id.as_ref().to_string(),
        }
        .serialize(ser)
    }
}

impl<'de> Deserialize<'de> for SubjectId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let w = SubjectIdWire::deserialize(de)?;
        validate_component("namespace", &w.namespace).map_err(serde::de::Error::custom)?;
        validate_component("id", &w.id).map_err(serde::de::Error::custom)?;
        Ok(Self {
            namespace: Arc::from(w.namespace),
            id: Arc::from(w.id),
        })
    }
}

impl SubjectId {
    /// Construct directly from already-validated components. Prefer
    /// `SubjectId::parse` for anything coming from the wire.
    pub fn new(namespace: impl Into<Arc<str>>, id: impl Into<Arc<str>>) -> Self {
        Self {
            namespace: namespace.into(),
            id: id.into(),
        }
    }

    /// Parse a `"namespace/id"` string. Returns `AeonError::config` on
    /// any rule violation — empty components, reserved characters,
    /// over-length, missing separator, whitespace.
    pub fn parse(s: &str) -> Result<Self, AeonError> {
        if s.len() > MAX_COMPONENT_LEN * 2 + 1 {
            return Err(AeonError::config(format!(
                "subject_id: value exceeds max length ({} bytes)",
                MAX_COMPONENT_LEN * 2 + 1
            )));
        }
        let (ns, id) = s.split_once('/').ok_or_else(|| {
            AeonError::config("subject_id: expected 'namespace/id' form, no '/' found".to_string())
        })?;
        validate_component("namespace", ns)?;
        validate_component("id", id)?;
        Ok(Self {
            namespace: Arc::from(ns),
            id: Arc::from(id),
        })
    }

    /// Canonical `namespace/id` string form. Callers that need to
    /// write the id to persistent storage (tombstone records, PoH
    /// null-receipts) use this — **never** `Debug` / `Display`.
    pub fn to_canonical(&self) -> String {
        format!("{}/{}", self.namespace, self.id)
    }

    /// Namespace-only match — used by the erasure API's tenant-wide
    /// wipe (`namespace/*` form). A wildcard request tombstones every
    /// subject under a namespace without needing to enumerate them.
    pub fn matches_namespace(&self, namespace: &str) -> bool {
        self.namespace.as_ref() == namespace
    }
}

/// **Note**: `Display` intentionally renders the canonical form. Do
/// not route this through a tracing span — subject-ids are PII; see
/// the module docstring. Callers that need the string for a log
/// message must hash it first.
impl fmt::Display for SubjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.id)
    }
}

/// Validate a bare namespace (no `/id` suffix) using the same rules
/// as [`SubjectId`] component parsing. Used by the erasure API's
/// `namespace/*` wildcard form, which has no id component to
/// validate alongside the namespace.
pub fn validate_namespace_for_wildcard(ns: &str) -> Result<(), AeonError> {
    validate_component("namespace", ns)
}

fn validate_component(name: &'static str, c: &str) -> Result<(), AeonError> {
    if c.is_empty() {
        return Err(AeonError::config(format!("subject_id: {name} is empty")));
    }
    if c.len() > MAX_COMPONENT_LEN {
        return Err(AeonError::config(format!(
            "subject_id: {name} exceeds {MAX_COMPONENT_LEN} bytes"
        )));
    }
    for b in c.bytes() {
        // Allow: [A-Za-z0-9._:-]. Slash is the separator; whitespace
        // is explicitly rejected. Other punctuation kept out to avoid
        // URL-encoding / shell-quoting surprises in the REST API.
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':');
        if !ok {
            return Err(AeonError::config(format!(
                "subject_id: {name} contains disallowed byte 0x{b:02x}"
            )));
        }
    }
    Ok(())
}

// ── Metadata helpers ─────────────────────────────────────────────────────

/// Collect every `aeon.subject_id` attached to the event's metadata.
/// Tolerates repeated keys (multi-subject events); skips entries that
/// fail to parse and logs a metric-only counter — a malformed subject
/// id should not drop the event but must not reach the erasure path.
///
/// Callers that need to distinguish "missing" from "malformed" should
/// use [`try_collect_subject_ids`] instead.
pub fn collect_subject_ids<I>(metadata: I) -> Vec<SubjectId>
where
    I: IntoIterator,
    I::Item: MetaPair,
{
    let mut out = Vec::new();
    for pair in metadata {
        if pair.key() == METADATA_KEY_SUBJECT_ID {
            if let Ok(sid) = SubjectId::parse(pair.value()) {
                out.push(sid);
            }
        }
    }
    out
}

/// Strict variant — bubbles up the first parse error. Used by the
/// erasure API's "accept subject request" path where a malformed id
/// must be reported back to the caller rather than silently dropped.
pub fn try_collect_subject_ids<I>(metadata: I) -> Result<Vec<SubjectId>, AeonError>
where
    I: IntoIterator,
    I::Item: MetaPair,
{
    let mut out = Vec::new();
    for pair in metadata {
        if pair.key() == METADATA_KEY_SUBJECT_ID {
            out.push(SubjectId::parse(pair.value())?);
        }
    }
    Ok(out)
}

/// Adapter trait so the helpers work with both `Event.metadata`
/// (`SmallVec<[(Arc<str>, Arc<str>); 4]>`) and owned `(String, String)`
/// tuples used in the REST API without an allocation detour.
pub trait MetaPair {
    fn key(&self) -> &str;
    fn value(&self) -> &str;
}

impl MetaPair for (Arc<str>, Arc<str>) {
    fn key(&self) -> &str {
        self.0.as_ref()
    }
    fn value(&self) -> &str {
        self.1.as_ref()
    }
}

impl MetaPair for &(Arc<str>, Arc<str>) {
    fn key(&self) -> &str {
        self.0.as_ref()
    }
    fn value(&self) -> &str {
        self.1.as_ref()
    }
}

impl MetaPair for (String, String) {
    fn key(&self) -> &str {
        &self.0
    }
    fn value(&self) -> &str {
        &self.1
    }
}

impl MetaPair for &(String, String) {
    fn key(&self) -> &str {
        &self.0
    }
    fn value(&self) -> &str {
        &self.1
    }
}

impl MetaPair for (&str, &str) {
    fn key(&self) -> &str {
        self.0
    }
    fn value(&self) -> &str {
        self.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_canonical_form() {
        let s = SubjectId::parse("tenant-acme/user-123").unwrap();
        assert_eq!(s.namespace.as_ref(), "tenant-acme");
        assert_eq!(s.id.as_ref(), "user-123");
        assert_eq!(s.to_canonical(), "tenant-acme/user-123");
    }

    #[test]
    fn parse_rejects_missing_separator() {
        assert!(SubjectId::parse("no-slash-here").is_err());
    }

    #[test]
    fn parse_rejects_empty_components() {
        assert!(SubjectId::parse("/id").is_err());
        assert!(SubjectId::parse("ns/").is_err());
        assert!(SubjectId::parse("/").is_err());
    }

    #[test]
    fn parse_rejects_whitespace() {
        assert!(SubjectId::parse(" ns/id").is_err());
        assert!(SubjectId::parse("ns/id ").is_err());
        assert!(SubjectId::parse("ns /id").is_err());
    }

    #[test]
    fn parse_rejects_unsafe_chars() {
        // Common shell/URL metacharacters must be rejected so the
        // canonical form round-trips safely through REST paths and
        // log lines (when redacted as a unit rather than escaped).
        for bad in &["ns/id!", "ns;id/oops", "ns/id?x=1", "ns/id&y", "ns%2Fid/x"] {
            assert!(SubjectId::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn parse_accepts_allowed_alphabet() {
        // Alphanumeric, dot, underscore, dash, colon.
        let s = SubjectId::parse("tenant.acme_eu-2:v1/user.id_42-x:v2").unwrap();
        assert_eq!(s.namespace.as_ref(), "tenant.acme_eu-2:v1");
        assert_eq!(s.id.as_ref(), "user.id_42-x:v2");
    }

    #[test]
    fn parse_rejects_over_length_components() {
        let long = "a".repeat(MAX_COMPONENT_LEN + 1);
        assert!(SubjectId::parse(&format!("{long}/id")).is_err());
        assert!(SubjectId::parse(&format!("ns/{long}")).is_err());
    }

    #[test]
    fn parse_accepts_boundary_length_components() {
        let max = "a".repeat(MAX_COMPONENT_LEN);
        let s = SubjectId::parse(&format!("{max}/{max}")).unwrap();
        assert_eq!(s.namespace.len(), MAX_COMPONENT_LEN);
        assert_eq!(s.id.len(), MAX_COMPONENT_LEN);
    }

    #[test]
    fn namespace_wildcard_match() {
        let s = SubjectId::parse("tenant-acme/user-123").unwrap();
        assert!(s.matches_namespace("tenant-acme"));
        assert!(!s.matches_namespace("tenant-other"));
        assert!(!s.matches_namespace(""));
    }

    #[test]
    fn serde_roundtrip_json() {
        let s = SubjectId::new("tenant-acme", "user-123");
        let j = serde_json::to_string(&s).unwrap();
        let back: SubjectId = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }

    // ── metadata helpers ──

    fn md(pairs: &[(&str, &str)]) -> Vec<(Arc<str>, Arc<str>)> {
        pairs
            .iter()
            .map(|(k, v)| (Arc::from(*k), Arc::from(*v)))
            .collect()
    }

    #[test]
    fn collect_empty_metadata() {
        let m: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        assert!(collect_subject_ids(m).is_empty());
    }

    #[test]
    fn collect_single_subject() {
        let m = md(&[
            ("content-type", "application/json"),
            ("aeon.subject_id", "tenant-a/user-1"),
        ]);
        let subs = collect_subject_ids(m);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].to_canonical(), "tenant-a/user-1");
    }

    #[test]
    fn collect_multiple_repeated_keys() {
        // Per 6.1.b — repeated keys represent multi-subject events.
        let m = md(&[
            ("aeon.subject_id", "tenant-a/user-1"),
            ("aeon.subject_id", "tenant-a/user-2"),
            ("aeon.subject_id", "tenant-b/shared-doc-7"),
        ]);
        let subs = collect_subject_ids(m);
        assert_eq!(subs.len(), 3);
        assert!(subs.iter().any(|s| s.id.as_ref() == "shared-doc-7"));
    }

    #[test]
    fn collect_tolerates_malformed_ids() {
        let m = md(&[
            ("aeon.subject_id", "tenant-a/user-1"),
            ("aeon.subject_id", "malformed no slash"),
        ]);
        let subs = collect_subject_ids(m);
        // Silently drops the malformed entry.
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn try_collect_propagates_parse_error() {
        let m = md(&[
            ("aeon.subject_id", "tenant-a/user-1"),
            ("aeon.subject_id", "malformed no slash"),
        ]);
        assert!(try_collect_subject_ids(m).is_err());
    }

    #[test]
    fn collect_ignores_unrelated_keys() {
        let m = md(&[
            ("x-user-id", "tenant-a/user-1"),
            ("aeon.Subject_Id", "wrong-case/nope"),
        ]);
        let subs = collect_subject_ids(m);
        assert!(subs.is_empty());
    }
}
