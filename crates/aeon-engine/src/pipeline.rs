//! Pipeline orchestrator — wires source→processor→sink with SPSC ring buffers.
//!
//! The pipeline runs three async tasks:
//! 1. **Source task**: polls `source.next_batch()`, pushes events into source→processor SPSC
//! 2. **Processor task**: pops events from SPSC, calls `processor.process_batch()`,
//!    pushes outputs into processor→sink SPSC
//! 3. **Sink task**: pops outputs from SPSC, calls `sink.write_batch()`
//!
//! Backpressure: SPSC full → producer yields → upstream pauses. Zero data loss by design.

use aeon_types::{AeonError, Event, Output, Processor, Sink, Source};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Default SPSC ring buffer capacity (events/outputs).
const DEFAULT_BUFFER_CAPACITY: usize = 8192;

/// Pipeline configuration.
pub struct PipelineConfig {
    /// SPSC buffer capacity between source and processor.
    pub source_buffer_capacity: usize,
    /// SPSC buffer capacity between processor and sink.
    pub sink_buffer_capacity: usize,
    /// Maximum batch size for processor (limits work per iteration).
    pub max_batch_size: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            source_buffer_capacity: DEFAULT_BUFFER_CAPACITY,
            sink_buffer_capacity: DEFAULT_BUFFER_CAPACITY,
            max_batch_size: 1024,
        }
    }
}

/// Pipeline metrics — atomic counters for concurrent access.
pub struct PipelineMetrics {
    pub events_received: AtomicU64,
    pub events_processed: AtomicU64,
    pub outputs_sent: AtomicU64,
}

impl PipelineMetrics {
    pub fn new() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_processed: AtomicU64::new(0),
            outputs_sent: AtomicU64::new(0),
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

        let out_count = outputs.len() as u64;
        sink.write_batch(outputs).await?;
        metrics.outputs_sent.fetch_add(out_count, Ordering::Relaxed);
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
) -> Result<(), AeonError>
where
    S: Source + Send + 'static,
    P: Processor + Send + Sync + 'static,
    K: Sink + Send + 'static,
{
    let (mut src_prod, mut src_cons) =
        rtrb::RingBuffer::<Vec<Event>>::new(config.source_buffer_capacity);
    let (mut sink_prod, mut sink_cons) =
        rtrb::RingBuffer::<Vec<Output>>::new(config.sink_buffer_capacity);

    let shutdown_src = Arc::clone(&shutdown);
    let metrics_src = Arc::clone(&metrics);

    // Source task: poll source, push event batches into SPSC
    let source_handle = tokio::spawn(async move {
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
    let processor_handle = tokio::spawn(async move {
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

    // Sink task: pop outputs, write to sink
    let sink_handle = tokio::spawn(async move {
        loop {
            match sink_cons.pop() {
                Ok(outputs) => {
                    let count = outputs.len() as u64;
                    sink.write_batch(outputs).await?;
                    metrics_sink
                        .outputs_sent
                        .fetch_add(count, Ordering::Relaxed);
                }
                Err(_) => {
                    if sink_cons.is_abandoned() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
        sink.flush().await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processor::PassthroughProcessor;
    use aeon_connectors::{BlackholeSink, MemorySink, MemorySource};
    use aeon_types::PartitionId;
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
        )
        .await
        .unwrap();

        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 50_000);
        assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 50_000);
    }
}
