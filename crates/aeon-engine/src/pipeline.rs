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
use aeon_types::{AeonError, Event, Output, PartitionId, Processor, Sink, Source};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

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
}

impl PipelineMetrics {
    pub fn new() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_processed: AtomicU64::new(0),
            outputs_sent: AtomicU64::new(0),
            checkpoints_written: AtomicU64::new(0),
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
    mut sink: K,
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
    let (mut sink_prod, mut sink_cons) =
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
    let delivery_strategy = config.delivery.strategy;
    let flush_interval = config.delivery.flush.interval;
    let max_pending = config.delivery.flush.max_pending;
    let adaptive_flush = config.delivery.flush.adaptive;
    let adaptive_min_divisor = config.delivery.flush.adaptive_min_divisor;
    let adaptive_max_multiplier = config.delivery.flush.adaptive_max_multiplier;

    // Initialize checkpoint writer if WAL backend is configured.
    let checkpoint_writer = if config.delivery.checkpoint.backend == CheckpointBackend::Wal {
        let dir = config
            .delivery
            .checkpoint
            .dir
            .unwrap_or_else(|| {
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

    // Sink task: pop outputs, write to sink.
    // PerEvent/OrderedBatch: write_batch blocks on delivery acks.
    // UnorderedBatch: write_batch enqueues fast, flush() called at checkpoint intervals.
    //
    // Delivery ledger integration:
    // - Track each output with source_event_id before write_batch
    // - Mark acked on successful delivery
    // - Populate checkpoint source_offsets from ledger
    let sink_core = core_assignment.map(|c| c.sink);
    let sink_ledger = ledger;
    let sink_handle = tokio::spawn(async move {
        if let Some(core) = sink_core {
            pin_to_core(core);
        }
        let mut last_flush = Instant::now();
        let mut pending_count: u64 = 0;
        let mut delivered_since_checkpoint: u64 = 0;
        let mut failed_since_checkpoint: u64 = 0;
        let mut ckpt_writer = checkpoint_writer;

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
                                    let partition =
                                        o.source_partition.unwrap_or(PartitionId::new(0));
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
                            let delivered_count =
                                batch_result.delivered.len() as u64;
                            let total_count = count;
                            metrics_sink
                                .outputs_sent
                                .fetch_add(delivered_count, Ordering::Relaxed);
                            delivered_since_checkpoint += delivered_count;
                            acked_since_last_flush += delivered_count;
                            events_since_last_flush += total_count;
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
                            write_checkpoint(
                                &mut ckpt_writer,
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
                            write_checkpoint(
                                &mut ckpt_writer,
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
        if delivered_since_checkpoint > 0 || failed_since_checkpoint > 0 {
            write_checkpoint(
                &mut ckpt_writer,
                &sink_ledger,
                &metrics_sink,
                &mut delivered_since_checkpoint,
                &mut failed_since_checkpoint,
            );
        }

        Ok::<(), AeonError>(())
    });

    // Wait for all tasks
    let (src_result, proc_result, sink_result) =
        tokio::join!(source_handle, processor_handle, sink_handle);

    // Propagate errors
    src_result.map_err(|e| AeonError::processor(format!("source task panicked: {e}")))??;
    proc_result.map_err(|e| AeonError::processor(format!("processor task panicked: {e}")))??;
    sink_result.map_err(|e| AeonError::processor(format!("sink task panicked: {e}")))??;

    Ok(())
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
}
