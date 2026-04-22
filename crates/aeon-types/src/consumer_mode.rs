//! `ConsumerMode` — per-source opt-in between Aeon-owned partition
//! ownership (manual assign) and broker-coordinated consumer groups.
//!
//! Applies to pull-source connectors whose upstream supports both
//! models (Kafka / Redpanda via `assign` vs `subscribe`; Redis Streams /
//! Valkey Streams via XREADGROUP single-consumer vs multi-consumer).
//!
//! Hard invariant: `ConsumerMode::Group` is **mutually exclusive** with
//! cluster-coordinated partition ownership. When a source advertises
//! broker-coordinated partitioning (see
//! `Source::broker_coordinated_partitions`) the pipeline start path
//! must refuse to attach a Raft-driven partition reassign watcher —
//! the broker rebalance protocol and Aeon's `partition_table` would
//! otherwise fight over the same resource.
//!
//! Aeon's EO-2 ledger remains the source of truth for delivery in
//! both modes — `broker_commit` is `false` by default and should stay
//! `false` unless the operator has an explicit reason to double-book
//! commits (it produces no durability benefit and can mask
//! inconsistencies).

/// How a source interacts with its upstream's partition-ownership model.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ConsumerMode {
    /// Manual partition assignment. Aeon owns the partition table; the
    /// connector issues `assign()` / reads a specific stream key. This is
    /// the default and is the only mode compatible with cluster-
    /// coordinated partition ownership (CL-6 transfer, Raft
    /// `partition_table`).
    #[default]
    Single,

    /// Broker-coordinated consumer group. The broker owns partition
    /// rebalance; multiple Aeon instances in the same `group_id` share
    /// the upstream load. Mutually exclusive with Raft-driven partition
    /// ownership — see module-level docs.
    Group {
        /// Consumer group identifier shared across all Aeon instances
        /// participating in the same group.
        group_id: String,
        /// If `true`, the broker's own auto-commit is enabled. The
        /// default is `false`: Aeon's EO-2 ledger drives commits via
        /// the native per-tier primitive (XACK, `commit_transaction`,
        /// etc.), and broker auto-commit would only double-book
        /// acknowledgements.
        broker_commit: bool,
    },
}

impl ConsumerMode {
    /// Returns `true` when the mode delegates partition ownership to
    /// the upstream broker's rebalance protocol.
    pub fn is_broker_coordinated(&self) -> bool {
        matches!(self, Self::Group { .. })
    }

    /// Group id when in `Group` mode, else `None`.
    pub fn group_id(&self) -> Option<&str> {
        match self {
            Self::Single => None,
            Self::Group { group_id, .. } => Some(group_id.as_str()),
        }
    }

    /// Whether broker auto-commit is enabled. `false` for `Single` and
    /// for any correctly-configured `Group` mode; overridable by the
    /// operator in narrow escape-hatch scenarios.
    pub fn broker_commit(&self) -> bool {
        match self {
            Self::Single => false,
            Self::Group { broker_commit, .. } => *broker_commit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_is_default() {
        assert_eq!(ConsumerMode::default(), ConsumerMode::Single);
        assert!(!ConsumerMode::default().is_broker_coordinated());
        assert!(ConsumerMode::default().group_id().is_none());
        assert!(!ConsumerMode::default().broker_commit());
    }

    #[test]
    fn group_is_broker_coordinated() {
        let m = ConsumerMode::Group {
            group_id: "aeon-ingest".to_string(),
            broker_commit: false,
        };
        assert!(m.is_broker_coordinated());
        assert_eq!(m.group_id(), Some("aeon-ingest"));
        assert!(!m.broker_commit());
    }

    #[test]
    fn group_respects_broker_commit_flag() {
        let m = ConsumerMode::Group {
            group_id: "x".to_string(),
            broker_commit: true,
        };
        assert!(m.broker_commit());
    }
}
