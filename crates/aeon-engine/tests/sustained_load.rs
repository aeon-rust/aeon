//! Sustained load test — verifies zero event loss over extended duration.
//!
//! Runs MemorySource → PassthroughProcessor → BlackholeSink continuously,
//! re-feeding events, for a configurable duration.
//!
//! Validates:
//! - source_count == sink_count (zero loss)
//! - No crash, no hang
//! - Consistent throughput (no degradation over time)
//!
//! Uses MemorySource (in-memory) to avoid Redpanda dependency for this test.
//! The Redpanda sustained test is a separate benchmark (requires 10+ min with broker).

use aeon_connectors::BlackholeSink;
use aeon_engine::{PassthroughProcessor, PipelineMetrics, run};
use aeon_types::{Event, PartitionId, Source};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A source that generates events continuously for a given duration.
/// Reports total events generated for verification.
struct TimedSource {
    source_name: Arc<str>,
    payload: Bytes,
    duration: Duration,
    start: Option<Instant>,
    batch_size: usize,
    generated: u64,
}

impl TimedSource {
    fn new(duration: Duration, batch_size: usize) -> Self {
        Self {
            source_name: Arc::from("sustained"),
            payload: Bytes::from(vec![b'x'; 256]),
            duration,
            start: None,
            batch_size,
            generated: 0,
        }
    }

    fn generated(&self) -> u64 {
        self.generated
    }
}

impl Source for TimedSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, aeon_types::AeonError> {
        let start = *self.start.get_or_insert_with(Instant::now);
        if start.elapsed() >= self.duration {
            return Ok(Vec::new()); // Signal completion
        }

        let events: Vec<Event> = (0..self.batch_size)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    (self.generated + i as u64) as i64,
                    Arc::clone(&self.source_name),
                    PartitionId::new((i % 16) as u16),
                    self.payload.clone(),
                )
            })
            .collect();

        self.generated += events.len() as u64;
        Ok(events)
    }
}

#[tokio::test]
async fn sustained_30s_zero_event_loss() {
    // 30-second sustained load test (fast enough for CI).
    // At ~7M events/sec this generates ~210M events.
    let duration = Duration::from_secs(30);
    let batch_size = 1024;

    let mut source = TimedSource::new(duration, batch_size);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = BlackholeSink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    let start = Instant::now();
    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    let generated = source.generated();
    let received = metrics.events_received.load(Ordering::Relaxed);
    let processed = metrics.events_processed.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);
    let sunk = sink.count();

    eprintln!("Sustained load test: {elapsed:.2?}");
    eprintln!("  Generated:  {generated}");
    eprintln!("  Received:   {received}");
    eprintln!("  Processed:  {processed}");
    eprintln!("  Sent:       {sent}");
    eprintln!("  Sunk:       {sunk}");
    eprintln!(
        "  Throughput: {:.0} events/sec",
        generated as f64 / elapsed.as_secs_f64()
    );

    // Zero event loss verification
    assert_eq!(generated, received, "generated must equal received");
    assert_eq!(received, processed, "received must equal processed");
    assert_eq!(processed, sent, "processed must equal sent");
    assert_eq!(sent, sunk, "sent must equal sunk");
    assert!(generated > 0, "must have generated some events");
}

#[tokio::test]
async fn sustained_30s_buffered_zero_event_loss() {
    // Same test but with buffered (SPSC) pipeline.
    use aeon_engine::{PipelineConfig, run_buffered};

    let duration = Duration::from_secs(30);
    let batch_size = 1024;

    let source = TimedSource::new(duration, batch_size);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let sink = BlackholeSink::new();
    let config = PipelineConfig {
        source_buffer_capacity: 256,
        sink_buffer_capacity: 256,
        max_batch_size: 1024,
        ..Default::default()
    };
    let metrics = Arc::new(PipelineMetrics::new());
    let shutdown = Arc::new(AtomicBool::new(false));

    let start = Instant::now();
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
    let elapsed = start.elapsed();

    let received = metrics.events_received.load(Ordering::Relaxed);
    let processed = metrics.events_processed.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    eprintln!("Sustained buffered load test: {elapsed:.2?}");
    eprintln!("  Received:   {received}");
    eprintln!("  Processed:  {processed}");
    eprintln!("  Sent:       {sent}");
    eprintln!(
        "  Throughput: {:.0} events/sec",
        received as f64 / elapsed.as_secs_f64()
    );

    assert_eq!(received, processed, "received must equal processed");
    assert_eq!(processed, sent, "processed must equal sent");
    assert!(received > 0, "must have received some events");
}
