//! Delivery types — shared across engine and connectors.
//!
//! These types control how sinks deliver outputs and how the pipeline
//! manages acknowledgment flow. Defined in `aeon-types` because sink
//! implementations need access to `DeliveryStrategy` to decide their
//! internal write/flush behavior.

use uuid::Uuid;

use crate::AeonError;

/// Controls how events are delivered to the downstream by the sink connector.
///
/// This is a **per-pipeline** configuration driven by downstream requirements.
/// The pipeline orchestrator and sink implementations both use this to decide
/// delivery behavior. Aeon defines the contract; each connector fulfills it
/// using its downstream's native capabilities (transactions, idempotent
/// producers, batch APIs, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeliveryStrategy {
    /// Send events one at a time, await confirmation before sending next.
    ///
    /// Strictest guarantee. Lowest throughput.
    /// Use for: regulatory audit trails requiring per-event confirmation.
    ///
    /// Measured: ~1.8K events/sec (Redpanda, Docker in-network, Run 6b).
    PerEvent,

    /// Send batch in sequence, collect acks at batch boundary.
    ///
    /// Ordering preserved within and across batches. `write_batch()` sends
    /// all events in order, then awaits all ack futures at end of batch.
    /// Each downstream uses its native ordering mechanism:
    /// - Kafka/Redpanda: idempotent producer (`enable.idempotence=true`)
    /// - PostgreSQL/MySQL: single transaction (`BEGIN` → batch INSERT → `COMMIT`)
    /// - Redis/Valkey: `MULTI`/`EXEC` (atomic batch)
    /// - NATS JetStream: sequential publish, batch ack await
    /// - File: sequential write, single fsync at batch end
    ///
    /// Use for: bank transactions, CDC, event sourcing, task queues.
    /// Expected: ~30-40K events/sec (Redpanda), ~20-50K/sec (PostgreSQL).
    #[default]
    OrderedBatch,

    /// Send batch concurrently, collect acks at flush intervals.
    ///
    /// No ordering guarantee. `write_batch()` enqueues all events and returns
    /// immediately (non-blocking). `flush()` collects all pending delivery acks.
    /// Downstream sorts by UUIDv7 sequence when ordering is needed.
    ///
    /// Use for: analytics, bulk loads, search indexing, monitoring, data warehouses.
    /// Measured: ~41.6K events/sec (Redpanda, Docker in-network, Run 6b).
    UnorderedBatch,
}

impl DeliveryStrategy {
    /// Returns `true` if this strategy blocks on delivery acks in `write_batch()`.
    ///
    /// `PerEvent` blocks per-event. `OrderedBatch` blocks at batch boundary.
    /// `UnorderedBatch` does not block (acks collected at flush).
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            DeliveryStrategy::PerEvent | DeliveryStrategy::OrderedBatch
        )
    }

    /// Returns `true` if this strategy preserves event ordering.
    pub fn preserves_order(&self) -> bool {
        matches!(
            self,
            DeliveryStrategy::PerEvent | DeliveryStrategy::OrderedBatch
        )
    }

    /// Returns `true` if this strategy sends events one at a time.
    pub fn is_per_event(&self) -> bool {
        matches!(self, DeliveryStrategy::PerEvent)
    }
}

/// What to do when event delivery fails within a batch.
///
/// Configured per-pipeline. Connector-specific behavior varies —
/// e.g., PostgreSQL can ROLLBACK + replay, Kafka retries individual
/// messages via idempotent producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatchFailurePolicy {
    /// Retry the failed event(s), continue batch from failure point.
    ///
    /// Connector decides how: Kafka retries the message via idempotent producer,
    /// PostgreSQL uses SAVEPOINT + retry the statement, Redis retries the command.
    /// Respects `max_retries` from `DeliveryConfig`.
    #[default]
    RetryFailed,

    /// Fail the entire batch. Checkpoint ensures replay from last committed offset.
    ///
    /// Clean semantics for transactional downstreams (ROLLBACK entire transaction).
    /// Use for: PostgreSQL/MySQL where partial commits are unacceptable.
    FailBatch,

    /// Skip the failed event, record in DLQ, continue batch.
    ///
    /// For downstreams where partial delivery is acceptable.
    /// Use for: analytics, search indexing, monitoring.
    SkipToDlq,
}

/// Result of a `write_batch()` call — connects sinks to the delivery ledger.
///
/// Every sink connector returns `BatchResult`. The pipeline engine reads it
/// and applies the configured `BatchFailurePolicy`.
#[derive(Debug, Default)]
pub struct BatchResult {
    /// Event IDs that were successfully delivered and acked by the downstream.
    pub delivered: Vec<Uuid>,

    /// Event IDs that were enqueued but not yet acked (UnorderedBatch mode).
    /// These will be resolved at the next `flush()` call.
    pub pending: Vec<Uuid>,

    /// Event IDs that failed delivery, with the corresponding error.
    pub failed: Vec<(Uuid, AeonError)>,
}

impl BatchResult {
    /// Create an empty result.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Create a result where all events were delivered.
    pub fn all_delivered(ids: Vec<Uuid>) -> Self {
        Self {
            delivered: ids,
            pending: Vec::new(),
            failed: Vec::new(),
        }
    }

    /// Create a result where all events are pending (enqueued, not yet acked).
    pub fn all_pending(ids: Vec<Uuid>) -> Self {
        Self {
            delivered: Vec::new(),
            pending: ids,
            failed: Vec::new(),
        }
    }

    /// Total number of events in this result.
    pub fn total(&self) -> usize {
        self.delivered.len() + self.pending.len() + self.failed.len()
    }

    /// Returns `true` if any events failed.
    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }

    /// Returns `true` if all events were delivered (none pending, none failed).
    pub fn all_succeeded(&self) -> bool {
        self.pending.is_empty() && self.failed.is_empty()
    }
}

/// Delivery guarantee semantics for the pipeline.
///
/// Orthogonal to `DeliveryStrategy` — you can have any combination:
/// PerEvent + AtLeastOnce, OrderedBatch + ExactlyOnce, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeliverySemantics {
    /// Checkpoint + source offset replay on failure.
    ///
    /// On crash recovery, events between the last checkpoint and the crash
    /// are replayed from the source. Rare duplicates possible (only during
    /// checkpoint-interval failures). Downstream deduplicates via UUIDv7
    /// or `IdempotentSink`.
    #[default]
    AtLeastOnce,

    /// Kafka transactions / IdempotentSink / UUIDv7 dedup.
    ///
    /// Transactional commit ensures no duplicates. Requires sink support
    /// (e.g., Kafka transactions, IdempotentSink trait). Higher latency
    /// due to transaction coordination overhead.
    ExactlyOnce,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_strategy_default_is_ordered_batch() {
        assert_eq!(DeliveryStrategy::default(), DeliveryStrategy::OrderedBatch);
    }

    #[test]
    fn delivery_strategy_blocking_semantics() {
        assert!(DeliveryStrategy::PerEvent.is_blocking());
        assert!(DeliveryStrategy::OrderedBatch.is_blocking());
        assert!(!DeliveryStrategy::UnorderedBatch.is_blocking());
    }

    #[test]
    fn delivery_strategy_ordering_semantics() {
        assert!(DeliveryStrategy::PerEvent.preserves_order());
        assert!(DeliveryStrategy::OrderedBatch.preserves_order());
        assert!(!DeliveryStrategy::UnorderedBatch.preserves_order());
    }

    #[test]
    fn delivery_strategy_per_event() {
        assert!(DeliveryStrategy::PerEvent.is_per_event());
        assert!(!DeliveryStrategy::OrderedBatch.is_per_event());
        assert!(!DeliveryStrategy::UnorderedBatch.is_per_event());
    }

    #[test]
    fn batch_failure_policy_default_is_retry_failed() {
        assert_eq!(
            BatchFailurePolicy::default(),
            BatchFailurePolicy::RetryFailed
        );
    }

    #[test]
    fn batch_result_all_delivered() {
        let ids = vec![Uuid::now_v7(), Uuid::now_v7()];
        let result = BatchResult::all_delivered(ids.clone());
        assert_eq!(result.delivered.len(), 2);
        assert!(result.pending.is_empty());
        assert!(result.failed.is_empty());
        assert!(result.all_succeeded());
        assert!(!result.has_failures());
        assert_eq!(result.total(), 2);
    }

    #[test]
    fn batch_result_all_pending() {
        let ids = vec![Uuid::now_v7()];
        let result = BatchResult::all_pending(ids);
        assert!(!result.all_succeeded());
        assert!(!result.has_failures());
        assert_eq!(result.total(), 1);
    }

    #[test]
    fn batch_result_with_failures() {
        let result = BatchResult {
            delivered: vec![Uuid::now_v7()],
            pending: vec![],
            failed: vec![(Uuid::now_v7(), AeonError::connection("timeout"))],
        };
        assert!(result.has_failures());
        assert!(!result.all_succeeded());
        assert_eq!(result.total(), 2);
    }

    #[test]
    fn batch_result_empty() {
        let result = BatchResult::empty();
        assert_eq!(result.total(), 0);
        assert!(result.all_succeeded());
    }

    #[test]
    fn delivery_semantics_default_is_at_least_once() {
        assert_eq!(DeliverySemantics::default(), DeliverySemantics::AtLeastOnce);
    }

    #[test]
    fn delivery_strategy_debug_format() {
        assert_eq!(format!("{:?}", DeliveryStrategy::PerEvent), "PerEvent");
        assert_eq!(
            format!("{:?}", DeliveryStrategy::OrderedBatch),
            "OrderedBatch"
        );
        assert_eq!(
            format!("{:?}", DeliveryStrategy::UnorderedBatch),
            "UnorderedBatch"
        );
    }

    #[test]
    fn delivery_semantics_debug_format() {
        assert_eq!(
            format!("{:?}", DeliverySemantics::AtLeastOnce),
            "AtLeastOnce"
        );
        assert_eq!(
            format!("{:?}", DeliverySemantics::ExactlyOnce),
            "ExactlyOnce"
        );
    }

    #[test]
    fn batch_failure_policy_debug_format() {
        assert_eq!(
            format!("{:?}", BatchFailurePolicy::RetryFailed),
            "RetryFailed"
        );
        assert_eq!(format!("{:?}", BatchFailurePolicy::FailBatch), "FailBatch");
        assert_eq!(format!("{:?}", BatchFailurePolicy::SkipToDlq), "SkipToDlq");
    }

    #[test]
    fn delivery_strategy_clone_and_copy() {
        let strategy = DeliveryStrategy::UnorderedBatch;
        let cloned = strategy;
        assert_eq!(strategy, cloned);
    }

    #[test]
    fn delivery_semantics_clone_and_copy() {
        let sem = DeliverySemantics::ExactlyOnce;
        let cloned = sem;
        assert_eq!(sem, cloned);
    }

    #[test]
    fn batch_failure_policy_clone_and_copy() {
        let policy = BatchFailurePolicy::SkipToDlq;
        let cloned = policy;
        assert_eq!(policy, cloned);
    }
}
