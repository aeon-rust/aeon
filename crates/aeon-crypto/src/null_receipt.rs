//! S6.4 — PoH null-receipt builder.
//!
//! When a compactor physically deletes event(s) in response to a
//! GDPR erasure tombstone, the Proof-of-History chain must remain
//! verifiable. The null-receipt is how that works.
//!
//! # Design contract
//!
//! The original PoH entry for the batch that contained the erased
//! event(s) is **not** modified — PoH entries are immutable by
//! construction (hash-chained and optionally Ed25519-signed). The
//! content of the erased event is physically gone from L2/L3.
//!
//! In its place, the compactor appends one additional PoH entry
//! whose Merkle tree root is computed from the canonical bytes of a
//! [`NullReceipt`] — a tamper-evident attestation that:
//!
//! - Subject(s) X were requested for erasure (via tombstone id).
//! - Event(s) Y₁..Yₙ (by UUIDv7) have been physically deleted.
//! - The original content hash of each erased event is preserved
//!   (non-reversible) so an auditor can confirm chain continuity
//!   but cannot recover the content.
//!
//! Downstream replay past an erased event is impossible by design:
//! the content bytes are gone, only the content hash remains.
//!
//! # What this module does NOT do
//!
//! - It does not talk to L2/L3. The compactor owns the delete path.
//! - It does not append to a `PohChain`. It returns the payload
//!   bytes; the compactor calls [`crate::poh::PohChain::append_batch`]
//!   with those bytes and the current timestamp.
//! - It does not sign. Signing is the PoH chain's responsibility via
//!   the existing `SigningKey` path.
//!
//! This separation keeps the crypto primitive independent of the
//! compactor's I/O concerns — pure data in, pure data out.

use aeon_types::SubjectId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::hash::{Hash512, sha512};

/// Domain-separation prefix baked into every null-receipt. Prevents a
/// future attacker from crafting a non-receipt payload whose bytes
/// happen to parse as a receipt. Stable across versions — changing it
/// would invalidate all existing on-chain receipts.
pub const NULL_RECEIPT_MAGIC: &[u8] = b"aeon-null-receipt-v1";

/// One event's proof-of-erasure. The content bytes are gone; this
/// structure is what remains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErasedEventRecord {
    /// UUIDv7 of the event that was deleted. An auditor can cross-
    /// reference this against the source's offset log (if still
    /// retained) to confirm an event with this id existed.
    pub event_id: Uuid,

    /// SHA-512 of the original event payload, computed **before**
    /// physical delete. Non-reversible — cannot be used to recover
    /// the payload, but lets an auditor verify that the deletion
    /// targeted a specific known-content event rather than an
    /// arbitrary substitute.
    pub original_content_hash: Hash512,
}

/// A full null-receipt. One per compaction pass per tombstone. The
/// receipt is serialized via bincode and handed to the PoH chain as
/// the payload of a new batch — the resulting entry becomes the
/// on-chain proof of erasure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NullReceipt {
    /// UUIDv7 of the [`aeon_types::ErasureTombstone`] that authorized
    /// this erasure. Links receipt → request → original caller.
    pub tombstone_id: Uuid,

    /// Canonical selector form (e.g. `"tenant-a/user-1"` or
    /// `"tenant-a/*"`). Stored verbatim — the tombstone is the
    /// primary audit record; this is a redundancy for log-only
    /// inspection of the PoH chain without cross-store joins.
    pub selector_canonical: String,

    /// Salted-hash of each subject affected. The raw [`SubjectId`] is
    /// PII-adjacent (GDPR Art. 4(5)) and **must not** be written in
    /// the clear to an append-only on-chain record. The pipeline
    /// operator owns the salt; the hash alone is useless without it.
    pub subject_hashes: Vec<Hash512>,

    /// One record per erased event.
    pub erased: Vec<ErasedEventRecord>,

    /// Unix-epoch nanoseconds when the compactor finalized the
    /// physical delete. Equal to the timestamp of the PoH entry
    /// that carries this receipt.
    pub finalized_at_nanos: i64,
}

impl NullReceipt {
    /// Canonical, domain-separated bytes used as the Merkle-tree
    /// payload for the PoH entry. Stable: changing the layout
    /// requires a new [`NULL_RECEIPT_MAGIC`] bump.
    ///
    /// Format: `MAGIC || bincode(self)`. The magic prevents collision
    /// between receipt bytes and any other payload an attacker might
    /// slip into a batch.
    pub fn to_chain_bytes(&self) -> Result<Vec<u8>, aeon_types::AeonError> {
        let encoded = bincode::serialize(self)
            .map_err(|e| aeon_types::AeonError::state(format!("null-receipt serialize: {e}")))?;
        let mut out = Vec::with_capacity(NULL_RECEIPT_MAGIC.len() + encoded.len());
        out.extend_from_slice(NULL_RECEIPT_MAGIC);
        out.extend_from_slice(&encoded);
        Ok(out)
    }

    /// Parse the inverse of [`Self::to_chain_bytes`]. Returns an error
    /// if the magic prefix is absent or the bincode body is malformed.
    pub fn from_chain_bytes(bytes: &[u8]) -> Result<Self, aeon_types::AeonError> {
        let body = bytes.strip_prefix(NULL_RECEIPT_MAGIC).ok_or_else(|| {
            aeon_types::AeonError::state("null-receipt: missing magic prefix".to_string())
        })?;
        bincode::deserialize(body)
            .map_err(|e| aeon_types::AeonError::state(format!("null-receipt deserialize: {e}")))
    }

    /// How many events this receipt accounts for.
    pub fn erased_count(&self) -> usize {
        self.erased.len()
    }
}

/// Compute the salted-hash of a subject id. The caller supplies the
/// pipeline salt (rotated on pipeline reset) so the hash is not
/// universal — two pipelines processing the same subject id produce
/// different hashes, preventing cross-pipeline correlation attacks.
///
/// Format: `SHA-512("aeon-subject-hash-v1" || salt || canonical)`.
/// The length prefix on each component prevents length-extension /
/// ambiguity between adjacent fields.
pub fn salted_subject_hash(salt: &[u8], subject: &SubjectId) -> Hash512 {
    const DOMAIN: &[u8] = b"aeon-subject-hash-v1";
    let canonical = subject.to_canonical();
    let canonical_bytes = canonical.as_bytes();

    let mut buf = Vec::with_capacity(DOMAIN.len() + 4 + salt.len() + 4 + canonical_bytes.len());
    buf.extend_from_slice(DOMAIN);
    buf.extend_from_slice(&(salt.len() as u32).to_le_bytes());
    buf.extend_from_slice(salt);
    buf.extend_from_slice(&(canonical_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(canonical_bytes);
    sha512(&buf)
}

/// Compute the content hash of an event's original payload — call
/// this **before** the physical delete so the bytes are still
/// present. Returned hash goes into [`ErasedEventRecord::original_content_hash`].
pub fn content_hash(payload: &[u8]) -> Hash512 {
    sha512(payload)
}

/// Build a null-receipt. The compactor walks each subject matched by
/// the tombstone, computes the salted-hash, and collects every
/// erased event. This function just assembles — all cryptographic
/// decisions (salt, content hash) are made at the call site so a
/// test can inject deterministic values.
pub struct NullReceiptBuilder {
    tombstone_id: Uuid,
    selector_canonical: String,
    subject_hashes: Vec<Hash512>,
    erased: Vec<ErasedEventRecord>,
}

impl NullReceiptBuilder {
    /// Start a new receipt for the given tombstone + selector.
    pub fn new(tombstone_id: Uuid, selector_canonical: impl Into<String>) -> Self {
        Self {
            tombstone_id,
            selector_canonical: selector_canonical.into(),
            subject_hashes: Vec::new(),
            erased: Vec::new(),
        }
    }

    /// Add one salted subject hash. The caller computed the salt from
    /// the pipeline's rotated salt material.
    pub fn add_subject_hash(mut self, hash: Hash512) -> Self {
        self.subject_hashes.push(hash);
        self
    }

    /// Add one erased-event record.
    pub fn add_erased_event(mut self, event_id: Uuid, original_content_hash: Hash512) -> Self {
        self.erased.push(ErasedEventRecord {
            event_id,
            original_content_hash,
        });
        self
    }

    /// Finalize with the compactor's wall-clock timestamp.
    pub fn finalize(self, finalized_at_nanos: i64) -> NullReceipt {
        NullReceipt {
            tombstone_id: self.tombstone_id,
            selector_canonical: self.selector_canonical,
            subject_hashes: self.subject_hashes,
            erased: self.erased,
            finalized_at_nanos,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_receipt() -> NullReceipt {
        let sub = SubjectId::parse("tenant-a/user-1").unwrap();
        NullReceiptBuilder::new(Uuid::now_v7(), "tenant-a/user-1")
            .add_subject_hash(salted_subject_hash(b"pipeline-salt", &sub))
            .add_erased_event(Uuid::now_v7(), content_hash(b"payload-1"))
            .add_erased_event(Uuid::now_v7(), content_hash(b"payload-2"))
            .finalize(1_700_000_000_000_000_000)
    }

    #[test]
    fn builder_collects_all_records() {
        let r = sample_receipt();
        assert_eq!(r.erased_count(), 2);
        assert_eq!(r.subject_hashes.len(), 1);
        assert_eq!(r.selector_canonical, "tenant-a/user-1");
    }

    #[test]
    fn chain_bytes_has_magic_prefix() {
        let r = sample_receipt();
        let bytes = r.to_chain_bytes().unwrap();
        assert!(bytes.starts_with(NULL_RECEIPT_MAGIC));
    }

    #[test]
    fn chain_bytes_roundtrip() {
        let r = sample_receipt();
        let bytes = r.to_chain_bytes().unwrap();
        let back = NullReceipt::from_chain_bytes(&bytes).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn from_chain_bytes_rejects_missing_magic() {
        let r = sample_receipt();
        let bytes = bincode::serialize(&r).unwrap();
        // bytes lack the magic prefix — must fail.
        let err = NullReceipt::from_chain_bytes(&bytes).unwrap_err();
        assert!(format!("{err}").contains("missing magic"));
    }

    #[test]
    fn from_chain_bytes_rejects_truncated() {
        let r = sample_receipt();
        let mut bytes = r.to_chain_bytes().unwrap();
        bytes.truncate(NULL_RECEIPT_MAGIC.len() + 3);
        assert!(NullReceipt::from_chain_bytes(&bytes).is_err());
    }

    #[test]
    fn salted_subject_hash_is_deterministic() {
        let sub = SubjectId::parse("tenant-a/user-1").unwrap();
        let h1 = salted_subject_hash(b"salt-a", &sub);
        let h2 = salted_subject_hash(b"salt-a", &sub);
        assert_eq!(h1, h2);
    }

    #[test]
    fn salted_subject_hash_differs_by_salt() {
        let sub = SubjectId::parse("tenant-a/user-1").unwrap();
        let h1 = salted_subject_hash(b"salt-a", &sub);
        let h2 = salted_subject_hash(b"salt-b", &sub);
        assert_ne!(h1, h2);
    }

    #[test]
    fn salted_subject_hash_differs_by_subject() {
        let s1 = SubjectId::parse("tenant-a/user-1").unwrap();
        let s2 = SubjectId::parse("tenant-a/user-2").unwrap();
        let h1 = salted_subject_hash(b"salt", &s1);
        let h2 = salted_subject_hash(b"salt", &s2);
        assert_ne!(h1, h2);
    }

    #[test]
    fn salted_subject_hash_differs_across_namespaces() {
        // Cross-tenant isolation: same id under different namespace
        // must hash differently even with identical salt.
        let s1 = SubjectId::parse("tenant-a/user-1").unwrap();
        let s2 = SubjectId::parse("tenant-b/user-1").unwrap();
        let h1 = salted_subject_hash(b"salt", &s1);
        let h2 = salted_subject_hash(b"salt", &s2);
        assert_ne!(h1, h2);
    }

    #[test]
    fn content_hash_differs_for_different_payloads() {
        assert_ne!(content_hash(b"a"), content_hash(b"b"));
    }

    #[test]
    fn content_hash_is_deterministic() {
        assert_eq!(content_hash(b"aeon"), content_hash(b"aeon"));
    }

    #[test]
    fn tamper_receipt_body_fails_roundtrip_equality() {
        let r = sample_receipt();
        let mut bytes = r.to_chain_bytes().unwrap();
        // Flip a bit in the bincode body (after the magic).
        let pos = NULL_RECEIPT_MAGIC.len() + 5;
        bytes[pos] ^= 0xff;
        // Either deserialize fails outright, or the parsed value
        // differs from the original — in either case the chain can
        // detect tampering by recomputing the PoH entry.
        if let Ok(back) = NullReceipt::from_chain_bytes(&bytes) {
            assert_ne!(back, r);
        }
    }

    #[test]
    fn receipt_serde_json_roundtrip() {
        let r = sample_receipt();
        let j = serde_json::to_string(&r).unwrap();
        let back: NullReceipt = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn wildcard_selector_roundtrips() {
        let r = NullReceiptBuilder::new(Uuid::now_v7(), "tenant-a/*").finalize(1_000);
        let bytes = r.to_chain_bytes().unwrap();
        let back = NullReceipt::from_chain_bytes(&bytes).unwrap();
        assert_eq!(back.selector_canonical, "tenant-a/*");
    }

    #[test]
    fn chain_bytes_is_deterministic() {
        let sub = SubjectId::parse("tenant-a/user-1").unwrap();
        let id = Uuid::now_v7();
        let evt = Uuid::now_v7();
        let payload_hash = content_hash(b"same");
        let r1 = NullReceiptBuilder::new(id, "tenant-a/user-1")
            .add_subject_hash(salted_subject_hash(b"salt", &sub))
            .add_erased_event(evt, payload_hash)
            .finalize(42);
        let r2 = NullReceiptBuilder::new(id, "tenant-a/user-1")
            .add_subject_hash(salted_subject_hash(b"salt", &sub))
            .add_erased_event(evt, payload_hash)
            .finalize(42);
        assert_eq!(r1.to_chain_bytes().unwrap(), r2.to_chain_bytes().unwrap());
    }
}
