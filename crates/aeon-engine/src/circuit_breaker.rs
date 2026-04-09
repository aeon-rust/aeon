//! Circuit breaker — protects downstream systems from cascading failures.
//!
//! State machine: Closed → Open → Half-Open → Closed (on success) or Open (on failure).
//!
//! - **Closed**: normal operation. Track consecutive failures.
//! - **Open**: all calls immediately rejected. After cooldown, transition to Half-Open.
//! - **Half-Open**: allow one probe call. If it succeeds → Closed. If it fails → Open.

use aeon_types::AeonError;
use std::time::{Duration, Instant};

/// Circuit breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation. Calls pass through.
    Closed,
    /// Calls rejected. Waiting for cooldown to expire.
    Open,
    /// Allowing one probe call to test recovery.
    HalfOpen,
}

/// Circuit breaker configuration.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before tripping to Open.
    pub failure_threshold: u32,
    /// How long to stay Open before transitioning to Half-Open.
    pub cooldown: Duration,
    /// Number of consecutive successes in Half-Open to transition back to Closed.
    pub success_threshold: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown: Duration::from_secs(30),
            success_threshold: 1,
        }
    }
}

/// Circuit breaker protecting a downstream call.
#[derive(Debug)]
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: CircuitState,
    consecutive_failures: u32,
    consecutive_successes: u32,
    /// When the circuit tripped open.
    opened_at: Option<Instant>,
    /// Total number of times the circuit has tripped open.
    trip_count: u64,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given config.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: CircuitState::Closed,
            consecutive_failures: 0,
            consecutive_successes: 0,
            opened_at: None,
            trip_count: 0,
        }
    }

    /// Current circuit state.
    pub fn state(&self) -> CircuitState {
        self.state
    }

    /// Number of times the circuit has tripped open.
    pub fn trip_count(&self) -> u64 {
        self.trip_count
    }

    /// Check if a call is allowed. Returns `Ok(())` if allowed, `Err` if rejected.
    ///
    /// Also handles the Open → Half-Open transition when cooldown expires.
    pub fn check(&mut self) -> Result<(), AeonError> {
        match self.state {
            CircuitState::Closed => Ok(()),
            CircuitState::Open => {
                // Check if cooldown has expired
                if let Some(opened_at) = self.opened_at {
                    if opened_at.elapsed() >= self.config.cooldown {
                        self.state = CircuitState::HalfOpen;
                        self.consecutive_successes = 0;
                        return Ok(());
                    }
                }
                Err(AeonError::connection("circuit breaker is open"))
            }
            CircuitState::HalfOpen => Ok(()),
        }
    }

    /// Record a successful call.
    pub fn record_success(&mut self) {
        match self.state {
            CircuitState::Closed => {
                self.consecutive_failures = 0;
            }
            CircuitState::HalfOpen => {
                self.consecutive_successes += 1;
                if self.consecutive_successes >= self.config.success_threshold {
                    self.state = CircuitState::Closed;
                    self.consecutive_failures = 0;
                }
            }
            CircuitState::Open => {
                // Shouldn't happen (calls are rejected), but reset if it does
                self.consecutive_failures = 0;
            }
        }
    }

    /// Record a failed call.
    pub fn record_failure(&mut self) {
        match self.state {
            CircuitState::Closed => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= self.config.failure_threshold {
                    self.trip_open();
                }
            }
            CircuitState::HalfOpen => {
                // Probe failed — go back to Open
                self.trip_open();
            }
            CircuitState::Open => {
                // Already open, nothing to do
            }
        }
    }

    fn trip_open(&mut self) {
        self.state = CircuitState::Open;
        self.opened_at = Some(Instant::now());
        self.consecutive_successes = 0;
        self.trip_count += 1;
    }

    /// Reset the circuit breaker to Closed state.
    pub fn reset(&mut self) {
        self.state = CircuitState::Closed;
        self.consecutive_failures = 0;
        self.consecutive_successes = 0;
        self.opened_at = None;
    }

    /// Execute an operation through the circuit breaker.
    pub async fn call<F, Fut, T>(&mut self, operation: F) -> Result<T, AeonError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, AeonError>>,
    {
        self.check()?;

        match operation().await {
            Ok(value) => {
                self.record_success();
                Ok(value)
            }
            Err(err) => {
                self.record_failure();
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            cooldown: Duration::from_millis(50),
            success_threshold: 1,
        }
    }

    #[test]
    fn starts_closed() {
        let cb = CircuitBreaker::new(fast_config());
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.trip_count(), 0);
    }

    #[test]
    fn stays_closed_on_success() {
        let mut cb = CircuitBreaker::new(fast_config());
        cb.record_success();
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn trips_open_after_threshold_failures() {
        let mut cb = CircuitBreaker::new(fast_config());

        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);
        cb.record_failure(); // threshold = 3
        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.trip_count(), 1);
    }

    #[test]
    fn open_rejects_calls() {
        let mut cb = CircuitBreaker::new(fast_config());

        // Trip it open
        for _ in 0..3 {
            cb.record_failure();
        }

        assert!(cb.check().is_err());
    }

    #[test]
    fn success_resets_failure_count() {
        let mut cb = CircuitBreaker::new(fast_config());

        cb.record_failure();
        cb.record_failure();
        cb.record_success(); // resets consecutive failures
        cb.record_failure();
        cb.record_failure();

        // Still closed — success reset the counter
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn transitions_to_half_open_after_cooldown() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(1), // very short for test
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);

        cb.record_failure(); // trips open
        assert_eq!(cb.state(), CircuitState::Open);

        // Wait for cooldown
        std::thread::sleep(Duration::from_millis(5));

        // check() should transition to HalfOpen
        assert!(cb.check().is_ok());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn half_open_closes_on_success() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(1),
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);

        cb.record_failure();
        std::thread::sleep(Duration::from_millis(5));
        cb.check().unwrap(); // → HalfOpen

        cb.record_success(); // → Closed
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn half_open_reopens_on_failure() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(1),
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);

        cb.record_failure(); // trip 1
        std::thread::sleep(Duration::from_millis(5));
        cb.check().unwrap(); // → HalfOpen
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        cb.record_failure(); // → Open again (trip 2)
        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.trip_count(), 2);
    }

    #[test]
    fn reset_returns_to_closed() {
        let mut cb = CircuitBreaker::new(fast_config());

        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        cb.reset();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.check().is_ok());
    }

    #[tokio::test]
    async fn call_succeeds_when_closed() {
        let mut cb = CircuitBreaker::new(fast_config());

        let result = cb.call(|| async { Ok::<_, AeonError>(42) }).await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn call_rejected_when_open() {
        let mut cb = CircuitBreaker::new(fast_config());

        for _ in 0..3 {
            cb.record_failure();
        }

        let result = cb.call(|| async { Ok::<_, AeonError>(42) }).await;
        assert!(result.is_err());
    }

    #[test]
    fn multiple_success_threshold() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(1),
            success_threshold: 3, // need 3 successes to close
        };
        let mut cb = CircuitBreaker::new(config);

        cb.record_failure();
        std::thread::sleep(Duration::from_millis(5));
        cb.check().unwrap(); // → HalfOpen

        cb.record_success();
        assert_eq!(cb.state(), CircuitState::HalfOpen); // not yet
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::HalfOpen); // not yet
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed); // now!
    }
}
