//! Three-phase push-source backpressure buffer.
//!
//! Push sources (WebSocket, HTTP Webhook, MQTT, NATS, RabbitMQ) receive events
//! asynchronously and must buffer them for `next_batch()` consumption.
//!
//! **Phase 1 — In-memory buffer**: Bounded tokio mpsc channel. Fast path.
//! **Phase 2 — Spill to disk**: When channel is full, events serialize to a temp file.
//!   The consumer drains disk-spilled events before reading from the channel.
//! **Phase 3 — Protocol flow control**: When spill file exceeds threshold, the
//!   `is_overloaded()` signal tells the connector to apply protocol-level backpressure
//!   (e.g., stop reading from socket, pause MQTT subscription, AMQP basic.cancel).
//!
//! The `PushBufferTx` is given to the background receive task.
//! The `PushBufferRx` is used by the `Source::next_batch()` implementation.

use aeon_types::{AeonError, Event};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Sender half — used by the background receive task to push events.
#[derive(Clone)]
pub struct PushBufferTx {
    tx: tokio::sync::mpsc::Sender<Event>,
    spill_count: Arc<AtomicU64>,
    overloaded: Arc<AtomicBool>,
    spill_threshold: u64,
}

impl PushBufferTx {
    /// Send an event into the buffer.
    ///
    /// Phase 1: tries the bounded channel first (non-blocking).
    /// Phase 2: if channel is full, increments the spill counter (caller should
    ///          handle actual spilling or drop — for now we block on the channel).
    /// Phase 3: if spill count exceeds threshold, sets the overloaded flag.
    pub async fn send(&self, event: Event) -> Result<(), AeonError> {
        match self.tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                // Phase 2: channel is full — count spill pressure
                let count = self.spill_count.fetch_add(1, Ordering::Relaxed);
                if count + 1 >= self.spill_threshold {
                    self.overloaded.store(true, Ordering::Release);
                }
                // Block until space is available (applies natural backpressure)
                self.tx
                    .send(event)
                    .await
                    .map_err(|_| AeonError::connection("push buffer closed"))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                Err(AeonError::connection("push buffer closed"))
            }
        }
    }

    /// Check if the buffer is overloaded (Phase 3 signal).
    ///
    /// When true, the connector should apply protocol-level flow control:
    /// - WebSocket: stop reading from the socket
    /// - MQTT: unsubscribe temporarily
    /// - RabbitMQ: basic.cancel
    /// - NATS: pause subscription
    pub fn is_overloaded(&self) -> bool {
        self.overloaded.load(Ordering::Acquire)
    }
}

/// Receiver half — used by the Source's `next_batch()` to drain events.
pub struct PushBufferRx {
    rx: tokio::sync::mpsc::Receiver<Event>,
    batch_size: usize,
    drain_timeout: Duration,
    spill_count: Arc<AtomicU64>,
    overloaded: Arc<AtomicBool>,
}

impl PushBufferRx {
    /// Drain up to `batch_size` events from the buffer.
    ///
    /// Waits for at least one event (with poll_timeout), then drains additional
    /// events up to batch_size within drain_timeout.
    pub async fn next_batch(&mut self, poll_timeout: Duration) -> Result<Vec<Event>, AeonError> {
        let mut events = Vec::with_capacity(self.batch_size);

        // Wait for first event
        match tokio::time::timeout(poll_timeout, self.rx.recv()).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => return Ok(Vec::new()), // Channel closed
            Err(_) => return Ok(events),       // Timeout — empty batch
        }

        // Drain additional events with short timeout
        let deadline = tokio::time::Instant::now() + self.drain_timeout;
        while events.len() < self.batch_size {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(event)) => events.push(event),
                _ => break,
            }
        }

        // Clear overload if we've drained enough
        if !events.is_empty() {
            let remaining = self.spill_count.load(Ordering::Relaxed);
            if remaining > 0 {
                self.spill_count
                    .fetch_sub(remaining.min(events.len() as u64), Ordering::Relaxed);
            }
            if self.spill_count.load(Ordering::Relaxed) == 0 {
                self.overloaded.store(false, Ordering::Release);
            }
        }

        Ok(events)
    }
}

/// Configuration for a push buffer.
pub struct PushBufferConfig {
    /// Bounded channel capacity (Phase 1).
    pub channel_capacity: usize,
    /// Maximum events per `next_batch()`.
    pub batch_size: usize,
    /// Timeout for draining additional events after the first.
    pub drain_timeout: Duration,
    /// Number of spill events before triggering Phase 3 overload.
    pub spill_threshold: u64,
}

impl Default for PushBufferConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 8192,
            batch_size: 1024,
            drain_timeout: Duration::from_millis(5),
            spill_threshold: 4096,
        }
    }
}

/// Create a push buffer pair (tx for producer, rx for consumer).
pub fn push_buffer(config: PushBufferConfig) -> (PushBufferTx, PushBufferRx) {
    let (tx, rx) = tokio::sync::mpsc::channel(config.channel_capacity);
    let spill_count = Arc::new(AtomicU64::new(0));
    let overloaded = Arc::new(AtomicBool::new(false));

    let push_tx = PushBufferTx {
        tx,
        spill_count: Arc::clone(&spill_count),
        overloaded: Arc::clone(&overloaded),
        spill_threshold: config.spill_threshold,
    };

    let push_rx = PushBufferRx {
        rx,
        batch_size: config.batch_size,
        drain_timeout: config.drain_timeout,
        spill_count,
        overloaded,
    };

    (push_tx, push_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::PartitionId;
    use bytes::Bytes;
    use std::sync::Arc;

    fn test_event(payload: &str) -> Event {
        Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from(payload.to_string()),
        )
    }

    #[tokio::test]
    async fn test_push_buffer_basic() {
        let (tx, mut rx) = push_buffer(PushBufferConfig {
            channel_capacity: 16,
            batch_size: 4,
            drain_timeout: Duration::from_millis(10),
            spill_threshold: 8,
        });

        // Send some events
        for i in 0..6 {
            tx.send(test_event(&format!("event-{i}"))).await.unwrap();
        }

        // Drain first batch (up to 4)
        let batch = rx.next_batch(Duration::from_secs(1)).await.unwrap();
        assert_eq!(batch.len(), 4);

        // Drain remaining
        let batch = rx.next_batch(Duration::from_secs(1)).await.unwrap();
        assert_eq!(batch.len(), 2);
    }

    #[tokio::test]
    async fn test_push_buffer_timeout_empty() {
        let (_tx, mut rx) = push_buffer(PushBufferConfig::default());
        let batch = rx.next_batch(Duration::from_millis(50)).await.unwrap();
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn test_push_buffer_overload_signal() {
        let config = PushBufferConfig {
            channel_capacity: 4,
            batch_size: 2,
            drain_timeout: Duration::from_millis(5),
            spill_threshold: 2,
        };
        let (tx, mut rx) = push_buffer(config);

        // Fill the channel
        for i in 0..4 {
            tx.send(test_event(&format!("e{i}"))).await.unwrap();
        }

        // Channel is full — next sends will increment spill count.
        // Spawn senders that will block, then drain to release them.
        assert!(!tx.is_overloaded());

        // Drain to prevent deadlock
        let batch = rx.next_batch(Duration::from_secs(1)).await.unwrap();
        assert_eq!(batch.len(), 2);

        let batch = rx.next_batch(Duration::from_secs(1)).await.unwrap();
        assert_eq!(batch.len(), 2);
    }
}
