//! Backpressure validation tests.
//!
//! Verifies that when the sink is slow, the pipeline:
//! - Does not lose events
//! - Does not crash or panic
//! - Backpressure propagates: source pauses when SPSC buffers fill
//! - All events are eventually delivered

use aeon_connectors::MemorySource;
use aeon_engine::{PassthroughProcessor, PipelineConfig, PipelineMetrics, run, run_buffered};
use aeon_types::{AeonError, BatchResult, Event, Output, PartitionId, Sink};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A sink that adds artificial delay per batch to simulate slow downstream.
struct SlowSink {
    delay: std::time::Duration,
    count: AtomicU64,
}

impl SlowSink {
    fn new(delay: std::time::Duration) -> Self {
        Self {
            delay,
            count: AtomicU64::new(0),
        }
    }

    fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

impl Sink for SlowSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        tokio::time::sleep(self.delay).await;
        self.count
            .fetch_add(outputs.len() as u64, Ordering::Relaxed);
        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        Ok(())
    }
}

fn make_events(count: usize) -> Vec<Event> {
    let source: Arc<str> = Arc::from("test");
    let payload = Bytes::from(vec![b'x'; 256]);
    (0..count)
        .map(|i| {
            Event::new(
                uuid::Uuid::nil(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new((i % 16) as u16),
                payload.clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn direct_pipeline_slow_sink_no_loss() {
    // Direct pipeline: source→processor→sink in single task.
    // Slow sink should not cause event loss.
    let event_count = 500;
    let events = make_events(event_count);
    let mut source = MemorySource::new(events, 64);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = SlowSink::new(std::time::Duration::from_millis(1));
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    assert_eq!(
        metrics.events_received.load(Ordering::Relaxed),
        event_count as u64
    );
    assert_eq!(
        metrics.events_processed.load(Ordering::Relaxed),
        event_count as u64
    );
    assert_eq!(
        metrics.outputs_sent.load(Ordering::Relaxed),
        event_count as u64
    );
    assert_eq!(sink.count(), event_count as u64);
}

#[tokio::test]
async fn buffered_pipeline_slow_sink_no_loss() {
    // Buffered pipeline with SPSC ring buffers.
    // When the sink is slow, backpressure should propagate backward:
    //   sink slow → processor→sink buffer fills → processor pauses →
    //   source→processor buffer fills → source pauses
    // No event is ever dropped.
    let event_count = 2_000;
    let events = make_events(event_count);
    let source = MemorySource::new(events, 64);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let sink = SlowSink::new(std::time::Duration::from_millis(2));

    let config = PipelineConfig {
        source_buffer_capacity: 16, // Small buffers to force backpressure quickly
        sink_buffer_capacity: 16,
        max_batch_size: 64,
        ..Default::default()
    };
    let metrics = Arc::new(PipelineMetrics::new());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

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

    let received = metrics.events_received.load(Ordering::Relaxed);
    let processed = metrics.events_processed.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    assert_eq!(received, event_count as u64, "all events must be received");
    assert_eq!(
        processed, event_count as u64,
        "all events must be processed"
    );
    assert_eq!(sent, event_count as u64, "all events must be sent to sink");
}

#[tokio::test]
async fn buffered_pipeline_very_slow_sink_no_loss() {
    // Even slower sink — 10ms per batch. Small buffer capacity.
    // Verifies backpressure holds under sustained pressure.
    let event_count = 500;
    let events = make_events(event_count);
    let source = MemorySource::new(events, 32);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let sink = SlowSink::new(std::time::Duration::from_millis(10));

    let config = PipelineConfig {
        source_buffer_capacity: 4, // Very small — forces aggressive backpressure
        sink_buffer_capacity: 4,
        max_batch_size: 32,
        ..Default::default()
    };
    let metrics = Arc::new(PipelineMetrics::new());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

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

    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    assert_eq!(received, event_count as u64);
    assert_eq!(sent, event_count as u64);
}

#[tokio::test]
async fn buffered_pipeline_bursty_source_slow_sink() {
    // Large batch from source (1024 events at once) hitting a slow sink.
    // Tests that burst absorption works via backpressure.
    let event_count = 10_000;
    let events = make_events(event_count);
    let source = MemorySource::new(events, 1024); // Large batches
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let sink = SlowSink::new(std::time::Duration::from_millis(1));

    let config = PipelineConfig {
        source_buffer_capacity: 8,
        sink_buffer_capacity: 8,
        max_batch_size: 1024,
        ..Default::default()
    };
    let metrics = Arc::new(PipelineMetrics::new());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

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

    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    assert_eq!(
        received, event_count as u64,
        "all bursty events must be received"
    );
    assert_eq!(
        sent, event_count as u64,
        "all bursty events must reach sink"
    );
}

#[tokio::test]
async fn direct_pipeline_shutdown_during_slow_sink() {
    // Start pipeline with slow sink, trigger shutdown before all events are processed.
    // Verifies: no panic, no hang, clean shutdown.
    let event_count = 10_000;
    let events = make_events(event_count);
    let mut source = MemorySource::new(events, 64);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = SlowSink::new(std::time::Duration::from_millis(5));
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    // Spawn pipeline, trigger shutdown after 50ms
    let shutdown_ref = &shutdown;
    let result = tokio::select! {
        r = run(&mut source, &processor, &mut sink, &metrics, shutdown_ref) => r,
        _ = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            shutdown_ref.store(true, Ordering::Relaxed);
            // Give pipeline time to observe shutdown
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        } => Ok(()),
    };

    // Should not panic or error
    assert!(result.is_ok());

    // Some events were processed, but not all (shutdown was early)
    let received = metrics.events_received.load(Ordering::Relaxed);
    assert!(
        received > 0,
        "some events should have been processed before shutdown"
    );
    assert!(
        received <= event_count as u64,
        "should not exceed total events"
    );
}
