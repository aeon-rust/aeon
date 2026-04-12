//! MQTT sink — publishes outputs to an MQTT topic.
//!
//! Each output payload is published as an MQTT message to the configured topic.
//!
//! # Why `DeliveryStrategy` is not honored here
//!
//! Unlike the Kafka, RabbitMQ, and Redis Streams sinks, this connector does
//! **not** branch on `DeliveryStrategy`. `rumqttc::AsyncClient::publish()`
//! returns `Result<(), ClientError>` as soon as the request has been enqueued
//! on rumqttc's internal flume channel to the background `EventLoop` task —
//! it does not return a future that can be awaited for the broker PUBACK /
//! PUBCOMP. Those acks are consumed by the `EventLoop::poll()` loop spawned
//! in `MqttSink::new`, and the sink currently has no side channel to observe
//! them per packet-id.
//!
//! Consequently, `publish(...).await` is already semantically equivalent to
//! `UnorderedBatch`: publishes are batched inside rumqttc's own request queue
//! and confirmed by the broker in the background. A Vec of `pending_confirms`
//! would have nothing to hold and `join_all` would have nothing to await.
//!
//! Real per-publish confirmation would require:
//! 1. Capturing `Incoming::PubAck` / `Incoming::PubComp` events from the
//!    `EventLoop::poll()` loop and matching them to a packet-id → oneshot map
//!    held by the sink.
//! 2. Threading that map through a publish path that reserves packet-ids
//!    before sending.
//!
//! See `docs/CONNECTOR-AUDIT.md` §4.4. MQTT is post-Gate 2 per `CLAUDE.md` so
//! this investment is deferred.

use aeon_types::{AeonError, BackoffPolicy, BatchResult, Output, Sink};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::time::Duration;

/// Configuration for `MqttSink`.
pub struct MqttSinkConfig {
    /// MQTT broker host.
    pub host: String,
    /// MQTT broker port.
    pub port: u16,
    /// Client ID.
    pub client_id: String,
    /// Default topic to publish to.
    pub topic: String,
    /// QoS level for publishing.
    pub qos: QoS,
    /// Keep alive interval.
    pub keep_alive: Duration,
    /// Channel capacity for the MQTT event loop.
    pub cap: usize,
    /// Reconnect backoff policy — applied when the event loop returns an error
    /// (connection drop, protocol error). Exponential + jitter prevents
    /// thundering-herd reconnect storms during broker outages (TR-3).
    pub backoff: BackoffPolicy,
}

impl MqttSinkConfig {
    /// Create a config for publishing to an MQTT topic.
    pub fn new(host: impl Into<String>, port: u16, topic: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            client_id: format!("aeon-mqtt-sink-{}", std::process::id()),
            topic: topic.into(),
            qos: QoS::AtLeastOnce,
            keep_alive: Duration::from_secs(30),
            cap: 256,
            backoff: BackoffPolicy::default(),
        }
    }

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
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
}

/// MQTT output sink.
///
/// Publishes each output as an MQTT message. The event loop is polled
/// in a background task to handle protocol-level communication (PingResp, etc.).
pub struct MqttSink {
    client: AsyncClient,
    config: MqttSinkConfig,
    delivered: u64,
    _eventloop_handle: tokio::task::JoinHandle<()>,
}

impl MqttSink {
    /// Connect to the MQTT broker.
    pub async fn new(config: MqttSinkConfig) -> Result<Self, AeonError> {
        let mut mqttoptions = MqttOptions::new(&config.client_id, &config.host, config.port);
        mqttoptions.set_keep_alive(config.keep_alive);

        let (client, mut eventloop) = AsyncClient::new(mqttoptions, config.cap);

        tracing::info!(
            host = %config.host,
            port = config.port,
            topic = %config.topic,
            "MqttSink connecting"
        );

        // Spawn event loop poller to handle protocol traffic.
        // TR-3: shared BackoffPolicy with jitter replaces hardcoded sleep(1s).
        // Reset on any successful poll so the next outage restarts at initial_ms.
        let backoff_policy = config.backoff;
        let handle = tokio::spawn(async move {
            let mut backoff = backoff_policy.iter();
            loop {
                match eventloop.poll().await {
                    Ok(_) => backoff.reset(),
                    Err(e) => {
                        let delay = backoff.next_delay();
                        tracing::error!(error = %e, delay_ms = delay.as_millis() as u64,
                            "mqtt sink eventloop error; backing off before retry");
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        });

        Ok(Self {
            client,
            config,
            delivered: 0,
            _eventloop_handle: handle,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

impl Sink for MqttSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
        for output in &outputs {
            // FT-11: publish_bytes accepts bytes::Bytes directly — clone is refcount-only.
            self.client
                .publish_bytes(
                    self.config.topic.clone(),
                    self.config.qos,
                    false, // retain
                    output.payload.clone(),
                )
                .await
                .map_err(|e| AeonError::connection(format!("mqtt publish failed: {e}")))?;
            self.delivered += 1;
        }

        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // MQTT protocol handles delivery via QoS — no explicit flush needed
        Ok(())
    }
}
