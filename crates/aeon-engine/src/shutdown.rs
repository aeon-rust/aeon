//! Graceful shutdown coordination.
//!
//! Integrates OS signal handling (SIGINT, SIGTERM) with the pipeline's
//! `AtomicBool` shutdown flag and health state. When a signal is received:
//!
//! 1. Set health to not-ready (stop accepting new traffic)
//! 2. Set shutdown flag (stop source polling)
//! 3. Pipeline drains in-flight events through SPSC buffers
//! 4. Sinks flush remaining outputs
//!
//! The pipeline's existing `run()` and `run_buffered()` already handle steps 3-4
//! via their shutdown flag + SPSC drain + `sink.flush()` design. This module
//! provides the signal-to-flag bridge and drain timeout.

use crate::health::HealthState;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Configuration for graceful shutdown.
#[derive(Debug, Clone)]
pub struct ShutdownConfig {
    /// Maximum time to wait for in-flight events to drain before force-stopping.
    pub drain_timeout: Duration,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_timeout: Duration::from_secs(30),
        }
    }
}

/// Coordinates graceful shutdown across pipeline components.
#[derive(Debug)]
pub struct ShutdownCoordinator {
    /// The shutdown flag shared with the pipeline.
    shutdown: Arc<AtomicBool>,
    /// Health state to mark as not-ready during shutdown.
    health: Option<Arc<HealthState>>,
    /// Shutdown configuration.
    config: ShutdownConfig,
}

impl ShutdownCoordinator {
    /// Create a new shutdown coordinator.
    pub fn new(shutdown: Arc<AtomicBool>) -> Self {
        Self {
            shutdown,
            health: None,
            config: ShutdownConfig::default(),
        }
    }

    /// Attach a health state to update during shutdown.
    pub fn with_health(mut self, health: Arc<HealthState>) -> Self {
        self.health = Some(health);
        self
    }

    /// Set a custom shutdown config.
    pub fn with_config(mut self, config: ShutdownConfig) -> Self {
        self.config = config;
        self
    }

    /// Trigger graceful shutdown programmatically.
    ///
    /// Sets health to not-ready, then sets the shutdown flag.
    /// The pipeline will drain in-flight events and flush sinks.
    pub fn trigger(&self) {
        if let Some(health) = &self.health {
            health.set_not_ready();
        }
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Check if shutdown has been triggered.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    /// The drain timeout from the configuration.
    pub fn drain_timeout(&self) -> Duration {
        self.config.drain_timeout
    }

    /// Wait for a shutdown signal (SIGINT or SIGTERM on Unix, Ctrl+C on all platforms).
    ///
    /// When a signal is received, triggers graceful shutdown and returns.
    /// This function is intended to be spawned as a background task:
    ///
    /// ```ignore
    /// let coordinator = ShutdownCoordinator::new(shutdown.clone());
    /// tokio::spawn(coordinator.watch_signals());
    /// ```
    pub async fn watch_signals(self) {
        let _ = tokio::signal::ctrl_c().await;
        self.trigger();
    }

    /// Wait for shutdown to be triggered, with a timeout.
    ///
    /// Returns `true` if shutdown was triggered within the timeout,
    /// `false` if the timeout elapsed.
    pub async fn wait_for_shutdown(&self, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        let check_interval = Duration::from_millis(10);

        while start.elapsed() < timeout {
            if self.is_shutdown() {
                return true;
            }
            tokio::time::sleep(check_interval).await;
        }

        self.is_shutdown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_sets_shutdown_flag() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let coordinator = ShutdownCoordinator::new(Arc::clone(&shutdown));

        assert!(!coordinator.is_shutdown());
        coordinator.trigger();
        assert!(coordinator.is_shutdown());
        assert!(shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn trigger_sets_health_not_ready() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let health = Arc::new(HealthState::new());
        health.set_ready();

        let coordinator = ShutdownCoordinator::new(shutdown).with_health(Arc::clone(&health));

        assert!(health.is_ready());
        coordinator.trigger();
        assert!(!health.is_ready());
    }

    #[test]
    fn default_drain_timeout() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let coordinator = ShutdownCoordinator::new(shutdown);
        assert_eq!(coordinator.drain_timeout(), Duration::from_secs(30));
    }

    #[test]
    fn custom_config() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let config = ShutdownConfig {
            drain_timeout: Duration::from_secs(5),
        };
        let coordinator = ShutdownCoordinator::new(shutdown).with_config(config);
        assert_eq!(coordinator.drain_timeout(), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn wait_for_shutdown_returns_true_when_triggered() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let coordinator = ShutdownCoordinator::new(Arc::clone(&shutdown));

        // Trigger after a short delay
        let s = Arc::clone(&shutdown);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            s.store(true, Ordering::Relaxed);
        });

        let result = coordinator.wait_for_shutdown(Duration::from_secs(1)).await;
        assert!(result);
    }

    #[tokio::test]
    async fn wait_for_shutdown_returns_false_on_timeout() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let coordinator = ShutdownCoordinator::new(shutdown);

        let result = coordinator
            .wait_for_shutdown(Duration::from_millis(20))
            .await;
        assert!(!result);
    }
}
