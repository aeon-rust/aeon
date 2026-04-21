//! MQTT source — subscribes to a topic and receives messages.
//!
//! Push-source: a background task reads from the MQTT EventLoop and pushes
//! events into a PushBuffer. `next_batch()` drains the buffer.
//!
//! **Backpressure**: `PushBufferTx::send(event).await` blocks when the
//! channel is full. Because the reader task is the only thing calling
//! `EventLoop::poll()`, suspending on `tx.send` also suspends the poll loop
//! — rumqttc stops reading from the broker's TCP socket, the OS receive
//! window fills, and the broker naturally slows its delivery to us (or,
//! for QoS 1/2, stops delivering until we catch up and ack). No messages
//! are dropped. This is the same TCP-window-driven backpressure the
//! WebSocket source relies on.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_types::{AeonError, BackoffPolicy, CoreLocalUuidGenerator, Event, PartitionId, Source};
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
    /// Reconnect backoff policy — applied when the MQTT eventloop returns an
    /// error (connection drop, protocol error, etc.). Exponential growth with
    /// jitter prevents thundering-herd reconnect storms during broker outages.
    pub backoff: BackoffPolicy,
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
            backoff: BackoffPolicy::default(),
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

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
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

        let uuid_gen = CoreLocalUuidGenerator::new(0);
        let handle = tokio::spawn(mqtt_reader(eventloop, tx, source_name, config.backoff, uuid_gen));

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
    backoff_policy: BackoffPolicy,
    mut uuid_gen: CoreLocalUuidGenerator,
) {
    let mut backoff = backoff_policy.iter();
    loop {
        match eventloop.poll().await {
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish))) => {
                // Successful delivery — reset backoff so the next outage
                // starts again at `initial_ms` rather than wherever we left off.
                backoff.reset();

                // FT-11: rumqttc Publish.payload is bytes::Bytes — clone is refcount-only.
                let payload = publish.payload.clone();
                let mut event = Event::new(
                    uuid_gen.next_uuid(),
                    0,
                    Arc::clone(&source_name),
                    PartitionId::new(0),
                    payload,
                );
                // Store MQTT topic as metadata
                event
                    .metadata
                    .push((Arc::from("mqtt.topic"), Arc::from(publish.topic.as_str())));

                // tx.send blocks when the push buffer is full. Suspending
                // the reader task stops `eventloop.poll()`, which stops
                // draining rumqttc's internal receiver, which stops reading
                // from the TCP socket — the OS receive window fills and the
                // broker naturally slows delivery (for QoS 1/2, it stops
                // delivering until we catch up and ack). Messages are never
                // dropped.
                if tx.send(event).await.is_err() {
                    break; // Buffer closed
                }
            }
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                // Successful (re)connection — reset backoff.
                backoff.reset();
            }
            Ok(_) => {} // Other events (PingResp, SubAck, etc.)
            Err(e) => {
                let delay = backoff.next_delay();
                tracing::error!(error = %e, delay_ms = delay.as_millis() as u64,
                    "mqtt eventloop error; backing off before retry");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

impl Source for MqttSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }
}
