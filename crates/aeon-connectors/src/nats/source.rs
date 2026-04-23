//! NATS JetStream source — pull consumer.
//!
//! Uses JetStream pull-based consumer for durable, at-least-once delivery.
//! Messages are acknowledged after being returned from `next_batch()`.

use aeon_types::{
    AeonError, BackoffPolicy, CoreLocalUuidGenerator, Event, OutboundAuthSigner, PartitionId,
    Source,
};
use std::sync::{Arc, Mutex};
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
    /// Reconnect backoff policy (TR-3). Applied when the JetStream message
    /// pull stream errors out (typically a broken connection). Exponential
    /// growth with jitter prevents reconnect storms during NATS outages.
    pub backoff: BackoffPolicy,
    /// S10 outbound auth. When `Some`, the signer drives `ConnectOptions`
    /// construction — see `nats::auth::connect_with_auth`.
    pub auth: Option<Arc<OutboundAuthSigner>>,
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
            backoff: BackoffPolicy::default(),
            auth: None,
        }
    }

    /// S10: attach an outbound-auth signer.
    pub fn with_auth(mut self, signer: Arc<OutboundAuthSigner>) -> Self {
        self.auth = Some(signer);
        self
    }

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
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
    /// TR-3 reconnect backoff. Advanced on message-stream errors; reset on
    /// every successful message delivery.
    backoff: aeon_types::Backoff,
    uuid_gen: Mutex<CoreLocalUuidGenerator>,
}

impl NatsSource {
    /// Connect to NATS and create/bind to a JetStream consumer.
    pub async fn new(config: NatsSourceConfig) -> Result<Self, AeonError> {
        let client = super::auth::connect_with_auth(&config.url, config.auth.as_ref()).await?;

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

        let backoff = config.backoff.iter();
        Ok(Self {
            consumer,
            config,
            messages: None,
            backoff,
            uuid_gen: Mutex::new(CoreLocalUuidGenerator::new(0)),
        })
    }
}

impl Source for NatsSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // Initialize or re-initialize the message stream. On transient errors
        // (TR-3) we drop the stream and sleep with jittered exponential
        // backoff so repeated re-stream attempts during a NATS outage don't
        // generate a reconnect storm. Returning Ok(vec![]) rather than
        // propagating Err keeps the pipeline alive through the outage.
        if self.messages.is_none() {
            match self.consumer.messages().await {
                Ok(msgs) => {
                    self.messages = Some(msgs);
                }
                Err(e) => {
                    let delay = self.backoff.next_delay();
                    tracing::warn!(
                        error = %e,
                        delay_ms = delay.as_millis() as u64,
                        "nats messages stream init failed; backing off before retry"
                    );
                    tokio::time::sleep(delay).await;
                    return Ok(Vec::new());
                }
            }
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
                // Successful delivery — reset backoff so the next outage
                // starts again at `initial_ms`.
                self.backoff.reset();

                // FT-11: async_nats Message.payload is bytes::Bytes — clone is refcount-only.
                let payload = msg.payload.clone();
                // B2: stream_sequence is the JetStream-assigned monotonic
                // per-stream sequence number, the canonical resume anchor.
                let stream_seq = msg.info().ok().map(|i| i.stream_sequence as i64);
                let mut event = Event::new(
                    self.uuid_gen
                        .lock()
                        .map_err(|_| AeonError::connection("UUID generator mutex poisoned"))?
                        .next_uuid(),
                    0,
                    Arc::clone(&self.config.source_name),
                    PartitionId::new(0),
                    payload,
                );
                event = event.with_source_ts(now);
                if let Some(seq) = stream_seq {
                    event = event.with_source_offset(seq);
                }

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
                // Stream-level error (typically a broken connection). Drop the
                // message stream so the next next_batch() re-creates it, then
                // back off.
                let delay = self.backoff.next_delay();
                tracing::warn!(
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "nats recv error; dropping stream and backing off before retry"
                );
                self.messages = None;
                tokio::time::sleep(delay).await;
                return Ok(events);
            }
            _ => return Ok(events), // Timeout or stream ended cleanly
        }

        // Drain additional messages
        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(5);
        while events.len() < self.config.batch_size {
            match tokio::time::timeout_at(drain_deadline, messages.next()).await {
                Ok(Some(Ok(msg))) => {
                    // FT-11: async_nats Message.payload is bytes::Bytes — clone is refcount-only.
                    let payload = msg.payload.clone();
                    let stream_seq = msg.info().ok().map(|i| i.stream_sequence as i64);
                    let mut event = Event::new(
                        self.uuid_gen
                            .lock()
                            .map_err(|_| AeonError::connection("UUID generator mutex poisoned"))?
                            .next_uuid(),
                        0,
                        Arc::clone(&self.config.source_name),
                        PartitionId::new(0),
                        payload,
                    );
                    event = event.with_source_ts(now);
                    if let Some(seq) = stream_seq {
                        event = event.with_source_offset(seq);
                    }
                    events.push(event);
                    let _ = msg.ack().await;
                }
                _ => break,
            }
        }

        Ok(events)
    }
}
