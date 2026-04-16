//! EO-2 durability modes — how aggressively the pipeline persists event
//! bodies to Aeon-owned storage (L2) and flushes checkpoint state (L3/WAL).
//!
//! Orthogonal to `DeliveryStrategy` (which governs sink batching shape).
//! See `docs/EO-2-DURABILITY-DESIGN.md` §7.
//!
//! | Mode             | L2 body write | L3 checkpoint cadence   | Push-ack boundary |
//! |------------------|---------------|-------------------------|-------------------|
//! | `None`           | skipped       | periodic, best-effort   | on ingest         |
//! | `UnorderedBatch` | per event     | per flush interval      | after L2 write    |
//! | `OrderedBatch`   | per event     | per batch boundary      | after L2 write    |
//! | `PerEvent`       | per event     | per event (fsync)       | after L2 + fsync  |
//!
//! Pull sources never write L2 regardless of mode — the broker is the buffer
//! of record. Push and poll sources honour the mode strictly.

use serde::{Deserialize, Serialize};

/// Event-body durability level for a pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityMode {
    /// No L2 event-body persistence. Push/poll sources ack on ingest.
    /// At-most-once under crash between ingest and sink ack.
    #[default]
    None,

    /// L2 write per event, L3 checkpoint flush on the delivery flush interval.
    /// No fsync per event — relies on periodic fsync by `CheckpointPersist`.
    UnorderedBatch,

    /// L2 write per event, L3 checkpoint flush at each batch boundary
    /// (end of a `write_batch()` → ack cycle).
    OrderedBatch,

    /// L2 write + fsync per event, L3 checkpoint per event.
    /// Highest durability, lowest throughput ceiling.
    PerEvent,
}

impl DurabilityMode {
    /// Whether this mode requires the pipeline to write event bodies to L2
    /// before acking upstream push sources. Pull sources skip L2 regardless.
    pub fn requires_l2_body_store(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether each L2 append must be followed by fsync before the source
    /// is acked.
    pub fn fsync_per_event(self) -> bool {
        matches!(self, Self::PerEvent)
    }

    /// Short log/metric label.
    pub fn tag(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::UnorderedBatch => "unordered_batch",
            Self::OrderedBatch => "ordered_batch",
            Self::PerEvent => "per_event",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_none() {
        assert_eq!(DurabilityMode::default(), DurabilityMode::None);
    }

    #[test]
    fn requires_l2_matches_design() {
        assert!(!DurabilityMode::None.requires_l2_body_store());
        assert!(DurabilityMode::UnorderedBatch.requires_l2_body_store());
        assert!(DurabilityMode::OrderedBatch.requires_l2_body_store());
        assert!(DurabilityMode::PerEvent.requires_l2_body_store());
    }

    #[test]
    fn only_per_event_fsyncs_per_event() {
        assert!(!DurabilityMode::None.fsync_per_event());
        assert!(!DurabilityMode::UnorderedBatch.fsync_per_event());
        assert!(!DurabilityMode::OrderedBatch.fsync_per_event());
        assert!(DurabilityMode::PerEvent.fsync_per_event());
    }

    #[test]
    fn json_roundtrip() {
        for m in [
            DurabilityMode::None,
            DurabilityMode::UnorderedBatch,
            DurabilityMode::OrderedBatch,
            DurabilityMode::PerEvent,
        ] {
            let j = serde_json::to_string(&m).unwrap();
            let back: DurabilityMode = serde_json::from_str(&j).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn tags_distinct() {
        let all = [
            DurabilityMode::None,
            DurabilityMode::UnorderedBatch,
            DurabilityMode::OrderedBatch,
            DurabilityMode::PerEvent,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a.tag(), b.tag());
                }
            }
        }
    }
}
