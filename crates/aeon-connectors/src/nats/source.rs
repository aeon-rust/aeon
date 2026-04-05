//! NATS JetStream source — pull consumer.
//!
//! Uses JetStream pull-based consumer for durable, at-least-once delivery.
//! Messages are acknowledged after being returned from `next_batch()`.

use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;

/// Configuration for `NatsSource`.
pub struct NatsSourceConfig {
    /// NATS server URL (e.g., "nats://localhost:4222").
    pub url: String,
    /// JetStream stream name.
    pub stream: String,
    /// Consumer name (durable).
    pub consumer: String,
    /// Subject filter for the consumer.
    pub subject: String,
    /// Maximum messages per `next_batch()`.
    pub batch_size: usize,
    /// Fetch timeout.
    pub fetch_timeout: Duration,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
}

impl NatsSourceConfig {
    /// Create a config for consuming from a JetStream stream.
    pub fn new(
        url: impl Into<String>,
        stream: impl Into<String>,
        subject: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            stream: stream.into(),
            consumer: "aeon".to_string(),
            subject: subject.into(),
            batch_size: 1024,
            fetch_timeout: Duration::from_secs(1),
            source_name: Arc::from("nats"),
        }
    }

    /// Set the durable consumer name.
    pub fn with_consumer(mut self, name: impl Into<String>) -> Self {
        self.consumer = name.into();
        self
    }

    /// Set batch size.
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set fetch timeout.
    pub fn with_fetch_timeout(mut self, timeout: Duration) -> Self {
        self.fetch_timeout = timeout;
        self
    }
}

/// NATS JetStream event source.
///
/// Uses a pull-based consumer for batch fetching. Messages are acked
/// individually after successful delivery from `next_batch()`.
pub struct NatsSource {
    consumer:
        async_nats::jetstream::consumer::Consumer<async_nats::jetstream::consumer::pull::Config>,
    config: NatsSourceConfig,
    messages: Option<async_nats::jetstream::consumer::pull::Stream>,
}

impl NatsSource {
    /// Connect to NATS and create/bind to a JetStream consumer.
    pub async fn new(config: NatsSourceConfig) -> Result<Self, AeonError> {
        let client = async_nats::connect(&config.url)
            .await
            .map_err(|e| AeonError::connection(format!("nats connect failed: {e}")))?;

        let jetstream = async_nats::jetstream::new(client);

        // Get or create stream
        let stream = jetstream.get_stream(&config.stream).await.map_err(|e| {
            AeonError::connection(format!("nats stream '{}' not found: {e}", config.stream))
        })?;

        // Create or get durable consumer
        let consumer = stream
            .get_or_create_consumer(
                &config.consumer,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(config.consumer.clone()),
                    filter_subject: config.subject.clone(),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| AeonError::connection(format!("nats consumer create failed: {e}")))?;

        tracing::info!(
            stream = %config.stream,
            consumer_name = %config.consumer,
            subject = %config.subject,
            "NatsSource connected"
        );

        Ok(Self {
            consumer,
            config,
            messages: None,
        })
    }
}

impl Source for NatsSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // Initialize message stream if needed
        if self.messages.is_none() {
            let msgs =
                self.consumer.messages().await.map_err(|e| {
                    AeonError::connection(format!("nats messages stream failed: {e}"))
                })?;
            self.messages = Some(msgs);
        }

        let messages = self
            .messages
            .as_mut()
            .ok_or_else(|| AeonError::state("NatsSource messages not initialized"))?;

        let mut events = Vec::with_capacity(self.config.batch_size);
        let now = std::time::Instant::now();

        // Read first message with full timeout
        use futures_util::StreamExt;
        let first = tokio::time::timeout(self.config.fetch_timeout, messages.next()).await;

        match first {
            Ok(Some(Ok(msg))) => {
                let payload = Bytes::from(msg.payload.to_vec());
                let mut event = Event::new(
                    uuid::Uuid::nil(),
                    0,
                    Arc::clone(&self.config.source_name),
                    PartitionId::new(0),
                    payload,
                );
                event = event.with_source_ts(now);

                // Propagate NATS headers as metadata
                if let Some(headers) = &msg.headers {
                    for (key, values) in headers.iter() {
                        for value in values {
                            event.metadata.push((
                                Arc::from(key.to_string().as_str()),
                                Arc::from(value.as_str()),
                            ));
                        }
                    }
                }

                events.push(event);
                let _ = msg.ack().await;
            }
            Ok(Some(Err(e))) => {
                return Err(AeonError::connection(format!("nats recv error: {e}")));
            }
            _ => return Ok(events), // Timeout or stream ended
        }

        // Drain additional messages
        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(5);
        while events.len() < self.config.batch_size {
            match tokio::time::timeout_at(drain_deadline, messages.next()).await {
                Ok(Some(Ok(msg))) => {
                    let payload = Bytes::from(msg.payload.to_vec());
                    let mut event = Event::new(
                        uuid::Uuid::nil(),
                        0,
                        Arc::clone(&self.config.source_name),
                        PartitionId::new(0),
                        payload,
                    );
                    event = event.with_source_ts(now);
                    events.push(event);
                    let _ = msg.ack().await;
                }
                _ => break,
            }
        }

        Ok(events)
    }
}
