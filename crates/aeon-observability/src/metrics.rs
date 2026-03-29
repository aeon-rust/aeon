//! Enhanced Prometheus metrics — per-partition counters, latency histograms,
//! backpressure gauges, DLQ counts, circuit breaker state.
//!
//! All metrics use atomic operations — no locks on the hot path.

use crate::histogram::LatencyHistogram;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Maximum number of partitions tracked (pre-allocated to avoid allocation).
const MAX_PARTITIONS: usize = 64;

/// Comprehensive pipeline metrics.
///
/// Extends the basic PipelineMetrics with per-partition tracking,
/// latency histograms, and fault tolerance counters.
pub struct PipelineObservability {
    // ── Throughput counters ──
    /// Total events received from all sources.
    pub events_received: AtomicU64,
    /// Total events processed by all processors.
    pub events_processed: AtomicU64,
    /// Total outputs sent to all sinks.
    pub outputs_sent: AtomicU64,

    // ── Per-partition counters ──
    /// Events received per partition.
    partition_received: [AtomicU64; MAX_PARTITIONS],
    /// Events processed per partition.
    partition_processed: [AtomicU64; MAX_PARTITIONS],

    // ── Latency ──
    /// End-to-end event latency (source → sink).
    pub e2e_latency: LatencyHistogram,
    /// Source batch latency.
    pub source_latency: LatencyHistogram,
    /// Processor batch latency.
    pub processor_latency: LatencyHistogram,
    /// Sink batch latency.
    pub sink_latency: LatencyHistogram,

    // ── Backpressure ──
    /// Current source→processor buffer utilization (0-100%).
    pub source_buffer_pct: AtomicU64,
    /// Current processor→sink buffer utilization (0-100%).
    pub sink_buffer_pct: AtomicU64,

    // ── Fault tolerance ──
    /// Total events sent to DLQ.
    pub dlq_count: AtomicU64,
    /// Total retries attempted.
    pub retry_count: AtomicU64,
    /// Circuit breaker state (0=Closed, 1=Open, 2=HalfOpen).
    pub circuit_breaker_state: AtomicI64,
    /// Total circuit breaker trips.
    pub circuit_breaker_trips: AtomicU64,

    // ── Batch ──
    /// Current batch size (adaptive).
    pub current_batch_size: AtomicU64,
}

impl PipelineObservability {
    /// Create a new observability instance with all counters at zero.
    pub fn new() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_processed: AtomicU64::new(0),
            outputs_sent: AtomicU64::new(0),
            partition_received: std::array::from_fn(|_| AtomicU64::new(0)),
            partition_processed: std::array::from_fn(|_| AtomicU64::new(0)),
            e2e_latency: LatencyHistogram::new(),
            source_latency: LatencyHistogram::new(),
            processor_latency: LatencyHistogram::new(),
            sink_latency: LatencyHistogram::new(),
            source_buffer_pct: AtomicU64::new(0),
            sink_buffer_pct: AtomicU64::new(0),
            dlq_count: AtomicU64::new(0),
            retry_count: AtomicU64::new(0),
            circuit_breaker_state: AtomicI64::new(0),
            circuit_breaker_trips: AtomicU64::new(0),
            current_batch_size: AtomicU64::new(0),
        }
    }

    /// Record events received for a partition.
    #[inline]
    pub fn record_received(&self, partition: u16, count: u64) {
        self.events_received.fetch_add(count, Ordering::Relaxed);
        let idx = (partition as usize).min(MAX_PARTITIONS - 1);
        self.partition_received[idx].fetch_add(count, Ordering::Relaxed);
    }

    /// Record events processed for a partition.
    #[inline]
    pub fn record_processed(&self, partition: u16, count: u64) {
        self.events_processed.fetch_add(count, Ordering::Relaxed);
        let idx = (partition as usize).min(MAX_PARTITIONS - 1);
        self.partition_processed[idx].fetch_add(count, Ordering::Relaxed);
    }

    /// Record outputs sent.
    #[inline]
    pub fn record_sent(&self, count: u64) {
        self.outputs_sent.fetch_add(count, Ordering::Relaxed);
    }

    /// Get events received for a specific partition.
    pub fn partition_received(&self, partition: u16) -> u64 {
        let idx = (partition as usize).min(MAX_PARTITIONS - 1);
        self.partition_received[idx].load(Ordering::Relaxed)
    }

    /// Get events processed for a specific partition.
    pub fn partition_processed(&self, partition: u16) -> u64 {
        let idx = (partition as usize).min(MAX_PARTITIONS - 1);
        self.partition_processed[idx].load(Ordering::Relaxed)
    }

    /// Format all metrics as Prometheus exposition text.
    pub fn to_prometheus(&self) -> String {
        let mut out = String::with_capacity(4096);

        // Throughput counters
        let received = self.events_received.load(Ordering::Relaxed);
        let processed = self.events_processed.load(Ordering::Relaxed);
        let sent = self.outputs_sent.load(Ordering::Relaxed);

        out.push_str("# HELP aeon_events_received_total Total events received\n");
        out.push_str("# TYPE aeon_events_received_total counter\n");
        out.push_str(&format!("aeon_events_received_total {received}\n"));

        out.push_str("# HELP aeon_events_processed_total Total events processed\n");
        out.push_str("# TYPE aeon_events_processed_total counter\n");
        out.push_str(&format!("aeon_events_processed_total {processed}\n"));

        out.push_str("# HELP aeon_outputs_sent_total Total outputs sent\n");
        out.push_str("# TYPE aeon_outputs_sent_total counter\n");
        out.push_str(&format!("aeon_outputs_sent_total {sent}\n"));

        // Per-partition counters (only emit non-zero)
        out.push_str("# HELP aeon_partition_received_total Events received per partition\n");
        out.push_str("# TYPE aeon_partition_received_total counter\n");
        for i in 0..MAX_PARTITIONS {
            let count = self.partition_received[i].load(Ordering::Relaxed);
            if count > 0 {
                out.push_str(&format!(
                    "aeon_partition_received_total{{partition=\"{i}\"}} {count}\n"
                ));
            }
        }

        // Latency histograms
        out.push_str(
            &self
                .e2e_latency
                .to_prometheus("aeon_e2e_latency_seconds", "End-to-end event latency"),
        );
        out.push_str(
            &self
                .source_latency
                .to_prometheus("aeon_source_latency_seconds", "Source batch latency"),
        );
        out.push_str(
            &self
                .processor_latency
                .to_prometheus("aeon_processor_latency_seconds", "Processor batch latency"),
        );
        out.push_str(
            &self
                .sink_latency
                .to_prometheus("aeon_sink_latency_seconds", "Sink batch latency"),
        );

        // Backpressure gauges
        let src_buf = self.source_buffer_pct.load(Ordering::Relaxed);
        let sink_buf = self.sink_buffer_pct.load(Ordering::Relaxed);
        out.push_str("# HELP aeon_source_buffer_pct Source buffer utilization percentage\n");
        out.push_str("# TYPE aeon_source_buffer_pct gauge\n");
        out.push_str(&format!("aeon_source_buffer_pct {src_buf}\n"));
        out.push_str("# HELP aeon_sink_buffer_pct Sink buffer utilization percentage\n");
        out.push_str("# TYPE aeon_sink_buffer_pct gauge\n");
        out.push_str(&format!("aeon_sink_buffer_pct {sink_buf}\n"));

        // Fault tolerance
        let dlq = self.dlq_count.load(Ordering::Relaxed);
        let retries = self.retry_count.load(Ordering::Relaxed);
        let cb_state = self.circuit_breaker_state.load(Ordering::Relaxed);
        let cb_trips = self.circuit_breaker_trips.load(Ordering::Relaxed);

        out.push_str("# HELP aeon_dlq_total Events sent to dead-letter queue\n");
        out.push_str("# TYPE aeon_dlq_total counter\n");
        out.push_str(&format!("aeon_dlq_total {dlq}\n"));

        out.push_str("# HELP aeon_retries_total Retry attempts\n");
        out.push_str("# TYPE aeon_retries_total counter\n");
        out.push_str(&format!("aeon_retries_total {retries}\n"));

        out.push_str("# HELP aeon_circuit_breaker_state Circuit breaker state (0=closed, 1=open, 2=half-open)\n");
        out.push_str("# TYPE aeon_circuit_breaker_state gauge\n");
        out.push_str(&format!("aeon_circuit_breaker_state {cb_state}\n"));

        out.push_str("# HELP aeon_circuit_breaker_trips_total Circuit breaker trip count\n");
        out.push_str("# TYPE aeon_circuit_breaker_trips_total counter\n");
        out.push_str(&format!("aeon_circuit_breaker_trips_total {cb_trips}\n"));

        // Batch size
        let batch = self.current_batch_size.load(Ordering::Relaxed);
        out.push_str("# HELP aeon_batch_size Current adaptive batch size\n");
        out.push_str("# TYPE aeon_batch_size gauge\n");
        out.push_str(&format!("aeon_batch_size {batch}\n"));

        out
    }
}

impl Default for PipelineObservability {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_metrics_are_zero() {
        let m = PipelineObservability::new();
        assert_eq!(m.events_received.load(Ordering::Relaxed), 0);
        assert_eq!(m.events_processed.load(Ordering::Relaxed), 0);
        assert_eq!(m.outputs_sent.load(Ordering::Relaxed), 0);
        assert_eq!(m.dlq_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn record_received_tracks_partition() {
        let m = PipelineObservability::new();
        m.record_received(0, 10);
        m.record_received(1, 20);
        m.record_received(0, 5);

        assert_eq!(m.events_received.load(Ordering::Relaxed), 35);
        assert_eq!(m.partition_received(0), 15);
        assert_eq!(m.partition_received(1), 20);
        assert_eq!(m.partition_received(2), 0);
    }

    #[test]
    fn record_processed_tracks_partition() {
        let m = PipelineObservability::new();
        m.record_processed(3, 100);
        assert_eq!(m.partition_processed(3), 100);
        assert_eq!(m.events_processed.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn record_sent() {
        let m = PipelineObservability::new();
        m.record_sent(50);
        m.record_sent(25);
        assert_eq!(m.outputs_sent.load(Ordering::Relaxed), 75);
    }

    #[test]
    fn partition_overflow_clamped() {
        let m = PipelineObservability::new();
        // Partition 999 should clamp to MAX_PARTITIONS - 1
        m.record_received(999, 1);
        assert_eq!(m.partition_received(999), 1);
        // Same bucket as MAX_PARTITIONS - 1
        assert_eq!(m.partition_received(MAX_PARTITIONS as u16 - 1), 1);
    }

    #[test]
    fn prometheus_output_contains_all_metrics() {
        let m = PipelineObservability::new();
        m.record_received(0, 100);
        m.record_processed(0, 95);
        m.record_sent(90);
        m.dlq_count.store(5, Ordering::Relaxed);
        m.circuit_breaker_state.store(1, Ordering::Relaxed);

        let output = m.to_prometheus();

        assert!(output.contains("aeon_events_received_total 100"));
        assert!(output.contains("aeon_events_processed_total 95"));
        assert!(output.contains("aeon_outputs_sent_total 90"));
        assert!(output.contains("aeon_partition_received_total{partition=\"0\"} 100"));
        assert!(output.contains("aeon_dlq_total 5"));
        assert!(output.contains("aeon_circuit_breaker_state 1"));
        assert!(output.contains("aeon_e2e_latency_seconds"));
        assert!(output.contains("aeon_source_buffer_pct"));
    }

    #[test]
    fn latency_histograms_work() {
        let m = PipelineObservability::new();
        m.e2e_latency.record_us(100);
        m.e2e_latency.record_us(200);
        m.source_latency.record_us(50);

        assert_eq!(m.e2e_latency.count(), 2);
        assert_eq!(m.source_latency.count(), 1);
    }
}
