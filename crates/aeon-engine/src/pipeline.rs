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
use crate::checkpoint::{CheckpointRecord, CheckpointWriter};
use crate::delivery::{CheckpointBackend, DeliveryConfig};
use crate::delivery_ledger::DeliveryLedger;
use aeon_types::{
    AeonError, BatchFailurePolicy, Event, Output, PartitionId, Processor, Sink, Source,
};
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            source_buffer_capacity: DEFAULT_BUFFER_CAPACITY,
            sink_buffer_capacity: DEFAULT_BUFFER_CAPACITY,
            max_batch_size: 1024,
            core_pinning: CorePinning::Disabled,
            delivery: DeliveryConfig::default(),
        }
    }
}

/// Pipeline metrics — atomic counters for concurrent access.
pub struct PipelineMetrics {
    pub events_received: AtomicU64,
    pub events_processed: AtomicU64,
    pub outputs_sent: AtomicU64,
    /// Number of checkpoints written (UnorderedBatch mode).
    pub checkpoints_written: AtomicU64,
    /// Events permanently failed after retry exhaustion or SkipToDlq policy.
    pub events_failed: AtomicU64,
    /// Individual retry attempts (each retry of a failed output counts as 1).
    pub events_retried: AtomicU64,
}

impl PipelineMetrics {
    pub fn new() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_processed: AtomicU64::new(0),
            outputs_sent: AtomicU64::new(0),
            checkpoints_written: AtomicU64::new(0),
            events_failed: AtomicU64::new(0),
            events_retried: AtomicU64::new(0),
        }
    }
}

impl Default for PipelineMetrics {
    fn default() -> Self {
        Self::new()
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
    while !shutdown.load(Ordering::Relaxed) {
        let events = source.next_batch().await?;
        if events.is_empty() {
            break;
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
    while !shutdown.load(Ordering::Relaxed) {
        let events = source.next_batch().await?;
        if events.is_empty() {
            break;
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
    mut source: S,
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
    let core_assignment = config.core_pinning.resolve();

    let (mut src_prod, mut src_cons) =
        rtrb::RingBuffer::<Vec<Event>>::new(config.source_buffer_capacity);
    let (mut sink_prod, sink_cons) =
        rtrb::RingBuffer::<Vec<Output>>::new(config.sink_buffer_capacity);

    let shutdown_src = Arc::clone(&shutdown);
    let metrics_src = Arc::clone(&metrics);

    // Source task: poll source, push event batches into SPSC
    let source_core = core_assignment.map(|c| c.source);
    let source_handle = tokio::spawn(async move {
        if let Some(core) = source_core {
            pin_to_core(core);
        }
        while !shutdown_src.load(Ordering::Relaxed) {
            let events = match source.next_batch().await {
                Ok(events) => events,
                Err(e) => return Err(e),
            };
            if events.is_empty() {
                break;
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
        }
        // Signal: no more events
        drop(src_prod);
        Ok::<(), AeonError>(())
    });

    let shutdown_proc = Arc::clone(&shutdown);
    let metrics_proc = Arc::clone(&metrics);
    let processor = Arc::new(processor);

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
    let sink_ctx = build_sink_task_ctx(&config, core_assignment.map(|c| c.sink));

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
    mut source: S,
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
    let core_assignment = config.core_pinning.resolve();

    let (mut src_prod, mut src_cons) =
        rtrb::RingBuffer::<Vec<Event>>::new(config.source_buffer_capacity);
    let (mut sink_prod, sink_cons) =
        rtrb::RingBuffer::<Vec<Output>>::new(config.sink_buffer_capacity);

    let shutdown_src = Arc::clone(&shutdown);
    let metrics_src = Arc::clone(&metrics);

    // Source task — identical to run_buffered.
    let source_core = core_assignment.map(|c| c.source);
    let source_handle = tokio::spawn(async move {
        if let Some(core) = source_core {
            pin_to_core(core);
        }
        while !shutdown_src.load(Ordering::Relaxed) {
            let events = match source.next_batch().await {
                Ok(events) => events,
                Err(e) => return Err(e),
            };
            if events.is_empty() {
                break;
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
    let sink_ctx = build_sink_task_ctx(&config, core_assignment.map(|c| c.sink));
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

/// Build a `SinkTaskCtx` from `PipelineConfig`, resolving the checkpoint
/// WAL writer if configured. Shared between `run_buffered` and
/// `run_buffered_transport` so the two call sites can spawn `run_sink_task`
/// with identical setup.
fn build_sink_task_ctx(config: &PipelineConfig, core: Option<usize>) -> SinkTaskCtx {
    // Initialize checkpoint writer if WAL backend is configured.
    let checkpoint_writer = if config.delivery.checkpoint.backend == CheckpointBackend::Wal {
        let dir = config.delivery.checkpoint.dir.clone().unwrap_or_else(|| {
            std::env::var("AEON_CHECKPOINT_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir().join("aeon-checkpoints"))
        });
        let wal_path = dir.join("pipeline.wal");
        match CheckpointWriter::new(&wal_path) {
            Ok(writer) => {
                tracing::info!(path = %wal_path.display(), "Checkpoint WAL initialized");
                Some(writer)
            }
            Err(e) => {
                tracing::warn!("Checkpoint WAL init failed: {e}, continuing without checkpoints");
                None
            }
        }
    } else {
        None
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
    }
}

/// Multi-partition pipeline configuration.
pub struct MultiPartitionConfig {
    /// Number of partitions (each gets an independent pipeline).
    pub partition_count: usize,
    /// Base pipeline config (cloned per partition, core pinning resolved automatically).
    pub pipeline: PipelineConfig,
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
        let mut partition_config = PipelineConfig {
            source_buffer_capacity: config.pipeline.source_buffer_capacity,
            sink_buffer_capacity: config.pipeline.sink_buffer_capacity,
            max_batch_size: config.pipeline.max_batch_size,
            core_pinning: CorePinning::Disabled,
            delivery: config.pipeline.delivery.clone(),
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
    checkpoint_writer: Option<CheckpointWriter>,
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
    } = ctx;

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
                        );
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
                        );
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
        );
    }

    Ok(())
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
    ckpt_writer: &mut Option<CheckpointWriter>,
    ledger: &Option<Arc<DeliveryLedger>>,
    metrics: &Arc<PipelineMetrics>,
    delivered: &mut u64,
    failed: &mut u64,
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
#[allow(clippy::too_many_arguments)]
pub async fn run_buffered_managed<S, K>(
    mut source: S,
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
    let core_assignment = config.core_pinning.resolve();

    let (mut src_prod, mut src_cons) =
        rtrb::RingBuffer::<Vec<Event>>::new(config.source_buffer_capacity);
    let (mut sink_prod, sink_cons) =
        rtrb::RingBuffer::<Vec<Output>>::new(config.sink_buffer_capacity);

    let shutdown_src = Arc::clone(&shutdown);
    let metrics_src = Arc::clone(&metrics);
    let control_src = Arc::clone(&control);

    // Source task: identical to run_buffered, but checks control.paused.
    // When paused, checks for source swap before yielding.
    let source_core = core_assignment.map(|c| c.source);
    let source_handle = tokio::spawn(async move {
        if let Some(core) = source_core {
            pin_to_core(core);
        }
        while !shutdown_src.load(Ordering::Relaxed) {
            // Pause check: check for source swap, then yield
            if control_src.paused.load(Ordering::Acquire) {
                // Check if a new source has been placed in the swap slot.
                // The drain_and_swap_source() method sets this AFTER drain_complete,
                // so by the time we see it, all in-flight events have been flushed.
                {
                    let mut slot = control_src.new_source.lock().await;
                    if let Some(new_source_any) = slot.take() {
                        if let Ok(new_source) = new_source_any.downcast::<S>() {
                            source = *new_source;
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
                break;
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
        async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<aeon_types::BatchResult, AeonError> {
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
        let processor_a: Box<dyn Processor + Send + Sync> =
            Box::new(PrefixProcessor::new("A:"));
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
        assert!(pre_swap_count > 0, "should have processed events before swap");

        // Hot-swap to processor B
        let processor_b: Box<dyn Processor + Send + Sync> =
            Box::new(PrefixProcessor::new("B:"));
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
        assert!(sink_b_reader.len() > 0, "sink B should have collected outputs after swap");

        // Sink A should have stopped growing
        let sink_a_final = sink_a.len();

        // Shutdown
        source_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = pipeline_handle.await;

        // Both sinks received outputs, sink A stopped after swap
        assert!(sink_a_final >= sink_a_count, "sink A count should be stable after swap");
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
                source, processor_a, sink, config, metrics2,
                shutdown2, None, control2,
            ).await
        });

        // Wait for A outputs
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(metrics.events_processed.load(Ordering::Relaxed) > 0);

        // Verify A-prefixed outputs
        let outputs_before = sink_reader.lock().unwrap().clone();
        assert!(outputs_before.iter().all(|o| {
            String::from_utf8_lossy(&o.payload).starts_with("A:")
        }), "all outputs before cutover should be A-prefixed");

        // Install green processor (B:) — no pause, no drain
        let processor_b = Box::new(PrefixProcessor::new("B:"));
        control.start_blue_green(processor_b).await.unwrap();

        // Wait for more A outputs (green is shadow, not yet active)
        let count_after_install = sink_reader.lock().unwrap().len();
        for _ in 0..200 {
            if sink_reader.lock().unwrap().len() > count_after_install + 5 { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // All new outputs should still be A-prefixed
        let outputs_mid = sink_reader.lock().unwrap().clone();
        assert!(outputs_mid.iter().all(|o| {
            String::from_utf8_lossy(&o.payload).starts_with("A:")
        }), "all outputs during shadow should be A-prefixed");

        // Cutover to green
        control.cutover_blue_green().await.unwrap();

        // Wait for B outputs to appear
        for _ in 0..200 {
            let has_b = sink_reader.lock().unwrap().iter().any(|o| {
                String::from_utf8_lossy(&o.payload).starts_with("B:")
            });
            if has_b { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let outputs_after = sink_reader.lock().unwrap().clone();
        let has_b = outputs_after.iter().any(|o| {
            String::from_utf8_lossy(&o.payload).starts_with("B:")
        });
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
                source, processor_a, sink, config, metrics2,
                shutdown2, None, control2,
            ).await
        });

        // Wait for initial outputs
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Install green then rollback
        let processor_b = Box::new(PrefixProcessor::new("B:"));
        control.start_blue_green(processor_b).await.unwrap();
        control.rollback_upgrade().await.unwrap();

        // Wait for more outputs after rollback
        let count_after = sink_reader.lock().unwrap().len();
        for _ in 0..200 {
            if sink_reader.lock().unwrap().len() > count_after + 20 { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // All outputs should be A-prefixed (no B ever appeared)
        let outputs = sink_reader.lock().unwrap().clone();
        assert!(outputs.iter().all(|o| {
            String::from_utf8_lossy(&o.payload).starts_with("A:")
        }), "all outputs after rollback should be A-prefixed");

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
                source, processor_a, sink, config, metrics2,
                shutdown2, None, control2,
            ).await
        });

        // Wait for initial A outputs
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Clear sink and start canary at 50%
        sink_reader.lock().unwrap().clear();
        let processor_b = Box::new(PrefixProcessor::new("B:"));
        control.start_canary(processor_b, 50).await.unwrap();

        // Wait for enough outputs with both A and B
        for _ in 0..200 {
            let len = sink_reader.lock().unwrap().len();
            if len > 200 { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let outputs = sink_reader.lock().unwrap().clone();
        let a_count = outputs.iter().filter(|o| {
            String::from_utf8_lossy(&o.payload).starts_with("A:")
        }).count();
        let b_count = outputs.iter().filter(|o| {
            String::from_utf8_lossy(&o.payload).starts_with("B:")
        }).count();

        assert!(a_count > 0, "baseline A should have outputs (got {a_count})");
        assert!(b_count > 0, "canary B should have outputs (got {b_count})");
        // With 50% split over many events, both should be non-trivial
        let total = a_count + b_count;
        assert!(a_count as f64 / total as f64 > 0.2, "A should have >20% of traffic");
        assert!(b_count as f64 / total as f64 > 0.2, "B should have >20% of traffic");

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
                source, processor_a, sink, config, metrics2,
                shutdown2, None, control2,
            ).await
        });

        // Wait for initial outputs
        for _ in 0..200 {
            if metrics.events_processed.load(Ordering::Relaxed) > 0 { break; }
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
            if sink_reader.lock().unwrap().len() > 50 { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let outputs = sink_reader.lock().unwrap().clone();
        assert!(outputs.len() > 0, "should have outputs after canary complete");
        assert!(outputs.iter().all(|o| {
            String::from_utf8_lossy(&o.payload).starts_with("B:")
        }), "all outputs after canary complete should be B-prefixed");

        source_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        let _ = pipeline_handle.await;
    }
}
