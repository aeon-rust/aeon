//! Delivery configuration — flush strategy, checkpoint backend, and pipeline delivery settings.
//!
//! These types extend `PipelineConfig` with delivery-specific configuration.
//! `DeliveryStrategy`, `BatchFailurePolicy`, and `DeliverySemantics` live in `aeon-types`
//! (sinks need them); everything else is engine-internal.

use aeon_types::{BatchFailurePolicy, DeliverySemantics, DeliveryStrategy, DurabilityMode};
use std::path::PathBuf;
use std::time::Duration;

/// Controls when the pipeline flushes pending outputs to the sink.
///
/// In `UnorderedBatch` delivery strategy, outputs are enqueued without awaiting acks.
/// The flush strategy determines when acks are collected.
///
/// In `OrderedBatch` strategy, flush is less critical since acks are collected at
/// batch boundary, but the interval still drives checkpoint persistence timing.
#[derive(Debug, Clone)]
pub struct FlushStrategy {
    /// How often to trigger a checkpoint flush.
    /// Default: 1 second.
    pub interval: Duration,

    /// Maximum number of pending (unacked) outputs before forcing a flush.
    /// Acts as a backpressure signal — if pending outputs exceed this,
    /// the pipeline pauses source ingestion until flush completes.
    /// Default: 50,000.
    pub max_pending: usize,

    /// Enable adaptive flush: auto-reduce interval when ack failure rate spikes.
    /// Uses the existing `BatchTuner` hill-climbing algorithm applied to flush timing.
    /// Default: false.
    pub adaptive: bool,

    /// Minimum flush interval as a divisor of `interval` for adaptive tuning.
    /// E.g., 10 means min = interval / 10. Default: 10.
    pub adaptive_min_divisor: u32,

    /// Maximum flush interval as a multiplier of `interval` for adaptive tuning.
    /// E.g., 5 means max = interval * 5. Default: 5.
    pub adaptive_max_multiplier: u32,
}

impl Default for FlushStrategy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            max_pending: 50_000,
            adaptive: false,
            adaptive_min_divisor: 10,
            adaptive_max_multiplier: 5,
        }
    }
}

/// Backend for persisting checkpoint records.
///
/// Checkpoints record source offsets and delivery state so the pipeline
/// can resume from a known-good state after crashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckpointBackend {
    /// Append-only WAL file with CRC32 integrity.
    /// Survives process crash. ~100µs per checkpoint write.
    /// Best for: single-node, bare-metal, Docker deployments.
    #[default]
    Wal,

    /// Use the multi-tier state store (L1 DashMap → L2 mmap → L3 RocksDB).
    /// Durability depends on tier configuration.
    /// Best for: when L2/L3 tiers are already active.
    StateStore,

    /// No persistence. Checkpoints exist only in memory.
    /// Best for: dev/test, stateless processors where replay is acceptable.
    None,
}

/// Checkpoint persistence configuration.
#[derive(Debug, Clone)]
pub struct CheckpointConfig {
    /// Where to persist checkpoint records.
    pub backend: CheckpointBackend,

    /// How long to retain checkpoint history before rotation/cleanup.
    /// Default: 24 hours.
    pub retention: Duration,

    /// Directory for WAL checkpoint files.
    /// Default: None (uses system temp dir). Set to a durable path for production.
    pub dir: Option<std::path::PathBuf>,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            backend: CheckpointBackend::Wal,
            retention: Duration::from_secs(24 * 60 * 60), // 24 hours
            dir: None,
        }
    }
}

/// EO-2 L2 event-body store configuration.
///
/// Only consulted when `DeliveryConfig::durability != None` and the pipeline
/// contains at least one push or poll source. Pull-only pipelines ignore this
/// block entirely.
#[derive(Debug, Clone)]
pub struct L2BodyStoreConfig {
    /// Root directory under which per-pipeline per-partition segment files
    /// are written: `{root}/{pipeline}/p{partition:05}/{start_seq:020}.l2b`.
    /// `None` means use the system temp dir (dev-only default).
    pub root: Option<PathBuf>,

    /// Rollover threshold in bytes. Default: 256 MiB.
    pub segment_bytes: u64,
}

impl Default for L2BodyStoreConfig {
    fn default() -> Self {
        Self {
            root: None,
            segment_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Complete delivery configuration for a pipeline.
///
/// Controls delivery strategy, failure policy, delivery guarantees, flush timing,
/// and checkpoint persistence. Nested inside `PipelineConfig`.
#[derive(Debug, Clone, Default)]
pub struct DeliveryConfig {
    /// Delivery strategy: `OrderedBatch` (default), `PerEvent`, or `UnorderedBatch`.
    pub strategy: DeliveryStrategy,

    /// Delivery guarantee: `AtLeastOnce` (default) or `ExactlyOnce`.
    pub semantics: DeliverySemantics,

    /// What to do when events fail within a batch:
    /// `RetryFailed` (default), `FailBatch`, or `SkipToDlq`.
    pub failure_policy: BatchFailurePolicy,

    /// Flush strategy for batched modes (interval, max_pending, adaptive).
    pub flush: FlushStrategy,

    /// Checkpoint persistence backend and retention.
    pub checkpoint: CheckpointConfig,

    /// EO-2: event-body durability level. Default: `None` (backwards-compatible).
    pub durability: DurabilityMode,

    /// EO-2: L2 event-body store configuration. Only consulted when
    /// `durability != None` and a push/poll source is present.
    pub l2_body: L2BodyStoreConfig,

    /// Maximum number of retries per event before routing to DLQ.
    /// Only used with `RetryFailed` and `SkipToDlq` policies.
    /// Default: 3.
    pub max_retries: u32,

    /// Backoff duration between retries.
    /// Default: 100ms.
    pub retry_backoff: Duration,
}

impl DeliveryConfig {
    /// Create a config suitable for benchmarking (no checkpoints, no retries).
    pub fn benchmark(strategy: DeliveryStrategy) -> Self {
        Self {
            strategy,
            semantics: DeliverySemantics::AtLeastOnce,
            failure_policy: BatchFailurePolicy::FailBatch,
            flush: FlushStrategy {
                interval: Duration::from_millis(100),
                max_pending: 50_000,
                adaptive: false,
                ..Default::default()
            },
            checkpoint: CheckpointConfig {
                backend: CheckpointBackend::None,
                retention: Duration::ZERO,
                dir: None,
            },
            max_retries: 0,
            retry_backoff: Duration::ZERO,
            durability: DurabilityMode::None,
            l2_body: L2BodyStoreConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_config_defaults() {
        let config = DeliveryConfig::default();
        assert_eq!(config.strategy, DeliveryStrategy::OrderedBatch);
        assert_eq!(config.semantics, DeliverySemantics::AtLeastOnce);
        assert_eq!(config.failure_policy, BatchFailurePolicy::RetryFailed);
        assert_eq!(config.flush.interval, Duration::from_secs(1));
        assert_eq!(config.flush.max_pending, 50_000);
        assert!(!config.flush.adaptive);
        assert_eq!(config.checkpoint.backend, CheckpointBackend::Wal);
        assert_eq!(config.checkpoint.retention, Duration::from_secs(86_400));
        assert_eq!(config.max_retries, 0); // u32 default
        assert_eq!(config.retry_backoff, Duration::ZERO);
    }

    #[test]
    fn delivery_config_benchmark() {
        let config = DeliveryConfig::benchmark(DeliveryStrategy::UnorderedBatch);
        assert_eq!(config.strategy, DeliveryStrategy::UnorderedBatch);
        assert_eq!(config.checkpoint.backend, CheckpointBackend::None);
        assert_eq!(config.failure_policy, BatchFailurePolicy::FailBatch);
        assert_eq!(config.max_retries, 0);
    }

    #[test]
    fn flush_strategy_custom() {
        let flush = FlushStrategy {
            interval: Duration::from_millis(500),
            max_pending: 100_000,
            adaptive: true,
            adaptive_min_divisor: 20,
            adaptive_max_multiplier: 10,
        };
        assert_eq!(flush.interval, Duration::from_millis(500));
        assert_eq!(flush.max_pending, 100_000);
        assert!(flush.adaptive);
        assert_eq!(flush.adaptive_min_divisor, 20);
        assert_eq!(flush.adaptive_max_multiplier, 10);
    }

    #[test]
    fn checkpoint_backend_variants() {
        assert_eq!(CheckpointBackend::default(), CheckpointBackend::Wal);
        assert_ne!(CheckpointBackend::StateStore, CheckpointBackend::None);
        assert_ne!(CheckpointBackend::StateStore, CheckpointBackend::Wal);
    }

    #[test]
    fn delivery_config_ordered_batch_exactly_once() {
        let config = DeliveryConfig {
            strategy: DeliveryStrategy::OrderedBatch,
            semantics: DeliverySemantics::ExactlyOnce,
            failure_policy: BatchFailurePolicy::FailBatch,
            flush: FlushStrategy {
                interval: Duration::from_millis(200),
                max_pending: 25_000,
                adaptive: true,
                ..Default::default()
            },
            checkpoint: CheckpointConfig {
                backend: CheckpointBackend::StateStore,
                retention: Duration::from_secs(3600),
                dir: None,
            },
            max_retries: 5,
            retry_backoff: Duration::from_millis(200),
            durability: DurabilityMode::None,
            l2_body: L2BodyStoreConfig::default(),
        };
        assert_eq!(config.strategy, DeliveryStrategy::OrderedBatch);
        assert_eq!(config.semantics, DeliverySemantics::ExactlyOnce);
        assert_eq!(config.failure_policy, BatchFailurePolicy::FailBatch);
        assert!(config.flush.adaptive);
        assert_eq!(config.checkpoint.backend, CheckpointBackend::StateStore);
        assert_eq!(config.max_retries, 5);
    }

    #[test]
    fn delivery_config_unordered_skip_to_dlq() {
        let config = DeliveryConfig {
            strategy: DeliveryStrategy::UnorderedBatch,
            failure_policy: BatchFailurePolicy::SkipToDlq,
            ..Default::default()
        };
        assert_eq!(config.strategy, DeliveryStrategy::UnorderedBatch);
        assert_eq!(config.failure_policy, BatchFailurePolicy::SkipToDlq);
    }

    #[test]
    fn checkpoint_config_none_backend() {
        let config = CheckpointConfig {
            backend: CheckpointBackend::None,
            retention: Duration::ZERO,
            dir: None,
        };
        assert_eq!(config.backend, CheckpointBackend::None);
    }

    #[test]
    fn flush_strategy_default_values() {
        let flush = FlushStrategy::default();
        assert_eq!(flush.interval.as_millis(), 1000);
        assert_eq!(flush.max_pending, 50_000);
        assert!(!flush.adaptive);
    }
}
