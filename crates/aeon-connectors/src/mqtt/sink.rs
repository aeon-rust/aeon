//! MQTT sink — publishes outputs to an MQTT topic.
//!
//! Each output payload is published as an MQTT message to the configured topic.

use aeon_types::{AeonError, BatchResult, Output, Sink};
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

        // Spawn event loop poller to handle protocol traffic
        let handle = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(error = %e, "mqtt sink eventloop error");
                        tokio::time::sleep(Duration::from_secs(1)).await;
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
            self.client
                .publish(
                    &self.config.topic,
                    self.config.qos,
                    false, // retain
                    output.payload.to_vec(),
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
