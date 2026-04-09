//! Lock-free latency histogram for P50/P95/P99 tracking.
//!
//! Uses fixed exponential buckets with atomic counters — zero allocation
//! on the hot path. Suitable for per-event latency recording.
//!
//! Bucket boundaries (microseconds):
//! 1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 25000, 50000, 100000, +Inf
//!
//! This covers sub-microsecond to 100ms latencies, which spans the full
//! range of pipeline event processing.

use std::sync::atomic::{AtomicU64, Ordering};

/// Fixed bucket boundaries in microseconds.
const BUCKET_BOUNDS_US: &[u64] = &[
    1, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000,
];

/// Number of buckets (boundaries + 1 for +Inf).
const NUM_BUCKETS: usize = 16; // 15 boundaries + 1 overflow

/// Lock-free latency histogram with fixed exponential buckets.
///
/// Each bucket is an `AtomicU64` counter. Recording a value finds the
/// appropriate bucket via binary search and increments it atomically.
/// No locks, no allocations on the hot path.
pub struct LatencyHistogram {
    /// Bucket counters. Index 0..14 correspond to BUCKET_BOUNDS_US, index 15 is +Inf.
    buckets: [AtomicU64; NUM_BUCKETS],
    /// Running sum of all recorded values (microseconds) for mean calculation.
    sum_us: AtomicU64,
    /// Total number of observations.
    count: AtomicU64,
}

impl LatencyHistogram {
    /// Create a new empty histogram.
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Record a latency in nanoseconds.
    #[inline]
    pub fn record_ns(&self, nanos: u64) {
        let us = nanos / 1_000;
        self.record_us(us);
    }

    /// Record a latency in microseconds.
    #[inline]
    pub fn record_us(&self, micros: u64) {
        // Binary search for the bucket
        let idx = match BUCKET_BOUNDS_US.binary_search(&micros) {
            Ok(i) => i,  // Exact match — goes in this bucket
            Err(i) => i, // First bucket with bound > micros, or NUM_BUCKETS-1
        };
        let bucket_idx = idx.min(NUM_BUCKETS - 1);

        self.buckets[bucket_idx].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(micros, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Total number of observations.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Sum of all recorded values in microseconds.
    pub fn sum_us(&self) -> u64 {
        self.sum_us.load(Ordering::Relaxed)
    }

    /// Get the approximate percentile value in microseconds.
    ///
    /// `percentile` should be between 0.0 and 1.0 (e.g., 0.99 for P99).
    /// Returns the upper bound of the bucket containing the percentile.
    /// Returns 0 if no observations have been recorded.
    pub fn percentile_us(&self, percentile: f64) -> u64 {
        let total = self.count();
        if total == 0 {
            return 0;
        }

        let target = (total as f64 * percentile).ceil() as u64;
        let mut cumulative = 0u64;

        for (i, bucket) in self.buckets.iter().enumerate() {
            cumulative += bucket.load(Ordering::Relaxed);
            if cumulative >= target {
                if i < BUCKET_BOUNDS_US.len() {
                    return BUCKET_BOUNDS_US[i];
                }
                // +Inf bucket — return the last finite bound as approximation
                return *BUCKET_BOUNDS_US.last().unwrap_or(&0);
            }
        }

        *BUCKET_BOUNDS_US.last().unwrap_or(&0)
    }

    /// P50 in microseconds.
    pub fn p50_us(&self) -> u64 {
        self.percentile_us(0.50)
    }

    /// P95 in microseconds.
    pub fn p95_us(&self) -> u64 {
        self.percentile_us(0.95)
    }

    /// P99 in microseconds.
    pub fn p99_us(&self) -> u64 {
        self.percentile_us(0.99)
    }

    /// Mean latency in microseconds.
    pub fn mean_us(&self) -> f64 {
        let c = self.count();
        if c == 0 {
            return 0.0;
        }
        self.sum_us() as f64 / c as f64
    }

    /// Get all bucket counts as (upper_bound_us, cumulative_count) pairs.
    /// Suitable for Prometheus histogram exposition format.
    pub fn cumulative_buckets(&self) -> Vec<(String, u64)> {
        let mut cumulative = 0u64;
        let mut result = Vec::with_capacity(NUM_BUCKETS);

        for (i, bucket) in self.buckets.iter().enumerate() {
            cumulative += bucket.load(Ordering::Relaxed);
            let label = if i < BUCKET_BOUNDS_US.len() {
                // Convert microseconds to seconds for Prometheus
                let secs = BUCKET_BOUNDS_US[i] as f64 / 1_000_000.0;
                format!("{secs}")
            } else {
                "+Inf".to_string()
            };
            result.push((label, cumulative));
        }

        result
    }

    /// Format as Prometheus histogram text.
    pub fn to_prometheus(&self, name: &str, help: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!("# HELP {name} {help}\n"));
        out.push_str(&format!("# TYPE {name} histogram\n"));

        for (bound, count) in self.cumulative_buckets() {
            out.push_str(&format!("{name}_bucket{{le=\"{bound}\"}} {count}\n"));
        }

        out.push_str(&format!(
            "{name}_sum {}\n",
            self.sum_us() as f64 / 1_000_000.0
        ));
        out.push_str(&format!("{name}_count {}\n", self.count()));

        out
    }

    /// Reset all counters.
    pub fn reset(&self) {
        for bucket in &self.buckets {
            bucket.store(0, Ordering::Relaxed);
        }
        self.sum_us.store(0, Ordering::Relaxed);
        self.count.store(0, Ordering::Relaxed);
    }
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram() {
        let h = LatencyHistogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.sum_us(), 0);
        assert_eq!(h.p50_us(), 0);
        assert_eq!(h.p95_us(), 0);
        assert_eq!(h.p99_us(), 0);
        assert_eq!(h.mean_us(), 0.0);
    }

    #[test]
    fn record_single_value() {
        let h = LatencyHistogram::new();
        h.record_us(50); // 50µs → bucket [50]
        assert_eq!(h.count(), 1);
        assert_eq!(h.sum_us(), 50);
        assert_eq!(h.p50_us(), 50);
    }

    #[test]
    fn record_ns_converts_correctly() {
        let h = LatencyHistogram::new();
        h.record_ns(50_000); // 50µs
        assert_eq!(h.count(), 1);
        assert_eq!(h.sum_us(), 50);
    }

    #[test]
    fn percentiles_with_distribution() {
        let h = LatencyHistogram::new();

        // Record 100 values: 1µs each (all in first bucket)
        for _ in 0..90 {
            h.record_us(1);
        }
        // 10 slow values in the 1000µs bucket
        for _ in 0..10 {
            h.record_us(800);
        }

        assert_eq!(h.count(), 100);
        assert_eq!(h.p50_us(), 1); // 50th percentile is in first bucket
        assert_eq!(h.p95_us(), 1000); // 95th percentile hits the slow bucket
    }

    #[test]
    fn mean_calculation() {
        let h = LatencyHistogram::new();
        h.record_us(100);
        h.record_us(200);
        h.record_us(300);
        assert!((h.mean_us() - 200.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cumulative_buckets_are_monotonic() {
        let h = LatencyHistogram::new();
        h.record_us(10);
        h.record_us(100);
        h.record_us(1000);

        let buckets = h.cumulative_buckets();
        let counts: Vec<u64> = buckets.iter().map(|(_, c)| *c).collect();

        // Each count should be >= the previous
        for window in counts.windows(2) {
            assert!(window[1] >= window[0]);
        }

        // Last bucket should equal total count
        assert_eq!(*counts.last().unwrap(), 3);
    }

    #[test]
    fn prometheus_format() {
        let h = LatencyHistogram::new();
        h.record_us(50);
        h.record_us(500);

        let output = h.to_prometheus("aeon_event_latency_seconds", "Event processing latency");
        assert!(output.contains("# TYPE aeon_event_latency_seconds histogram"));
        assert!(output.contains("aeon_event_latency_seconds_count 2"));
        assert!(output.contains("aeon_event_latency_seconds_bucket{le=\"+Inf\"} 2"));
    }

    #[test]
    fn reset_clears_all() {
        let h = LatencyHistogram::new();
        h.record_us(100);
        h.record_us(200);
        h.reset();
        assert_eq!(h.count(), 0);
        assert_eq!(h.sum_us(), 0);
        assert_eq!(h.p50_us(), 0);
    }

    #[test]
    fn overflow_bucket() {
        let h = LatencyHistogram::new();
        h.record_us(999_999); // way above 100_000 → +Inf bucket
        assert_eq!(h.count(), 1);
        // P50 returns last finite bound for +Inf
        assert_eq!(h.p50_us(), 100_000);
    }

    #[test]
    fn very_small_latency() {
        let h = LatencyHistogram::new();
        h.record_us(0); // 0µs → first bucket (<=1)
        assert_eq!(h.count(), 1);
        assert_eq!(h.p50_us(), 1); // bucket bound is 1
    }
}
