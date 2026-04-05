//! Adaptive batch size tuner using hill-climbing algorithm.
//!
//! Automatically adjusts batch size at runtime to maximize throughput.
//! Uses a simple hill-climbing approach: measure throughput at current batch size,
//! try a larger or smaller size, keep whichever is faster.
//!
//! The tuner runs periodically (every N batches) and adjusts by a step size
//! that shrinks over time as it converges on the optimal batch size.

use std::time::{Duration, Instant};

/// Adaptive batch size tuner.
///
/// Periodically probes batch size changes and keeps the better-performing size.
/// Converges to the optimal batch size for the current workload.
pub struct BatchTuner {
    /// Current batch size.
    current_size: usize,
    /// Minimum allowed batch size.
    min_size: usize,
    /// Maximum allowed batch size.
    max_size: usize,
    /// Step size for adjustments (shrinks as we converge).
    step: usize,
    /// Minimum step size (stop shrinking below this).
    min_step: usize,
    /// Best observed throughput (events per second).
    best_throughput: f64,
    /// Direction: true = try larger, false = try smaller.
    try_larger: bool,
    /// Events processed since last measurement.
    events_since_measure: u64,
    /// Time of last measurement.
    last_measure: Instant,
    /// How many events between measurements.
    measure_interval: u64,
    /// Number of adjustments made (for diagnostics).
    adjustments: u32,
}

impl BatchTuner {
    /// Create a new tuner with given bounds.
    ///
    /// - `initial`: starting batch size
    /// - `min`: minimum batch size (never go below this)
    /// - `max`: maximum batch size (never go above this)
    pub fn new(initial: usize, min: usize, max: usize) -> Self {
        assert!(min <= initial && initial <= max);
        assert!(min > 0);
        Self {
            current_size: initial,
            min_size: min,
            max_size: max,
            step: (max - min) / 4,
            min_step: 1.max((max - min) / 64),
            best_throughput: 0.0,
            try_larger: true,
            events_since_measure: 0,
            last_measure: Instant::now(),
            measure_interval: (initial * 100) as u64, // ~100 batches between measurements
            adjustments: 0,
        }
    }

    /// Get the current recommended batch size.
    #[inline]
    pub fn batch_size(&self) -> usize {
        self.current_size
    }

    /// Report that a batch of events was processed.
    /// Call this after each `process_batch` or `write_batch`.
    /// Returns `true` if the batch size was adjusted.
    #[inline]
    pub fn report(&mut self, events: u64) -> bool {
        self.events_since_measure += events;
        if self.events_since_measure >= self.measure_interval {
            self.evaluate()
        } else {
            false
        }
    }

    /// Evaluate current throughput and adjust batch size.
    fn evaluate(&mut self) -> bool {
        let elapsed = self.last_measure.elapsed();
        if elapsed < Duration::from_millis(1) {
            // Too fast to measure accurately — accumulate more
            return false;
        }

        let throughput = self.events_since_measure as f64 / elapsed.as_secs_f64();

        // Reset counters
        self.events_since_measure = 0;
        self.last_measure = Instant::now();

        if self.best_throughput == 0.0 {
            // First measurement — just record baseline
            self.best_throughput = throughput;
            return false;
        }

        if throughput > self.best_throughput * 1.01 {
            // Improvement — keep direction, continue stepping
            self.best_throughput = throughput;
            self.step_in_direction();
            self.adjustments += 1;
            true
        } else if throughput < self.best_throughput * 0.99 {
            // Regression — reverse direction, reduce step
            self.try_larger = !self.try_larger;
            self.step = (self.step / 2).max(self.min_step);
            self.revert_step();
            self.adjustments += 1;
            true
        } else {
            // Within 1% — converged at this step size, reduce step for finer tuning
            self.step = (self.step / 2).max(self.min_step);
            false
        }
    }

    /// Apply a step in the current direction.
    fn step_in_direction(&mut self) {
        if self.try_larger {
            self.current_size = (self.current_size + self.step).min(self.max_size);
        } else {
            self.current_size = self
                .current_size
                .saturating_sub(self.step)
                .max(self.min_size);
        }
        self.measure_interval = (self.current_size * 100) as u64;
    }

    /// Revert the last step (used when throughput regressed).
    fn revert_step(&mut self) {
        if self.try_larger {
            // We reversed to try_larger, so the bad step was try_smaller
            self.current_size = (self.current_size + self.step).min(self.max_size);
        } else {
            self.current_size = self
                .current_size
                .saturating_sub(self.step)
                .max(self.min_size);
        }
        self.measure_interval = (self.current_size * 100) as u64;
    }

    /// Number of adjustments made so far.
    pub fn adjustments(&self) -> u32 {
        self.adjustments
    }

    /// Current step size (for diagnostics).
    pub fn step_size(&self) -> usize {
        self.step
    }
}

/// Adaptive flush interval tuner for batched delivery mode.
///
/// Adjusts the sink flush interval based on ack success rate feedback from
/// the delivery ledger. When the sink is healthy (high success rate), the
/// interval increases to maximize throughput. When failures spike, the
/// interval decreases to minimize data at risk.
///
/// Uses hill-climbing: probe interval changes, keep whichever direction
/// improves the success-weighted throughput metric.
///
/// Cost: one `Duration` comparison + one f64 division per flush cycle (~2ns).
pub struct FlushTuner {
    /// Current flush interval.
    current_interval: Duration,
    /// Minimum flush interval (floor — never flush faster than this).
    min_interval: Duration,
    /// Maximum flush interval (ceiling — never wait longer than this).
    max_interval: Duration,
    /// Step size for adjustments (shrinks as we converge).
    step: Duration,
    /// Minimum step size.
    min_step: Duration,
    /// Best observed metric (success_rate * throughput).
    best_metric: f64,
    /// Direction: true = try longer interval, false = try shorter.
    try_longer: bool,
    /// Events processed since last evaluation.
    events_since_eval: u64,
    /// Successful acks since last evaluation.
    acks_since_eval: u64,
    /// Time of last evaluation.
    last_eval: Instant,
    /// Minimum events before evaluating (avoids noise on small samples).
    min_sample: u64,
    /// Number of adjustments made.
    adjustments: u32,
}

impl FlushTuner {
    /// Create a new flush tuner.
    ///
    /// - `initial`: starting flush interval
    /// - `min`: minimum interval (fastest flush rate)
    /// - `max`: maximum interval (longest between flushes)
    pub fn new(initial: Duration, min: Duration, max: Duration) -> Self {
        assert!(min <= initial && initial <= max);
        assert!(!min.is_zero());
        let range = max - min;
        Self {
            current_interval: initial,
            min_interval: min,
            max_interval: max,
            step: range / 4,
            min_step: Duration::from_millis(10).max(range / 64),
            best_metric: 0.0,
            try_longer: true,
            events_since_eval: 0,
            acks_since_eval: 0,
            last_eval: Instant::now(),
            min_sample: 100,
            adjustments: 0,
        }
    }

    /// Get the current recommended flush interval.
    #[inline]
    pub fn interval(&self) -> Duration {
        self.current_interval
    }

    /// Report a flush cycle result.
    ///
    /// - `events`: total events in this flush cycle
    /// - `acked`: successfully acked events in this flush cycle
    ///
    /// Returns `true` if the interval was adjusted.
    #[inline]
    pub fn report(&mut self, events: u64, acked: u64) -> bool {
        self.events_since_eval += events;
        self.acks_since_eval += acked;
        if self.events_since_eval >= self.min_sample {
            self.evaluate()
        } else {
            false
        }
    }

    /// Evaluate and potentially adjust the flush interval.
    fn evaluate(&mut self) -> bool {
        let elapsed = self.last_eval.elapsed();
        if elapsed < Duration::from_millis(10) {
            return false;
        }

        let throughput = self.events_since_eval as f64 / elapsed.as_secs_f64();
        let success_rate = if self.events_since_eval > 0 {
            self.acks_since_eval as f64 / self.events_since_eval as f64
        } else {
            1.0
        };

        // Composite metric: throughput weighted by success rate.
        // High throughput with low success = bad. Moderate throughput with
        // perfect success = good.
        let metric = throughput * success_rate * success_rate;

        // Reset counters
        self.events_since_eval = 0;
        self.acks_since_eval = 0;
        self.last_eval = Instant::now();

        if self.best_metric == 0.0 {
            self.best_metric = metric;
            return false;
        }

        if metric > self.best_metric * 1.01 {
            // Improvement — keep direction
            self.best_metric = metric;
            self.step_in_direction();
            self.adjustments += 1;
            true
        } else if metric < self.best_metric * 0.99 {
            // Regression — reverse direction, reduce step
            self.try_longer = !self.try_longer;
            self.step = self.step.max(self.min_step * 2) / 2;
            if self.step < self.min_step {
                self.step = self.min_step;
            }
            self.revert_step();
            self.adjustments += 1;
            true
        } else {
            // Converged — reduce step for finer tuning
            self.step = self.step.max(self.min_step * 2) / 2;
            if self.step < self.min_step {
                self.step = self.min_step;
            }
            false
        }
    }

    /// Apply a step in the current direction.
    fn step_in_direction(&mut self) {
        if self.try_longer {
            self.current_interval = (self.current_interval + self.step).min(self.max_interval);
        } else {
            self.current_interval = self
                .current_interval
                .saturating_sub(self.step)
                .max(self.min_interval);
        }
    }

    /// Revert the last step.
    fn revert_step(&mut self) {
        if self.try_longer {
            self.current_interval = (self.current_interval + self.step).min(self.max_interval);
        } else {
            self.current_interval = self
                .current_interval
                .saturating_sub(self.step)
                .max(self.min_interval);
        }
    }

    /// Number of adjustments made so far.
    pub fn adjustments(&self) -> u32 {
        self.adjustments
    }

    /// Current step size.
    pub fn step_size(&self) -> Duration {
        self.step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuner_starts_at_initial_size() {
        let tuner = BatchTuner::new(256, 16, 4096);
        assert_eq!(tuner.batch_size(), 256);
    }

    #[test]
    fn tuner_respects_bounds() {
        let mut tuner = BatchTuner::new(16, 16, 4096);
        // Force many "try smaller" iterations
        tuner.try_larger = false;
        for _ in 0..100 {
            tuner.step_in_direction();
        }
        assert!(tuner.batch_size() >= 16);

        let mut tuner = BatchTuner::new(4096, 16, 4096);
        tuner.try_larger = true;
        for _ in 0..100 {
            tuner.step_in_direction();
        }
        assert!(tuner.batch_size() <= 4096);
    }

    #[test]
    fn tuner_converges() {
        let mut tuner = BatchTuner::new(256, 16, 4096);

        // Simulate reporting events — this exercises the measurement path
        // Without real timing, we can at least verify no panics and bounds hold
        for _ in 0..10_000 {
            tuner.report(256);
        }

        assert!(tuner.batch_size() >= 16);
        assert!(tuner.batch_size() <= 4096);
    }

    #[test]
    fn tuner_step_shrinks() {
        let tuner = BatchTuner::new(256, 16, 4096);
        let initial_step = tuner.step_size();
        assert!(initial_step > 0);

        // After many evaluations the step should be at or above min_step
        let mut tuner = BatchTuner::new(256, 16, 4096);
        tuner.best_throughput = 1_000_000.0;
        // Force several "converged" evaluations
        for _ in 0..20 {
            tuner.events_since_measure = tuner.measure_interval;
            tuner.last_measure = Instant::now() - Duration::from_millis(100);
            tuner.evaluate();
        }

        assert!(tuner.step_size() <= initial_step);
        assert!(tuner.step_size() >= tuner.min_step);
    }

    #[test]
    fn tuner_adjustments_tracked() {
        let tuner = BatchTuner::new(256, 16, 4096);
        assert_eq!(tuner.adjustments(), 0);
    }

    // --- FlushTuner tests ---

    #[test]
    fn flush_tuner_starts_at_initial_interval() {
        let tuner = FlushTuner::new(
            Duration::from_millis(500),
            Duration::from_millis(50),
            Duration::from_secs(5),
        );
        assert_eq!(tuner.interval(), Duration::from_millis(500));
    }

    #[test]
    fn flush_tuner_respects_bounds() {
        let mut tuner = FlushTuner::new(
            Duration::from_millis(50),
            Duration::from_millis(50),
            Duration::from_secs(5),
        );
        // Force many "try shorter" iterations
        tuner.try_longer = false;
        for _ in 0..100 {
            tuner.step_in_direction();
        }
        assert!(tuner.interval() >= Duration::from_millis(50));

        let mut tuner = FlushTuner::new(
            Duration::from_secs(5),
            Duration::from_millis(50),
            Duration::from_secs(5),
        );
        tuner.try_longer = true;
        for _ in 0..100 {
            tuner.step_in_direction();
        }
        assert!(tuner.interval() <= Duration::from_secs(5));
    }

    #[test]
    fn flush_tuner_report_accumulates() {
        let mut tuner = FlushTuner::new(
            Duration::from_millis(500),
            Duration::from_millis(50),
            Duration::from_secs(5),
        );
        // Report below min_sample — should not evaluate
        assert!(!tuner.report(10, 10));
        assert!(!tuner.report(10, 10));
        assert_eq!(tuner.adjustments(), 0);
    }

    #[test]
    fn flush_tuner_adjustments_tracked() {
        let tuner = FlushTuner::new(
            Duration::from_millis(500),
            Duration::from_millis(50),
            Duration::from_secs(5),
        );
        assert_eq!(tuner.adjustments(), 0);
    }

    #[test]
    fn flush_tuner_step_shrinks_on_convergence() {
        let mut tuner = FlushTuner::new(
            Duration::from_millis(500),
            Duration::from_millis(50),
            Duration::from_secs(5),
        );
        let initial_step = tuner.step_size();
        assert!(initial_step > Duration::ZERO);

        tuner.best_metric = 1_000_000.0;
        // Force several "converged" evaluations (metric within 1%)
        for _ in 0..20 {
            tuner.events_since_eval = tuner.min_sample;
            tuner.acks_since_eval = tuner.min_sample; // 100% success
            tuner.last_eval = Instant::now() - Duration::from_millis(100);
            tuner.evaluate();
        }

        assert!(tuner.step_size() <= initial_step);
        assert!(tuner.step_size() >= tuner.min_step);
    }

    #[test]
    fn flush_tuner_converges() {
        let mut tuner = FlushTuner::new(
            Duration::from_millis(500),
            Duration::from_millis(50),
            Duration::from_secs(5),
        );

        // Simulate many flush cycles with perfect success
        for _ in 0..1_000 {
            tuner.report(200, 200);
        }

        assert!(tuner.interval() >= Duration::from_millis(50));
        assert!(tuner.interval() <= Duration::from_secs(5));
    }
}
