//! MQTT source — subscribes to a topic and receives messages.
//!
//! Push-source: a background task reads from the MQTT EventLoop and pushes
//! events into a PushBuffer. `next_batch()` drains the buffer.
//! Phase 3 backpressure: when overloaded, pause the event loop polling.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS};
use std::sync::Arc;
use std::time::Duration;

/// Configuration for `MqttSource`.
pub struct MqttSourceConfig {
    /// MQTT broker host.
    pub host: String,
    /// MQTT broker port.
    pub port: u16,
    /// Client ID.
    pub client_id: String,
    /// Topic to subscribe to.
    pub topic: String,
    /// QoS level (0, 1, or 2).
    pub qos: QoS,
    /// Push buffer configuration.
    pub buffer_config: PushBufferConfig,
    /// Timeout waiting for first event in `next_batch()`.
    pub poll_timeout: Duration,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Keep alive interval.
    pub keep_alive: Duration,
    /// Channel capacity for the MQTT event loop.
    pub cap: usize,
}

impl MqttSourceConfig {
    /// Create a config for subscribing to an MQTT topic.
    pub fn new(host: impl Into<String>, port: u16, topic: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            client_id: format!("aeon-mqtt-{}", std::process::id()),
            topic: topic.into(),
            qos: QoS::AtLeastOnce,
            buffer_config: PushBufferConfig::default(),
            poll_timeout: Duration::from_secs(1),
            source_name: Arc::from("mqtt"),
            keep_alive: Duration::from_secs(30),
            cap: 256,
        }
    }

    /// Set client ID.
    pub fn with_client_id(mut self, id: impl Into<String>) -> Self {
        self.client_id = id.into();
        self
    }

    /// Set QoS level.
    pub fn with_qos(mut self, qos: QoS) -> Self {
        self.qos = qos;
        self
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set the poll timeout.
    pub fn with_poll_timeout(mut self, timeout: Duration) -> Self {
        self.poll_timeout = timeout;
        self
    }
}

/// MQTT event source.
///
/// Spawns a background task that polls the MQTT event loop and pushes
/// incoming Publish messages into the push buffer. `next_batch()` drains events.
pub struct MqttSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    _client: AsyncClient,
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl MqttSource {
    /// Connect to the MQTT broker and subscribe to the topic.
    pub async fn new(config: MqttSourceConfig) -> Result<Self, AeonError> {
        let mut mqttoptions = MqttOptions::new(&config.client_id, &config.host, config.port);
        mqttoptions.set_keep_alive(config.keep_alive);

        let (client, eventloop) = AsyncClient::new(mqttoptions, config.cap);

        // Subscribe to topic
        client
            .subscribe(&config.topic, config.qos)
            .await
            .map_err(|e| AeonError::connection(format!("mqtt subscribe failed: {e}")))?;

        tracing::info!(
            host = %config.host,
            port = config.port,
            topic = %config.topic,
            "MqttSource subscribed"
        );

        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = config.source_name;

        let handle = tokio::spawn(mqtt_reader(eventloop, tx, source_name));

        Ok(Self {
            rx,
            poll_timeout: config.poll_timeout,
            _client: client,
            _reader_handle: handle,
        })
    }
}

async fn mqtt_reader(
    mut eventloop: EventLoop,
    tx: crate::push_buffer::PushBufferTx,
    source_name: Arc<str>,
) {
    loop {
        // Phase 3: if overloaded, back off before polling
        if tx.is_overloaded() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        match eventloop.poll().await {
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish))) => {
                let payload = Bytes::from(publish.payload.to_vec());
                let mut event = Event::new(
                    uuid::Uuid::nil(),
                    0,
                    Arc::clone(&source_name),
                    PartitionId::new(0),
                    payload,
                );
                // Store MQTT topic as metadata
                event
                    .metadata
                    .push((Arc::from("mqtt.topic"), Arc::from(publish.topic.as_str())));

                if tx.send(event).await.is_err() {
                    break; // Buffer closed
                }
            }
            Ok(_) => {} // Other events (ConnAck, PingResp, etc.)
            Err(e) => {
                tracing::error!(error = %e, "mqtt eventloop error");
                // Back off on error before retrying
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

impl Source for MqttSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }
}
