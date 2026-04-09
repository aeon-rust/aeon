//! RabbitMQ sink — AMQP producer.
//!
//! Publishes outputs to a RabbitMQ exchange. Each output becomes an AMQP
//! basic.publish with the payload as the message body.

use aeon_types::{AeonError, BatchResult, Output, Sink};
use lapin::options::*;
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
}

/// RabbitMQ output sink.
///
/// Publishes each output as an AMQP message. With publisher confirms enabled
/// (default), waits for broker acknowledgment for at-least-once delivery.
pub struct RabbitMqSink {
    channel: Channel,
    config: RabbitMqSinkConfig,
    delivered: u64,
    _connection: Connection,
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
            "RabbitMqSink connected"
        );

        Ok(Self {
            channel,
            config,
            delivered: 0,
            _connection: conn,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

impl Sink for RabbitMqSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
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

            if self.config.publisher_confirms {
                confirm.await.map_err(|e| {
                    AeonError::connection(format!("rabbitmq publish confirm failed: {e}"))
                })?;
            }

            self.delivered += 1;
        }

        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // With publisher confirms, each message is already confirmed in write_batch
        Ok(())
    }
}
