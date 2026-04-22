//! RabbitMQ source — AMQP consumer.
//!
//! Push-source: a background task consumes from a RabbitMQ queue and pushes
//! events into a PushBuffer. Messages are acknowledged after being pushed.
//! Phase 3 backpressure: when overloaded, uses basic.cancel to stop delivery.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_types::{
    AeonError, BackoffPolicy, CoreLocalUuidGenerator, Event, PartitionId, Source, SourceKind,
};
use bytes::Bytes;
use lapin::options::*;
use lapin::types::FieldTable;
use lapin::{Channel, Connection, ConnectionProperties, Consumer};
use std::sync::Arc;
use std::time::Duration;

/// Configuration for `RabbitMqSource`.
pub struct RabbitMqSourceConfig {
    /// AMQP URI (e.g., "amqp://aeon:aeon_dev@localhost:5672").
    pub uri: String,
    /// Queue to consume from.
    pub queue: String,
    /// Consumer tag.
    pub consumer_tag: String,
    /// Prefetch count (QoS).
    pub prefetch_count: u16,
    /// Push buffer configuration.
    pub buffer_config: PushBufferConfig,
    /// Timeout waiting for first event in `next_batch()`.
    pub poll_timeout: Duration,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Whether to declare the queue (create if not exists).
    pub declare_queue: bool,
    /// Reconnect backoff policy (TR-3). Applied when the AMQP connection or
    /// consumer stream dies. Exponential + jitter prevents reconnect storms
    /// against a flaky RabbitMQ broker.
    pub backoff: BackoffPolicy,
}

impl RabbitMqSourceConfig {
    /// Create a config for consuming from a RabbitMQ queue.
    pub fn new(uri: impl Into<String>, queue: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            queue: queue.into(),
            consumer_tag: format!("aeon-{}", std::process::id()),
            prefetch_count: 256,
            buffer_config: PushBufferConfig::default(),
            poll_timeout: Duration::from_secs(1),
            source_name: Arc::from("rabbitmq"),
            declare_queue: true,
            backoff: BackoffPolicy::default(),
        }
    }

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Set consumer tag.
    pub fn with_consumer_tag(mut self, tag: impl Into<String>) -> Self {
        self.consumer_tag = tag.into();
        self
    }

    /// Set prefetch count (QoS).
    pub fn with_prefetch_count(mut self, count: u16) -> Self {
        self.prefetch_count = count;
        self
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set whether to declare the queue.
    pub fn with_declare_queue(mut self, declare: bool) -> Self {
        self.declare_queue = declare;
        self
    }

    /// Set the poll timeout.
    pub fn with_poll_timeout(mut self, timeout: Duration) -> Self {
        self.poll_timeout = timeout;
        self
    }
}

/// RabbitMQ event source.
///
/// Spawns a background task that consumes from the queue via AMQP
/// basic.consume and pushes events into the push buffer. The reader task
/// transparently reconnects with jittered exponential backoff (TR-3) on
/// connection loss, so a flaky broker doesn't silently kill the source.
pub struct RabbitMqSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl RabbitMqSource {
    /// Connect to RabbitMQ, declare queue if needed, and start consuming.
    ///
    /// Returns an error if the **initial** connection fails. Subsequent
    /// reconnections are handled transparently by the background reader.
    pub async fn new(config: RabbitMqSourceConfig) -> Result<Self, AeonError> {
        // Validate the initial connection synchronously so misconfiguration
        // surfaces loudly to the operator. The connection is dropped before
        // the reader task starts; the reader re-establishes its own handle
        // so the reconnect loop owns a single connection/channel/consumer
        // triple throughout its lifetime.
        let _validate = connect_and_consume(&config).await?;
        drop(_validate);

        let (tx, rx) = push_buffer(config.buffer_config.clone());
        let poll_timeout = config.poll_timeout;
        let handle = tokio::spawn(rabbitmq_reader(config, tx));

        Ok(Self {
            rx,
            poll_timeout,
            _reader_handle: handle,
        })
    }
}

/// Establish connection → channel → QoS → (optional queue declare) → consumer.
/// Returns the triple so callers can drive the stream.
async fn connect_and_consume(
    config: &RabbitMqSourceConfig,
) -> Result<(Connection, Channel, Consumer), AeonError> {
    let conn = Connection::connect(&config.uri, ConnectionProperties::default())
        .await
        .map_err(|e| AeonError::connection(format!("rabbitmq connect failed: {e}")))?;

    let channel = conn
        .create_channel()
        .await
        .map_err(|e| AeonError::connection(format!("rabbitmq channel create failed: {e}")))?;

    channel
        .basic_qos(config.prefetch_count, BasicQosOptions::default())
        .await
        .map_err(|e| AeonError::connection(format!("rabbitmq basic_qos failed: {e}")))?;

    if config.declare_queue {
        channel
            .queue_declare(
                &config.queue,
                QueueDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| AeonError::connection(format!("rabbitmq queue_declare failed: {e}")))?;
    }

    let consumer = channel
        .basic_consume(
            &config.queue,
            &config.consumer_tag,
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .map_err(|e| AeonError::connection(format!("rabbitmq basic_consume failed: {e}")))?;

    tracing::info!(
        queue = %config.queue,
        consumer_tag = %config.consumer_tag,
        prefetch = config.prefetch_count,
        "RabbitMqSource consuming"
    );

    Ok((conn, channel, consumer))
}

async fn rabbitmq_reader(config: RabbitMqSourceConfig, tx: crate::push_buffer::PushBufferTx) {
    let mut backoff = config.backoff.iter();
    let mut uuid_gen = CoreLocalUuidGenerator::new(0);

    // Outer reconnect loop. Each iteration owns a fresh
    // Connection+Channel+Consumer triple. On any disconnect the inner loop
    // breaks and we back off before attempting to reconnect.
    'reconnect: loop {
        let (conn, channel, consumer) = match connect_and_consume(&config).await {
            Ok(triple) => {
                backoff.reset();
                triple
            }
            Err(e) => {
                let delay = backoff.next_delay();
                tracing::warn!(
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "rabbitmq reconnect failed; backing off before retry"
                );
                tokio::time::sleep(delay).await;
                continue 'reconnect;
            }
        };

        if !drive_consumer(consumer, &channel, &tx, &config.source_name, &mut uuid_gen).await {
            // Buffer closed — drop the connection and exit cleanly.
            drop(channel);
            drop(conn);
            return;
        }

        // Inner loop exited because of a delivery/channel error. Drop the
        // triple so the broker sees a clean disconnect, then back off.
        drop(channel);
        drop(conn);
        let delay = backoff.next_delay();
        tracing::warn!(
            delay_ms = delay.as_millis() as u64,
            "rabbitmq consumer stream ended; backing off before reconnect"
        );
        tokio::time::sleep(delay).await;
    }
}

/// Drive an active consumer stream. Returns `false` when the push buffer is
/// closed (shutdown); `true` when the stream needs a reconnect.
async fn drive_consumer(
    mut consumer: Consumer,
    channel: &Channel,
    tx: &crate::push_buffer::PushBufferTx,
    source_name: &Arc<str>,
    uuid_gen: &mut CoreLocalUuidGenerator,
) -> bool {
    use futures_util::StreamExt;

    while let Some(delivery_result) = consumer.next().await {
        match delivery_result {
            Ok(delivery) => {
                // Phase 3: if overloaded, nack and requeue
                if tx.is_overloaded() {
                    tracing::warn!("rabbitmq source overloaded, nacking message");
                    let _ = delivery
                        .nack(BasicNackOptions {
                            requeue: true,
                            ..Default::default()
                        })
                        .await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }

                let payload = Bytes::from(delivery.data.clone());
                let mut event = Event::new(
                    uuid_gen.next_uuid(),
                    0,
                    Arc::clone(source_name),
                    PartitionId::new(0),
                    payload,
                );

                if let Some(headers) = &delivery.properties.headers() {
                    for (key, value) in headers.inner() {
                        let value_str = format!("{value:?}");
                        event
                            .metadata
                            .push((Arc::from(key.as_str()), Arc::from(value_str.as_str())));
                    }
                }

                if let Some(routing_key) = delivery.routing_key.as_str().into() {
                    event
                        .metadata
                        .push((Arc::from("amqp.routing_key"), Arc::from(routing_key)));
                }

                if tx.send(event).await.is_err() {
                    return false; // Buffer closed — shutdown signal.
                }

                let _ = delivery.ack(BasicAckOptions::default()).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "rabbitmq delivery error");
                if !channel.status().connected() {
                    tracing::error!("rabbitmq channel disconnected");
                    return true; // Trigger reconnect.
                }
            }
        }
    }
    // Stream ended cleanly (consumer cancelled or channel closed) — reconnect.
    true
}

impl Source for RabbitMqSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }

    fn source_kind(&self) -> SourceKind {
        SourceKind::Push
    }
}
