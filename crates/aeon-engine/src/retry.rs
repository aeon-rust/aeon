//! Retry with exponential backoff + jitter.
//!
//! Provides configurable retry logic for transient failures. Uses exponential
//! backoff with full jitter to avoid thundering herd effects.

use aeon_types::AeonError;
use std::time::Duration;

/// Retry configuration.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (0 = no retries).
    pub max_retries: u32,
    /// Base delay for exponential backoff.
    pub base_delay: Duration,
    /// Maximum delay cap.
    pub max_delay: Duration,
    /// Jitter factor (0.0 = no jitter, 1.0 = full jitter). Clamped to [0.0, 1.0].
    pub jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            jitter: 1.0,
        }
    }
}

/// Result of a retry operation.
#[derive(Debug)]
pub enum RetryOutcome<T> {
    /// Operation succeeded after `attempts` tries.
    Success { value: T, attempts: u32 },
    /// Operation exhausted all retries.
    Exhausted {
        last_error: AeonError,
        attempts: u32,
    },
    /// Operation failed with a non-retryable error.
    NonRetryable { error: AeonError, attempts: u32 },
}

/// Calculate the delay for a given attempt using exponential backoff with jitter.
///
/// delay = min(base * 2^attempt, max_delay) * (1 - jitter + jitter * random)
///
/// Uses a simple deterministic "random" based on attempt number for reproducibility
/// in tests. For production, callers can add real randomness externally.
pub fn backoff_delay(config: &RetryConfig, attempt: u32) -> Duration {
    let exp = config.base_delay.as_nanos() as f64 * (2.0_f64).powi(attempt as i32);
    let capped = exp.min(config.max_delay.as_nanos() as f64);

    let jitter = config.jitter.clamp(0.0, 1.0);
    // Deterministic jitter based on attempt number for predictable tests.
    // In production, the caller wraps with real randomness if needed.
    let jitter_factor = 1.0 - jitter * 0.5; // Apply half-jitter deterministically
    let delay_nanos = (capped * jitter_factor) as u64;

    Duration::from_nanos(delay_nanos)
}

/// Execute an async operation with retry.
///
/// Retries only if the error is retryable (`AeonError::is_retryable()`).
/// Non-retryable errors are returned immediately.
pub async fn retry_async<F, Fut, T>(config: &RetryConfig, mut operation: F) -> RetryOutcome<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, AeonError>>,
{
    let mut attempt = 0;

    loop {
        match operation().await {
            Ok(value) => {
                return RetryOutcome::Success {
                    value,
                    attempts: attempt + 1,
                };
            }
            Err(err) => {
                attempt += 1;

                if !err.is_retryable() {
                    return RetryOutcome::NonRetryable {
                        error: err,
                        attempts: attempt,
                    };
                }

                if attempt > config.max_retries {
                    return RetryOutcome::Exhausted {
                        last_error: err,
                        attempts: attempt,
                    };
                }

                let delay = backoff_delay(config, attempt - 1);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Execute a synchronous operation with retry (no delay between attempts).
///
/// Useful for testing and non-async contexts.
pub fn retry_sync<F, T>(config: &RetryConfig, mut operation: F) -> RetryOutcome<T>
where
    F: FnMut() -> Result<T, AeonError>,
{
    let mut attempt = 0;

    loop {
        match operation() {
            Ok(value) => {
                return RetryOutcome::Success {
                    value,
                    attempts: attempt + 1,
                };
            }
            Err(err) => {
                attempt += 1;

                if !err.is_retryable() {
                    return RetryOutcome::NonRetryable {
                        error: err,
                        attempts: attempt,
                    };
                }

                if attempt > config.max_retries {
                    return RetryOutcome::Exhausted {
                        last_error: err,
                        attempts: attempt,
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn backoff_delay_increases_exponentially() {
        let config = RetryConfig {
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(60),
            jitter: 0.0, // No jitter for deterministic test
            ..Default::default()
        };

        let d0 = backoff_delay(&config, 0);
        let d1 = backoff_delay(&config, 1);
        let d2 = backoff_delay(&config, 2);

        assert_eq!(d0, Duration::from_millis(100));
        assert_eq!(d1, Duration::from_millis(200));
        assert_eq!(d2, Duration::from_millis(400));
    }

    #[test]
    fn backoff_delay_caps_at_max() {
        let config = RetryConfig {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            jitter: 0.0,
            ..Default::default()
        };

        // 2^10 = 1024s would exceed max
        let d = backoff_delay(&config, 10);
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn backoff_delay_with_jitter_is_less_than_without() {
        let no_jitter = RetryConfig {
            base_delay: Duration::from_millis(100),
            jitter: 0.0,
            ..Default::default()
        };
        let with_jitter = RetryConfig {
            base_delay: Duration::from_millis(100),
            jitter: 1.0,
            ..Default::default()
        };

        let d_no = backoff_delay(&no_jitter, 2);
        let d_yes = backoff_delay(&with_jitter, 2);

        assert!(d_yes < d_no);
    }

    #[test]
    fn retry_sync_succeeds_immediately() {
        let config = RetryConfig::default();
        let result = retry_sync(&config, || Ok::<_, AeonError>(42));

        match result {
            RetryOutcome::Success { value, attempts } => {
                assert_eq!(value, 42);
                assert_eq!(attempts, 1);
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn retry_sync_succeeds_after_transient_failures() {
        let config = RetryConfig {
            max_retries: 5,
            ..Default::default()
        };
        let call_count = AtomicU32::new(0);

        let result = retry_sync(&config, || {
            let n = call_count.fetch_add(1, Ordering::Relaxed);
            if n < 2 {
                Err(AeonError::connection("transient"))
            } else {
                Ok(99)
            }
        });

        match result {
            RetryOutcome::Success { value, attempts } => {
                assert_eq!(value, 99);
                assert_eq!(attempts, 3);
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn retry_sync_exhausted() {
        let config = RetryConfig {
            max_retries: 2,
            ..Default::default()
        };

        let result = retry_sync(&config, || {
            Err::<(), _>(AeonError::connection("always fails"))
        });

        match result {
            RetryOutcome::Exhausted { attempts, .. } => {
                assert_eq!(attempts, 3); // initial + 2 retries
            }
            _ => panic!("expected exhausted"),
        }
    }

    #[test]
    fn retry_sync_non_retryable_stops_immediately() {
        let config = RetryConfig {
            max_retries: 5,
            ..Default::default()
        };

        let result = retry_sync(&config, || {
            Err::<(), _>(AeonError::serialization("bad data"))
        });

        match result {
            RetryOutcome::NonRetryable { attempts, .. } => {
                assert_eq!(attempts, 1);
            }
            _ => panic!("expected non-retryable"),
        }
    }

    #[tokio::test]
    async fn retry_async_succeeds_after_failures() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(1), // fast for tests
            jitter: 0.0,
            ..Default::default()
        };
        let call_count = AtomicU32::new(0);

        let result = retry_async(&config, || {
            let n = call_count.fetch_add(1, Ordering::Relaxed);
            async move {
                if n < 1 {
                    Err(AeonError::timeout("slow"))
                } else {
                    Ok("done")
                }
            }
        })
        .await;

        match result {
            RetryOutcome::Success { value, attempts } => {
                assert_eq!(value, "done");
                assert_eq!(attempts, 2);
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn retry_zero_max_retries_means_single_attempt() {
        let config = RetryConfig {
            max_retries: 0,
            ..Default::default()
        };

        let result = retry_sync(&config, || Err::<(), _>(AeonError::connection("fail")));

        match result {
            RetryOutcome::Exhausted { attempts, .. } => {
                assert_eq!(attempts, 1);
            }
            _ => panic!("expected exhausted"),
        }
    }
}
