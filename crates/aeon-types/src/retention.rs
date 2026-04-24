//! S5 — Configurable retention for L2 body segments and L3 ack records.
//!
//! Declares a pipeline's preference for keeping acked data around past
//! the point where the ack-driven GC would otherwise reclaim it. Two
//! knobs, one per tier, both inert by default so existing pipelines pay
//! zero cost and existing tests / benches remain unchanged:
//!
//! - `l2.body.hold_after_ack` — L2 segments whose `max_seq` is below
//!   the min-across-sinks ack watermark are eligible for reclaim. With
//!   a non-zero hold, `eo2_gc_sweep` stamps the first-seen-eligible
//!   `Instant` per segment and waits `hold_after_ack` before the
//!   `remove_file` call. Useful for a short debug replay window on
//!   push/poll pipelines where the upstream's own retention is not
//!   trustworthy (e.g., webhook → blackhole in a lab run).
//!
//! - `l3.ack.max_records` — `L3CheckpointStore` and `WalCheckpointStore`
//!   stop growing unbounded. After each `append`, records older than
//!   `next - max_records` are purged. `None` preserves the pre-S5
//!   "keep everything forever" behaviour.
//!
//! The manifest surface is deliberately minimal: two scalar fields,
//! both serde-defaulting to the inert value. Compliance regimes that
//! require a specific retention posture (e.g., "keep ack records ≥ 90
//! days") will consume these values in the S4.2 validator.

use serde::{Deserialize, Serialize};

/// Per-pipeline retention declaration. Nests under `DurabilityBlock`
/// on `PipelineManifest` — retention is meaningful only when
/// durability mode drives L2 writes / checkpoints in the first place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionBlock {
    #[serde(default, skip_serializing_if = "L2RetentionBlock::is_default")]
    pub l2_body: L2RetentionBlock,
    #[serde(default, skip_serializing_if = "L3RetentionBlock::is_default")]
    pub l3_ack: L3RetentionBlock,
}

impl RetentionBlock {
    /// `true` when the block is fully inert — neither tier has a
    /// declared retention override. Used by the manifest `skip_serializing_if`
    /// so default blocks stay out of serialised YAML/JSON.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// L2 body segment retention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct L2RetentionBlock {
    /// Minimum seconds an acked segment is kept on disk before
    /// `eo2_gc_sweep` actually deletes it. `None` (default) means the
    /// existing ack-driven GC removes the file as soon as the segment's
    /// `max_seq` falls below the min-across-sinks ack watermark.
    ///
    /// String at the YAML surface ("300s", "5m") so manifests read
    /// naturally; the engine's runtime knob is a `Duration`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_after_ack: Option<String>,
}

impl L2RetentionBlock {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// L3 ack / checkpoint record retention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct L3RetentionBlock {
    /// Cap on the number of historical checkpoint records kept in the
    /// checkpoint store. After each `append`, records whose id is
    /// below `next_id - max_records` are purged. `None` (default)
    /// keeps every record forever — the pre-S5 behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_records: Option<u32>,
}

impl L3RetentionBlock {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_fully_inert() {
        let b = RetentionBlock::default();
        assert!(b.is_default());
        assert!(b.l2_body.hold_after_ack.is_none());
        assert!(b.l3_ack.max_records.is_none());
    }

    #[test]
    fn default_block_serialises_compactly() {
        let b = RetentionBlock::default();
        let j = serde_json::to_string(&b).unwrap();
        // Nested defaults are skipped — the outer braces remain.
        assert_eq!(j, "{}");
    }

    #[test]
    fn populated_block_roundtrips() {
        let b = RetentionBlock {
            l2_body: L2RetentionBlock {
                hold_after_ack: Some("5m".into()),
            },
            l3_ack: L3RetentionBlock {
                max_records: Some(10_000),
            },
        };
        let j = serde_json::to_string(&b).unwrap();
        assert!(j.contains("hold_after_ack"));
        assert!(j.contains("max_records"));
        let back: RetentionBlock = serde_json::from_str(&j).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn missing_fields_parse_as_defaults() {
        let j = r#"{"l3_ack":{"max_records":500}}"#;
        let b: RetentionBlock = serde_json::from_str(j).unwrap();
        assert!(b.l2_body.is_default());
        assert_eq!(b.l3_ack.max_records, Some(500));
    }

    #[test]
    fn l2_partial_block_roundtrips() {
        let b = RetentionBlock {
            l2_body: L2RetentionBlock {
                hold_after_ack: Some("30s".into()),
            },
            l3_ack: L3RetentionBlock::default(),
        };
        let j = serde_json::to_string(&b).unwrap();
        // l3_ack is default, skipped.
        assert!(!j.contains("l3_ack"));
        let back: RetentionBlock = serde_json::from_str(&j).unwrap();
        assert_eq!(back, b);
    }
}
