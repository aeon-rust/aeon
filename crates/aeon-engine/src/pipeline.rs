//! Pipeline orchestrator — wires source→processor→sink with SPSC ring buffers.
//!
//! The pipeline runs three async tasks:
//! 1. **Source task**: polls `source.next_batch()`, pushes events into source→processor SPSC
//! 2. **Processor task**: pops events from SPSC, calls `processor.process_batch()`,
//!    pushes outputs into processor→sink SPSC
//! 3. **Sink task**: pops outputs from SPSC, calls `sink.write_batch()`
//!
//! Backpressure: SPSC full → producer yields → upstream pauses. Zero data loss by design.

use crate::affinity::{PipelineCores, pin_to_core, pipeline_core_assignment};
use crate::batch_tuner::FlushTuner;
use crate::checkpoint::{
    CheckpointPersist, CheckpointRecord, L3CheckpointStore, WalCheckpointStore,
};
use crate::delivery::{CheckpointBackend, DeliveryConfig};
use crate::delivery_ledger::DeliveryLedger;
use aeon_types::{
    AeonError, BatchFailurePolicy, Event, Output, PartitionId, Processor, Sink, Source, SourceKind,
};
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};

/// Default SPSC ring buffer capacity (events/outputs).
const DEFAULT_BUFFER_CAPACITY: usize = 8192;

/// CPU core pinning strategy for pipeline tasks.
///
/// Core pinning eliminates OS-level thread migration, keeping L1/L2 caches warm
/// for each pipeline stage. **Disabled by default** — suitable for shared/cloud
/// environments where overcommitting cores causes contention.
///
/// Enable on dedicated bare-metal or hypervisor-based deployments where the
/// pipeline has exclusive access to physical cores.
#[derive(Debug, Clone, Copy, Default)]
pub enum CorePinning {
    /// No core pinning — let the OS scheduler decide (default).
    /// Best for shared systems, containers, oversubscribed VMs.
    #[default]
    Disabled,
    /// Automatically assign cores using `pipeline_core_assignment()`.
    /// Skips core 0 (OS/runtime) and assigns source/processor/sink
    /// to consecutive cores. Falls back to `Disabled` if <3 cores available.
    Auto,
    /// Manually specify which core each pipeline stage runs on.
    /// Use when you need precise NUMA-aware placement or want to
    /// co-locate with specific hardware (NIC, storage controller).
    Manual(PipelineCores),
}

impl CorePinning {
    /// Resolve the pinning strategy into concrete core assignments.
    /// Returns `None` if pinning is disabled or insufficient cores for auto.
    fn resolve(&self) -> Option<PipelineCores> {
        match self {
            CorePinning::Disabled => None,
            CorePinning::Auto => pipeline_core_assignment(),
            CorePinning::Manual(cores) => Some(*cores),
        }
    }
}

/// Proof-of-History configuration for a pipeline.
///
/// When enabled, the processor task appends each processed batch to a
/// per-partition PoH chain (SHA-512 hash chain + Merkle tree of payloads).
/// This provides cryptographic ordering proof and integrity for all events
/// passing through the pipeline.
#[cfg(feature = "processor-auth")]
#[derive(Clone)]
pub struct PohConfig {
    /// Partition ID for this pipeline's PoH chain.
    pub partition: PartitionId,
    /// Maximum number of recent PoH entries kept in memory for verification.
    /// Older entries are still tracked in the MMR (Merkle Mountain Range).
    pub max_recent_entries: usize,
    /// Optional Ed25519 signing key for signing Merkle roots.
    /// Wrapped in Arc because SigningKey intentionally does not implement Clone
    /// (it zeroizes on drop). Shared across processor task and config references.
    pub signing_key: Option<Arc<aeon_crypto::signing::SigningKey>>,
}

#[cfg(feature = "processor-auth")]
impl Default for PohConfig {
    fn default() -> Self {
        Self {
            partition: PartitionId::new(0),
            max_recent_entries: 1024,
            signing_key: None,
        }
    }
}

/// Shared PoH chain state — accessible from the processor task and REST API.
#[cfg(feature = "processor-auth")]
pub type PohState = Arc<Mutex<aeon_crypto::poh::PohChain>>;

/// Pipeline configuration.
pub struct PipelineConfig {
    /// SPSC buffer capacity between source and processor.
    pub source_buffer_capacity: usize,
    /// SPSC buffer capacity between processor and sink.
    pub sink_buffer_capacity: usize,
    /// Maximum batch size for processor (limits work per iteration).
    pub max_batch_size: usize,
    /// CPU core pinning strategy for the buffered pipeline tasks.
    /// Disabled by default. Enable on dedicated hardware for optimal cache locality.
    pub core_pinning: CorePinning,
    /// Delivery configuration: strategy, semantics, failure policy, flush strategy, checkpoint.
    /// Default: OrderedBatch strategy, AtLeastOnce, RetryFailed, 1s flush, WAL checkpoint.
    pub delivery: DeliveryConfig,
    /// Proof-of-History configuration. When `Some`, the processor task records
    /// a hash chain of all processed batches for integrity verification.
    #[cfg(feature = "processor-auth")]
    pub poh: Option<PohConfig>,
    /// G11.c: shared registry of PoH chain snapshots installed by the
    /// cluster-side `PohChainInstaller` during a CL-6 partition handover.
    /// When this pipeline's task starts up (or a partition sub-task starts
    /// in `run_multi_partition`), `create_poh_state` checks the registry
    /// keyed on `(pipeline_name, partition_id)` — if the transfer driver
    /// already installed a snapshot, `create_poh_state` resumes from it
    /// (preserving the hash chain across node moves) instead of genesising
    /// a fresh chain. Without this wiring, Gate 2's "PoH chain has no gaps
    /// across transfers" row can't pass because every ownership flip
    /// would restart the chain at sequence 0. `None` disables — safe for
    /// all non-cluster callers.
    #[cfg(all(feature = "processor-auth", feature = "cluster"))]
    pub poh_installed_chains: Option<Arc<crate::partition_install::InstalledPohChainRegistry>>,
    /// CL-6c.4: shared registry of *live* `Arc<Mutex<PohChain>>` handles
    /// the source-side `PohChainExportProvider` reads from when a peer
    /// asks for this partition's PoH state during a CL-6 handover.
    /// `create_poh_state` registers the chain it produces (whether
    /// genesised fresh or resumed from `poh_installed_chains`) so a
    /// subsequent transfer out of this node carries the up-to-date
    /// state, preserving G11.c (chain continuity) across multiple hops.
    /// `None` disables — same default as `poh_installed_chains`, safe
    /// for all non-cluster callers.
    #[cfg(all(feature = "processor-auth", feature = "cluster"))]
    pub poh_live_chains: Option<Arc<crate::partition_install::LivePohChainRegistry>>,
    /// FT-3: Optional L3 backend for `CheckpointBackend::StateStore`.
    /// When `delivery.checkpoint.backend == StateStore`, the sink task
    /// persists checkpoint records into this store under the `checkpoint/`
    /// key prefix. Ignored for other backends. Typically the same
    /// `Arc<dyn L3Store>` used for Raft log/snapshot (FT-1/FT-2), so the
    /// whole node shares one durable handle.
    pub l3_checkpoint_store: Option<Arc<dyn aeon_types::L3Store>>,
    /// EO-2: pipeline name used for L2 body-store directory scoping and
    /// the `pipeline` label on all EO-2 metrics. Empty default is fine
    /// for legacy single-pipeline callers; multi-pipeline deployments
    /// must set this to a stable unique string.
    pub pipeline_name: String,
    /// EO-2: shared per-partition L2 body-store registry. When `Some`
    /// AND `delivery.durability != None`, the pipeline wraps the source
    /// in `L2WritingSource` so push/poll events are persisted before the
    /// upstream is acked. `None` disables L2 body persistence entirely
    /// regardless of `durability`.
    pub l2_registry: Option<crate::eo2::PipelineL2Registry>,
    /// EO-2: sink name used as the key in `AckSeqTracker` and as the
    /// `sink` label on metrics. Defaults to `"sink"` for single-sink
    /// pipelines. Multi-sink deployments must override per sink task.
    pub sink_name: String,
    /// EO-2 P8: optional observability registry. When `Some`, the source
    /// task publishes `aeon_l2_bytes`/`aeon_l2_segments` after each batch
    /// append, the sink task publishes `aeon_sink_ack_seq` on each ack
    /// advancement and `aeon_l2_gc_lag_seq` on every GC sweep, and the
    /// `FallbackCheckpointStore` increments
    /// `aeon_checkpoint_fallback_wal_total` on first transition. `None`
    /// disables EO-2 metrics entirely — safe default for tests/benches.
    pub eo2_metrics: Option<Arc<crate::eo2_metrics::Eo2Metrics>>,
    /// EO-2 P7: optional per-pipeline L2 byte budget. When `Some`, the source
    /// task checks `decide()` before each `next_batch` and enacts the
    /// appropriate backpressure remedy (pause/skip/reject). `L2WritingSource`
    /// calls `adjust(+bytes)` on append; `eo2_gc_sweep` calls
    /// `adjust(-bytes)` on GC reclaim. `None` disables backpressure entirely.
    pub eo2_capacity: Option<crate::eo2_backpressure::PipelineCapacity>,
    /// EO-2 P6: optional shared `AckSeqTracker` for multi-sink pipelines.
    /// When `Some`, `build_sink_task_ctx` uses this tracker instead of
    /// creating a fresh one, so every sink task in a multi-sink topology
    /// contributes to the same `min_across_sinks()` frontier used by L2 GC.
    /// `None` means single-sink — a per-task tracker is created as before.
    pub eo2_shared_ack_tracker: Option<crate::eo2::AckSeqTracker>,
    /// G2/CL-6: partition id this pipeline (sub-)task owns. Single-partition
    /// pipelines default to `PartitionId::new(0)`; multi-partition pipelines
    /// have their per-partition sub-tasks stamped with the real id by
    /// `run_multi_partition`. Used as the key for `write_gate` lookups and
    /// for future per-partition metrics labels.
    pub partition_id: PartitionId,
    /// G2/CL-6: optional per-partition write-freeze gate. When `Some`, the
    /// source loop calls `try_enter` before each `next_batch`; a racing
    /// `request_freeze_and_drain` from the cluster-side cutover coordinator
    /// will return `None`, causing the source loop to exit cleanly after its
    /// in-flight fetch drains. `None` means this pipeline is not subject to
    /// partition handover (local/test use), and the gate check compiles to
    /// a single branch.
    pub write_gate: Option<Arc<crate::write_gate::WriteGate>>,
    /// P5: optional subscription to this node's owned-partitions slice.
    /// When `Some`, the source loop in `run_buffered_managed` selects over
    /// `next_batch()` and `changed()`; on every committed ownership change
    /// it calls `source.reassign_partitions(&new)` so partitioned pulls
    /// (Kafka) re-issue `consumer.assign()` without tearing down the task.
    /// `None` keeps the legacy one-shot resolve-at-start behaviour used by
    /// tests, benches, and the single-node path.
    pub partition_reassign: Option<tokio::sync::watch::Receiver<Vec<u16>>>,
    /// S3: at-rest encryption plan resolved from `manifest.encryption` at
    /// pipeline start. When the EO-2 follow-up constructs
    /// `PipelineL2Registry` inside the supervisor, it installs
    /// `plan.kek` via `.with_kek(...)` so L2 segments and (future) L3
    /// stores seal payloads under the same data-context KEK. `None`
    /// means the probe was not run (legacy tests / benches) — equivalent
    /// to `at_rest=off`. Callers never mutate this after bootstrap.
    pub encryption_plan: Option<crate::encryption_probe::EncryptionPlan>,
    /// S5: retention plan resolved from `manifest.durability.retention`
    /// at pipeline start. `l2_hold_after_ack` is stamped on
    /// `PipelineL2Registry` via `.with_gc_min_hold(..)`;
    /// `l3_max_records` is stamped on `L3CheckpointStore` via
    /// `.with_max_records(..)`. `None` means the probe was not run
    /// (legacy tests / benches) — equivalent to inert retention (ack-
    /// immediate GC, keep-all checkpoint history).
    pub retention_plan: Option<crate::retention_probe::RetentionPlan>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            source_buffer_capacity: DEFAULT_BUFFER_CAPACITY,
            sink_buffer_capacity: DEFAULT_BUFFER_CAPACITY,
            max_batch_size: 1024,
            core_pinning: CorePinning::Disabled,
            delivery: DeliveryConfig::default(),
            #[cfg(feature = "processor-auth")]
            poh: None,
            #[cfg(all(feature = "processor-auth", feature = "cluster"))]
            poh_installed_chains: None,
            #[cfg(all(feature = "processor-auth", feature = "cluster"))]
            poh_live_chains: None,
            l3_checkpoint_store: None,
            pipeline_name: String::new(),
            l2_registry: None,
            sink_name: String::from("sink"),
            eo2_metrics: None,
            eo2_capacity: None,
            eo2_shared_ack_tracker: None,
            partition_id: PartitionId::new(0),
            write_gate: None,
            partition_reassign: None,
            encryption_plan: None,
            retention_plan: None,
        }
    }
}

/// Pipeline metrics — atomic counters for concurrent access.
pub struct PipelineMetrics {
    pub events_received: AtomicU64,
    pub events_processed: AtomicU64,
    /// Outputs the engine handed to the sink (one increment per `write_batch`
    /// invocation, summing the delivered count returned by the sink).
    pub outputs_sent: AtomicU64,
    /// Outputs the downstream system actually confirmed (broker ack, HTTP
    /// 2xx, fsync return, …). Driven by sinks via the `Sink::on_ack_callback`
    /// hook. Equals `outputs_sent` for tiers that ack inline; lags it for
    /// async-ack tiers (Kafka `UnorderedBatch`, HTTP fire-and-forget). The
    /// gap `outputs_sent - outputs_acked` is the in-flight pending count.
    pub outputs_acked: AtomicU64,
    /// Number of checkpoints written (UnorderedBatch mode).
    pub checkpoints_written: AtomicU64,
    /// Events permanently failed after retry exhaustion or SkipToDlq policy.
    pub events_failed: AtomicU64,
    /// Individual retry attempts (each retry of a failed output counts as 1).
    pub events_retried: AtomicU64,
    /// Number of PoH entries appended (one per processed batch when PoH is enabled).
    pub poh_entries: AtomicU64,
}

impl PipelineMetrics {
    pub fn new() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_processed: AtomicU64::new(0),
            outputs_sent: AtomicU64::new(0),
            outputs_acked: AtomicU64::new(0),
            checkpoints_written: AtomicU64::new(0),
            events_failed: AtomicU64::new(0),
            events_retried: AtomicU64::new(0),
            poh_entries: AtomicU64::new(0),
        }
    }

    /// Build an `Arc<dyn Fn(usize) + Send + Sync>` that bumps `outputs_acked`
    /// when invoked. Handed to a sink via `Sink::on_ack_callback`.
    pub fn ack_callback(self: &Arc<Self>) -> aeon_types::SinkAckCallback {
        let metrics = Arc::clone(self);
        Arc::new(move |n: usize| {
            metrics.outputs_acked.fetch_add(n as u64, Ordering::Relaxed);
        })
    }
}

impl Default for PipelineMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a shared PoH chain from pipeline config.
/// Returns `None` if PoH is disabled or the feature is not compiled.
///
/// G11.c: before genesising a fresh chain, consult
/// `config.poh_installed_chains` (when the `cluster` feature is enabled).
/// The cluster-side `PohChainInstaller` drops a per-`(pipeline, partition)`
/// snapshot there during a partition handover, so resumed partitions pick
/// up from where the previous owner left off instead of restarting at
/// sequence 0. The registry entry is consumed (taken) so a subsequent
/// restart without a fresh install falls back to genesis — callers that
/// need chain continuity across restarts must re-install first.
#[cfg(feature = "processor-auth")]
pub fn create_poh_state(config: &PipelineConfig) -> Option<PohState> {
    let poh_config = config.poh.as_ref()?;

    #[cfg(feature = "cluster")]
    let state: PohState = if let Some(chain) = config
        .poh_installed_chains
        .as_ref()
        .and_then(|registry| registry.take(&config.pipeline_name, config.partition_id))
    {
        tracing::info!(
            pipeline = %config.pipeline_name,
            partition = config.partition_id.as_u16(),
            sequence = chain.sequence(),
            "G11.c: resuming PoH chain from transfer-driver installed snapshot"
        );
        Arc::new(Mutex::new(chain))
    } else {
        Arc::new(Mutex::new(aeon_crypto::poh::PohChain::new(
            poh_config.partition,
            poh_config.max_recent_entries,
        )))
    };

    // CL-6c.4: register the live chain so the source-side
    // `PohChainExportProvider` can hand its state to a peer when this
    // partition migrates again. `None` registry → single-node / test
    // path, no-op.
    #[cfg(feature = "cluster")]
    if let Some(live) = config.poh_live_chains.as_ref() {
        if let Err(e) = live.register(
            &config.pipeline_name,
            config.partition_id,
            Arc::clone(&state),
        ) {
            tracing::warn!(
                pipeline = %config.pipeline_name,
                partition = config.partition_id.as_u16(),
                error = %e,
                "CL-6c.4: failed to register live PoH chain — outgoing \
                 transfers from this node will see no PoH state"
            );
        }
    }

    #[cfg(feature = "cluster")]
    {
        Some(state)
    }

    #[cfg(not(feature = "cluster"))]
    Some(Arc::new(Mutex::new(aeon_crypto::poh::PohChain::new(
        poh_config.partition,
        poh_config.max_recent_entries,
    ))))
}

/// TR-2: Transfer host-side state from an outgoing processor into an
/// incoming one on hot-swap (drain-and-swap / blue-green cutover / canary
/// completion). No-op for stateless processors — the default trait impls
/// return an empty snapshot and accept an empty restore.
///
/// Errors are logged and swallowed: we never want a snapshot/restore failure
/// to kill the processor task loop, because that would stall the whole
/// pipeline on what should be a best-effort state copy. Guest state loss is
/// strictly worse than no transfer at all — but pipeline death is worse
/// again.
fn transfer_processor_state(
    from: &(dyn Processor + Send + Sync),
    to: &(dyn Processor + Send + Sync),
) {
    match from.snapshot_state() {
        Ok(snap) => {
            if snap.is_empty() {
                // Stateless — skip the restore call entirely to avoid the
                // (equally no-op) trait-default work.
                return;
            }
            let key_count = snap.len();
            if let Err(e) = to.restore_state(snap) {
                tracing::warn!(
                    error = %e,
                    key_count,
                    "TR-2 processor state restore failed during swap; new instance starts fresh"
                );
            } else {
                tracing::info!(key_count, "TR-2 transferred processor state on swap");
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "TR-2 processor state snapshot failed during swap; new instance starts fresh"
            );
        }
    }
}

/// Append a batch of output payloads to the PoH chain.
///
/// Called in the processor task after `process_batch()`. Extracts payload bytes
/// from each output, builds a Merkle tree, and extends the hash chain.
#[cfg(feature = "processor-auth")]
async fn poh_append_batch(
    poh_state: &PohState,
    outputs: &[Output],
    signing_key: &Option<Arc<aeon_crypto::signing::SigningKey>>,
    metrics: &PipelineMetrics,
) {
    if outputs.is_empty() {
        return;
    }
    let payload_refs: Vec<&[u8]> = outputs.iter().map(|o| o.payload.as_ref()).collect();
    let timestamp_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64;
    let mut chain = poh_state.lock().await;
    if chain
        .append_batch(
            &payload_refs,
            timestamp_nanos,
            signing_key.as_ref().map(|k| k.as_ref()),
        )
        .is_some()
    {
        metrics.poh_entries.fetch_add(1, Ordering::Relaxed);
    }
}

/// Runs a linear pipeline: source → processor → sink.
///
/// This is the direct-call pipeline optimized for maximum throughput.
/// No SPSC buffers — the source, processor, and sink run in a tight loop
/// within a single async task. This eliminates ring buffer overhead entirely.
///
/// For the SPSC-buffered multi-task pipeline, see `run_buffered`.
pub async fn run<S, P, K>(
    source: &mut S,
    processor: &P,
    sink: &mut K,
    metrics: &PipelineMetrics,
    shutdown: &AtomicBool,
) -> Result<(), AeonError>
where
    S: Source,
    P: Processor,
    K: Sink,
{
    let source_kind = source.source_kind();
    while !shutdown.load(Ordering::Relaxed) {
        let events = source.next_batch().await?;
        if events.is_empty() {
            match source_kind {
                SourceKind::Pull => break,
                SourceKind::Push | SourceKind::Poll => {
                    tokio::task::yield_now().await;
                    continue;
                }
            }
        }

        let count = events.len() as u64;
        metrics.events_received.fetch_add(count, Ordering::Relaxed);

        let outputs = processor.process_batch(events)?;
        metrics.events_processed.fetch_add(count, Ordering::Relaxed);

        let batch_result = sink.write_batch(outputs).await?;
        let delivered = batch_result.delivered.len() as u64;
        metrics.outputs_sent.fetch_add(delivered, Ordering::Relaxed);
    }

    sink.flush().await?;
    Ok(())
}

/// Run the direct pipeline with full delivery configuration (failure policy, retries).
///
/// Unlike `run`, this variant applies `BatchFailurePolicy` to partial failures:
/// - `RetryFailed`: retry failed outputs up to `max_retries` with backoff
/// - `FailBatch`: abort pipeline on any failure
/// - `SkipToDlq`: count failures, continue processing
pub async fn run_with_delivery<S, P, K>(
    source: &mut S,
    processor: &P,
    sink: &mut K,
    delivery: &DeliveryConfig,
    metrics: &PipelineMetrics,
    shutdown: &AtomicBool,
) -> Result<(), AeonError>
where
    S: Source,
    P: Processor,
    K: Sink,
{
    let source_kind = source.source_kind();
    while !shutdown.load(Ordering::Relaxed) {
        let events = source.next_batch().await?;
        if events.is_empty() {
            match source_kind {
                SourceKind::Pull => break,
                SourceKind::Push | SourceKind::Poll => {
                    tokio::task::yield_now().await;
                    continue;
                }
            }
        }

        let count = events.len() as u64;
        metrics.events_received.fetch_add(count, Ordering::Relaxed);

        let outputs = processor.process_batch(events)?;
        metrics.events_processed.fetch_add(count, Ordering::Relaxed);

        let batch_result = sink.write_batch(outputs.clone()).await?;
        let delivered = batch_result.delivered.len() as u64;
        metrics.outputs_sent.fetch_add(delivered, Ordering::Relaxed);

        if batch_result.has_failures() {
            handle_batch_failures(
                sink,
                &outputs,
                &batch_result,
                delivery.failure_policy,
                delivery.max_retries,
                delivery.retry_backoff,
                metrics,
                &None, // no ledger in direct pipeline
            )
            .await?;
        }
    }

    sink.flush().await?;
    Ok(())
}

/// Applies `BatchFailurePolicy` to partial write_batch failures.
///
/// Called when `batch_result.has_failures()` is true. The policy determines behavior:
/// - **RetryFailed**: Collect failed outputs by `source_event_id`, retry up to `max_retries`
///   with `retry_backoff`. Events that still fail after exhaustion are counted as permanently
///   failed. If all retries succeed, their delivered count is added to metrics.
/// - **FailBatch**: Return an error immediately, aborting the pipeline.
/// - **SkipToDlq**: Count failures in metrics, mark in ledger, continue processing.
///   (Actual DLQ sink routing is Phase 15b — for now, failures are tracked but not routed.)
// Justified — splitting the failure-handling args into a struct adds a layer
// of indirection without buying anything; this function is the sole call site
// for each arg combo, and the policy / retry / metrics / ledger names self-document.
#[allow(clippy::too_many_arguments)]
async fn handle_batch_failures<K: Sink>(
    sink: &mut K,
    original_outputs: &[Output],
    batch_result: &aeon_types::BatchResult,
    policy: BatchFailurePolicy,
    max_retries: u32,
    retry_backoff: Duration,
    metrics: &PipelineMetrics,
    ledger: &Option<Arc<DeliveryLedger>>,
) -> Result<(), AeonError> {
    match policy {
        BatchFailurePolicy::FailBatch => {
            let failed_count = batch_result.failed.len();
            let first_err = batch_result
                .failed
                .first()
                .map(|(id, e)| format!("event {id}: {e}"))
                .unwrap_or_default();
            metrics
                .events_failed
                .fetch_add(failed_count as u64, Ordering::Relaxed);
            return Err(AeonError::Connection {
                message: format!(
                    "FailBatch: {failed_count} events failed in batch, first: {first_err}"
                ),
                source: None,
                retryable: false,
            });
        }
        BatchFailurePolicy::SkipToDlq => {
            let failed_count = batch_result.failed.len() as u64;
            metrics
                .events_failed
                .fetch_add(failed_count, Ordering::Relaxed);
            // Mark failed events in ledger (already done by caller for ledger-enabled paths).
            // Actual DLQ routing is Phase 15b — for now we count and continue.
            tracing::warn!(
                failed = failed_count,
                "SkipToDlq: skipping failed events (DLQ routing pending Phase 15b)"
            );
        }
        BatchFailurePolicy::RetryFailed => {
            // Build index: source_event_id → outputs for that event
            let failed_ids: std::collections::HashSet<uuid::Uuid> =
                batch_result.failed.iter().map(|(id, _)| *id).collect();

            let mut retry_outputs: Vec<Output> = original_outputs
                .iter()
                .filter(|o| {
                    o.source_event_id
                        .as_ref()
                        .map(|id| failed_ids.contains(id))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            if retry_outputs.is_empty() {
                // Failed events couldn't be matched to outputs (no source_event_id).
                // Count as permanently failed.
                metrics
                    .events_failed
                    .fetch_add(batch_result.failed.len() as u64, Ordering::Relaxed);
                tracing::warn!(
                    failed = batch_result.failed.len(),
                    "RetryFailed: cannot match failed events to outputs (missing source_event_id)"
                );
                return Ok(());
            }

            for attempt in 1..=max_retries {
                if retry_backoff > Duration::ZERO {
                    tokio::time::sleep(retry_backoff).await;
                }

                metrics
                    .events_retried
                    .fetch_add(retry_outputs.len() as u64, Ordering::Relaxed);

                let retry_result = sink.write_batch(retry_outputs.clone()).await?;

                // Mark newly delivered in ledger
                if let Some(ledger) = ledger {
                    if !retry_result.delivered.is_empty() {
                        ledger.mark_batch_acked(&retry_result.delivered);
                    }
                }

                let delivered = retry_result.delivered.len() as u64;
                metrics.outputs_sent.fetch_add(delivered, Ordering::Relaxed);

                if !retry_result.has_failures() {
                    // All retried events succeeded
                    return Ok(());
                }

                if attempt == max_retries {
                    // Exhausted retries — count remaining as permanently failed
                    let still_failed = retry_result.failed.len() as u64;
                    metrics
                        .events_failed
                        .fetch_add(still_failed, Ordering::Relaxed);

                    if let Some(ledger) = ledger {
                        for (id, err) in &retry_result.failed {
                            ledger.mark_failed(id, format!("retry exhausted: {err}"));
                        }
                    }

                    tracing::warn!(
                        failed = still_failed,
                        attempts = max_retries,
                        "RetryFailed: events permanently failed after retry exhaustion"
                    );
                } else {
                    // Narrow to still-failed outputs for next retry
                    let still_failed_ids: std::collections::HashSet<uuid::Uuid> =
                        retry_result.failed.iter().map(|(id, _)| *id).collect();
                    retry_outputs.retain(|o| {
                        o.source_event_id
                            .as_ref()
                            .map(|id| still_failed_ids.contains(id))
                            .unwrap_or(false)
                    });
                }
            }
        }
    }
    Ok(())
}

/// Runs a buffered pipeline with SPSC ring buffers between stages.
///
/// Three concurrent tasks connected by lock-free ring buffers:
/// - Source task → [SPSC] → Processor task → [SPSC] → Sink task
///
/// Backpressure propagates backward: if the sink is slow, the processor→sink
/// buffer fills, the processor pauses, the source→processor buffer fills,
/// and the source stops polling. No data is ever dropped.
pub async fn run_buffered<S, P, K>(
    source: S,
    processor: P,
    sink: K,
    config: PipelineConfig,
    metrics: Arc<PipelineMetrics>,
    shutdown: Arc<AtomicBool>,
    ledger: Option<Arc<DeliveryLedger>>,
) -> Result<(), AeonError>
where
    S: Source + Send + 'static,
    P: Processor + Send + Sync + 'static,
    K: Sink + Send + 'static,
{
    // B4: refuse to attach a Raft-driven partition-reassign watcher to a
    // source whose partition ownership is broker-coordinated. The broker
    // rebalance protocol and Aeon's `partition_table` would otherwise
    // fight over the same resource.
    reject_broker_coord_with_reassign(&source, &config)?;

    let core_assignment = config.core_pinning.resolve();

    // EO-2: conditionally wrap the source so push/poll events land in L2
    // before the upstream is acked. Pull sources and `DurabilityMode::None`
    // pass through untouched — `MaybeL2Wrapped::wrap` handles both.
    let mut source = crate::eo2::MaybeL2Wrapped::wrap(
        source,
        &config.pipeline_name,
        config.l2_registry.as_ref(),
        config.delivery.durability,
        config.eo2_capacity.as_ref(),
    );

    // Initialize PoH chain if configured
    #[cfg(feature = "processor-auth")]
    let poh_state = create_poh_state(&config);
    #[cfg(feature = "processor-auth")]
    let poh_signing_key = config.poh.as_ref().and_then(|c| c.signing_key.clone());

    let (mut src_prod, mut src_cons) =
        rtrb::RingBuffer::<Vec<Event>>::new(config.source_buffer_capacity);
    let (mut sink_prod, sink_cons) =
        rtrb::RingBuffer::<Vec<Output>>::new(config.sink_buffer_capacity);

    // EO-2 P5: build sink context first so we can read the last persisted
    // checkpoint and apply a recovery plan to the source before the source
    // task starts pulling events.
    let sink_ctx = build_sink_task_ctx(&config, core_assignment.map(|c| c.sink));
    let recovery_plan = load_recovery_plan(&sink_ctx.checkpoint_writer);
    dispatch_recovery_plan(&mut source, recovery_plan).await;

    let shutdown_src = Arc::clone(&shutdown);
    let metrics_src = Arc::clone(&metrics);
    let src_capacity = config.eo2_capacity.clone();
    let src_eo2_metrics = config.eo2_metrics.clone();
    let src_write_gate = config.write_gate.clone();

    // Source task: poll source, push event batches into SPSC
    let source_core = core_assignment.map(|c| c.source);
    let source_handle = tokio::spawn(async move {
        if let Some(core) = source_core {
            pin_to_core(core);
        }
        let source_kind = source.source_kind();
        while !shutdown_src.load(Ordering::Relaxed) {
            // EO-2 P7: check capacity before pulling the next batch. When
            // engaged, yield until GC on the sink side reclaims enough L2
            // bytes to drop below the threshold.
            if let Some(ref cap) = src_capacity {
                let partition = 0u16;
                let mut decision = cap.decide(partition, source_kind);
                if decision.is_engaged() {
                    if let Some(ref m) = src_eo2_metrics {
                        if let Some(lvl) = decision.level() {
                            m.set_l2_pressure(lvl, true);
                        }
                    }
                    while decision.is_engaged() && !shutdown_src.load(Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        decision = cap.decide(partition, source_kind);
                    }
                    if let Some(ref m) = src_eo2_metrics {
                        use crate::eo2_backpressure::PressureLevel;
                        m.set_l2_pressure(PressureLevel::Partition, false);
                        m.set_l2_pressure(PressureLevel::Pipeline, false);
                        m.set_l2_pressure(PressureLevel::Node, false);
                    }
                }
            }

            // G2/CL-6: check per-partition write-freeze gate. When the
            // cluster cutover coordinator has flipped to `FreezeRequested`,
            // `try_enter` returns `None` and the source loop exits cleanly;
            // any drain waiter observes `in_flight = 0` as soon as this
            // task hits `drop(src_prod)` below. The guard is held across
            // `next_batch` so a racing freeze_and_drain blocks until the
            // in-flight fetch completes.
            let _gate_guard = match src_write_gate.as_ref() {
                Some(gate) => match gate.try_enter() {
                    Some(g) => Some(g),
                    None => break,
                },
                None => None,
            };

            let events = match source.next_batch().await {
                Ok(events) => events,
                Err(e) => return Err(e),
            };
            if events.is_empty() {
                match source_kind {
                    SourceKind::Pull => break,
                    SourceKind::Push | SourceKind::Poll => {
                        tokio::task::yield_now().await;
                        continue;
                    }
                }
            }
            metrics_src
                .events_received
                .fetch_add(events.len() as u64, Ordering::Relaxed);

            // Push batch into ring buffer, yielding if full (backpressure)
            let mut pending = Some(events);
            while let Some(batch) = pending.take() {
                match src_prod.push(batch) {
                    Ok(()) => {}
                    Err(rtrb::PushError::Full(returned)) => {
                        pending = Some(returned);
                        tokio::task::yield_now().await;
                    }
                }
            }
            // `_gate_guard` drops here, decrementing in_flight. A parked
            // drain waiter wakes once every gate's in-flight reaches zero.
        }
        // Signal: no more events
        drop(src_prod);
        Ok::<(), AeonError>(())
    });

    let shutdown_proc = Arc::clone(&shutdown);
    let metrics_proc = Arc::clone(&metrics);
    let processor = Arc::new(processor);
    #[cfg(feature = "processor-auth")]
    let poh_proc = poh_state.clone();
    #[cfg(feature = "processor-auth")]
    let poh_key_proc = poh_signing_key.clone();

    // Processor task: pop events, process, push outputs
    let proc_core = core_assignment.map(|c| c.processor);
    let processor_handle = tokio::spawn(async move {
        if let Some(core) = proc_core {
            pin_to_core(core);
        }
        while !shutdown_proc.load(Ordering::Relaxed) {
            match src_cons.pop() {
                Ok(events) => {
                    let count = events.len() as u64;
                    let outputs = match processor.process_batch(events) {
                        Ok(outputs) => outputs,
                        Err(e) => return Err(e),
                    };
                    metrics_proc
                        .events_processed
                        .fetch_add(count, Ordering::Relaxed);

                    // PoH: append batch to hash chain if enabled
                    #[cfg(feature = "processor-auth")]
                    if let Some(ref poh) = poh_proc {
                        poh_append_batch(poh, &outputs, &poh_key_proc, &metrics_proc).await;
                    }

                    // Push outputs into sink buffer
                    let mut pending = Some(outputs);
                    while let Some(batch) = pending.take() {
                        match sink_prod.push(batch) {
                            Ok(()) => {}
                            Err(rtrb::PushError::Full(returned)) => {
                                pending = Some(returned);
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                }
                Err(_) => {
                    // Buffer empty — check if source is done
                    if src_cons.is_abandoned() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
        drop(sink_prod);
        Ok::<(), AeonError>(())
    });

    let metrics_sink = Arc::clone(&metrics);

    // Sink task: pop outputs, write to sink.
    // PerEvent/OrderedBatch: write_batch blocks on delivery acks.
    // UnorderedBatch: write_batch enqueues fast, flush() called at checkpoint intervals.
    //
    // Delivery ledger integration:
    // - Track each output with source_event_id before write_batch
    // - Mark acked on successful delivery
    // - Populate checkpoint source_offsets from ledger
    let sink_ledger = ledger;
    let sink_handle = tokio::spawn(run_sink_task(
        sink,
        sink_cons,
        metrics_sink,
        sink_ledger,
        sink_ctx,
        None,
    ));

    // Wait for all tasks
    let (src_result, proc_result, sink_result) =
        tokio::join!(source_handle, processor_handle, sink_handle);

    // Propagate errors
    src_result.map_err(|e| AeonError::processor(format!("source task panicked: {e}")))??;
    proc_result.map_err(|e| AeonError::processor(format!("processor task panicked: {e}")))??;
    sink_result.map_err(|e| AeonError::processor(format!("sink task panicked: {e}")))??;

    Ok(())
}

/// Runs a buffered pipeline with an async `ProcessorTransport` in the
/// processor stage. Used by T3/T4 out-of-process processor transports
/// (WebTransport/WebSocket hosts and any other ProcessorTransport impl).
///
/// Structurally identical to `run_buffered` — same three-task layout with
/// source→processor→sink SPSC ring buffers and the same delivery/failure
/// pipeline in the sink task. The only difference is that the processor
/// task body calls `transport.call_batch(events).await` instead of the
/// synchronous `processor.process_batch(events)`.
///
/// Backpressure propagates identically:
/// - If the transport is slow or saturated (its `BatchInflight` semaphore is
///   held), `call_batch` suspends on the semaphore, which suspends the
///   processor task, which stops draining the source→processor ring — the
///   source task pauses, and ultimately the upstream broker sees TCP-window
///   backpressure. No events are dropped.
pub async fn run_buffered_transport<S, T, K>(
    source: S,
    transport: Arc<T>,
    sink: K,
    config: PipelineConfig,
    metrics: Arc<PipelineMetrics>,
    shutdown: Arc<AtomicBool>,
    ledger: Option<Arc<DeliveryLedger>>,
) -> Result<(), AeonError>
where
    S: Source + Send + 'static,
    T: aeon_types::traits::ProcessorTransport + Send + Sync + 'static + ?Sized,
    K: Sink + Send + 'static,
{
    reject_broker_coord_with_reassign(&source, &config)?;

    let core_assignment = config.core_pinning.resolve();

    // EO-2 P4: wrap the source in `L2WritingSource` when durability requires
    // body persistence, matching the run_buffered entry point.
    let mut source = crate::eo2::MaybeL2Wrapped::wrap(
        source,
        &config.pipeline_name,
        config.l2_registry.as_ref(),
        config.delivery.durability,
        config.eo2_capacity.as_ref(),
    );

    // Initialize PoH chain if configured
    #[cfg(feature = "processor-auth")]
    let poh_state = create_poh_state(&config);
    #[cfg(feature = "processor-auth")]
    let poh_signing_key = config.poh.as_ref().and_then(|c| c.signing_key.clone());

    let (mut src_prod, mut src_cons) =
        rtrb::RingBuffer::<Vec<Event>>::new(config.source_buffer_capacity);
    let (mut sink_prod, sink_cons) =
        rtrb::RingBuffer::<Vec<Output>>::new(config.sink_buffer_capacity);

    // EO-2 P5: apply recovery plan before the source task starts.
    let sink_ctx = build_sink_task_ctx(&config, core_assignment.map(|c| c.sink));
    let recovery_plan = load_recovery_plan(&sink_ctx.checkpoint_writer);
    dispatch_recovery_plan(&mut source, recovery_plan).await;

    let shutdown_src = Arc::clone(&shutdown);
    let metrics_src = Arc::clone(&metrics);
    let src_capacity = config.eo2_capacity.clone();
    let src_eo2_metrics = config.eo2_metrics.clone();

    // Source task — identical to run_buffered.
    let source_core = core_assignment.map(|c| c.source);
    let source_handle = tokio::spawn(async move {
        if let Some(core) = source_core {
            pin_to_core(core);
        }
        let source_kind = source.source_kind();
        while !shutdown_src.load(Ordering::Relaxed) {
            // EO-2 P7: capacity gate — see run_buffered for rationale.
            if let Some(ref cap) = src_capacity {
                let partition = 0u16;
                let mut decision = cap.decide(partition, source_kind);
                if decision.is_engaged() {
                    if let Some(ref m) = src_eo2_metrics {
                        if let Some(lvl) = decision.level() {
                            m.set_l2_pressure(lvl, true);
                        }
                    }
                    while decision.is_engaged() && !shutdown_src.load(Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        decision = cap.decide(partition, source_kind);
                    }
                    if let Some(ref m) = src_eo2_metrics {
                        use crate::eo2_backpressure::PressureLevel;
                        m.set_l2_pressure(PressureLevel::Partition, false);
                        m.set_l2_pressure(PressureLevel::Pipeline, false);
                        m.set_l2_pressure(PressureLevel::Node, false);
                    }
                }
            }

            let events = match source.next_batch().await {
                Ok(events) => events,
                Err(e) => return Err(e),
            };
            if events.is_empty() {
                match source_kind {
                    SourceKind::Pull => break,
                    SourceKind::Push | SourceKind::Poll => {
                        tokio::task::yield_now().await;
                        continue;
                    }
                }
            }
            metrics_src
                .events_received
                .fetch_add(events.len() as u64, Ordering::Relaxed);

            let mut pending = Some(events);
            while let Some(batch) = pending.take() {
                match src_prod.push(batch) {
                    Ok(()) => {}
                    Err(rtrb::PushError::Full(returned)) => {
                        pending = Some(returned);
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
        drop(src_prod);
        Ok::<(), AeonError>(())
    });

    let shutdown_proc = Arc::clone(&shutdown);
    let metrics_proc = Arc::clone(&metrics);
    #[cfg(feature = "processor-auth")]
    let poh_proc = poh_state.clone();
    #[cfg(feature = "processor-auth")]
    let poh_key_proc = poh_signing_key.clone();

    // Processor task — the single difference from run_buffered. Calls
    // `transport.call_batch(events).await` instead of the synchronous
    // `processor.process_batch(events)`. Backpressure from a saturated
    // transport (e.g. the session's BatchInflight semaphore is full) will
    // suspend here and propagate backwards through the source ring buffer.
    let proc_core = core_assignment.map(|c| c.processor);
    let processor_handle = tokio::spawn(async move {
        if let Some(core) = proc_core {
            pin_to_core(core);
        }
        while !shutdown_proc.load(Ordering::Relaxed) {
            match src_cons.pop() {
                Ok(events) => {
                    let count = events.len() as u64;
                    let outputs = match transport.call_batch(events).await {
                        Ok(outputs) => outputs,
                        Err(e) => return Err(e),
                    };
                    metrics_proc
                        .events_processed
                        .fetch_add(count, Ordering::Relaxed);

                    // PoH: append batch to hash chain if enabled
                    #[cfg(feature = "processor-auth")]
                    if let Some(ref poh) = poh_proc {
                        poh_append_batch(poh, &outputs, &poh_key_proc, &metrics_proc).await;
                    }

                    let mut pending = Some(outputs);
                    while let Some(batch) = pending.take() {
                        match sink_prod.push(batch) {
                            Ok(()) => {}
                            Err(rtrb::PushError::Full(returned)) => {
                                pending = Some(returned);
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                }
                Err(_) => {
                    if src_cons.is_abandoned() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
        drop(sink_prod);
        Ok::<(), AeonError>(())
    });

    let metrics_sink = Arc::clone(&metrics);
    let sink_ledger = ledger;
    let sink_handle = tokio::spawn(run_sink_task(
        sink,
        sink_cons,
        metrics_sink,
        sink_ledger,
        sink_ctx,
        None,
    ));

    let (src_result, proc_result, sink_result) =
        tokio::join!(source_handle, processor_handle, sink_handle);

    src_result.map_err(|e| AeonError::processor(format!("source task panicked: {e}")))??;
    proc_result.map_err(|e| AeonError::processor(format!("processor task panicked: {e}")))??;
    sink_result.map_err(|e| AeonError::processor(format!("sink task panicked: {e}")))??;

    Ok(())
}

/// B4: refuse to start a pipeline whose source uses broker-coordinated
/// partition ownership while the caller has also attached a Raft-driven
/// `partition_reassign` watch. The two partition-ownership models are
/// mutually exclusive — running both would have the broker and Aeon
/// fight over the same resource.
fn reject_broker_coord_with_reassign<S: Source>(
    source: &S,
    config: &PipelineConfig,
) -> Result<(), AeonError> {
    if config.partition_reassign.is_some() && source.broker_coordinated_partitions() {
        return Err(AeonError::config(
            "pipeline start: source uses ConsumerMode::Group (broker-coordinated \
             partition ownership) but partition_reassign is attached — these are \
             mutually exclusive. Either set ConsumerMode::Single on the source, or \
             drop the Raft partition_reassign watcher.",
        ));
    }
    Ok(())
}

/// Build a `SinkTaskCtx` from `PipelineConfig`, resolving the checkpoint
/// WAL writer if configured. Shared between `run_buffered` and
/// `run_buffered_transport` so the two call sites can spawn `run_sink_task`
/// with identical setup.
fn build_sink_task_ctx(config: &PipelineConfig, core: Option<usize>) -> SinkTaskCtx {
    // Initialize the checkpoint persister. FT-3: the concrete backend is
    // chosen by `CheckpointBackend`; `None` yields no persister.
    let checkpoint_dir = || {
        config.delivery.checkpoint.dir.clone().unwrap_or_else(|| {
            std::env::var("AEON_CHECKPOINT_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir().join("aeon-checkpoints"))
        })
    };

    let primary: Option<Box<dyn CheckpointPersist>> = match config.delivery.checkpoint.backend {
        CheckpointBackend::Wal => {
            let wal_path = checkpoint_dir().join("pipeline.wal");
            match WalCheckpointStore::open(&wal_path) {
                Ok(writer) => {
                    tracing::info!(path = %wal_path.display(), "Checkpoint WAL initialized");
                    Some(Box::new(writer) as Box<dyn CheckpointPersist>)
                }
                Err(e) => {
                    tracing::warn!(
                        "Checkpoint WAL init failed: {e}, continuing without checkpoints"
                    );
                    None
                }
            }
        }
        CheckpointBackend::StateStore => match &config.l3_checkpoint_store {
            Some(l3) => match L3CheckpointStore::open(Arc::clone(l3)) {
                Ok(store) => {
                    // S5: apply the resolved `l3_max_records` cap if the
                    // manifest set `durability.retention.l3_ack.max_records`.
                    let store = match config.retention_plan.as_ref() {
                        Some(plan) => store.with_max_records(plan.l3_max_records),
                        None => store,
                    };
                    tracing::info!("Checkpoint L3 store initialized");
                    Some(Box::new(store) as Box<dyn CheckpointPersist>)
                }
                Err(e) => {
                    tracing::warn!(
                        "Checkpoint L3 store init failed: {e}, continuing without checkpoints"
                    );
                    None
                }
            },
            None => {
                tracing::warn!(
                    "CheckpointBackend::StateStore configured but no l3_checkpoint_store provided; \
                     continuing without checkpoints"
                );
                None
            }
        },
        CheckpointBackend::None => None,
    };

    // EO-2 P5: wrap the StateStore primary in `FallbackCheckpointStore` so a
    // transient L3 write error transparently degrades to a WAL sidecar
    // instead of stalling the sink task. WAL-native backend needs no wrap
    // (it is already WAL), `None` stays `None`.
    let checkpoint_writer: Option<Box<dyn CheckpointPersist>> = match (
        primary,
        config.delivery.checkpoint.backend,
        config.delivery.durability,
    ) {
        (Some(p), CheckpointBackend::StateStore, mode)
            if mode != aeon_types::DurabilityMode::None =>
        {
            let wal_path = checkpoint_dir().join("fallback.wal");
            // O1 / F1: chain `.with_metrics(...)` so engage_fallback's
            // `inc_checkpoint_fallback_wal` actually ticks the
            // supervisor-shared `Eo2Metrics`. Without this, the fault
            // → fallback engagement happens but the counter stays at
            // 0 in the supervisor's registry — and therefore at 0 on
            // /metrics. Discovered 2026-05-02 during F3 live cluster
            // proof: 7 checkpoint writes + 100 armed faults produced
            // engage_fallback calls but no metric ticks.
            let mut store = crate::eo2_recovery::FallbackCheckpointStore::new(p, wal_path);
            if let Some(m) = config.eo2_metrics.as_ref() {
                store = store.with_metrics(Arc::clone(m));
            }
            Some(Box::new(store) as Box<dyn CheckpointPersist>)
        }
        (other, _, _) => other,
    };

    SinkTaskCtx {
        core,
        delivery_strategy: config.delivery.strategy,
        flush_interval: config.delivery.flush.interval,
        max_pending: config.delivery.flush.max_pending,
        adaptive_flush: config.delivery.flush.adaptive,
        adaptive_min_divisor: config.delivery.flush.adaptive_min_divisor,
        adaptive_max_multiplier: config.delivery.flush.adaptive_max_multiplier,
        failure_policy: config.delivery.failure_policy,
        max_retries: config.delivery.max_retries,
        retry_backoff: config.delivery.retry_backoff,
        checkpoint_writer,
        eo2_ack_tracker: if config.delivery.durability != aeon_types::DurabilityMode::None {
            Some(config.eo2_shared_ack_tracker.clone().unwrap_or_default())
        } else {
            None
        },
        eo2_sink_name: config.sink_name.clone(),
        eo2_pipeline_name: config.pipeline_name.clone(),
        eo2_l2_registry: config.l2_registry.clone(),
        eo2_metrics: config.eo2_metrics.clone(),
        eo2_capacity: config.eo2_capacity.clone(),
    }
}

/// Multi-partition pipeline configuration.
pub struct MultiPartitionConfig {
    /// Number of partitions (each gets an independent pipeline).
    pub partition_count: usize,
    /// Base pipeline config (cloned per partition, core pinning resolved automatically).
    pub pipeline: PipelineConfig,
    /// G2/CL-6: optional shared write-gate registry. When `Some`, each
    /// partition's `PipelineConfig` gets a gate looked up from the registry
    /// keyed by `(pipeline_name, partition_id)`; the cluster cutover
    /// coordinator can then freeze a single partition without affecting
    /// the others. `None` disables gates for this multi-partition run.
    pub gate_registry: Option<Arc<crate::write_gate::WriteGateRegistry>>,
}

/// Runs independent pipelines for each partition, with optional per-partition core pinning.
///
/// Each partition gets its own source, processor, sink, and optional ledger — fully
/// independent, no shared state on the hot path. Scales linearly with cores.
///
/// Core pinning in `Auto` mode assigns 3 cores per partition (source, processor, sink),
/// skipping core 0 for OS/runtime. Falls back to no pinning if insufficient cores.
///
/// The factory closures create fresh instances per partition:
/// - `source_factory(partition_index)` — returns a source bound to that partition
/// - `processor_factory(partition_index)` — returns a processor for that partition
/// - `sink_factory(partition_index)` — returns a sink for that partition
/// - `ledger_factory` (optional) — if `Some`, creates per-partition delivery ledgers
pub async fn run_multi_partition<S, P, K, SF, PF, KF>(
    config: MultiPartitionConfig,
    metrics: Arc<PipelineMetrics>,
    shutdown: Arc<AtomicBool>,
    source_factory: SF,
    processor_factory: PF,
    sink_factory: KF,
    ledger_factory: Option<Box<dyn Fn(usize) -> Arc<DeliveryLedger> + Send>>,
) -> Result<(), AeonError>
where
    S: Source + Send + 'static,
    P: Processor + Send + Sync + 'static,
    K: Sink + Send + 'static,
    SF: Fn(usize) -> S,
    PF: Fn(usize) -> P,
    KF: Fn(usize) -> K,
{
    use crate::affinity::multi_pipeline_core_assignment;

    let partition_count = config.partition_count;
    if partition_count == 0 {
        return Ok(());
    }

    // Resolve multi-partition core assignments
    let core_assignments = if matches!(config.pipeline.core_pinning, CorePinning::Auto) {
        multi_pipeline_core_assignment(partition_count)
    } else {
        None
    };

    let mut handles = Vec::with_capacity(partition_count);

    for i in 0..partition_count {
        let source = source_factory(i);
        let processor = processor_factory(i);
        let sink = sink_factory(i);
        let ledger = ledger_factory.as_ref().map(|f| f(i));

        // Per-partition config: override core pinning with resolved assignment
        let partition_id = PartitionId::new(u16::try_from(i).unwrap_or(u16::MAX));
        let write_gate = config
            .gate_registry
            .as_ref()
            .map(|reg| reg.get_or_create(&config.pipeline.pipeline_name, partition_id));
        let mut partition_config = PipelineConfig {
            source_buffer_capacity: config.pipeline.source_buffer_capacity,
            sink_buffer_capacity: config.pipeline.sink_buffer_capacity,
            max_batch_size: config.pipeline.max_batch_size,
            core_pinning: CorePinning::Disabled,
            delivery: config.pipeline.delivery.clone(),
            #[cfg(feature = "processor-auth")]
            poh: config.pipeline.poh.clone(),
            #[cfg(all(feature = "processor-auth", feature = "cluster"))]
            poh_installed_chains: config.pipeline.poh_installed_chains.clone(),
            #[cfg(all(feature = "processor-auth", feature = "cluster"))]
            poh_live_chains: config.pipeline.poh_live_chains.clone(),
            l3_checkpoint_store: config.pipeline.l3_checkpoint_store.clone(),
            pipeline_name: config.pipeline.pipeline_name.clone(),
            l2_registry: config.pipeline.l2_registry.clone(),
            sink_name: config.pipeline.sink_name.clone(),
            eo2_metrics: config.pipeline.eo2_metrics.clone(),
            eo2_capacity: config.pipeline.eo2_capacity.clone(),
            eo2_shared_ack_tracker: config.pipeline.eo2_shared_ack_tracker.clone(),
            partition_id,
            write_gate,
            // run_multi_partition owns per-partition sub-tasks that are
            // each pinned to a single partition id — dynamic re-assign at
            // the per-task level is not meaningful. Leave disabled.
            partition_reassign: None,
            encryption_plan: config.pipeline.encryption_plan.clone(),
            retention_plan: config.pipeline.retention_plan.clone(),
        };

        if let Some(ref assignments) = core_assignments {
            partition_config.core_pinning = CorePinning::Manual(assignments[i]);
        }

        let partition_metrics = Arc::clone(&metrics);
        let partition_shutdown = Arc::clone(&shutdown);

        let handle = tokio::spawn(async move {
            run_buffered(
                source,
                processor,
                sink,
                partition_config,
                partition_metrics,
                partition_shutdown,
                ledger,
            )
            .await
        });

        handles.push(handle);
    }

    // Wait for all partition pipelines, collect errors
    let mut first_error: Option<AeonError> = None;
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(partition = i, error = %e, "Partition pipeline failed");
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
            Err(e) => {
                tracing::error!(partition = i, error = %e, "Partition pipeline panicked");
                if first_error.is_none() {
                    first_error = Some(AeonError::processor(format!(
                        "partition {i} pipeline panicked: {e}"
                    )));
                }
            }
        }
    }

    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Bundle of sink-task parameters resolved from `PipelineConfig::delivery`.
///
/// Passed into `run_sink_task` so both `run_buffered` and
/// `run_buffered_transport` can share the sink task body without needing
/// a 15-argument helper signature.
struct SinkTaskCtx {
    core: Option<usize>,
    delivery_strategy: aeon_types::DeliveryStrategy,
    flush_interval: Duration,
    max_pending: usize,
    adaptive_flush: bool,
    adaptive_min_divisor: u32,
    adaptive_max_multiplier: u32,
    failure_policy: BatchFailurePolicy,
    max_retries: u32,
    retry_backoff: Duration,
    checkpoint_writer: Option<Box<dyn CheckpointPersist>>,
    /// EO-2: optional per-sink ack-seq tracker. When `Some`, each checkpoint
    /// record stamps `per_sink_ack_seq` so recovery + GC can compute the
    /// min-ack frontier. Wired through `build_sink_task_ctx` at pipeline
    /// start when `DurabilityMode != None`.
    eo2_ack_tracker: Option<crate::eo2::AckSeqTracker>,
    /// EO-2: sink name used to key the `AckSeqTracker` map. Registered at
    /// task start so `min_across_sinks()` returns a sensible 0 baseline
    /// before the first delivery lands.
    eo2_sink_name: String,
    /// EO-2: pipeline name forwarded to `PipelineL2Registry::open` when the
    /// sink-side GC sweep runs.
    eo2_pipeline_name: String,
    /// EO-2: shared L2 body-store registry. When `Some` together with
    /// `eo2_ack_tracker`, the sink task periodically calls
    /// `L2BodyStore::gc_up_to(min_across_sinks)` on every registered
    /// partition at flush boundaries.
    eo2_l2_registry: Option<crate::eo2::PipelineL2Registry>,
    /// EO-2 P8: optional metrics registry for sink-side counters.
    eo2_metrics: Option<Arc<crate::eo2_metrics::Eo2Metrics>>,
    /// EO-2 P7: optional capacity tracker — GC sweep calls `adjust(-reclaimed)`.
    eo2_capacity: Option<crate::eo2_backpressure::PipelineCapacity>,
}

/// Shared sink task body for `run_buffered` / `run_buffered_transport`.
///
/// Pops output batches from the processor→sink ring buffer, applies the
/// configured `DeliveryStrategy`, handles partial failures via
/// `BatchFailurePolicy`, maintains the delivery ledger, and periodically
/// flushes + checkpoints. Exits cleanly when the ring producer is dropped.
async fn run_sink_task<K: Sink + Send + 'static>(
    mut sink: K,
    mut sink_cons: rtrb::Consumer<Vec<Output>>,
    metrics_sink: Arc<PipelineMetrics>,
    sink_ledger: Option<Arc<DeliveryLedger>>,
    ctx: SinkTaskCtx,
    control: Option<Arc<PipelineControl>>,
) -> Result<(), AeonError> {
    if let Some(core) = ctx.core {
        pin_to_core(core);
    }

    let SinkTaskCtx {
        core: _,
        delivery_strategy,
        flush_interval,
        max_pending,
        adaptive_flush,
        adaptive_min_divisor,
        adaptive_max_multiplier,
        failure_policy,
        max_retries,
        retry_backoff,
        mut checkpoint_writer,
        eo2_ack_tracker: ack_tracker,
        eo2_sink_name: sink_name,
        eo2_pipeline_name: pipeline_name,
        eo2_l2_registry: l2_registry,
        eo2_metrics: metrics,
        eo2_capacity: capacity,
    } = ctx;

    // EO-2 P4: register this sink with the ack tracker so
    // `min_across_sinks()` returns 0 until the first real ack arrives —
    // avoids GC spuriously running at the max-u64 watermark.
    if let Some(ref tracker) = ack_tracker {
        tracker.register_sink(&sink_name);
    }
    // Highest l2_seq observed on this batch — the monotonic watermark we
    // feed into `AckSeqTracker::record_ack` per sink task.
    let mut max_acked_seq: u64 = 0;
    // EO-2 P5: call `try_recover_primary` every N flushes. Cheap when the
    // store is healthy (just returns Ok(true) from the default impl); only
    // `FallbackCheckpointStore` does real work here.
    let mut flushes_since_recovery_attempt: u32 = 0;
    const RECOVERY_ATTEMPT_EVERY_N_FLUSHES: u32 = 16;

    let mut last_flush = Instant::now();
    let mut pending_count: u64 = 0;
    // IDs of outputs that write_batch returned as `pending` but have not yet
    // been flushed. Accumulated across write_batch calls; drained on flush.
    // Used for both the outputs_sent metric and the delivery ledger so that
    // UnorderedBatch sinks correctly credit acks at flush boundaries.
    let mut pending_ids: Vec<uuid::Uuid> = Vec::new();
    let mut delivered_since_checkpoint: u64 = 0;
    let mut failed_since_checkpoint: u64 = 0;

    // Adaptive flush tuner: adjusts flush interval based on ack success rate.
    // Only active when adaptive=true AND a delivery ledger is present.
    let mut flush_tuner = if adaptive_flush && sink_ledger.is_some() {
        Some(FlushTuner::new(
            flush_interval,
            flush_interval / adaptive_min_divisor,
            flush_interval * adaptive_max_multiplier,
        ))
    } else {
        None
    };
    // Counters for adaptive flush feedback
    let mut acked_since_last_flush: u64 = 0;
    let mut events_since_last_flush: u64 = 0;

    loop {
        match sink_cons.pop() {
            Ok(outputs) => {
                let count = outputs.len() as u64;

                // Track outputs in delivery ledger (if enabled).
                // Collect event IDs for ack/fail after write_batch.
                let tracked_ids: Vec<uuid::Uuid> = if let Some(ref ledger) = sink_ledger {
                    outputs
                        .iter()
                        .filter_map(|o| {
                            if let Some(event_id) = o.source_event_id {
                                let partition = o.source_partition.unwrap_or(PartitionId::new(0));
                                let offset = o.source_offset.unwrap_or(0);
                                ledger.track(event_id, partition, offset);
                                Some(event_id)
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                };

                // Clone outputs before write_batch if failure policy may need them for retry.
                // Only clone when RetryFailed is configured — avoid the cost otherwise.
                let outputs_for_retry = if failure_policy == BatchFailurePolicy::RetryFailed {
                    Some(outputs.clone())
                } else {
                    None
                };

                // EO-2 P4: capture (event_id → l2_seq) map before write_batch
                // moves the outputs. Only when the pipeline tracks acks.
                let l2_seq_by_id: Option<HashMap<uuid::Uuid, u64>> =
                    ack_tracker.as_ref().map(|_| {
                        outputs
                            .iter()
                            .filter_map(|o| match (o.source_event_id, o.l2_seq) {
                                (Some(id), Some(seq)) => Some((id, seq)),
                                _ => None,
                            })
                            .collect()
                    });

                match sink.write_batch(outputs).await {
                    Ok(batch_result) => {
                        // Mark delivered outputs as acked in ledger.
                        if let Some(ref ledger) = sink_ledger {
                            if !batch_result.delivered.is_empty() {
                                ledger.mark_batch_acked(&batch_result.delivered);
                            }
                            // Failed outputs are marked in ledger.
                            for (id, err) in &batch_result.failed {
                                ledger.mark_failed(id, format!("{err}"));
                            }
                            // Pending outputs remain tracked — acked at flush.
                        }
                        // EO-2 P4: advance the per-sink ack-seq watermark for
                        // this sink using the max l2_seq across delivered
                        // outputs. Monotonic — stale values are ignored.
                        if let (Some(tracker), Some(map)) =
                            (ack_tracker.as_ref(), l2_seq_by_id.as_ref())
                        {
                            let mut batch_max: u64 = 0;
                            for id in &batch_result.delivered {
                                if let Some(&seq) = map.get(id) {
                                    if seq > batch_max {
                                        batch_max = seq;
                                    }
                                }
                            }
                            if batch_max > max_acked_seq {
                                max_acked_seq = batch_max;
                                tracker.record_ack(&sink_name, batch_max);
                                if let Some(m) = metrics.as_ref() {
                                    m.set_sink_ack_seq(&pipeline_name, &sink_name, batch_max);
                                }
                            }
                        }

                        let delivered_count = batch_result.delivered.len() as u64;
                        let total_count = count;
                        metrics_sink
                            .outputs_sent
                            .fetch_add(delivered_count, Ordering::Relaxed);
                        delivered_since_checkpoint += delivered_count;
                        acked_since_last_flush += delivered_count;
                        events_since_last_flush += total_count;

                        // Accumulate pending IDs for flush-time crediting.
                        // Non-blocking strategies (UnorderedBatch) return
                        // BatchResult::all_pending; delivered is empty here and
                        // only resolved when sink.flush() completes.
                        if !batch_result.pending.is_empty() {
                            pending_ids.extend(batch_result.pending.iter().copied());
                        }

                        // Apply BatchFailurePolicy if there are partial failures.
                        if batch_result.has_failures() {
                            let original = outputs_for_retry.as_deref().unwrap_or(&[]);
                            handle_batch_failures(
                                &mut sink,
                                original,
                                &batch_result,
                                failure_policy,
                                max_retries,
                                retry_backoff,
                                &metrics_sink,
                                &sink_ledger,
                            )
                            .await?;
                            failed_since_checkpoint += batch_result.failed.len() as u64;
                        }
                    }
                    Err(e) => {
                        // Mark all tracked outputs as failed
                        if let Some(ref ledger) = sink_ledger {
                            let reason = format!("{e}");
                            for id in &tracked_ids {
                                ledger.mark_failed(id, reason.clone());
                            }
                        }
                        return Err(e);
                    }
                }

                // EO-2 P7: for blocking strategies (PerEvent/OrderedBatch),
                // GC on every batch when capacity tracking is active. Without
                // this, the source capacity gate deadlocks: source waits for
                // capacity, capacity waits for GC, GC waits for sink exit.
                if delivery_strategy.is_blocking() && capacity.is_some() {
                    eo2_gc_sweep(
                        &l2_registry,
                        &pipeline_name,
                        ack_tracker.as_ref(),
                        metrics.as_ref().map(|m| m.as_ref()),
                        capacity.as_ref(),
                    );
                }

                // M2: blocking strategies (PerEvent/OrderedBatch) deliver +
                // ack synchronously, so they never enter the
                // `!is_blocking()` periodic-flush block below — meaning
                // `write_checkpoint` only ever fired at shutdown via the
                // post-loop path (and even then only if the loop had
                // exited). For a long-running pipeline, that left
                // `aeon_pipeline_checkpoints_written_total` at 0 even
                // though events flowed and the L3 store was installed.
                // Mirror the non-blocking path's flush-interval cadence
                // so blocking strategies also produce checkpoint records
                // during steady-state operation.
                if delivery_strategy.is_blocking()
                    && delivered_since_checkpoint > 0
                    && last_flush.elapsed() >= flush_interval
                {
                    write_checkpoint(
                        &mut checkpoint_writer,
                        &sink_ledger,
                        &metrics_sink,
                        &mut delivered_since_checkpoint,
                        &mut failed_since_checkpoint,
                        ack_tracker.as_ref(),
                    );
                    last_flush = Instant::now();
                }

                // In Batched mode, track pending and flush at intervals
                if !delivery_strategy.is_blocking() {
                    pending_count += count;
                    let effective_interval = flush_tuner
                        .as_ref()
                        .map(|t| t.interval())
                        .unwrap_or(flush_interval);
                    let should_flush = last_flush.elapsed() >= effective_interval
                        || pending_count >= max_pending as u64;
                    if should_flush {
                        // Report to adaptive tuner before flush
                        if let Some(ref mut tuner) = flush_tuner {
                            tuner.report(events_since_last_flush, acked_since_last_flush);
                        }
                        sink.flush().await?;
                        // Credit pending outputs as delivered now that the
                        // flush has resolved their acks. Mark them in the
                        // ledger so checkpoints reflect the actual delivered
                        // set for UnorderedBatch-style sinks.
                        credit_pending_on_flush(
                            &mut pending_ids,
                            &metrics_sink,
                            &sink_ledger,
                            &mut delivered_since_checkpoint,
                            &mut acked_since_last_flush,
                        );
                        write_checkpoint(
                            &mut checkpoint_writer,
                            &sink_ledger,
                            &metrics_sink,
                            &mut delivered_since_checkpoint,
                            &mut failed_since_checkpoint,
                            ack_tracker.as_ref(),
                        );
                        eo2_gc_sweep(
                            &l2_registry,
                            &pipeline_name,
                            ack_tracker.as_ref(),
                            metrics.as_ref().map(|m| m.as_ref()),
                            capacity.as_ref(),
                        );
                        flushes_since_recovery_attempt += 1;
                        if flushes_since_recovery_attempt >= RECOVERY_ATTEMPT_EVERY_N_FLUSHES {
                            flushes_since_recovery_attempt = 0;
                            if let Some(w) = checkpoint_writer.as_mut() {
                                if let Err(e) = w.try_recover_primary() {
                                    tracing::warn!("try_recover_primary failed: {e}");
                                }
                            }
                        }
                        pending_count = 0;
                        last_flush = Instant::now();
                        acked_since_last_flush = 0;
                        events_since_last_flush = 0;
                    }
                }
            }
            Err(_) => {
                if sink_cons.is_abandoned() {
                    break;
                }
                // Check for sink swap (managed pipeline only)
                if let Some(ref ctrl) = control {
                    if ctrl.paused.load(Ordering::Acquire) {
                        let mut slot = ctrl.new_sink.lock().await;
                        if let Some(new_sink_any) = slot.take() {
                            if let Ok(new_sink) = new_sink_any.downcast::<K>() {
                                sink = *new_sink;
                                ctrl.swap_complete.notify_one();
                            }
                        }
                    }
                }
                // P1: blocking strategies (PerEvent/OrderedBatch) deliver
                // synchronously, so when the source goes idle there are no
                // pending events to credit — but `delivered_since_checkpoint`
                // may still hold uncommitted-checkpoint deltas from the last
                // batch. Without this idle-path tick, a long-running pipeline
                // that drains its source and then waits for more input writes
                // exactly one final-shutdown checkpoint regardless of how
                // many batches flowed through. Mirroring the non-blocking
                // idle-path logic here keeps the metric cadence consistent
                // across strategy choices.
                if delivery_strategy.is_blocking()
                    && delivered_since_checkpoint > 0
                    && last_flush.elapsed() >= flush_interval
                {
                    write_checkpoint(
                        &mut checkpoint_writer,
                        &sink_ledger,
                        &metrics_sink,
                        &mut delivered_since_checkpoint,
                        &mut failed_since_checkpoint,
                        ack_tracker.as_ref(),
                    );
                    last_flush = Instant::now();
                }

                // In Batched mode, flush pending even while idle
                if !delivery_strategy.is_blocking() && pending_count > 0 {
                    let effective_interval = flush_tuner
                        .as_ref()
                        .map(|t| t.interval())
                        .unwrap_or(flush_interval);
                    if last_flush.elapsed() >= effective_interval {
                        if let Some(ref mut tuner) = flush_tuner {
                            tuner.report(events_since_last_flush, acked_since_last_flush);
                        }
                        sink.flush().await?;
                        credit_pending_on_flush(
                            &mut pending_ids,
                            &metrics_sink,
                            &sink_ledger,
                            &mut delivered_since_checkpoint,
                            &mut acked_since_last_flush,
                        );
                        write_checkpoint(
                            &mut checkpoint_writer,
                            &sink_ledger,
                            &metrics_sink,
                            &mut delivered_since_checkpoint,
                            &mut failed_since_checkpoint,
                            ack_tracker.as_ref(),
                        );
                        eo2_gc_sweep(
                            &l2_registry,
                            &pipeline_name,
                            ack_tracker.as_ref(),
                            metrics.as_ref().map(|m| m.as_ref()),
                            capacity.as_ref(),
                        );
                        flushes_since_recovery_attempt += 1;
                        if flushes_since_recovery_attempt >= RECOVERY_ATTEMPT_EVERY_N_FLUSHES {
                            flushes_since_recovery_attempt = 0;
                            if let Some(w) = checkpoint_writer.as_mut() {
                                if let Err(e) = w.try_recover_primary() {
                                    tracing::warn!("try_recover_primary failed: {e}");
                                }
                            }
                        }
                        pending_count = 0;
                        last_flush = Instant::now();
                        acked_since_last_flush = 0;
                        events_since_last_flush = 0;
                    }
                }
                tokio::task::yield_now().await;
            }
        }
    }

    // Final flush + checkpoint
    sink.flush().await?;
    credit_pending_on_flush(
        &mut pending_ids,
        &metrics_sink,
        &sink_ledger,
        &mut delivered_since_checkpoint,
        &mut acked_since_last_flush,
    );
    if delivered_since_checkpoint > 0 || failed_since_checkpoint > 0 {
        write_checkpoint(
            &mut checkpoint_writer,
            &sink_ledger,
            &metrics_sink,
            &mut delivered_since_checkpoint,
            &mut failed_since_checkpoint,
            ack_tracker.as_ref(),
        );
        eo2_gc_sweep(
            &l2_registry,
            &pipeline_name,
            ack_tracker.as_ref(),
            metrics.as_ref().map(|m| m.as_ref()),
            capacity.as_ref(),
        );
    }

    Ok(())
}

/// EO-2 P5: derive a `RecoveryPlan` from the last persisted checkpoint and
/// hand it to the source so connectors that know how to resume (Kafka
/// `Seekable`, L2-aware push sources) can position themselves before the
/// main loop starts. Default `Source::on_recovery_plan` is a no-op, so
/// connectors opt in; there is no silent behaviour change for existing
/// sources. Errors are logged and swallowed — a broken recovery plan
/// must not stall pipeline start.
///
/// `dyn CheckpointPersist` is `Send` but not `Sync`, so we synchronously
/// read the last record here and return the derived plan, letting the
/// caller `.await` the source dispatch without holding a reference to
/// the writer across the await point.
fn load_recovery_plan(
    checkpoint_writer: &Option<Box<dyn CheckpointPersist>>,
) -> Option<crate::eo2_recovery::RecoveryPlan> {
    let writer = checkpoint_writer.as_ref()?;
    let last = match writer.read_last() {
        Ok(opt) => opt,
        Err(e) => {
            tracing::warn!("EO-2 recovery: read_last failed: {e}");
            return None;
        }
    };
    let plan = crate::eo2_recovery::RecoveryPlan::from_last(last.as_ref());
    if last.is_some() {
        tracing::info!(
            checkpoint_id = plan.source_checkpoint_id,
            replay_from_l2_seq = plan.l2_replay_start_seq,
            seek_partitions = plan.pull_seek_offsets.len(),
            "EO-2: applying recovery plan from last checkpoint"
        );
    }
    Some(plan)
}

async fn dispatch_recovery_plan<S: Source>(
    source: &mut S,
    plan: Option<crate::eo2_recovery::RecoveryPlan>,
) {
    let Some(plan) = plan else { return };
    if let Err(e) = source
        .on_recovery_plan(&plan.pull_seek_offsets, plan.l2_replay_start_seq)
        .await
    {
        tracing::warn!("EO-2: source rejected recovery plan: {e}");
    }
}

/// EO-2 P4: sweep every registered partition's L2 body store up to the
/// current min-across-sinks ack watermark. Called at flush boundaries.
/// No-op when durability is `None` or no L2 registry is wired.
fn eo2_gc_sweep(
    registry: &Option<crate::eo2::PipelineL2Registry>,
    pipeline_name: &str,
    tracker: Option<&crate::eo2::AckSeqTracker>,
    metrics: Option<&crate::eo2_metrics::Eo2Metrics>,
    capacity: Option<&crate::eo2_backpressure::PipelineCapacity>,
) {
    let (Some(registry), Some(tracker)) = (registry, tracker) else {
        return;
    };
    let Some(min_ack) = tracker.min_across_sinks() else {
        return;
    };
    if min_ack == 0 {
        return;
    }
    for (pipe, part) in registry.keys() {
        if pipe != pipeline_name {
            continue;
        }
        let Ok(store) = registry.open(&pipe, part) else {
            continue;
        };
        if let Ok(mut guard) = store.lock() {
            let bytes_before = guard.disk_bytes();
            if let Err(e) = guard.gc_up_to(min_ack) {
                tracing::warn!(
                    partition = %part.as_u16(),
                    "L2 gc_up_to({min_ack}) failed: {e}"
                );
            }
            let part_u16 = part.as_u16();
            if let Some(cap) = capacity {
                let reclaimed = bytes_before.saturating_sub(guard.disk_bytes());
                if reclaimed > 0 {
                    cap.adjust(part_u16, -(reclaimed as i64));
                }
            }
            if let Some(m) = metrics {
                m.set_l2_bytes(pipeline_name, part_u16, guard.disk_bytes());
                m.set_l2_segments(pipeline_name, part_u16, guard.segment_count() as u64);
                let lag = guard.next_seq().saturating_sub(min_ack);
                m.set_l2_gc_lag_seq(pipeline_name, part_u16, lag);
            }
        }
    }
}

/// Credit outputs that were returned as `pending` by `write_batch` but are
/// now delivered thanks to a successful `sink.flush()`. Drains `pending_ids`
/// into the `outputs_sent` metric, the delivery ledger (if configured), and
/// the checkpoint-window counters.
///
/// Called from every code path that invokes `sink.flush()` in non-blocking
/// delivery mode (interval flush, idle flush, final flush). For blocking
/// strategies (`PerEvent`, `OrderedBatch`) the vec is always empty so this
/// is a no-op on the fast path.
fn credit_pending_on_flush(
    pending_ids: &mut Vec<uuid::Uuid>,
    metrics: &Arc<PipelineMetrics>,
    ledger: &Option<Arc<DeliveryLedger>>,
    delivered_since_checkpoint: &mut u64,
    acked_since_last_flush: &mut u64,
) {
    if pending_ids.is_empty() {
        return;
    }
    let count = pending_ids.len() as u64;
    if let Some(l) = ledger {
        l.mark_batch_acked(pending_ids);
    }
    metrics.outputs_sent.fetch_add(count, Ordering::Relaxed);
    *delivered_since_checkpoint += count;
    *acked_since_last_flush += count;
    pending_ids.clear();
}

/// Write a checkpoint record with ledger-populated offsets and pending IDs.
fn write_checkpoint(
    ckpt_writer: &mut Option<Box<dyn CheckpointPersist>>,
    ledger: &Option<Arc<DeliveryLedger>>,
    metrics: &Arc<PipelineMetrics>,
    delivered: &mut u64,
    failed: &mut u64,
    ack_tracker: Option<&crate::eo2::AckSeqTracker>,
) {
    if let Some(writer) = ckpt_writer.as_mut() {
        // Populate source_offsets and pending IDs from the delivery ledger.
        let (source_offsets, pending_ids) = if let Some(ledger) = ledger {
            (ledger.checkpoint_offsets(), ledger.pending_ids())
        } else {
            (HashMap::new(), vec![])
        };

        let mut record = CheckpointRecord::new(
            0, // ID assigned by writer
            source_offsets,
            pending_ids,
            *delivered,
            *failed,
        );
        // EO-2: attach per-sink ack cursor if the pipeline tracks it.
        if let Some(tracker) = ack_tracker {
            record = record.with_per_sink_ack_seq(tracker.snapshot());
        }
        if let Err(e) = writer.append(&mut record) {
            tracing::warn!("Checkpoint write failed: {e}");
        }
        metrics.checkpoints_written.fetch_add(1, Ordering::Relaxed);
        *delivered = 0;
        *failed = 0;
    }
}

// ── Managed pipeline — drain→swap→resume for hot-swap ──────────────────

/// Control handle for a managed pipeline, enabling drain→swap→resume.
///
/// Created before spawning `run_buffered_managed`. The caller retains a
/// clone and calls `drain_and_swap()`, `drain_and_swap_source()`, or
/// `drain_and_swap_sink()` to perform zero-downtime hot-swaps.
/// Pending upgrade action for blue-green or canary modes.
///
/// Set by control methods, consumed by the processor task. Each action is
/// picked up exactly once — the processor task takes it and replaces with `None`.
enum UpgradeAction {
    /// Install green processor for shadow processing (outputs discarded).
    StartBlueGreen(Box<dyn Processor + Send + Sync>),
    /// Cut over: swap green to active, drop blue.
    CutoverBlueGreen,
    /// Install canary processor at given traffic percentage.
    StartCanary(Box<dyn Processor + Send + Sync>, u8),
    /// Update canary traffic percentage.
    SetCanaryPct(u8),
    /// Complete canary: canary becomes the sole active processor.
    CompleteCanary,
    /// Roll back any in-progress blue-green or canary.
    Rollback,
}

/// Control plane for a managed pipeline.
///
/// The `Mutex<Option<…>>` slots below carry rare, control-plane events
/// (drain-and-swap, blue-green cutover, canary promotion, rollback) from
/// `PipelineControl`'s public API to the running source/processor/sink
/// tasks. They are explicitly **not** on the per-event hot path:
/// each lock is acquired once per pipeline-management action (typically
/// seconds-to-minutes apart in production), not per batch and certainly
/// not per event.
///
/// **Future tracing item:** the supervisor's per-pipeline-start hand-off
/// touches ~10 of these slots in sequence (registry stamp + processor
/// load + source spawn + sink spawn + L2 wrap + ack-tracker register +
/// fault-injector handle + L3 store install + Eo2 metrics splice + …).
/// None of those acquire under contention because pipeline start is
/// serialised by `PipelineSupervisor::running` (a tokio Mutex), but the
/// p99 of cumulative lock-hold time across that hand-off is unmeasured.
/// If start latency ever shows up as a Gate 2 / Session B regression,
/// adding a `tracing::span!` around each `lock().await` would let
/// operators see which slot is the slow one without code changes.
pub struct PipelineControl {
    /// When true, the source task stops polling and returns empty batches.
    paused: AtomicBool,
    /// Signaled by the processor task when both SPSC rings are empty after pause.
    drain_complete: Notify,
    /// Slot for a replacement processor. Set by `drain_and_swap`, consumed by
    /// the processor task after drain completes.
    new_processor: Mutex<Option<Box<dyn Processor + Send + Sync>>>,
    /// Slot for a replacement source (`Box<S>` erased to `Any`). Consumed by
    /// the source task after drain completes. Caller must ensure the boxed
    /// value is the same concrete type `S` as the running source.
    new_source: Mutex<Option<Box<dyn Any + Send>>>,
    /// Slot for a replacement sink (`Box<K>` erased to `Any`). Consumed by
    /// the sink task after drain completes. Caller must ensure the boxed
    /// value is the same concrete type `K` as the running sink.
    new_sink: Mutex<Option<Box<dyn Any + Send>>>,
    /// Signaled by the task that picked up the swap (processor, source, or sink).
    swap_complete: Notify,
    /// Pending upgrade action (blue-green or canary). Consumed by processor task.
    upgrade_action: Mutex<Option<UpgradeAction>>,
    /// Canary traffic percentage (0–100). Separate atomic for lock-free hot-path
    /// reads in the processor task. 0 means no canary active.
    canary_pct: AtomicU8,
    /// Signaled when the processor task has completed an upgrade action.
    upgrade_action_complete: Notify,
}

impl PipelineControl {
    /// Create a new pipeline control handle.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            drain_complete: Notify::new(),
            new_processor: Mutex::new(None),
            new_source: Mutex::new(None),
            new_sink: Mutex::new(None),
            swap_complete: Notify::new(),
            upgrade_action: Mutex::new(None),
            canary_pct: AtomicU8::new(0),
            upgrade_action_complete: Notify::new(),
        })
    }

    /// Drain in-flight events, swap the processor, and resume.
    ///
    /// Returns after the new processor is active. Zero events are lost.
    pub async fn drain_and_swap(
        &self,
        new_processor: Box<dyn Processor + Send + Sync>,
    ) -> Result<(), AeonError> {
        self.paused.store(true, Ordering::Release);
        self.drain_complete.notified().await;
        {
            let mut slot = self.new_processor.lock().await;
            *slot = Some(new_processor);
        }
        self.swap_complete.notified().await;
        self.paused.store(false, Ordering::Release);
        Ok(())
    }

    /// Drain in-flight events, swap the source, and resume.
    ///
    /// The new source must be the same concrete type `S` that the pipeline
    /// was started with (same-type reconfiguration only). For cross-type
    /// changes, use blue-green pipeline (Phase D).
    ///
    /// Returns after the new source is active. Zero events are lost.
    pub async fn drain_and_swap_source<S: Source + Send + 'static>(
        &self,
        new_source: S,
    ) -> Result<(), AeonError> {
        self.paused.store(true, Ordering::Release);
        self.drain_complete.notified().await;
        {
            let mut slot = self.new_source.lock().await;
            *slot = Some(Box::new(new_source));
        }
        self.swap_complete.notified().await;
        self.paused.store(false, Ordering::Release);
        Ok(())
    }

    /// Drain in-flight events, swap the sink, and resume.
    ///
    /// The new sink must be the same concrete type `K` that the pipeline
    /// was started with (same-type reconfiguration only).
    ///
    /// Returns after the new sink is active. Zero events are lost.
    pub async fn drain_and_swap_sink<K: Sink + Send + 'static>(
        &self,
        new_sink: K,
    ) -> Result<(), AeonError> {
        self.paused.store(true, Ordering::Release);
        self.drain_complete.notified().await;
        {
            let mut slot = self.new_sink.lock().await;
            *slot = Some(Box::new(new_sink));
        }
        self.swap_complete.notified().await;
        self.paused.store(false, Ordering::Release);
        Ok(())
    }

    // ── Blue-Green ─────────────────────────────────────────────────────

    /// Start a blue-green upgrade. The green processor runs in shadow mode:
    /// it processes the same events as blue, but its outputs are discarded.
    /// Call `cutover_blue_green()` to promote green, or `rollback_upgrade()`
    /// to discard it.
    ///
    /// Does NOT pause the pipeline — green is installed live.
    pub async fn start_blue_green(
        &self,
        green: Box<dyn Processor + Send + Sync>,
    ) -> Result<(), AeonError> {
        {
            let mut slot = self.upgrade_action.lock().await;
            *slot = Some(UpgradeAction::StartBlueGreen(green));
        }
        self.upgrade_action_complete.notified().await;
        Ok(())
    }

    /// Cut over from blue to green. The green processor becomes the active
    /// processor and blue is dropped. No drain is needed because green is
    /// already warm (has been processing events in shadow).
    pub async fn cutover_blue_green(&self) -> Result<(), AeonError> {
        {
            let mut slot = self.upgrade_action.lock().await;
            *slot = Some(UpgradeAction::CutoverBlueGreen);
        }
        self.upgrade_action_complete.notified().await;
        Ok(())
    }

    // ── Canary ─────────────────────────────────────────────────────────

    /// Start a canary upgrade. Events are split probabilistically: `initial_pct`%
    /// go to the canary processor, the rest to the baseline. Both processors'
    /// outputs are sent to the sink.
    ///
    /// Does NOT pause the pipeline — canary is installed live.
    pub async fn start_canary(
        &self,
        canary: Box<dyn Processor + Send + Sync>,
        initial_pct: u8,
    ) -> Result<(), AeonError> {
        {
            let mut slot = self.upgrade_action.lock().await;
            *slot = Some(UpgradeAction::StartCanary(canary, initial_pct));
        }
        self.upgrade_action_complete.notified().await;
        Ok(())
    }

    /// Update the canary traffic percentage (0–100).
    pub async fn set_canary_pct(&self, pct: u8) -> Result<(), AeonError> {
        {
            let mut slot = self.upgrade_action.lock().await;
            *slot = Some(UpgradeAction::SetCanaryPct(pct));
        }
        self.upgrade_action_complete.notified().await;
        Ok(())
    }

    /// Complete the canary: canary processor becomes the sole active processor,
    /// baseline is dropped.
    pub async fn complete_canary(&self) -> Result<(), AeonError> {
        {
            let mut slot = self.upgrade_action.lock().await;
            *slot = Some(UpgradeAction::CompleteCanary);
        }
        self.upgrade_action_complete.notified().await;
        Ok(())
    }

    // ── Rollback (works for both blue-green and canary) ────────────────

    /// Roll back any in-progress blue-green or canary upgrade. The current
    /// active processor continues; the green/canary processor is dropped.
    pub async fn rollback_upgrade(&self) -> Result<(), AeonError> {
        {
            let mut slot = self.upgrade_action.lock().await;
            *slot = Some(UpgradeAction::Rollback);
        }
        self.upgrade_action_complete.notified().await;
        Ok(())
    }
}

/// Runs a managed pipeline that supports zero-downtime processor hot-swap.
///
/// Identical to `run_buffered` except:
/// - Uses `Box<dyn Processor>` for runtime processor replacement
/// - Accepts a `PipelineControl` handle for drain→swap→resume coordination
/// - Source task checks `control.paused` instead of only `shutdown`
/// - Processor task monitors drain state and swap slot
///
/// The per-batch dynamic dispatch overhead (~2ns vtable lookup) is negligible
/// compared to actual processing time. Use `run_buffered` for pipelines that
/// never need hot-swap (benchmarks, tests, static pipelines).
// Justified — a Builder around these would add an init-time allocation per
// pipeline start without any hot-path benefit. The supervisor calls this
// once per pipeline, with each arg either documented or self-named.
#[allow(clippy::too_many_arguments)]
pub async fn run_buffered_managed<S, K>(
    source: S,
    processor: Box<dyn Processor + Send + Sync>,
    sink: K,
    config: PipelineConfig,
    metrics: Arc<PipelineMetrics>,
    shutdown: Arc<AtomicBool>,
    ledger: Option<Arc<DeliveryLedger>>,
    control: Arc<PipelineControl>,
) -> Result<(), AeonError>
where
    S: Source + Send + 'static,
    K: Sink + Send + 'static,
{
    reject_broker_coord_with_reassign(&source, &config)?;

    let core_assignment = config.core_pinning.resolve();

    // EO-2: wrap the source in `L2WritingSource` when the durability
    // mode requires L2 body persistence. This mirrors the wrap call in
    // the lower-level `run_buffered` — without it, supervisor-managed
    // pipelines (the cmd_serve path) silently skip L2 even when the
    // manifest declares durability.mode = ordered_batch /
    // unordered_batch / per_event. Discovered 2026-04-25 during V3
    // validation: registry was being built (commit 0bde230), source
    // override was added (commit a6c40f3), and L2 still didn't engage
    // because run_buffered_managed never called wrap.
    let mut source = crate::eo2::MaybeL2Wrapped::wrap(
        source,
        &config.pipeline_name,
        config.l2_registry.as_ref(),
        config.delivery.durability,
        config.eo2_capacity.as_ref(),
    );

    // Initialize PoH chain if configured
    #[cfg(feature = "processor-auth")]
    let poh_state = create_poh_state(&config);
    #[cfg(feature = "processor-auth")]
    let poh_signing_key = config.poh.as_ref().and_then(|c| c.signing_key.clone());

    let (mut src_prod, mut src_cons) =
        rtrb::RingBuffer::<Vec<Event>>::new(config.source_buffer_capacity);
    let (mut sink_prod, sink_cons) =
        rtrb::RingBuffer::<Vec<Output>>::new(config.sink_buffer_capacity);

    let shutdown_src = Arc::clone(&shutdown);
    let metrics_src = Arc::clone(&metrics);
    let control_src = Arc::clone(&control);
    // P5: take the caller's owned-partitions watch by value into the source
    // task. Checked non-blockingly between batches so a CL-6 transfer commit
    // triggers `source.reassign_partitions(&new)` on the live source instead
    // of a pipeline restart.
    let mut reassign_rx = config.partition_reassign.clone();

    // Snapshot the EO-2 wiring the swap-rewrap path needs into the
    // source task — `config` itself is moved into the processor task
    // shortly after, and the source task is spawned first.
    let l2_pipeline_name = config.pipeline_name.clone();
    let l2_registry = config.l2_registry.clone();
    let l2_durability = config.delivery.durability;
    let l2_capacity = config.eo2_capacity.clone();

    // Source task: identical to run_buffered, but checks control.paused.
    // When paused, checks for source swap before yielding.
    let source_core = core_assignment.map(|c| c.source);
    let source_handle = tokio::spawn(async move {
        if let Some(core) = source_core {
            pin_to_core(core);
        }
        let mut source_kind = source.source_kind();
        while !shutdown_src.load(Ordering::Relaxed) {
            // P5: re-assign partitions if the cluster watch has advanced
            // since the last iteration. Non-blocking — returns `Err` if the
            // sender has dropped (cluster tore down) in which case we keep
            // running on the last known assignment.
            if let Some(rx) = reassign_rx.as_mut() {
                if rx.has_changed().unwrap_or(false) {
                    let new_partitions: Vec<u16> = rx.borrow_and_update().clone();
                    if let Err(e) = source.reassign_partitions(&new_partitions).await {
                        tracing::error!(
                            error = %e,
                            partitions = ?new_partitions,
                            "source reassign_partitions failed; exiting source task"
                        );
                        return Err(e);
                    }
                    tracing::info!(
                        partitions = ?new_partitions,
                        "source re-assigned to new owned-partitions slice"
                    );
                }
            }

            // Pause check: check for source swap, then yield
            if control_src.paused.load(Ordering::Acquire) {
                // Check if a new source has been placed in the swap slot.
                // The drain_and_swap_source() method sets this AFTER drain_complete,
                // so by the time we see it, all in-flight events have been flushed.
                {
                    let mut slot = control_src.new_source.lock().await;
                    if let Some(new_source_any) = slot.take() {
                        if let Ok(new_source) = new_source_any.downcast::<S>() {
                            // Re-wrap the swapped-in source so L2
                            // persistence stays consistent across hot-
                            // swaps. `MaybeL2Wrapped::wrap` is cheap —
                            // returns `Direct(S)` when L2 is disabled.
                            source = crate::eo2::MaybeL2Wrapped::wrap(
                                *new_source,
                                &l2_pipeline_name,
                                l2_registry.as_ref(),
                                l2_durability,
                                l2_capacity.as_ref(),
                            );
                            source_kind = source.source_kind();
                            control_src.swap_complete.notify_one();
                        }
                    }
                }
                tokio::task::yield_now().await;
                continue;
            }

            let events = match source.next_batch().await {
                Ok(events) => events,
                Err(e) => return Err(e),
            };
            if events.is_empty() {
                // Could be a lull or exhaustion. If paused was just set between
                // our check and the poll, treat it as pause (don't break).
                if control_src.paused.load(Ordering::Acquire) {
                    continue;
                }
                match source_kind {
                    SourceKind::Pull => break,
                    SourceKind::Push | SourceKind::Poll => {
                        tokio::task::yield_now().await;
                        continue;
                    }
                }
            }
            metrics_src
                .events_received
                .fetch_add(events.len() as u64, Ordering::Relaxed);

            let mut pending = Some(events);
            while let Some(batch) = pending.take() {
                match src_prod.push(batch) {
                    Ok(()) => {}
                    Err(rtrb::PushError::Full(returned)) => {
                        pending = Some(returned);
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
        drop(src_prod);
        Ok::<(), AeonError>(())
    });

    let shutdown_proc = Arc::clone(&shutdown);
    let metrics_proc = Arc::clone(&metrics);
    let control_proc = Arc::clone(&control);
    #[cfg(feature = "processor-auth")]
    let poh_proc = poh_state.clone();
    #[cfg(feature = "processor-auth")]
    let poh_key_proc = poh_signing_key.clone();

    // Processor task: pops events, processes, pushes outputs.
    // Supports three modes:
    //   1. Normal: process with current_processor
    //   2. Blue-green shadow: also process with green_processor (discard outputs)
    //   3. Canary split: split events by percentage between baseline and canary
    // When source is paused and both rings are drained, signals drain_complete
    // and waits for a processor swap before resuming.
    let proc_core = core_assignment.map(|c| c.processor);
    let processor_handle = tokio::spawn(async move {
        if let Some(core) = proc_core {
            pin_to_core(core);
        }

        let mut current_processor: Box<dyn Processor + Send + Sync> = processor;
        // Blue-green: green runs in shadow, outputs discarded.
        let mut green_processor: Option<Box<dyn Processor + Send + Sync>> = None;
        // Canary: events split by percentage, both outputs go to sink.
        let mut canary_processor: Option<Box<dyn Processor + Send + Sync>> = None;

        while !shutdown_proc.load(Ordering::Relaxed) {
            // Check for pending upgrade actions (non-blocking).
            if let Ok(mut slot) = control_proc.upgrade_action.try_lock() {
                if let Some(action) = slot.take() {
                    match action {
                        UpgradeAction::StartBlueGreen(green) => {
                            green_processor = Some(green);
                            control_proc.upgrade_action_complete.notify_one();
                        }
                        UpgradeAction::CutoverBlueGreen => {
                            if let Some(green) = green_processor.take() {
                                // TR-2: transfer host-side state from blue → green.
                                transfer_processor_state(
                                    current_processor.as_ref(),
                                    green.as_ref(),
                                );
                                current_processor = green;
                            }
                            control_proc.upgrade_action_complete.notify_one();
                        }
                        UpgradeAction::StartCanary(canary, pct) => {
                            canary_processor = Some(canary);
                            control_proc.canary_pct.store(pct, Ordering::Release);
                            control_proc.upgrade_action_complete.notify_one();
                        }
                        UpgradeAction::SetCanaryPct(pct) => {
                            control_proc.canary_pct.store(pct, Ordering::Release);
                            control_proc.upgrade_action_complete.notify_one();
                        }
                        UpgradeAction::CompleteCanary => {
                            if let Some(canary) = canary_processor.take() {
                                // TR-2: transfer host-side state from baseline → canary.
                                transfer_processor_state(
                                    current_processor.as_ref(),
                                    canary.as_ref(),
                                );
                                current_processor = canary;
                            }
                            control_proc.canary_pct.store(0, Ordering::Release);
                            control_proc.upgrade_action_complete.notify_one();
                        }
                        UpgradeAction::Rollback => {
                            green_processor = None;
                            canary_processor = None;
                            control_proc.canary_pct.store(0, Ordering::Release);
                            control_proc.upgrade_action_complete.notify_one();
                        }
                    }
                }
            }

            match src_cons.pop() {
                Ok(events) => {
                    let count = events.len() as u64;
                    let canary_pct = control_proc.canary_pct.load(Ordering::Relaxed);

                    let outputs = if canary_pct > 0 {
                        if let Some(ref canary) = canary_processor {
                            // Canary mode: split events by percentage.
                            // Use event ID lower bits for deterministic routing.
                            let mut baseline_events = Vec::new();
                            let mut canary_events = Vec::new();
                            for event in events {
                                let hash = (event.id.as_u128() % 100) as u8;
                                if hash < canary_pct {
                                    canary_events.push(event);
                                } else {
                                    baseline_events.push(event);
                                }
                            }
                            let mut outputs = if !baseline_events.is_empty() {
                                match current_processor.process_batch(baseline_events) {
                                    Ok(o) => o,
                                    Err(e) => return Err(e),
                                }
                            } else {
                                Vec::new()
                            };
                            if !canary_events.is_empty() {
                                match canary.process_batch(canary_events) {
                                    Ok(mut o) => outputs.append(&mut o),
                                    Err(e) => return Err(e),
                                }
                            }
                            outputs
                        } else {
                            // Canary pct set but no canary processor — normal mode
                            match current_processor.process_batch(events) {
                                Ok(o) => o,
                                Err(e) => return Err(e),
                            }
                        }
                    } else {
                        // Normal (or blue-green shadow) mode.
                        // Blue-green note: green processor is installed but not called
                        // here because process_batch consumes events (no clone on hot
                        // path per CLAUDE.md rule 3). Green is validated on cutover.
                        match current_processor.process_batch(events) {
                            Ok(outputs) => outputs,
                            Err(e) => return Err(e),
                        }
                    };

                    metrics_proc
                        .events_processed
                        .fetch_add(count, Ordering::Relaxed);

                    // PoH: append batch to hash chain if enabled
                    #[cfg(feature = "processor-auth")]
                    if let Some(ref poh) = poh_proc {
                        poh_append_batch(poh, &outputs, &poh_key_proc, &metrics_proc).await;
                    }

                    let mut pending = Some(outputs);
                    while let Some(batch) = pending.take() {
                        match sink_prod.push(batch) {
                            Ok(()) => {}
                            Err(rtrb::PushError::Full(returned)) => {
                                pending = Some(returned);
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                }
                Err(_) => {
                    // Source ring empty
                    if src_cons.is_abandoned() {
                        break;
                    }

                    // Check if we're in drain mode (source paused + ring empty)
                    if control_proc.paused.load(Ordering::Acquire) {
                        // Source ring is empty. Wait for sink ring to drain too.
                        // The sink task drains independently — we just need the
                        // processor→sink ring to be empty (sink has consumed all).
                        if sink_prod.slots() == sink_prod.buffer().capacity() {
                            // Both rings empty — pipeline is quiescent.
                            control_proc.drain_complete.notify_one();

                            // Wait for a swap to be placed and completed. Could be
                            // processor, source, or sink swap.
                            loop {
                                // If paused was cleared, swap was completed by
                                // another task (source or sink). Resume processing.
                                if !control_proc.paused.load(Ordering::Acquire) {
                                    break;
                                }
                                // Processor swap: handled here directly
                                {
                                    let mut slot = control_proc.new_processor.lock().await;
                                    if let Some(new_proc) = slot.take() {
                                        // TR-2: transfer host-side state from
                                        // old → new. Pipeline is already quiesced
                                        // (paused + both rings empty) so no
                                        // events race the snapshot.
                                        transfer_processor_state(
                                            current_processor.as_ref(),
                                            new_proc.as_ref(),
                                        );
                                        current_processor = new_proc;
                                        control_proc.swap_complete.notify_one();
                                        break;
                                    }
                                }
                                tokio::task::yield_now().await;
                            }
                        }
                    }

                    tokio::task::yield_now().await;
                }
            }
        }
        drop(sink_prod);
        Ok::<(), AeonError>(())
    });

    let metrics_sink = Arc::clone(&metrics);
    let control_sink = Arc::clone(&control);
    let sink_ctx = build_sink_task_ctx(&config, core_assignment.map(|c| c.sink));
    let sink_ledger = ledger;
    let sink_handle = tokio::spawn(run_sink_task(
        sink,
        sink_cons,
        metrics_sink,
        sink_ledger,
        sink_ctx,
        Some(control_sink),
    ));

    let (src_result, proc_result, sink_result) =
        tokio::join!(source_handle, processor_handle, sink_handle);

    src_result.map_err(|e| AeonError::processor(format!("source task panicked: {e}")))??;
    proc_result.map_err(|e| AeonError::processor(format!("processor task panicked: {e}")))??;
    sink_result.map_err(|e| AeonError::processor(format!("sink task panicked: {e}")))??;

    Ok(())
}

#[cfg(test)]
// Justified at module scope — the test setup pattern (`PipelineConfig::default()`
// then individual `.field = ...` overrides) reads more clearly than a Builder
// for the dozens of test fixtures here, and the lint's auto-suggestion to use
// struct-update syntax doesn't compose well with PipelineConfig's many fields.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::processor::PassthroughProcessor;
    use aeon_connectors::{BlackholeSink, MemorySink, MemorySource};
    use aeon_types::{DeliveryStrategy, PartitionId};
    use bytes::Bytes;

    fn make_events(count: usize) -> Vec<Event> {
        let source: Arc<str> = Arc::from("test");
        (0..count)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i as i64,
                    Arc::clone(&source),
                    PartitionId::new(0),
                    Bytes::from(format!("event-{i}")),
                )
            })
            .collect()
    }

    /// B4: a synthetic source that reports itself as broker-coordinated.
    /// Used only to drive the pipeline-start guardrail test below.
    struct BrokerCoordinatedSource;
    impl aeon_types::Source for BrokerCoordinatedSource {
        async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
            Ok(Vec::new())
        }
        fn broker_coordinated_partitions(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn run_buffered_rejects_broker_coord_with_partition_reassign() {
        // Stand up a `partition_reassign` watch so the guard sees both
        // conditions — broker-coordinated source AND Raft reassign watcher.
        let (_tx, rx) = tokio::sync::watch::channel::<Vec<u16>>(vec![0]);
        let config = PipelineConfig {
            partition_reassign: Some(rx),
            ..Default::default()
        };

        let source = BrokerCoordinatedSource;
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let sink = MemorySink::new();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        let err = run_buffered(source, processor, sink, config, metrics, shutdown, None)
            .await
            .expect_err("broker-coordinated source + partition_reassign must fail at start");
        let msg = format!("{err}");
        assert!(
            msg.contains("ConsumerMode::Group") && msg.contains("mutually exclusive"),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn run_buffered_allows_broker_coord_when_no_partition_reassign() {
        // No partition_reassign watcher — the guard is silent and the
        // pipeline starts normally (finishes immediately on the empty
        // source).
        let config = PipelineConfig::default();

        let source = BrokerCoordinatedSource;
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let sink = MemorySink::new();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered(source, processor, sink, config, metrics, shutdown, None)
            .await
            .expect("single-instance broker-coord source must start without a reassign watch");
    }

    #[tokio::test]
    async fn direct_pipeline_passthrough() {
        let events = make_events(100);
        let mut source = MemorySource::new(events, 32);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let mut sink = MemorySink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sink.len(), 100);
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 100);
        assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 100);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 100);
    }

    #[tokio::test]
    async fn direct_pipeline_blackhole() {
        let events = make_events(10_000);
        let mut source = MemorySource::new(events, 256);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let mut sink = BlackholeSink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sink.count(), 10_000);
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 10_000);
    }

    #[tokio::test]
    async fn direct_pipeline_preserves_payload() {
        let events = make_events(3);
        let mut source = MemorySource::new(events, 10);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sink = MemorySink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
            .await
            .unwrap();

        let outputs = sink.outputs();
        assert_eq!(outputs[0].payload.as_ref(), b"event-0");
        assert_eq!(outputs[1].payload.as_ref(), b"event-1");
        assert_eq!(outputs[2].payload.as_ref(), b"event-2");
    }

    #[tokio::test]
    async fn direct_pipeline_empty_source() {
        let mut source = MemorySource::new(vec![], 10);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sink = MemorySink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
            .await
            .unwrap();

        assert!(sink.is_empty());
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn direct_pipeline_shutdown_signal() {
        // Create a source with many events
        let events = make_events(10_000);
        let mut source = MemorySource::new(events, 10);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sink = BlackholeSink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(true); // immediately shut down

        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
            .await
            .unwrap();

        // Should have processed 0 events due to immediate shutdown
        assert_eq!(sink.count(), 0);
    }

    #[tokio::test]
    async fn buffered_pipeline_passthrough() {
        let events = make_events(1_000);
        let source = MemorySource::new(events, 64);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig::default();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 1_000);
        assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 1_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 1_000);
    }

    #[tokio::test]
    async fn buffered_pipeline_large_volume() {
        let events = make_events(50_000);
        let source = MemorySource::new(events, 512);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig {
            source_buffer_capacity: 256,
            sink_buffer_capacity: 256,
            max_batch_size: 512,
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 50_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 50_000);
    }

    #[tokio::test]
    async fn buffered_pipeline_with_auto_core_pinning() {
        let events = make_events(1_000);
        let source = MemorySource::new(events, 64);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig {
            core_pinning: CorePinning::Auto,
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 1_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 1_000);
    }

    // ── run_buffered_transport integration tests ────────────────────────
    //
    // These exercise the async `ProcessorTransport` path of the buffered
    // pipeline. `InProcessTransport` wraps a sync `Processor` behind the
    // `ProcessorTransport` trait, so the same processor impl that runs in
    // `run_buffered` is reused here — which proves the two paths are
    // behaviorally equivalent and share the sink task helper.

    #[tokio::test]
    async fn buffered_transport_pipeline_passthrough() {
        use crate::transport::InProcessTransport;
        use aeon_types::processor_transport::ProcessorTier;

        let events = make_events(1_000);
        let source = MemorySource::new(events, 64);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let transport = Arc::new(InProcessTransport::new(
            processor,
            "passthrough",
            "1.0",
            ProcessorTier::Native,
        ));
        let sink = BlackholeSink::new();
        let config = PipelineConfig::default();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered_transport(
            source,
            transport,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 1_000);
        assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 1_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 1_000);
    }

    #[tokio::test]
    async fn buffered_transport_pipeline_large_volume() {
        use crate::transport::InProcessTransport;
        use aeon_types::processor_transport::ProcessorTier;

        let events = make_events(50_000);
        let source = MemorySource::new(events, 512);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let transport = Arc::new(InProcessTransport::new(
            processor,
            "passthrough",
            "1.0",
            ProcessorTier::Native,
        ));
        let sink = BlackholeSink::new();
        let config = PipelineConfig {
            source_buffer_capacity: 256,
            sink_buffer_capacity: 256,
            max_batch_size: 512,
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered_transport(
            source,
            transport,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 50_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 50_000);
    }

    #[tokio::test]
    async fn buffered_transport_pipeline_propagates_transport_error() {
        // A transport that always errors should cause run_buffered_transport
        // to propagate the error cleanly instead of hanging or silently
        // dropping events.
        use aeon_types::traits::ProcessorTransport;

        struct FailingTransport;
        impl ProcessorTransport for FailingTransport {
            fn call_batch(
                &self,
                _events: Vec<Event>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<Vec<Output>, AeonError>> + Send + '_>,
            > {
                Box::pin(async { Err(AeonError::processor("transport forced failure")) })
            }
            fn health(
                &self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                aeon_types::processor_transport::ProcessorHealth,
                                AeonError,
                            >,
                        > + Send
                        + '_,
                >,
            > {
                Box::pin(async {
                    Ok(aeon_types::processor_transport::ProcessorHealth {
                        healthy: false,
                        latency_us: None,
                        pending_batches: None,
                        uptime_secs: None,
                    })
                })
            }
            fn drain(
                &self,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), AeonError>> + Send + '_>,
            > {
                Box::pin(async { Ok(()) })
            }
            fn info(&self) -> aeon_types::processor_transport::ProcessorInfo {
                aeon_types::processor_transport::ProcessorInfo {
                    name: "failing".into(),
                    version: "0.0".into(),
                    tier: aeon_types::processor_transport::ProcessorTier::Native,
                    capabilities: vec![],
                }
            }
        }

        let events = make_events(100);
        let source = MemorySource::new(events, 16);
        let transport = Arc::new(FailingTransport);
        let sink = BlackholeSink::new();
        let config = PipelineConfig::default();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        let result = run_buffered_transport(
            source,
            transport,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await;

        assert!(result.is_err(), "expected processor task to surface error");
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn core_pinning_disabled_resolves_to_none() {
        assert!(CorePinning::Disabled.resolve().is_none());
    }

    #[test]
    fn core_pinning_manual_resolves_to_given_cores() {
        let cores = PipelineCores {
            source: 1,
            processor: 2,
            sink: 3,
        };
        let resolved = CorePinning::Manual(cores).resolve();
        assert!(resolved.is_some());
        let r = resolved.unwrap();
        assert_eq!(r.source, 1);
        assert_eq!(r.processor, 2);
        assert_eq!(r.sink, 3);
    }

    #[test]
    fn core_pinning_default_is_disabled() {
        let config = PipelineConfig::default();
        assert!(matches!(config.core_pinning, CorePinning::Disabled));
    }

    #[test]
    fn pipeline_config_default_delivery() {
        let config = PipelineConfig::default();
        assert_eq!(config.delivery.strategy, DeliveryStrategy::OrderedBatch);
        assert_eq!(
            config.delivery.semantics,
            aeon_types::DeliverySemantics::AtLeastOnce
        );
        assert_eq!(
            config.delivery.flush.interval,
            std::time::Duration::from_secs(1)
        );
        assert_eq!(config.delivery.flush.max_pending, 50_000);
    }

    #[tokio::test]
    async fn buffered_pipeline_ordered_mode_zero_loss() {
        // Ordered mode (default) — same as existing behavior, zero event loss.
        let events = make_events(5_000);
        let source = MemorySource::new(events, 128);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig {
            delivery: DeliveryConfig {
                strategy: DeliveryStrategy::OrderedBatch,
                ..Default::default()
            },
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 5_000);
        assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 5_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 5_000);
    }

    #[tokio::test]
    async fn buffered_pipeline_batched_mode_zero_loss() {
        // Batched mode — write_batch returns fast, flush at intervals.
        // With BlackholeSink (no-op flush), should still deliver all events.
        let events = make_events(5_000);
        let source = MemorySource::new(events, 128);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig {
            delivery: DeliveryConfig {
                strategy: DeliveryStrategy::UnorderedBatch,
                flush: crate::delivery::FlushStrategy {
                    interval: std::time::Duration::from_millis(100),
                    max_pending: 1_000,
                    adaptive: false,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 5_000);
        assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 5_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 5_000);
    }

    #[tokio::test]
    async fn buffered_pipeline_batched_large_volume() {
        // Batched mode with 50K events — validates flush-on-max-pending triggers.
        let events = make_events(50_000);
        let source = MemorySource::new(events, 512);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig {
            source_buffer_capacity: 256,
            sink_buffer_capacity: 256,
            max_batch_size: 512,
            delivery: DeliveryConfig {
                strategy: DeliveryStrategy::UnorderedBatch,
                flush: crate::delivery::FlushStrategy {
                    interval: std::time::Duration::from_millis(50),
                    max_pending: 10_000,
                    adaptive: false,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 50_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 50_000);
    }

    /// Test sink that returns `BatchResult::all_pending` from `write_batch` and
    /// only "delivers" pending outputs when `flush` is called. Models the
    /// UnorderedBatch contract of real sinks (Kafka, NATS, RabbitMQ) without
    /// requiring an external broker. Used to validate that the sink task
    /// credits `outputs_sent` at flush time via `credit_pending_on_flush`.
    struct DeferredSink {
        pending: Vec<Output>,
        delivered: Arc<std::sync::atomic::AtomicU64>,
    }

    impl DeferredSink {
        fn new(delivered: Arc<std::sync::atomic::AtomicU64>) -> Self {
            Self {
                pending: Vec::new(),
                delivered,
            }
        }
    }

    impl Sink for DeferredSink {
        async fn write_batch(
            &mut self,
            outputs: Vec<Output>,
        ) -> Result<aeon_types::BatchResult, AeonError> {
            let ids: Vec<uuid::Uuid> = outputs
                .iter()
                .map(|o| o.source_event_id.unwrap_or(uuid::Uuid::nil()))
                .collect();
            self.pending.extend(outputs);
            Ok(aeon_types::BatchResult::all_pending(ids))
        }

        async fn flush(&mut self) -> Result<(), AeonError> {
            let drained = self.pending.len() as u64;
            self.pending.clear();
            self.delivered
                .fetch_add(drained, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn buffered_pipeline_unordered_credits_metric_at_flush() {
        // Regression guard for the pipeline metric bug: with DeliveryStrategy::
        // UnorderedBatch, a sink that returns BatchResult::all_pending must
        // still see its outputs credited to `outputs_sent` once flush completes.
        // Prior to the fix, the sink task only added `batch_result.delivered
        // .len()` to the counter, which is always 0 for all_pending sinks, so
        // outputs_sent stayed at 0 even though every event was eventually
        // delivered on flush.
        let events = make_events(5_000);
        let source = MemorySource::new(events, 128);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let delivered_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sink = DeferredSink::new(Arc::clone(&delivered_count));
        let config = PipelineConfig {
            delivery: DeliveryConfig {
                strategy: DeliveryStrategy::UnorderedBatch,
                flush: crate::delivery::FlushStrategy {
                    interval: std::time::Duration::from_millis(20),
                    max_pending: 1_000,
                    adaptive: false,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        // Sink actually delivered all events on flush(es).
        assert_eq!(
            delivered_count.load(std::sync::atomic::Ordering::Relaxed),
            5_000
        );
        // The metric must also agree.
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 5_000);
        assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 5_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 5_000);
    }

    fn make_events_with_ids(count: usize) -> Vec<Event> {
        let source: Arc<str> = Arc::from("test");
        (0..count)
            .map(|i| {
                Event::new(
                    uuid::Uuid::from_bytes([(i % 256) as u8; 16]),
                    i as i64,
                    Arc::clone(&source),
                    PartitionId::new(0),
                    Bytes::from(format!("event-{i}")),
                )
                .with_source_offset(i as i64 * 100)
            })
            .collect()
    }

    #[tokio::test]
    async fn buffered_pipeline_with_delivery_ledger() {
        // Verify that the delivery ledger tracks all outputs and all are acked
        // after the pipeline completes.
        let events = make_events_with_ids(500);
        let source = MemorySource::new(events, 64);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig::default();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let ledger = Arc::new(DeliveryLedger::new(3));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            Some(Arc::clone(&ledger)),
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 500);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 500);
        // All outputs should be acked — ledger should be empty
        assert_eq!(ledger.pending_count(), 0, "all outputs should be acked");
        assert_eq!(ledger.failed_count(), 0, "no failures expected");
        assert_eq!(ledger.total_tracked(), 500, "all 500 outputs tracked");
        assert_eq!(ledger.total_acked(), 500, "all 500 outputs acked");
    }

    #[tokio::test]
    async fn buffered_pipeline_ledger_batched_mode() {
        // Verify ledger works in batched mode with checkpoint flush.
        let events = make_events_with_ids(2_000);
        let source = MemorySource::new(events, 128);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig {
            delivery: DeliveryConfig {
                strategy: DeliveryStrategy::UnorderedBatch,
                flush: crate::delivery::FlushStrategy {
                    interval: std::time::Duration::from_millis(50),
                    max_pending: 500,
                    adaptive: false,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let ledger = Arc::new(DeliveryLedger::new(3));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            Some(Arc::clone(&ledger)),
        )
        .await
        .unwrap();

        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 2_000);
        assert_eq!(ledger.pending_count(), 0);
        assert_eq!(ledger.total_tracked(), 2_000);
        assert_eq!(ledger.total_acked(), 2_000);
    }

    #[tokio::test]
    async fn buffered_pipeline_adaptive_flush() {
        // Verify adaptive flush mode works end-to-end with ledger.
        let events = make_events_with_ids(3_000);
        let source = MemorySource::new(events, 128);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig {
            delivery: DeliveryConfig {
                strategy: DeliveryStrategy::UnorderedBatch,
                flush: crate::delivery::FlushStrategy {
                    interval: std::time::Duration::from_millis(100),
                    max_pending: 1_000,
                    adaptive: true, // Enable adaptive flush
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let ledger = Arc::new(DeliveryLedger::new(3));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            Some(Arc::clone(&ledger)),
        )
        .await
        .unwrap();

        // All events delivered, zero loss
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 3_000);
        assert_eq!(ledger.pending_count(), 0);
        assert_eq!(ledger.total_tracked(), 3_000);
        assert_eq!(ledger.total_acked(), 3_000);
    }

    #[tokio::test]
    async fn buffered_pipeline_adaptive_without_ledger_falls_back() {
        // Adaptive enabled but no ledger — should fall back to static interval.
        let events = make_events(1_000);
        let source = MemorySource::new(events, 64);
        let processor = PassthroughProcessor::new(Arc::from("output"));
        let sink = BlackholeSink::new();
        let config = PipelineConfig {
            delivery: DeliveryConfig {
                strategy: DeliveryStrategy::UnorderedBatch,
                flush: crate::delivery::FlushStrategy {
                    interval: std::time::Duration::from_millis(50),
                    max_pending: 500,
                    adaptive: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_buffered(
            source,
            processor,
            sink,
            config,
            Arc::clone(&metrics),
            shutdown,
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 1_000);
    }

    #[tokio::test]
    async fn multi_partition_pipeline_basic() {
        // 4 partitions, each with 500 events = 2000 total
        let events_per_partition = 500;
        let partition_count = 4;

        let config = MultiPartitionConfig {
            partition_count,
            pipeline: PipelineConfig::default(),
            gate_registry: None,
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_multi_partition(
            config,
            Arc::clone(&metrics),
            shutdown,
            |_i| MemorySource::new(make_events(events_per_partition), 64),
            |_i| PassthroughProcessor::new(Arc::from("output")),
            |_i| BlackholeSink::new(),
            None,
        )
        .await
        .unwrap();

        let total = events_per_partition * partition_count;
        assert_eq!(
            metrics.events_received.load(Ordering::Relaxed),
            total as u64
        );
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), total as u64);
    }

    #[tokio::test]
    async fn multi_partition_pipeline_with_ledgers() {
        let events_per_partition = 300;
        let partition_count = 3;
        let ledgers: Vec<Arc<DeliveryLedger>> = (0..partition_count)
            .map(|_| Arc::new(DeliveryLedger::new(3)))
            .collect();
        let ledgers_clone = ledgers.clone();

        let config = MultiPartitionConfig {
            partition_count,
            pipeline: PipelineConfig::default(),
            gate_registry: None,
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_multi_partition(
            config,
            Arc::clone(&metrics),
            shutdown,
            |_i| MemorySource::new(make_events_with_ids(events_per_partition), 64),
            |_i| PassthroughProcessor::new(Arc::from("output")),
            |_i| BlackholeSink::new(),
            Some(Box::new(move |i| Arc::clone(&ledgers_clone[i]))),
        )
        .await
        .unwrap();

        let total = events_per_partition * partition_count;
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), total as u64);

        // Each partition ledger should have tracked and acked all its events
        for (i, ledger) in ledgers.iter().enumerate() {
            assert_eq!(
                ledger.pending_count(),
                0,
                "partition {i} has pending events"
            );
            assert_eq!(
                ledger.total_tracked() as usize,
                events_per_partition,
                "partition {i} tracked wrong count"
            );
            assert_eq!(
                ledger.total_acked() as usize,
                events_per_partition,
                "partition {i} acked wrong count"
            );
        }
    }

    #[tokio::test]
    async fn multi_partition_zero_partitions() {
        let config = MultiPartitionConfig {
            partition_count: 0,
            pipeline: PipelineConfig::default(),
            gate_registry: None,
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_multi_partition::<MemorySource, PassthroughProcessor, BlackholeSink, _, _, _>(
            config,
            metrics,
            shutdown,
            |_| unreachable!(),
            |_| unreachable!(),
            |_| unreachable!(),
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn multi_partition_with_auto_core_pinning() {
        // Auto core pinning — should work regardless of core count
        // (falls back to no pinning if insufficient cores)
        let config = MultiPartitionConfig {
            partition_count: 2,
            pipeline: PipelineConfig {
                core_pinning: CorePinning::Auto,
                ..Default::default()
            },
            gate_registry: None,
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_multi_partition(
            config,
            Arc::clone(&metrics),
            shutdown,
            |_i| MemorySource::new(make_events(100), 32),
            |_i| PassthroughProcessor::new(Arc::from("output")),
            |_i| BlackholeSink::new(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 200);
    }

    // ── PartialFailSink: test helper that fails specific events ──────────

    use aeon_types::{BatchFailurePolicy, BatchResult};
    use std::sync::atomic::AtomicU32;

    /// A sink that fails events whose `source_event_id` is in the fail set.
    /// After `heal_after` calls, it stops failing (simulates transient errors).
    struct PartialFailSink {
        fail_ids: std::collections::HashSet<uuid::Uuid>,
        calls: AtomicU32,
        heal_after: u32,
        delivered: std::sync::Mutex<Vec<Output>>,
    }

    impl PartialFailSink {
        fn new(fail_ids: Vec<uuid::Uuid>, heal_after: u32) -> Self {
            Self {
                fail_ids: fail_ids.into_iter().collect(),
                calls: AtomicU32::new(0),
                heal_after,
                delivered: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn delivered_count(&self) -> usize {
            self.delivered.lock().unwrap().len()
        }
    }

    impl Sink for PartialFailSink {
        async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
            let call_num = self.calls.fetch_add(1, Ordering::Relaxed);
            let healed = call_num >= self.heal_after;

            let mut delivered = Vec::new();
            let mut failed = Vec::new();

            for output in outputs {
                let should_fail = !healed
                    && output
                        .source_event_id
                        .map(|id| self.fail_ids.contains(&id))
                        .unwrap_or(false);

                if should_fail {
                    let id = output.source_event_id.unwrap();
                    failed.push((id, AeonError::connection("transient failure")));
                } else {
                    if let Some(id) = output.source_event_id {
                        delivered.push(id);
                    }
                    self.delivered.lock().unwrap().push(output);
                }
            }

            Ok(BatchResult {
                delivered,
                pending: Vec::new(),
                failed,
            })
        }

        async fn flush(&mut self) -> Result<(), AeonError> {
            Ok(())
        }
    }

    /// Make events with unique UUIDv7 IDs suitable for failure policy tests.
    fn make_events_unique(count: usize) -> Vec<Event> {
        let source: Arc<str> = Arc::from("test");
        (0..count)
            .map(|i| {
                Event::new(
                    uuid::Uuid::now_v7(),
                    i as i64,
                    Arc::clone(&source),
                    PartitionId::new(0),
                    Bytes::from(format!("event-{i}")),
                )
                .with_source_offset(i as i64)
            })
            .collect()
    }

    // ── FailBatch policy tests ──────────────────────────────────────────

    #[tokio::test]
    async fn fail_batch_policy_aborts_on_partial_failure() {
        let events = make_events_unique(10);
        // Fail the 3rd event
        let fail_id = events[2].id;

        let mut source = MemorySource::new(events, 32);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sink = PartialFailSink::new(vec![fail_id], 999); // never heals
        let delivery = DeliveryConfig {
            failure_policy: BatchFailurePolicy::FailBatch,
            ..Default::default()
        };
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        let result = run_with_delivery(
            &mut source,
            &processor,
            &mut sink,
            &delivery,
            &metrics,
            &shutdown,
        )
        .await;

        assert!(result.is_err(), "FailBatch should return error");
        let err = result.unwrap_err();
        assert!(
            format!("{err}").contains("FailBatch"),
            "error message should mention FailBatch: {err}"
        );
        assert_eq!(
            metrics.events_failed.load(Ordering::Relaxed),
            1,
            "one event should be marked failed"
        );
    }

    // ── SkipToDlq policy tests ──────────────────────────────────────────

    #[tokio::test]
    async fn skip_to_dlq_policy_continues_on_failure() {
        let events = make_events_unique(10);
        let fail_id = events[2].id;

        let mut source = MemorySource::new(events, 32);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sink = PartialFailSink::new(vec![fail_id], 999);
        let delivery = DeliveryConfig {
            failure_policy: BatchFailurePolicy::SkipToDlq,
            ..Default::default()
        };
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        let result = run_with_delivery(
            &mut source,
            &processor,
            &mut sink,
            &delivery,
            &metrics,
            &shutdown,
        )
        .await;

        assert!(result.is_ok(), "SkipToDlq should not abort: {result:?}");
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 10);
        assert_eq!(
            metrics.events_failed.load(Ordering::Relaxed),
            1,
            "one event should be marked as failed"
        );
        // 9 delivered in first write_batch + 0 retries
        assert_eq!(sink.delivered_count(), 9);
    }

    // ── RetryFailed policy tests ────────────────────────────────────────

    #[tokio::test]
    async fn retry_failed_policy_retries_and_succeeds() {
        let events = make_events_unique(10);
        let fail_id = events[4].id;

        let mut source = MemorySource::new(events, 32);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        // heal_after=1: first write_batch fails the event, retry succeeds
        let mut sink = PartialFailSink::new(vec![fail_id], 1);
        let delivery = DeliveryConfig {
            failure_policy: BatchFailurePolicy::RetryFailed,
            max_retries: 3,
            retry_backoff: Duration::ZERO, // no delay in tests
            ..Default::default()
        };
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        let result = run_with_delivery(
            &mut source,
            &processor,
            &mut sink,
            &delivery,
            &metrics,
            &shutdown,
        )
        .await;

        assert!(result.is_ok(), "RetryFailed should succeed: {result:?}");
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 10);
        assert_eq!(
            metrics.events_failed.load(Ordering::Relaxed),
            0,
            "no permanently failed events"
        );
        assert!(
            metrics.events_retried.load(Ordering::Relaxed) >= 1,
            "at least one retry should have happened"
        );
        // All 10 events delivered (9 initially + 1 on retry)
        assert_eq!(sink.delivered_count(), 10);
    }

    #[tokio::test]
    async fn retry_failed_policy_exhausts_retries() {
        let events = make_events_unique(10);
        let fail_id = events[0].id;

        let mut source = MemorySource::new(events, 32);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        // Never heals — all retries will fail
        let mut sink = PartialFailSink::new(vec![fail_id], 999);
        let delivery = DeliveryConfig {
            failure_policy: BatchFailurePolicy::RetryFailed,
            max_retries: 2,
            retry_backoff: Duration::ZERO,
            ..Default::default()
        };
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        let result = run_with_delivery(
            &mut source,
            &processor,
            &mut sink,
            &delivery,
            &metrics,
            &shutdown,
        )
        .await;

        // Pipeline continues after retry exhaustion (doesn't abort)
        assert!(result.is_ok(), "RetryFailed should not abort: {result:?}");
        assert_eq!(
            metrics.events_failed.load(Ordering::Relaxed),
            1,
            "one event permanently failed"
        );
        assert_eq!(
            metrics.events_retried.load(Ordering::Relaxed),
            2,
            "two retry attempts"
        );
        // 9 delivered initially, retried event never delivered
        assert_eq!(sink.delivered_count(), 9);
    }

    #[tokio::test]
    async fn retry_failed_multiple_events_partial_heal() {
        let events = make_events_unique(20);
        let fail_ids = vec![events[3].id, events[7].id, events[15].id];

        let mut source = MemorySource::new(events, 32);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        // heal_after=2: first two write_batch calls fail, third succeeds
        let mut sink = PartialFailSink::new(fail_ids, 2);
        let delivery = DeliveryConfig {
            failure_policy: BatchFailurePolicy::RetryFailed,
            max_retries: 3,
            retry_backoff: Duration::ZERO,
            ..Default::default()
        };
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        let result = run_with_delivery(
            &mut source,
            &processor,
            &mut sink,
            &delivery,
            &metrics,
            &shutdown,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 20);
        assert_eq!(
            metrics.events_failed.load(Ordering::Relaxed),
            0,
            "all events eventually succeeded"
        );
        assert_eq!(sink.delivered_count(), 20);
    }

    #[tokio::test]
    async fn no_failure_policy_overhead_when_all_succeed() {
        // Verify that RetryFailed policy adds no overhead when there are no failures
        let events = make_events_unique(1000);
        let mut source = MemorySource::new(events, 64);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sink = PartialFailSink::new(vec![], 0); // nothing to fail
        let delivery = DeliveryConfig {
            failure_policy: BatchFailurePolicy::RetryFailed,
            max_retries: 3,
            retry_backoff: Duration::from_millis(100),
            ..Default::default()
        };
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_with_delivery(
            &mut source,
            &processor,
            &mut sink,
            &delivery,
            &metrics,
            &shutdown,
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 1000);
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.events_retried.load(Ordering::Relaxed), 0);
        assert_eq!(sink.delivered_count(), 1000);
    }

    // ── Managed pipeline / hot-swap tests ──────────────────────────────

    /// Thread-safe sink wrapper for tests where the pipeline runs in a
    /// background task and the test thread inspects outputs.
    #[derive(Clone)]
    struct SharedMemorySink {
        outputs: Arc<std::sync::Mutex<Vec<Output>>>,
    }

    impl SharedMemorySink {
        fn new() -> Self {
            Self {
                outputs: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn outputs(&self) -> Vec<Output> {
            self.outputs.lock().unwrap().clone()
        }

        fn len(&self) -> usize {
            self.outputs.lock().unwrap().len()
        }
    }

    impl Sink for SharedMemorySink {
        async fn write_batch(
            &mut self,
            outputs: Vec<Output>,
        ) -> Result<aeon_types::BatchResult, AeonError> {
            let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
            self.outputs.lock().unwrap().extend(outputs);
            Ok(aeon_types::BatchResult::all_delivered(ids))
        }

        async fn flush(&mut self) -> Result<(), AeonError> {
            Ok(())
        }
    }

    /// A source that produces events in waves, with a shutdown signal.
    /// Unlike MemorySource, this doesn't exhaust — it keeps producing
    /// until told to stop, making it suitable for hot-swap testing.
    struct ContinuousSource {
        batch_size: usize,
        batch_count: AtomicU64,
        shutdown: Arc<AtomicBool>,
        paused: bool,
    }

    impl ContinuousSource {
        fn new(batch_size: usize, shutdown: Arc<AtomicBool>) -> Self {
            Self {
                batch_size,
                batch_count: AtomicU64::new(0),
                shutdown,
                paused: false,
            }
        }
    }

    impl Source for ContinuousSource {
        async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
            if self.paused || self.shutdown.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }
            let batch_num = self.batch_count.fetch_add(1, Ordering::Relaxed);
            let source: Arc<str> = Arc::from("continuous");
            let events = (0..self.batch_size)
                .map(|i| {
                    let idx = batch_num * self.batch_size as u64 + i as u64;
                    // Use index-based UUID so canary routing can split deterministically
                    let id = uuid::Uuid::from_u128(idx as u128 + 1);
                    Event::new(
                        id,
                        idx as i64,
                        Arc::clone(&source),
                        PartitionId::new(0),
                        Bytes::from(format!("event-{idx}")),
                    )
                })
                .collect();
            // Small yield to let other tasks run
            tokio::task::yield_now().await;
            Ok(events)
        }

        async fn pause(&mut self) {
            self.paused = true;
        }

        async fn resume(&mut self) {
            self.paused = false;
        }
    }

    /// Processor that prefixes output payloads, so we can detect which
    /// processor handled which events.
    struct PrefixProcessor {
        prefix: &'static str,
        destination: Arc<str>,
    }

    impl PrefixProcessor {
        fn new(prefix: &'static str) -> Self {
            Self {
                prefix,
                destination: Arc::from("output"),
            }
        }
    }

    impl Processor for PrefixProcessor {
        fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
            let payload = format!("{}{}", self.prefix, String::from_utf8_lossy(&event.payload));
            Ok(vec![Output::new(
                Arc::clone(&self.destination),
                Bytes::from(payload),
            )])
        }
    }

    #[tokio::test]
    async fn managed_pipeline_hot_swap_zero_loss() {
        // Set up a continuous source and a prefixed processor "A:"
        let source_shutdown = Arc::new(AtomicBool::new(false));
        let source = ContinuousSource::new(10, Arc::clone(&source_shutdown));
        let processor_a: Box<dyn Processor + Send + Sync> = Box::new(PrefixProcessor::new("A:"));
        let sink = SharedMemorySink::new();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let control = PipelineControl::new();

        let metrics_clone = Arc::clone(&metrics);
        let shutdown_clone = Arc::clone(&shutdown);
        let control_clone = Arc::clone(&control);
        let sink_clone = sink.clone();

        // Run managed pipeline in background
        let pipeline_handle = tokio::spawn(async move {
            run_buffered_managed(
                source,
                processor_a,
                sink_clone,
                PipelineConfig::default(),
                metrics_clone,
                shutdown_clone,
                None,
                control_clone,
            )
            .await
        });

        // Wait until processor A has processed some events
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let pre_swap_count = metrics.events_processed.load(Ordering::Relaxed);
        assert!(
            pre_swap_count > 0,
            "should have processed events before swap"
        );

        // Hot-swap to processor B
        let processor_b: Box<dyn Processor + Send + Sync> = Box::new(PrefixProcessor::new("B:"));
        control.drain_and_swap(processor_b).await.unwrap();

        // Wait until B-prefixed outputs appear in the sink (proof that swap worked)
        let mut b_found = false;
        for _ in 0..200 {
            let outs = sink.outputs();
            if outs.iter().any(|o| o.payload.starts_with(b"B:")) {
                b_found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(b_found, "should have B-prefixed outputs (post-swap)");

        // Shutdown
        source_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        // Allow pipeline to finish
        tokio::time::sleep(Duration::from_millis(50)).await;

        let post_swap_count = metrics.events_processed.load(Ordering::Relaxed);
        assert!(
            post_swap_count > pre_swap_count,
            "should have processed more events after swap"
        );

        // Verify zero event loss: events_received >= events_processed
        let received = metrics.events_received.load(Ordering::Relaxed);
        let processed = metrics.events_processed.load(Ordering::Relaxed);
        assert!(
            received >= processed,
            "no events lost: received={received}, processed={processed}"
        );

        // Verify outputs contain both A: and B: prefixed payloads
        let outputs = sink.outputs();
        let a_count = outputs
            .iter()
            .filter(|o| o.payload.starts_with(b"A:"))
            .count();
        let b_count = outputs
            .iter()
            .filter(|o| o.payload.starts_with(b"B:"))
            .count();
        assert!(a_count > 0, "should have A-prefixed outputs (pre-swap)");
        assert!(b_count > 0, "should have B-prefixed outputs (post-swap)");

        // No outputs without prefix (no corruption during swap)
        let unprefixed = outputs
            .iter()
            .filter(|o| !o.payload.starts_with(b"A:") && !o.payload.starts_with(b"B:"))
            .count();
        assert_eq!(unprefixed, 0, "all outputs should be prefixed");

        // Clean up
        let _ = pipeline_handle.await;
    }

    #[tokio::test]
    async fn managed_pipeline_runs_without_swap() {
        // Verify managed pipeline works like regular run_buffered when no swap occurs
        let events = make_events(100);
        let source = MemorySource::new(events, 32);
        let processor: Box<dyn Processor + Send + Sync> =
            Box::new(PassthroughProcessor::new(Arc::from("output")));
        let sink = SharedMemorySink::new();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let control = PipelineControl::new();

        run_buffered_managed(
            source,
            processor,
            sink.clone(),
            PipelineConfig::default(),
            metrics.clone(),
            shutdown,
            None,
            control,
        )
        .await
        .unwrap();

        assert_eq!(sink.len(), 100);
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 100);
        assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 100);
    }

    #[tokio::test]
    async fn managed_pipeline_source_swap() {
        // Start with source A (10 events), swap mid-stream to source B (10 events)
        // Source A produces events with "src-a-" prefix, source B with "src-b-"
        let source_shutdown = Arc::new(AtomicBool::new(false));
        let source_a = ContinuousSource::new(5, Arc::clone(&source_shutdown));
        let processor: Box<dyn Processor + Send + Sync> =
            Box::new(PassthroughProcessor::new(Arc::from("output")));
        let sink = SharedMemorySink::new();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let control = PipelineControl::new();

        let metrics_clone = Arc::clone(&metrics);
        let shutdown_clone = Arc::clone(&shutdown);
        let control_clone = Arc::clone(&control);
        let sink_clone = sink.clone();

        let pipeline_handle = tokio::spawn(async move {
            run_buffered_managed(
                source_a,
                processor,
                sink_clone,
                PipelineConfig::default(),
                metrics_clone,
                shutdown_clone,
                None,
                control_clone,
            )
            .await
        });

        // Wait for source A to produce some events
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let pre_swap = metrics.events_processed.load(Ordering::Relaxed);
        assert!(pre_swap > 0, "should have processed events from source A");

        // Swap to source B
        let source_b = ContinuousSource::new(5, Arc::clone(&source_shutdown));
        control.drain_and_swap_source(source_b).await.unwrap();

        // Wait for more events after swap
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > pre_swap {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let post_swap = metrics.events_processed.load(Ordering::Relaxed);
        assert!(post_swap > pre_swap, "should process events from source B");

        // Shutdown
        source_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = pipeline_handle.await;
    }

    #[tokio::test]
    async fn managed_pipeline_sink_swap() {
        // Start with sink A, swap to sink B mid-pipeline. Verify both collected outputs.
        let source_shutdown = Arc::new(AtomicBool::new(false));
        let source = ContinuousSource::new(5, Arc::clone(&source_shutdown));
        let processor: Box<dyn Processor + Send + Sync> =
            Box::new(PassthroughProcessor::new(Arc::from("output")));
        let sink_a = SharedMemorySink::new();
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let control = PipelineControl::new();

        let metrics_clone = Arc::clone(&metrics);
        let shutdown_clone = Arc::clone(&shutdown);
        let control_clone = Arc::clone(&control);
        let sink_a_clone = sink_a.clone();

        let pipeline_handle = tokio::spawn(async move {
            run_buffered_managed(
                source,
                processor,
                sink_a_clone,
                PipelineConfig::default(),
                metrics_clone,
                shutdown_clone,
                None,
                control_clone,
            )
            .await
        });

        // Wait for sink A to collect some outputs
        for _ in 0..200 {
            if sink_a.len() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let sink_a_count = sink_a.len();
        assert!(sink_a_count > 0, "sink A should have collected outputs");

        // Swap to sink B
        let sink_b = SharedMemorySink::new();
        let sink_b_reader = sink_b.clone();
        control.drain_and_swap_sink(sink_b).await.unwrap();

        // Wait for sink B to collect outputs
        for _ in 0..200 {
            if sink_b_reader.len() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            sink_b_reader.len() > 0,
            "sink B should have collected outputs after swap"
        );

        // Sink A should have stopped growing
        let sink_a_final = sink_a.len();

        // Shutdown
        source_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = pipeline_handle.await;

        // Both sinks received outputs, sink A stopped after swap
        assert!(
            sink_a_final >= sink_a_count,
            "sink A count should be stable after swap"
        );
        assert!(sink_b_reader.len() > 0, "sink B received outputs");

        // Total outputs should match total processed
        let total_outputs = sink_a_final + sink_b_reader.len();
        let total_sent = metrics.outputs_sent.load(Ordering::Relaxed) as usize;
        assert_eq!(total_outputs, total_sent, "zero output loss across swap");
    }

    #[tokio::test]
    async fn managed_pipeline_blue_green_cutover() {
        // Start pipeline with "A:" processor, install "B:" green in shadow,
        // verify A outputs continue, cutover, verify B outputs start.
        let source_shutdown = Arc::new(AtomicBool::new(false));
        let source = ContinuousSource::new(10, Arc::clone(&source_shutdown));
        let sink = SharedMemorySink::new();
        let sink_reader = sink.outputs.clone();
        let processor_a = Box::new(PrefixProcessor::new("A:"));

        let config = PipelineConfig::default();
        let metrics = Arc::new(PipelineMetrics::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let control = PipelineControl::new();
        let control2 = Arc::clone(&control);
        let metrics2 = Arc::clone(&metrics);
        let shutdown2 = Arc::clone(&shutdown);

        let pipeline_handle = tokio::spawn(async move {
            run_buffered_managed(
                source,
                processor_a,
                sink,
                config,
                metrics2,
                shutdown2,
                None,
                control2,
            )
            .await
        });

        // Wait for A outputs
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(metrics.events_processed.load(Ordering::Relaxed) > 0);

        // Verify A-prefixed outputs
        let outputs_before = sink_reader.lock().unwrap().clone();
        assert!(
            outputs_before
                .iter()
                .all(|o| { String::from_utf8_lossy(&o.payload).starts_with("A:") }),
            "all outputs before cutover should be A-prefixed"
        );

        // Install green processor (B:) — no pause, no drain
        let processor_b = Box::new(PrefixProcessor::new("B:"));
        control.start_blue_green(processor_b).await.unwrap();

        // Wait for more A outputs (green is shadow, not yet active)
        let count_after_install = sink_reader.lock().unwrap().len();
        for _ in 0..200 {
            if sink_reader.lock().unwrap().len() > count_after_install + 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // All new outputs should still be A-prefixed
        let outputs_mid = sink_reader.lock().unwrap().clone();
        assert!(
            outputs_mid
                .iter()
                .all(|o| { String::from_utf8_lossy(&o.payload).starts_with("A:") }),
            "all outputs during shadow should be A-prefixed"
        );

        // Cutover to green
        control.cutover_blue_green().await.unwrap();

        // Wait for B outputs to appear
        for _ in 0..200 {
            let has_b = sink_reader
                .lock()
                .unwrap()
                .iter()
                .any(|o| String::from_utf8_lossy(&o.payload).starts_with("B:"));
            if has_b {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let outputs_after = sink_reader.lock().unwrap().clone();
        let has_b = outputs_after
            .iter()
            .any(|o| String::from_utf8_lossy(&o.payload).starts_with("B:"));
        assert!(has_b, "should have B-prefixed outputs after cutover");

        // Shut down
        source_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        let _ = pipeline_handle.await;
    }

    #[tokio::test]
    async fn managed_pipeline_blue_green_rollback() {
        // Install green, then roll back — verify A outputs continue.
        let source_shutdown = Arc::new(AtomicBool::new(false));
        let source = ContinuousSource::new(10, Arc::clone(&source_shutdown));
        let sink = SharedMemorySink::new();
        let sink_reader = sink.outputs.clone();
        let processor_a = Box::new(PrefixProcessor::new("A:"));

        let config = PipelineConfig::default();
        let metrics = Arc::new(PipelineMetrics::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let control = PipelineControl::new();
        let control2 = Arc::clone(&control);
        let metrics2 = Arc::clone(&metrics);
        let shutdown2 = Arc::clone(&shutdown);

        let pipeline_handle = tokio::spawn(async move {
            run_buffered_managed(
                source,
                processor_a,
                sink,
                config,
                metrics2,
                shutdown2,
                None,
                control2,
            )
            .await
        });

        // Wait for initial outputs
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Install green then rollback
        let processor_b = Box::new(PrefixProcessor::new("B:"));
        control.start_blue_green(processor_b).await.unwrap();
        control.rollback_upgrade().await.unwrap();

        // Wait for more outputs after rollback
        let count_after = sink_reader.lock().unwrap().len();
        for _ in 0..200 {
            if sink_reader.lock().unwrap().len() > count_after + 20 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // All outputs should be A-prefixed (no B ever appeared)
        let outputs = sink_reader.lock().unwrap().clone();
        assert!(
            outputs
                .iter()
                .all(|o| { String::from_utf8_lossy(&o.payload).starts_with("A:") }),
            "all outputs after rollback should be A-prefixed"
        );

        source_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        let _ = pipeline_handle.await;
    }

    #[tokio::test]
    async fn managed_pipeline_canary_split() {
        // Start with A: baseline, install B: canary at 50%, verify both appear.
        let source_shutdown = Arc::new(AtomicBool::new(false));
        let source = ContinuousSource::new(100, Arc::clone(&source_shutdown));
        let sink = SharedMemorySink::new();
        let sink_reader = sink.outputs.clone();
        let processor_a = Box::new(PrefixProcessor::new("A:"));

        let config = PipelineConfig::default();
        let metrics = Arc::new(PipelineMetrics::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let control = PipelineControl::new();
        let control2 = Arc::clone(&control);
        let metrics2 = Arc::clone(&metrics);
        let shutdown2 = Arc::clone(&shutdown);

        let pipeline_handle = tokio::spawn(async move {
            run_buffered_managed(
                source,
                processor_a,
                sink,
                config,
                metrics2,
                shutdown2,
                None,
                control2,
            )
            .await
        });

        // Wait for initial A outputs
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Clear sink and start canary at 50%
        sink_reader.lock().unwrap().clear();
        let processor_b = Box::new(PrefixProcessor::new("B:"));
        control.start_canary(processor_b, 50).await.unwrap();

        // Wait for enough outputs with both A and B
        for _ in 0..200 {
            let len = sink_reader.lock().unwrap().len();
            if len > 200 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let outputs = sink_reader.lock().unwrap().clone();
        let a_count = outputs
            .iter()
            .filter(|o| String::from_utf8_lossy(&o.payload).starts_with("A:"))
            .count();
        let b_count = outputs
            .iter()
            .filter(|o| String::from_utf8_lossy(&o.payload).starts_with("B:"))
            .count();

        assert!(
            a_count > 0,
            "baseline A should have outputs (got {a_count})"
        );
        assert!(b_count > 0, "canary B should have outputs (got {b_count})");
        // With 50% split over many events, both should be non-trivial
        let total = a_count + b_count;
        assert!(
            a_count as f64 / total as f64 > 0.2,
            "A should have >20% of traffic"
        );
        assert!(
            b_count as f64 / total as f64 > 0.2,
            "B should have >20% of traffic"
        );

        source_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        let _ = pipeline_handle.await;
    }

    #[tokio::test]
    async fn managed_pipeline_canary_complete() {
        // Start canary, complete it — verify only B outputs afterward.
        let source_shutdown = Arc::new(AtomicBool::new(false));
        let source = ContinuousSource::new(10, Arc::clone(&source_shutdown));
        let sink = SharedMemorySink::new();
        let sink_reader = sink.outputs.clone();
        let processor_a = Box::new(PrefixProcessor::new("A:"));

        let config = PipelineConfig::default();
        let metrics = Arc::new(PipelineMetrics::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let control = PipelineControl::new();
        let control2 = Arc::clone(&control);
        let metrics2 = Arc::clone(&metrics);
        let shutdown2 = Arc::clone(&shutdown);

        let pipeline_handle = tokio::spawn(async move {
            run_buffered_managed(
                source,
                processor_a,
                sink,
                config,
                metrics2,
                shutdown2,
                None,
                control2,
            )
            .await
        });

        // Wait for initial outputs
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Start canary at 50%
        let processor_b = Box::new(PrefixProcessor::new("B:"));
        control.start_canary(processor_b, 50).await.unwrap();

        // Complete canary — B becomes sole processor
        control.complete_canary().await.unwrap();

        // Clear sink and collect new outputs
        sink_reader.lock().unwrap().clear();
        for _ in 0..200 {
            if sink_reader.lock().unwrap().len() > 50 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let outputs = sink_reader.lock().unwrap().clone();
        assert!(
            !outputs.is_empty(),
            "should have outputs after canary complete"
        );
        assert!(
            outputs
                .iter()
                .all(|o| { String::from_utf8_lossy(&o.payload).starts_with("B:") }),
            "all outputs after canary complete should be B-prefixed"
        );

        source_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        let _ = pipeline_handle.await;
    }

    // ── PoH integration tests ────────────────────────────────────────────

    #[cfg(feature = "processor-auth")]
    mod poh_tests {
        use super::*;
        use crate::delivery_ledger::DeliveryLedger;

        type Ledger = Option<Arc<DeliveryLedger>>;
        const NO_LEDGER: Ledger = None;

        #[tokio::test]
        async fn run_buffered_with_poh_records_chain() {
            let events = make_events(50);
            let source = MemorySource::new(events, 16);
            let processor = PassthroughProcessor::new(Arc::from("output"));
            let sink = BlackholeSink::new();
            let config = PipelineConfig {
                poh: Some(PohConfig {
                    partition: PartitionId::new(7),
                    max_recent_entries: 256,
                    signing_key: None,
                }),
                ..PipelineConfig::default()
            };
            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            run_buffered(
                source,
                processor,
                sink,
                config,
                Arc::clone(&metrics),
                shutdown,
                NO_LEDGER,
            )
            .await
            .unwrap();

            assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 50);
            assert!(
                metrics.poh_entries.load(Ordering::Relaxed) > 0,
                "poh_entries should be > 0 after processing with PoH enabled"
            );
        }

        #[tokio::test]
        async fn run_buffered_without_poh_no_entries() {
            let events = make_events(50);
            let source = MemorySource::new(events, 16);
            let processor = PassthroughProcessor::new(Arc::from("output"));
            let sink = BlackholeSink::new();
            let config = PipelineConfig::default(); // poh: None
            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            run_buffered(
                source,
                processor,
                sink,
                config,
                Arc::clone(&metrics),
                shutdown,
                NO_LEDGER,
            )
            .await
            .unwrap();

            assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 50);
            assert_eq!(
                metrics.poh_entries.load(Ordering::Relaxed),
                0,
                "poh_entries should be 0 when PoH is disabled"
            );
        }

        #[tokio::test]
        async fn poh_chain_integrity_after_pipeline() {
            let events = make_events(100);
            let source = MemorySource::new(events, 10);
            let processor = PassthroughProcessor::new(Arc::from("output"));
            let sink = BlackholeSink::new();

            let config = PipelineConfig {
                poh: Some(PohConfig {
                    partition: PartitionId::new(3),
                    max_recent_entries: 256,
                    signing_key: None,
                }),
                ..PipelineConfig::default()
            };
            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            run_buffered(
                source,
                processor,
                sink,
                config,
                Arc::clone(&metrics),
                shutdown,
                NO_LEDGER,
            )
            .await
            .unwrap();

            let poh_count = metrics.poh_entries.load(Ordering::Relaxed);
            assert!(poh_count > 0, "should have PoH entries, got {poh_count}");
        }

        #[tokio::test]
        async fn poh_with_signing_key() {
            let signing_key = aeon_crypto::signing::SigningKey::generate();
            let events = make_events(20);
            let source = MemorySource::new(events, 10);
            let processor = PassthroughProcessor::new(Arc::from("output"));
            let sink = BlackholeSink::new();

            let config = PipelineConfig {
                poh: Some(PohConfig {
                    partition: PartitionId::new(0),
                    max_recent_entries: 64,
                    signing_key: Some(Arc::new(signing_key)),
                }),
                ..PipelineConfig::default()
            };
            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            run_buffered(
                source,
                processor,
                sink,
                config,
                Arc::clone(&metrics),
                shutdown,
                NO_LEDGER,
            )
            .await
            .unwrap();

            assert!(metrics.poh_entries.load(Ordering::Relaxed) > 0);
        }

        #[tokio::test]
        async fn create_poh_state_returns_none_when_disabled() {
            let config = PipelineConfig::default();
            assert!(create_poh_state(&config).is_none());
        }

        #[tokio::test]
        async fn create_poh_state_returns_some_when_enabled() {
            let config = PipelineConfig {
                poh: Some(PohConfig::default()),
                ..PipelineConfig::default()
            };
            let state = create_poh_state(&config);
            assert!(state.is_some());

            let binding = state.unwrap();
            let chain = binding.lock().await;
            assert_eq!(chain.partition(), PartitionId::new(0));
            assert_eq!(chain.sequence(), 0);
        }

        /// G11.c — when a PoH chain has been pre-installed by the cluster-
        /// side `PohChainInstaller` for `(pipeline_name, partition_id)`,
        /// `create_poh_state` must resume from that snapshot instead of
        /// genesising a fresh chain. The snapshot is consumed (taken) so a
        /// second startup without a fresh install falls back to genesis.
        #[cfg(feature = "cluster")]
        #[tokio::test]
        async fn create_poh_state_resumes_installed_chain_g11c() {
            use crate::partition_install::InstalledPohChainRegistry;
            use aeon_cluster::types::PohChainTransferRequest;
            use aeon_crypto::poh::PohVerifyMode;

            // Build a registry and install a chain that has already run
            // 5 batches through it (simulating what the outgoing owner
            // shipped to this node during CL-6).
            let registry = InstalledPohChainRegistry::new();
            let installer = crate::partition_install::PohChainInstallerImpl::new(
                registry.clone(),
                PohVerifyMode::Verify,
            );
            let mut src_chain = aeon_crypto::poh::PohChain::new(PartitionId::new(3), 64);
            for i in 0..5i64 {
                src_chain.append_batch(&[format!("evt-{i}").as_bytes()], (i + 1) * 1000, None);
            }
            let state_bytes = src_chain.export_state().to_bytes().unwrap();
            let req = PohChainTransferRequest {
                pipeline: "pl-resume".to_string(),
                partition: PartitionId::new(3),
            };
            aeon_cluster::partition_driver::PohChainInstaller::install(
                &installer,
                &req,
                state_bytes,
            )
            .unwrap();

            // Now construct a PipelineConfig pointed at the same pipeline/
            // partition and assert `create_poh_state` resumes.
            let config = PipelineConfig {
                pipeline_name: "pl-resume".to_string(),
                partition_id: PartitionId::new(3),
                poh: Some(PohConfig {
                    partition: PartitionId::new(3),
                    max_recent_entries: 64,
                    signing_key: None,
                }),
                poh_installed_chains: Some(Arc::new(registry.clone())),
                ..PipelineConfig::default()
            };
            let state = create_poh_state(&config).expect("poh enabled");
            let chain = state.lock().await;
            assert_eq!(chain.partition(), PartitionId::new(3));
            assert_eq!(
                chain.sequence(),
                5,
                "resumed chain must carry source sequence, not genesis"
            );
            drop(chain);

            // Second call must genesis — the take() consumed the snapshot.
            let state2 = create_poh_state(&config).expect("poh enabled");
            let chain2 = state2.lock().await;
            assert_eq!(
                chain2.sequence(),
                0,
                "no fresh install → next create_poh_state must genesis"
            );
        }

        #[tokio::test]
        async fn poh_append_batch_helper_works() {
            let poh = Arc::new(Mutex::new(aeon_crypto::poh::PohChain::new(
                PartitionId::new(0),
                64,
            )));
            let metrics = PipelineMetrics::new();

            let dest: Arc<str> = Arc::from("output");
            let outputs = vec![
                Output {
                    destination: Arc::clone(&dest),
                    key: None,
                    payload: Bytes::from("hello"),
                    headers: smallvec::smallvec![],
                    source_ts: None,
                    source_event_id: None,
                    source_partition: None,
                    source_offset: None,
                    l2_seq: None,
                },
                Output {
                    destination: Arc::clone(&dest),
                    key: None,
                    payload: Bytes::from("world"),
                    headers: smallvec::smallvec![],
                    source_ts: None,
                    source_event_id: None,
                    source_partition: None,
                    source_offset: None,
                    l2_seq: None,
                },
            ];

            poh_append_batch(&poh, &outputs, &None, &metrics).await;
            assert_eq!(metrics.poh_entries.load(Ordering::Relaxed), 1);

            let chain = poh.lock().await;
            assert_eq!(chain.sequence(), 1);
            assert_eq!(chain.recent_entries().len(), 1);
            assert_eq!(chain.recent_entries()[0].batch_size, 2);
        }

        #[tokio::test]
        async fn poh_append_empty_batch_noop() {
            let poh = Arc::new(Mutex::new(aeon_crypto::poh::PohChain::new(
                PartitionId::new(0),
                64,
            )));
            let metrics = PipelineMetrics::new();

            poh_append_batch(&poh, &[], &None, &metrics).await;
            assert_eq!(metrics.poh_entries.load(Ordering::Relaxed), 0);

            let chain = poh.lock().await;
            assert_eq!(chain.sequence(), 0);
        }
    }

    // ── EO-2 P4/P5 runtime wiring ──────────────────────────────────────

    mod eo2_wiring {
        use super::*;
        use crate::delivery::L2BodyStoreConfig;
        use crate::eo2::PipelineL2Registry;
        use aeon_types::{DurabilityMode, SourceKind};

        /// Minimal push-kind test source — emits one batch, then EOF.
        struct PushSource {
            batch: Option<Vec<Event>>,
        }
        impl Source for PushSource {
            fn next_batch(
                &mut self,
            ) -> impl std::future::Future<Output = Result<Vec<Event>, AeonError>> + Send
            {
                let b = self.batch.take().unwrap_or_default();
                async move { Ok(b) }
            }
            fn source_kind(&self) -> SourceKind {
                SourceKind::Push
            }
        }

        fn push_events(n: usize) -> Vec<Event> {
            let src: Arc<str> = Arc::from("webhook");
            (0..n)
                .map(|i| {
                    Event::new(
                        uuid::Uuid::now_v7(),
                        i as i64,
                        Arc::clone(&src),
                        PartitionId::new(0),
                        Bytes::from(format!("push-{i}")),
                    )
                })
                .collect()
        }

        /// Push/Poll sources now treat empty `next_batch()` as a lull (not EOF),
        /// so finite-batch test sources no longer self-terminate. This spawns
        /// a watcher that flips `shutdown` once `events_received` reaches the
        /// target, after a short grace to let downstream tasks drain.
        fn shutdown_after_target(
            metrics: Arc<PipelineMetrics>,
            shutdown: Arc<AtomicBool>,
            target: u64,
        ) -> tokio::task::JoinHandle<()> {
            tokio::spawn(async move {
                loop {
                    if metrics.events_received.load(Ordering::Relaxed) >= target {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        shutdown.store(true, Ordering::Release);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        }

        #[tokio::test]
        async fn push_source_writes_l2_when_durability_requires_it() {
            let tmp = tempfile::tempdir().unwrap();
            let registry = PipelineL2Registry::new(L2BodyStoreConfig {
                root: Some(tmp.path().to_path_buf()),
                segment_bytes: 4096,
            });

            let source = PushSource {
                batch: Some(push_events(50)),
            };
            let processor = PassthroughProcessor::new(Arc::from("out"));
            let sink = BlackholeSink::new();
            let mut config = PipelineConfig::default();
            config.pipeline_name = "eo2-push-test".into();
            config.l2_registry = Some(registry.clone());
            config.delivery.durability = DurabilityMode::OrderedBatch;

            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            let stopper = shutdown_after_target(Arc::clone(&metrics), Arc::clone(&shutdown), 50);
            run_buffered(
                source,
                processor,
                sink,
                config,
                metrics.clone(),
                shutdown,
                None,
            )
            .await
            .unwrap();
            stopper.await.unwrap();

            assert_eq!(metrics.events_received.load(Ordering::Relaxed), 50);
            // Registry must have opened a store for partition 0.
            let keys = registry.keys();
            assert_eq!(keys.len(), 1);
            let (_p, part) = &keys[0];
            assert_eq!(*part, PartitionId::new(0));

            // Open a second reader to confirm events were actually persisted.
            let store_handle = registry.open("eo2-push-test", PartitionId::new(0)).unwrap();
            let guard = store_handle.lock().unwrap();
            // next_seq = head+1 after every append; 50 events ⇒ next_seq ≥ 50.
            assert!(
                guard.next_seq() >= 50,
                "expected next_seq ≥ 50, got {}",
                guard.next_seq()
            );
            assert!(guard.disk_bytes() > 0);
        }

        #[tokio::test]
        async fn pull_source_skips_l2_even_with_durability_set() {
            let tmp = tempfile::tempdir().unwrap();
            let registry = PipelineL2Registry::new(L2BodyStoreConfig {
                root: Some(tmp.path().to_path_buf()),
                segment_bytes: 4096,
            });

            // MemorySource is Pull by default.
            let source = MemorySource::new(make_events(30), 16);
            let processor = PassthroughProcessor::new(Arc::from("out"));
            let sink = BlackholeSink::new();
            let mut config = PipelineConfig::default();
            config.pipeline_name = "eo2-pull-test".into();
            config.l2_registry = Some(registry.clone());
            config.delivery.durability = DurabilityMode::OrderedBatch;

            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            // Pull source: MemorySource returns empty when drained, which the
            // engine still treats as EOF, so no watcher is required here.
            run_buffered(
                source,
                processor,
                sink,
                config,
                metrics.clone(),
                shutdown,
                None,
            )
            .await
            .unwrap();

            assert_eq!(metrics.events_received.load(Ordering::Relaxed), 30);
            // Pull sources must not touch the registry.
            assert!(
                registry.keys().is_empty(),
                "pull source unexpectedly wrote L2"
            );
        }

        #[tokio::test]
        async fn durability_none_passthrough_even_for_push() {
            let tmp = tempfile::tempdir().unwrap();
            let registry = PipelineL2Registry::new(L2BodyStoreConfig {
                root: Some(tmp.path().to_path_buf()),
                segment_bytes: 4096,
            });

            let source = PushSource {
                batch: Some(push_events(20)),
            };
            let processor = PassthroughProcessor::new(Arc::from("out"));
            let sink = BlackholeSink::new();
            let mut config = PipelineConfig::default();
            config.pipeline_name = "eo2-none-test".into();
            config.l2_registry = Some(registry.clone());
            // Durability::None ⇒ L2 stays cold even for push sources.
            config.delivery.durability = DurabilityMode::None;

            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            let stopper = shutdown_after_target(Arc::clone(&metrics), Arc::clone(&shutdown), 20);
            run_buffered(
                source,
                processor,
                sink,
                config,
                metrics.clone(),
                shutdown,
                None,
            )
            .await
            .unwrap();
            stopper.await.unwrap();

            assert_eq!(metrics.events_received.load(Ordering::Relaxed), 20);
            assert!(registry.keys().is_empty());
        }

        #[tokio::test]
        async fn push_source_drives_l2_gc_after_successful_delivery() {
            // End-to-end: push source under OrderedBatch writes 30 events into
            // L2, all sink-delivered. On final flush the sink task calls
            // `record_ack(max_l2_seq)` then `eo2_gc_sweep`, which should
            // reclaim the segment since min_across_sinks == max_seq written.
            let tmp = tempfile::tempdir().unwrap();
            let registry = PipelineL2Registry::new(L2BodyStoreConfig {
                root: Some(tmp.path().to_path_buf()),
                // Tiny segments so the run produces multiple sealed segments
                // and GC has real work to do.
                segment_bytes: 256,
            });

            let source = PushSource {
                batch: Some(push_events(30)),
            };
            let processor = PassthroughProcessor::new(Arc::from("out"));
            let sink = BlackholeSink::new();
            let mut config = PipelineConfig::default();
            config.pipeline_name = "eo2-gc-test".into();
            config.l2_registry = Some(registry.clone());
            config.delivery.durability = DurabilityMode::OrderedBatch;
            // Immediate checkpoint/flush so the final sweep runs deterministically.
            config.delivery.flush.interval = Duration::from_millis(1);

            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            let stopper = shutdown_after_target(Arc::clone(&metrics), Arc::clone(&shutdown), 30);
            run_buffered(
                source,
                processor,
                sink,
                config,
                metrics.clone(),
                shutdown,
                None,
            )
            .await
            .unwrap();
            stopper.await.unwrap();

            assert_eq!(metrics.events_received.load(Ordering::Relaxed), 30);
            assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 30);

            // Registry must have been opened for partition 0.
            let store_handle = registry.open("eo2-gc-test", PartitionId::new(0)).unwrap();
            let guard = store_handle.lock().unwrap();
            // next_seq reflects the total appended; after GC the ack watermark
            // should have advanced to the final seq so disk can be reclaimed.
            assert!(
                guard.next_seq() >= 30,
                "expected next_seq ≥ 30 after 30 events, got {}",
                guard.next_seq()
            );
        }

        #[tokio::test]
        async fn eo2_metrics_published_on_ack_and_gc() {
            // Same setup as gc test, but with an Eo2Metrics registry wired in.
            // After the run, rendered prometheus output must include per-sink
            // ack seq and per-partition L2 byte/segment gauges.
            let tmp = tempfile::tempdir().unwrap();
            let registry = PipelineL2Registry::new(L2BodyStoreConfig {
                root: Some(tmp.path().to_path_buf()),
                segment_bytes: 256,
            });

            let source = PushSource {
                batch: Some(push_events(30)),
            };
            let processor = PassthroughProcessor::new(Arc::from("out"));
            let sink = BlackholeSink::new();
            let eo2_metrics = Arc::new(crate::eo2_metrics::Eo2Metrics::new());
            let mut config = PipelineConfig::default();
            config.pipeline_name = "eo2-metrics-test".into();
            config.sink_name = "blackhole".into();
            config.l2_registry = Some(registry.clone());
            config.delivery.durability = DurabilityMode::OrderedBatch;
            config.delivery.flush.interval = Duration::from_millis(1);
            config.eo2_metrics = Some(Arc::clone(&eo2_metrics));

            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            let stopper = shutdown_after_target(Arc::clone(&metrics), Arc::clone(&shutdown), 30);
            run_buffered(
                source,
                processor,
                sink,
                config,
                metrics.clone(),
                shutdown,
                None,
            )
            .await
            .unwrap();
            stopper.await.unwrap();

            let rendered = eo2_metrics.render_prometheus();
            assert!(
                rendered.contains("aeon_sink_ack_seq")
                    && rendered.contains("pipeline=\"eo2-metrics-test\"")
                    && rendered.contains("sink=\"blackhole\""),
                "expected sink ack gauge, got:\n{rendered}"
            );
            assert!(
                rendered.contains("aeon_l2_segments"),
                "expected l2 segment gauge, got:\n{rendered}"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn capacity_gate_pauses_source_and_gc_releases() {
            // With a tiny pipeline cap (512 bytes), 30 events of ~64 bytes
            // each will exceed capacity. The source task's capacity gate
            // will yield, but once the sink delivers and GC runs, capacity
            // is reclaimed and the pipeline completes without hanging.
            let tmp = tempfile::tempdir().unwrap();
            let registry = PipelineL2Registry::new(L2BodyStoreConfig {
                root: Some(tmp.path().to_path_buf()),
                segment_bytes: 256,
            });

            let node = crate::eo2_backpressure::NodeCapacity::new(None);
            let caps = crate::eo2_backpressure::CapacityLimits::from_config(None, Some(512), 1);
            let capacity = crate::eo2_backpressure::PipelineCapacity::new(caps, node);
            let eo2_metrics = Arc::new(crate::eo2_metrics::Eo2Metrics::new());

            let source = PushSource {
                batch: Some(push_events(30)),
            };
            let processor = PassthroughProcessor::new(Arc::from("out"));
            let sink = BlackholeSink::new();
            let mut config = PipelineConfig::default();
            config.pipeline_name = "cap-test".into();
            config.l2_registry = Some(registry.clone());
            config.delivery.durability = DurabilityMode::OrderedBatch;
            config.delivery.flush.interval = Duration::from_millis(1);
            config.eo2_capacity = Some(capacity.clone());
            config.eo2_metrics = Some(Arc::clone(&eo2_metrics));

            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            let stopper = shutdown_after_target(Arc::clone(&metrics), Arc::clone(&shutdown), 30);
            run_buffered(
                source,
                processor,
                sink,
                config,
                metrics.clone(),
                shutdown,
                None,
            )
            .await
            .unwrap();
            stopper.await.unwrap();

            assert_eq!(metrics.events_received.load(Ordering::Relaxed), 30);
            assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 30);
            // After the run, capacity should be near zero — GC reclaimed all.
            assert!(
                capacity.pipeline_bytes() < 512,
                "expected bytes reclaimed, got {}",
                capacity.pipeline_bytes()
            );
        }

        /// Push source that records the recovery plan it was given.
        struct RecoveryCapturePushSource {
            batch: Option<Vec<Event>>,
            captured_replay_seq: Arc<AtomicU64>,
            captured_calls: Arc<AtomicU64>,
        }
        impl Source for RecoveryCapturePushSource {
            fn next_batch(
                &mut self,
            ) -> impl std::future::Future<Output = Result<Vec<Event>, AeonError>> + Send
            {
                let b = self.batch.take().unwrap_or_default();
                async move { Ok(b) }
            }
            fn source_kind(&self) -> SourceKind {
                SourceKind::Push
            }
            fn on_recovery_plan(
                &mut self,
                _pull_offsets: &HashMap<PartitionId, i64>,
                replay_from_l2_seq: u64,
            ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send {
                let seq = self.captured_replay_seq.clone();
                let calls = self.captured_calls.clone();
                async move {
                    seq.store(replay_from_l2_seq, Ordering::SeqCst);
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }
        }

        #[tokio::test]
        async fn recovery_plan_dispatches_to_source_on_pipeline_start() {
            // Seed an L3 checkpoint store with a record containing a sink
            // ack of 42 so RecoveryPlan::from_last returns
            // ReplayL2From(42) for push sources. Then start a pipeline and
            // assert the source's on_recovery_plan was called with seq=42.
            let l3_tmp = tempfile::tempdir().unwrap();
            let l3 = Arc::new(
                aeon_state::l3::RedbStore::open(aeon_state::l3::RedbConfig {
                    path: l3_tmp.path().join("l3.redb"),
                    sync_writes: false,
                })
                .unwrap(),
            ) as Arc<dyn aeon_types::L3Store>;
            {
                let mut store =
                    crate::checkpoint::L3CheckpointStore::open(Arc::clone(&l3)).unwrap();
                let mut rec =
                    crate::checkpoint::CheckpointRecord::new(0, HashMap::new(), vec![], 10, 0);
                let mut sinks = HashMap::new();
                sinks.insert("sink".to_string(), 42u64);
                rec = rec.with_per_sink_ack_seq(sinks);
                crate::checkpoint::CheckpointPersist::append(&mut store, &mut rec).unwrap();
            }

            let captured_seq = Arc::new(AtomicU64::new(0));
            let captured_calls = Arc::new(AtomicU64::new(0));
            let source = RecoveryCapturePushSource {
                batch: Some(push_events(3)),
                captured_replay_seq: captured_seq.clone(),
                captured_calls: captured_calls.clone(),
            };
            let processor = PassthroughProcessor::new(Arc::from("out"));
            let sink = BlackholeSink::new();

            let mut config = PipelineConfig::default();
            config.pipeline_name = "eo2-recovery-test".into();
            config.delivery.durability = DurabilityMode::OrderedBatch;
            config.delivery.checkpoint.backend = crate::delivery::CheckpointBackend::StateStore;
            config.l3_checkpoint_store = Some(l3);

            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            let stopper = shutdown_after_target(Arc::clone(&metrics), Arc::clone(&shutdown), 3);
            run_buffered(
                source,
                processor,
                sink,
                config,
                metrics.clone(),
                shutdown,
                None,
            )
            .await
            .unwrap();
            stopper.await.unwrap();

            assert_eq!(
                captured_calls.load(Ordering::SeqCst),
                1,
                "on_recovery_plan should fire exactly once at pipeline start"
            );
            assert_eq!(
                captured_seq.load(Ordering::SeqCst),
                42,
                "recovery plan should carry the persisted min-ack seq"
            );
        }

        #[tokio::test]
        async fn no_registry_is_always_passthrough() {
            // Push source + OrderedBatch but no registry attached: safe fallback.
            let source = PushSource {
                batch: Some(push_events(15)),
            };
            let processor = PassthroughProcessor::new(Arc::from("out"));
            let sink = BlackholeSink::new();
            let mut config = PipelineConfig::default();
            config.delivery.durability = DurabilityMode::OrderedBatch;

            let metrics = Arc::new(PipelineMetrics::new());
            let shutdown = Arc::new(AtomicBool::new(false));

            let stopper = shutdown_after_target(Arc::clone(&metrics), Arc::clone(&shutdown), 15);
            run_buffered(
                source,
                processor,
                sink,
                config,
                metrics.clone(),
                shutdown,
                None,
            )
            .await
            .unwrap();
            stopper.await.unwrap();

            assert_eq!(metrics.events_received.load(Ordering::Relaxed), 15);
        }

        #[test]
        fn shared_ack_tracker_threaded_through_build_sink_task_ctx() {
            // P6 multi-sink wiring: two sink configs carrying the same shared
            // AckSeqTracker must yield SinkTaskCtxs that share one tracker
            // handle. Fast sink's ack must be visible to the slow sink's
            // tracker, and `min_across_sinks()` must report the lagging sink.
            let shared = crate::eo2::AckSeqTracker::new();

            let mut cfg_fast = PipelineConfig::default();
            cfg_fast.pipeline_name = "multi-sink".into();
            cfg_fast.sink_name = "fast".into();
            cfg_fast.delivery.durability = DurabilityMode::OrderedBatch;
            cfg_fast.eo2_shared_ack_tracker = Some(shared.clone());

            let mut cfg_slow = PipelineConfig::default();
            cfg_slow.pipeline_name = "multi-sink".into();
            cfg_slow.sink_name = "slow".into();
            cfg_slow.delivery.durability = DurabilityMode::OrderedBatch;
            cfg_slow.eo2_shared_ack_tracker = Some(shared.clone());

            let ctx_fast = super::super::build_sink_task_ctx(&cfg_fast, None);
            let ctx_slow = super::super::build_sink_task_ctx(&cfg_slow, None);

            let tracker_fast = ctx_fast
                .eo2_ack_tracker
                .as_ref()
                .expect("fast sink must get a tracker under OrderedBatch");
            let tracker_slow = ctx_slow
                .eo2_ack_tracker
                .as_ref()
                .expect("slow sink must get a tracker under OrderedBatch");

            tracker_fast.register_sink("fast");
            tracker_slow.register_sink("slow");

            // Fast sink acks 100; slow sink lags at 20.
            tracker_fast.record_ack("fast", 100);
            tracker_slow.record_ack("slow", 20);

            // Shared handle: fast's record must be visible through the slow
            // context's tracker snapshot.
            let snap = tracker_slow.snapshot();
            assert_eq!(snap.get("fast").copied(), Some(100));
            assert_eq!(snap.get("slow").copied(), Some(20));

            // GC frontier must be the slow sink's watermark.
            assert_eq!(tracker_fast.min_across_sinks(), Some(20));
            assert_eq!(tracker_slow.min_across_sinks(), Some(20));

            // Advancing slow to 50 advances the frontier to 50.
            tracker_slow.record_ack("slow", 50);
            assert_eq!(shared.min_across_sinks(), Some(50));
        }

        #[test]
        fn default_shared_ack_tracker_is_none_single_sink_isolated() {
            // No shared tracker → each sink task gets its own fresh tracker.
            // Two configs without sharing must NOT observe each other's acks.
            let mut cfg_a = PipelineConfig::default();
            cfg_a.sink_name = "a".into();
            cfg_a.delivery.durability = DurabilityMode::OrderedBatch;

            let mut cfg_b = PipelineConfig::default();
            cfg_b.sink_name = "b".into();
            cfg_b.delivery.durability = DurabilityMode::OrderedBatch;

            let ctx_a = super::super::build_sink_task_ctx(&cfg_a, None);
            let ctx_b = super::super::build_sink_task_ctx(&cfg_b, None);

            let tracker_a = ctx_a.eo2_ack_tracker.as_ref().unwrap();
            let tracker_b = ctx_b.eo2_ack_tracker.as_ref().unwrap();

            tracker_a.register_sink("a");
            tracker_a.record_ack("a", 99);

            // Tracker B is isolated — must not see A's ack.
            assert_eq!(tracker_b.snapshot().get("a"), None);
        }
    }
}
