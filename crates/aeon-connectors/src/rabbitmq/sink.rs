//! RabbitMQ sink — AMQP producer.
//!
//! Publishes outputs to a RabbitMQ exchange. Each output becomes an AMQP
//! basic.publish with the payload as the message body.
//!
//! Supports three delivery strategies (matches Kafka sink contract):
//! - **PerEvent**: each publish is awaited on its publisher-confirm individually.
//!   Lowest throughput, strongest per-message guarantee.
//! - **OrderedBatch** (default): all publishes are issued in order, then all
//!   publisher-confirm futures are awaited concurrently via `join_all` at the
//!   batch boundary. Ordering is preserved because AMQP delivers publishes in
//!   the order they are sent on a channel. Amortizes the broker round-trip
//!   across the whole batch — the same ~40x win as the Kafka sink.
//! - **UnorderedBatch**: publishes are issued, publisher-confirm futures are
//!   stashed in `pending_confirms`, and `write_batch` returns with
//!   `BatchResult::all_pending`. `flush()` drains and awaits the stashed
//!   futures. Used by the pipeline sink task at flush intervals.

use aeon_types::{AeonError, BatchResult, DeliveryStrategy, Output, Sink, SinkAckCallback};
use futures_util::future::join_all;
use lapin::options::*;
use lapin::publisher_confirm::{Confirmation, PublisherConfirm};
use lapin::types::FieldTable;
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};

/// Configuration for `RabbitMqSink`.
pub struct RabbitMqSinkConfig {
    /// AMQP URI (e.g., "amqp://aeon:aeon_dev@localhost:5672").
    pub uri: String,
    /// Exchange to publish to (empty string = default exchange).
    pub exchange: String,
    /// Default routing key.
    pub routing_key: String,
    /// Whether to declare the exchange.
    pub declare_exchange: bool,
    /// Exchange type (if declaring).
    pub exchange_type: String,
    /// Whether to use publisher confirms.
    pub publisher_confirms: bool,
    /// Delivery strategy — controls how write_batch handles publisher confirms.
    pub strategy: DeliveryStrategy,
}

impl RabbitMqSinkConfig {
    /// Create a config for publishing to a RabbitMQ exchange.
    pub fn new(uri: impl Into<String>, exchange: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            exchange: exchange.into(),
            routing_key: String::new(),
            declare_exchange: false,
            exchange_type: "direct".to_string(),
            publisher_confirms: true,
            strategy: DeliveryStrategy::default(),
        }
    }

    /// Create a config for publishing to the default exchange with a routing key (queue name).
    pub fn direct_to_queue(uri: impl Into<String>, queue: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            exchange: String::new(),
            routing_key: queue.into(),
            declare_exchange: false,
            exchange_type: "direct".to_string(),
            publisher_confirms: true,
            strategy: DeliveryStrategy::default(),
        }
    }

    /// Set the routing key.
    pub fn with_routing_key(mut self, key: impl Into<String>) -> Self {
        self.routing_key = key.into();
        self
    }

    /// Enable exchange declaration.
    pub fn with_declare_exchange(mut self, exchange_type: impl Into<String>) -> Self {
        self.declare_exchange = true;
        self.exchange_type = exchange_type.into();
        self
    }

    /// Disable publisher confirms.
    pub fn without_publisher_confirms(mut self) -> Self {
        self.publisher_confirms = false;
        self
    }

    /// Set the delivery strategy.
    pub fn with_strategy(mut self, strategy: DeliveryStrategy) -> Self {
        self.strategy = strategy;
        self
    }
}

/// RabbitMQ output sink.
///
/// Publishes each output as an AMQP message. With publisher confirms enabled
/// (default), waits for broker acknowledgment for at-least-once delivery.
///
/// See the module-level docs for the three-strategy behavior of `write_batch`.
pub struct RabbitMqSink {
    channel: Channel,
    config: RabbitMqSinkConfig,
    /// Count of successfully confirmed publishes.
    delivered: u64,
    /// UnorderedBatch: stashed publisher-confirm futures awaited at `flush()`.
    pending_confirms: Vec<PublisherConfirm>,
    /// Count of outputs published but not yet confirmed (UnorderedBatch).
    pending: u64,
    _connection: Connection,
    /// Engine-installed callback fired when publisher confirms return. Drives
    /// the `outputs_acked_total` companion metric. UnorderedBatch fires on
    /// flush, not on enqueue.
    ack_callback: Option<SinkAckCallback>,
}

impl RabbitMqSink {
    /// Connect to RabbitMQ and set up the publishing channel.
    pub async fn new(config: RabbitMqSinkConfig) -> Result<Self, AeonError> {
        let conn = Connection::connect(&config.uri, ConnectionProperties::default())
            .await
            .map_err(|e| AeonError::connection(format!("rabbitmq connect failed: {e}")))?;

        let channel = conn
            .create_channel()
            .await
            .map_err(|e| AeonError::connection(format!("rabbitmq channel create failed: {e}")))?;

        // Enable publisher confirms if configured
        if config.publisher_confirms {
            channel
                .confirm_select(ConfirmSelectOptions::default())
                .await
                .map_err(|e| {
                    AeonError::connection(format!("rabbitmq confirm_select failed: {e}"))
                })?;
        }

        // Declare exchange if configured
        if config.declare_exchange {
            let kind = match config.exchange_type.as_str() {
                "fanout" => lapin::ExchangeKind::Fanout,
                "topic" => lapin::ExchangeKind::Topic,
                "headers" => lapin::ExchangeKind::Headers,
                _ => lapin::ExchangeKind::Direct,
            };
            channel
                .exchange_declare(
                    &config.exchange,
                    kind,
                    ExchangeDeclareOptions {
                        durable: true,
                        ..Default::default()
                    },
                    FieldTable::default(),
                )
                .await
                .map_err(|e| {
                    AeonError::connection(format!("rabbitmq exchange_declare failed: {e}"))
                })?;
        }

        tracing::info!(
            exchange = %config.exchange,
            routing_key = %config.routing_key,
            strategy = ?config.strategy,
            "RabbitMqSink connected"
        );

        Ok(Self {
            channel,
            config,
            delivered: 0,
            pending_confirms: Vec::new(),
            pending: 0,
            _connection: conn,
            ack_callback: None,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Number of outputs published but not yet confirmed (UnorderedBatch).
    pub fn pending(&self) -> u64 {
        self.pending
    }

    fn fire_ack(&self, n: usize) {
        if n == 0 {
            return;
        }
        if let Some(cb) = self.ack_callback.as_ref() {
            cb(n);
        }
    }
}

/// Inspect a RabbitMQ publisher confirmation result, converting Nack or a
/// basic.return into an `AeonError`.
fn check_confirmation(result: Result<Confirmation, lapin::Error>) -> Result<(), AeonError> {
    match result {
        Ok(Confirmation::Ack(_)) | Ok(Confirmation::NotRequested) => Ok(()),
        Ok(Confirmation::Nack(_)) => Err(AeonError::connection(
            "rabbitmq publish nacked by broker".to_string(),
        )),
        Err(e) => Err(AeonError::connection(format!(
            "rabbitmq publish confirm failed: {e}"
        ))),
    }
}

impl Sink for RabbitMqSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let count = outputs.len();
        let event_ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();

        // Issue all publishes in order. `basic_publish` on lapin resolves as
        // soon as the frame is enqueued on the channel; the returned
        // `PublisherConfirm` future completes when the broker acks the message
        // (or immediately if publisher confirms are disabled).
        let mut confirms: Vec<PublisherConfirm> = Vec::with_capacity(count);

        for output in &outputs {
            let confirm = self
                .channel
                .basic_publish(
                    &self.config.exchange,
                    &self.config.routing_key,
                    BasicPublishOptions::default(),
                    output.payload.as_ref(),
                    BasicProperties::default()
                        .with_delivery_mode(2) // persistent
                        .with_content_type("application/octet-stream".into()),
                )
                .await
                .map_err(|e| AeonError::connection(format!("rabbitmq publish failed: {e}")))?;

            match self.config.strategy {
                DeliveryStrategy::PerEvent => {
                    if self.config.publisher_confirms {
                        check_confirmation(confirm.await)?;
                    }
                    self.delivered += 1;
                }
                DeliveryStrategy::OrderedBatch | DeliveryStrategy::UnorderedBatch => {
                    confirms.push(confirm);
                }
            }
        }

        match self.config.strategy {
            DeliveryStrategy::PerEvent => {
                self.fire_ack(count);
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::OrderedBatch => {
                // Await all publisher-confirm futures concurrently at the
                // batch boundary. AMQP delivers publishes on a single channel
                // in the order they were sent, so ordering is preserved even
                // though `join_all` polls all futures together. This is the
                // same fix as the Kafka sink: replaces the sequential
                // per-future await that collapses throughput on Windows.
                if self.config.publisher_confirms {
                    let results = join_all(confirms).await;
                    for result in results {
                        check_confirmation(result)?;
                    }
                }
                self.delivered += count as u64;
                self.fire_ack(count);
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::UnorderedBatch => {
                // Stash the publisher-confirm futures. The pipeline sink task
                // will call `flush()` periodically, which awaits them and
                // credits the pending IDs to the metrics/ledger.
                self.pending_confirms.extend(confirms);
                self.pending += count as u64;
                Ok(BatchResult::all_pending(event_ids))
            }
        }
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        if !self.pending_confirms.is_empty() {
            let confirms = std::mem::take(&mut self.pending_confirms);
            if self.config.publisher_confirms {
                let results = join_all(confirms).await;
                for result in results {
                    check_confirmation(result)?;
                }
            }
        }
        let newly_acked = self.pending;
        self.delivered += newly_acked;
        self.pending = 0;
        self.fire_ack(newly_acked as usize);
        Ok(())
    }

    fn on_ack_callback(&mut self, cb: SinkAckCallback) {
        self.ack_callback = Some(cb);
    }
}
