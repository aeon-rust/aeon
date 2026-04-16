//! Chaos / Fault-Injection Tests
//!
//! Injects controlled faults into pipeline components and verifies:
//! - Pipeline propagates errors correctly (no silent data loss)
//! - Retryable errors surface as retryable pipeline failures
//! - Non-retryable errors surface as non-retryable pipeline failures
//! - Event counts remain consistent through partial failures

use aeon_engine::{PassthroughProcessor, PipelineMetrics, run};
use aeon_types::PartitionId;
use aeon_types::delivery::BatchResult;
use aeon_types::error::AeonError;
use aeon_types::event::{Event, Output};
use aeon_types::traits::{Processor, Sink, Source};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Source that fails after N successful batches.
struct FailingSource {
    events_per_batch: usize,
    batches_before_fail: u64,
    batch_count: u64,
    fail_mode: FailMode,
    source_name: Arc<str>,
}

#[derive(Clone, Copy)]
enum FailMode {
    /// Return a retryable connection error
    RetryableError,
    /// Return a non-retryable config error
    FatalError,
    /// Return empty batches (graceful completion) after the threshold
    GracefulStop,
}

impl FailingSource {
    fn new(events_per_batch: usize, batches_before_fail: u64, fail_mode: FailMode) -> Self {
        Self {
            events_per_batch,
            batches_before_fail,
            batch_count: 0,
            fail_mode,
            source_name: Arc::from("chaos-source"),
        }
    }
}

impl Source for FailingSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        if self.batch_count >= self.batches_before_fail {
            return match self.fail_mode {
                FailMode::RetryableError => {
                    Err(AeonError::connection("chaos: source connection lost"))
                }
                FailMode::FatalError => Err(AeonError::config("chaos: source fatal error")),
                FailMode::GracefulStop => Ok(Vec::new()),
            };
        }

        self.batch_count += 1;
        let events: Vec<Event> = (0..self.events_per_batch)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i as i64,
                    Arc::clone(&self.source_name),
                    PartitionId::new(0),
                    Bytes::from_static(b"chaos-payload"),
                )
            })
            .collect();
        Ok(events)
    }
}

/// Processor that fails on every Nth event.
struct FaultyProcessor {
    destination: Arc<str>,
    call_count: AtomicU64,
    fail_every_n: u64,
}

impl FaultyProcessor {
    fn new(fail_every_n: u64) -> Self {
        Self {
            destination: Arc::from("output"),
            call_count: AtomicU64::new(0),
            fail_every_n,
        }
    }
}

impl Processor for FaultyProcessor {
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
        let count = self.call_count.fetch_add(1, Ordering::Relaxed) + 1;
        if count % self.fail_every_n == 0 {
            return Err(AeonError::processor(format!(
                "chaos: processor fault on event #{count}"
            )));
        }
        Ok(vec![Output {
            destination: Arc::clone(&self.destination),
            key: None,
            payload: event.payload,
            headers: event.metadata,
            source_ts: event.source_ts,
            source_event_id: Some(event.id),
            source_partition: Some(event.partition),
            source_offset: None,
            l2_seq: None,
        }])
    }

    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        let mut outputs = Vec::with_capacity(events.len());
        for event in events {
            outputs.extend(self.process(event)?);
        }
        Ok(outputs)
    }
}

/// Sink that fails on every Nth write_batch call.
struct FaultySink {
    call_count: u64,
    fail_every_n: u64,
    total_written: u64,
}

impl FaultySink {
    fn new(fail_every_n: u64) -> Self {
        Self {
            call_count: 0,
            fail_every_n,
            total_written: 0,
        }
    }

    fn total_written(&self) -> u64 {
        self.total_written
    }
}

impl Sink for FaultySink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        self.call_count += 1;
        if self.call_count % self.fail_every_n == 0 {
            return Err(AeonError::connection("chaos: sink write failed"));
        }
        let count = outputs.len();
        self.total_written += count as u64;
        let ids: Vec<uuid::Uuid> = outputs.iter().filter_map(|o| o.source_event_id).collect();
        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        Ok(())
    }
}

// ─── Test 1: Source connection error propagates ──────────────────

#[tokio::test]
async fn chaos_source_connection_error_propagates() {
    let mut source = FailingSource::new(100, 5, FailMode::RetryableError);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = aeon_connectors::BlackholeSink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    let result = run(&mut source, &processor, &mut sink, &metrics, &shutdown).await;

    assert!(result.is_err(), "pipeline should propagate source error");
    let err = result.unwrap_err();
    assert!(
        err.is_retryable(),
        "source connection error should be retryable"
    );

    // Should have processed exactly 5 batches × 100 events before failing
    let received = metrics.events_received.load(Ordering::Relaxed);
    assert_eq!(received, 500, "should have received 5 batches before error");
}

// ─── Test 2: Source fatal error propagates ───────────────────────

#[tokio::test]
async fn chaos_source_fatal_error_propagates() {
    let mut source = FailingSource::new(50, 3, FailMode::FatalError);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = aeon_connectors::BlackholeSink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    let result = run(&mut source, &processor, &mut sink, &metrics, &shutdown).await;

    assert!(result.is_err(), "pipeline should propagate fatal error");
    let err = result.unwrap_err();
    assert!(!err.is_retryable(), "config error should not be retryable");

    let received = metrics.events_received.load(Ordering::Relaxed);
    assert_eq!(received, 150, "should have received 3 batches of 50");
}

// ─── Test 3: Processor error propagates ─────────────────────────

#[tokio::test]
async fn chaos_processor_error_propagates() {
    // Source delivers 10 batches of 100 events, processor fails on every 50th
    let mut source = FailingSource::new(100, 10, FailMode::GracefulStop);
    let processor = FaultyProcessor::new(50);
    let mut sink = aeon_connectors::BlackholeSink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    let result = run(&mut source, &processor, &mut sink, &metrics, &shutdown).await;

    // Pipeline should fail on the first batch when event #50 errors
    assert!(result.is_err(), "pipeline should propagate processor error");
}

// ─── Test 4: Sink write error propagates ────────────────────────

#[tokio::test]
async fn chaos_sink_error_propagates() {
    let mut source = FailingSource::new(100, 10, FailMode::GracefulStop);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = FaultySink::new(3); // fail every 3rd write_batch
    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    let result = run(&mut source, &processor, &mut sink, &metrics, &shutdown).await;

    assert!(result.is_err(), "pipeline should propagate sink error");
    let err = result.unwrap_err();
    assert!(
        err.is_retryable(),
        "connection error from sink should be retryable"
    );

    // Sink should have written some events before failure
    assert!(
        sink.total_written() > 0,
        "sink should have written events before the error"
    );
}

// ─── Test 5: Graceful shutdown via AtomicBool ────────────────────

#[tokio::test]
async fn chaos_shutdown_signal_stops_pipeline() {
    // Source would run for 1000 batches, but we signal shutdown after 100ms
    let mut source = FailingSource::new(100, 1000, FailMode::GracefulStop);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = aeon_connectors::BlackholeSink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = Arc::new(AtomicBool::new(false));

    // Signal shutdown after a short delay
    let shutdown_clone = Arc::clone(&shutdown);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        shutdown_clone.store(true, Ordering::Relaxed);
    });

    let result = run(&mut source, &processor, &mut sink, &metrics, &shutdown).await;

    assert!(result.is_ok(), "shutdown should be graceful, not an error");

    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);
    assert!(received > 0, "should have processed some events");
    assert_eq!(received, sent, "received and sent must match (no loss)");
}

// ─── Test 6: Event count consistency through normal operation ────

#[tokio::test]
async fn chaos_metrics_consistency_normal_path() {
    let batches = 50u64;
    let per_batch = 200;
    let mut source = FailingSource::new(per_batch, batches, FailMode::GracefulStop);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = aeon_connectors::BlackholeSink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    let result = run(&mut source, &processor, &mut sink, &metrics, &shutdown).await;
    assert!(result.is_ok(), "normal pipeline should succeed");

    let received = metrics.events_received.load(Ordering::Relaxed);
    let processed = metrics.events_processed.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);
    let expected = batches * per_batch as u64;

    assert_eq!(received, expected, "received must match generated");
    assert_eq!(received, processed, "received must equal processed");
    assert_eq!(processed, sent, "processed must equal sent");
}
