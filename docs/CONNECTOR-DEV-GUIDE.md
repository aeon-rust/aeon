# Aeon Connector Development Guide

This guide covers everything you need to build custom Source and Sink connectors
for Aeon. It walks through the trait contracts, required patterns, complete
working examples, and the steps to wire a new connector into the engine.

All code in this guide follows the mandatory rules in `CLAUDE.md`: no panics on
the hot path, zero-copy where possible (`Bytes`, `Arc<str>`), `Result<T, AeonError>`
everywhere, and batch-first APIs.

---

## Table of Contents

1. [Overview](#1-overview)
2. [Source Trait](#2-source-trait)
3. [Sink Trait](#3-sink-trait)
4. [Extended Traits](#4-extended-traits)
5. [Step-by-Step: Building a Custom Source](#5-step-by-step-building-a-custom-source)
6. [Step-by-Step: Building a Custom Sink](#6-step-by-step-building-a-custom-sink)
7. [Push-Based Sources](#7-push-based-sources)
8. [Delivery Strategy Integration](#8-delivery-strategy-integration)
9. [Feature Gating](#9-feature-gating)
10. [Testing](#10-testing)
11. [Registration](#11-registration)
12. [Best Practices](#12-best-practices)

---

## 1. Overview

### Aeon's Connector Architecture

Aeon pipelines follow a simple three-stage model:

```
Source (ingress)  -->  Processor (transform)  -->  Sink (egress)
```

Connectors are the Source and Sink components. A **Source** pulls (or receives)
data from an external system and produces `Event` values. A **Sink** receives
`Output` values from the processor and delivers them to an external system.

The pipeline engine wires these together through SPSC ring buffers:

```
Source::next_batch()
    --> SPSC Ring Buffer
        --> Processor::process_batch()
            --> SPSC Ring Buffer
                --> Sink::write_batch()
```

### When to Write a Custom Connector

Write a custom connector when:

- You need to ingest from or write to a system that Aeon does not yet support.
- You need specialized behavior for an existing system (e.g., custom
  authentication, a proprietary wire protocol, or a domain-specific batching
  strategy).

Built-in connectors: Memory, Blackhole, Stdout, File, Kafka/Redpanda, NATS,
MQTT, Redis Streams, WebSocket, HTTP, RabbitMQ, QUIC, WebTransport,
PostgreSQL CDC, MySQL CDC, MongoDB CDC.

### The Source/Sink Trait Contract

Both traits are defined in `aeon-types` (`crates/aeon-types/src/traits.rs`):

- **Sources** produce `Vec<Event>` batches. The engine calls `next_batch()`
  in a loop. An empty vec signals "no data right now" (not end-of-stream).
- **Sinks** consume `Vec<Output>` batches and return `BatchResult` indicating
  per-event delivery status. The engine calls `write_batch()` then periodically
  `flush()`.
- Both traits require `Send + Sync`.
- All methods return `Result<T, AeonError>` -- never panic.

---

## 2. Source Trait

### Full Trait Signature

```rust
pub trait Source: Send + Sync {
    fn next_batch(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Vec<Event>, AeonError>> + Send;
}
```

### Method Contract: `next_batch()`

| Behavior | Description |
|----------|-------------|
| Returns `Vec<Event>` | A batch of zero or more events. |
| Empty vec | "No data available right now." The engine will call again. This is **not** end-of-stream; exhaustible sources (files, memory) return empty vecs indefinitely once done. |
| Error | `Err(AeonError)` signals a problem. The engine applies retry logic for retryable errors (`AeonError::is_retryable()`). |
| Batch size | Determined by the source's internal configuration. The engine does not dictate batch size. |
| Blocking | The future may await I/O (network poll, file read). Use tokio async I/O, never `std::io`. |

### Event Construction

Every event must use the canonical `Event` envelope from `aeon-types`:

```rust
use aeon_types::{Event, PartitionId};
use bytes::Bytes;
use std::sync::Arc;

let event = Event::new(
    uuid::Uuid::now_v7(),             // UUIDv7 -- time-ordered
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0), // Unix epoch nanos
    Arc::from("my-source"),           // Interned source name (NOT String)
    PartitionId::new(0),              // Partition for parallel processing
    Bytes::from(raw_payload),         // Zero-copy payload
)
.with_source_ts(std::time::Instant::now())  // For latency tracking
.with_source_offset(42);                     // For checkpoint resume
```

Key rules:
- `source`: Use `Arc<str>`, interned once at init, cloned by pointer thereafter.
- `payload`: Use `Bytes` for zero-copy. Prefer `Bytes::copy_from_slice()` when
  the upstream library owns the buffer (e.g., rdkafka). Prefer
  `Bytes::from(vec)` when you already own the data.
- `metadata`: Use `SmallVec<[(Arc<str>, Arc<str>); 4]>`. Up to 4 headers stay
  inline (no heap allocation).

### Pull-Based vs Push-Based

**Pull-based** (simpler): `next_batch()` calls the external system directly.
Examples: Kafka (`consumer.recv()`), File (`reader.read_line()`), HTTP polling.

**Push-based** (background task): A spawned task receives data and writes to a
bounded channel. `next_batch()` drains the channel. Examples: WebSocket, MQTT,
RabbitMQ. See [Section 7](#7-push-based-sources) for the `PushBuffer` pattern.

---

## 3. Sink Trait

### Full Trait Signature

```rust
pub trait Sink: Send + Sync {
    fn write_batch(
        &mut self,
        outputs: Vec<Output>,
    ) -> impl std::future::Future<Output = Result<BatchResult, AeonError>> + Send;

    fn flush(
        &mut self,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;
}
```

### Method Contract: `write_batch()`

| Behavior | Description |
|----------|-------------|
| Accepts `Vec<Output>` | A batch of outputs from the processor. |
| Returns `BatchResult` | Reports which event IDs were delivered, which are pending (enqueued but unacked), and which failed. |
| Delivery behavior | Depends on the configured `DeliveryStrategy` (see [Section 8](#8-delivery-strategy-integration)). |
| Error | `Err(AeonError)` for catastrophic failures (connection lost). Per-event failures go in `BatchResult::failed`. |

### Method Contract: `flush()`

| Behavior | Description |
|----------|-------------|
| Ensures delivery | For `UnorderedBatch` mode, collects all pending acks. For `PerEvent` and `OrderedBatch`, typically a no-op. |
| Called periodically | The engine calls `flush()` at configurable intervals and at shutdown. |

### BatchResult

`BatchResult` connects sinks to the delivery ledger:

```rust
pub struct BatchResult {
    /// Event IDs successfully delivered and acked.
    pub delivered: Vec<Uuid>,
    /// Event IDs enqueued but not yet acked (UnorderedBatch mode).
    pub pending: Vec<Uuid>,
    /// Event IDs that failed, with the error.
    pub failed: Vec<(Uuid, AeonError)>,
}
```

Helper constructors:
- `BatchResult::all_delivered(ids)` -- all events confirmed.
- `BatchResult::all_pending(ids)` -- all events enqueued, acks deferred to `flush()`.
- `BatchResult::empty()` -- no events processed.

The event IDs come from `Output::source_event_id`. Extract them before writing:

```rust
let event_ids: Vec<_> = outputs
    .iter()
    .filter_map(|o| o.source_event_id)
    .collect();
```

### Output Structure

```rust
pub struct Output {
    pub destination: Arc<str>,                              // Sink/topic name
    pub key: Option<Bytes>,                                 // Partition routing key
    pub payload: Bytes,                                     // Zero-copy payload
    pub headers: SmallVec<[(Arc<str>, Arc<str>); 4]>,      // Inline headers
    pub source_ts: Option<Instant>,                         // Latency tracking
    pub source_event_id: Option<uuid::Uuid>,                // Traceability
    pub source_partition: Option<PartitionId>,               // Checkpoint
    pub source_offset: Option<i64>,                         // Checkpoint
}
```

---

## 4. Extended Traits

### Seekable (Replayable Sources)

For sources that can rewind to a previous offset (Kafka, file, Redis Streams):

```rust
pub trait Seekable: Source {
    fn seek(
        &mut self,
        offset: u64,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;
}
```

Implement `Seekable` when the external system supports offset-based replay.
The engine calls `seek()` during crash recovery to resume from the last
checkpointed offset.

### IdempotentSink (Exactly-Once Dedup)

For sinks that can deduplicate by event ID:

```rust
pub trait IdempotentSink: Sink {
    fn has_seen(
        &self,
        event_id: &uuid::Uuid,
    ) -> impl std::future::Future<Output = Result<bool, AeonError>> + Send;
}
```

Implement `IdempotentSink` when the downstream can check whether an event has
already been delivered (e.g., database UPSERT with UUIDv7 as primary key,
Redis `SETNX`). The engine uses this for exactly-once delivery semantics.

---

## 5. Step-by-Step: Building a Custom Source

This section walks through building a **TimerSource** that generates events at
a fixed interval -- a useful pattern for heartbeats, synthetic load generation,
and testing.

### Step 1: Create a Config Struct

Use the builder pattern with `with_*` methods. Provide sensible defaults.

```rust
use aeon_types::PartitionId;
use std::sync::Arc;
use std::time::Duration;

/// Configuration for `TimerSource`.
pub struct TimerSourceConfig {
    /// Interval between generated events.
    pub interval: Duration,
    /// Number of events per batch.
    pub batch_size: usize,
    /// Source identifier (interned).
    pub source_name: Arc<str>,
    /// Partition for generated events.
    pub partition: PartitionId,
    /// Optional limit on total events (None = infinite).
    pub max_events: Option<u64>,
}

impl TimerSourceConfig {
    /// Create a config with the given interval.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            batch_size: 1,
            source_name: Arc::from("timer"),
            partition: PartitionId::new(0),
            max_events: None,
        }
    }

    /// Set events per batch.
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set a maximum event count (source exhausts after this many).
    pub fn with_max_events(mut self, max: u64) -> Self {
        self.max_events = Some(max);
        self
    }

    /// Set the partition ID.
    pub fn with_partition(mut self, partition: PartitionId) -> Self {
        self.partition = partition;
        self
    }
}
```

### Step 2: Implement the Source Trait

```rust
use aeon_types::{AeonError, Event, Source};
use bytes::Bytes;

/// A source that generates events at a fixed interval.
pub struct TimerSource {
    config: TimerSourceConfig,
    emitted: u64,
}

impl TimerSource {
    pub fn new(config: TimerSourceConfig) -> Self {
        Self { config, emitted: 0 }
    }

    /// Number of events emitted so far.
    pub fn emitted(&self) -> u64 {
        self.emitted
    }
}

impl Source for TimerSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // Check if we have reached the max event limit.
        if let Some(max) = self.config.max_events {
            if self.emitted >= max {
                return Ok(Vec::new()); // Exhausted
            }
        }

        // Wait for the configured interval.
        tokio::time::sleep(self.config.interval).await;

        let now = std::time::Instant::now();
        let remaining = self.config.max_events
            .map(|max| (max - self.emitted) as usize)
            .unwrap_or(self.config.batch_size);
        let count = self.config.batch_size.min(remaining);

        let mut events = Vec::with_capacity(count);
        for _ in 0..count {
            let payload = Bytes::from(format!("tick-{}", self.emitted));
            let event = Event::new(
                uuid::Uuid::now_v7(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
                Arc::clone(&self.config.source_name),
                self.config.partition,
                payload,
            )
            .with_source_ts(now);

            events.push(event);
            self.emitted += 1;
        }

        Ok(events)
    }
}
```

### Key Points

- **End-of-stream**: Return `Ok(Vec::new())` when exhausted. The engine
  treats empty vecs as "no data right now" and keeps polling.
- **Error handling**: Map all errors through `AeonError`. Use
  `AeonError::connection()` for I/O errors, `AeonError::timeout()` for
  timeouts.
- **Source name**: Clone `Arc<str>` by reference (`Arc::clone(&self.config.source_name)`),
  not by value. This is a pointer copy, not a string copy.
- **UUIDv7**: Use `uuid::Uuid::now_v7()` for time-ordered, unique event IDs.
  For high-throughput sources, use `CoreLocalUuidGenerator` for ~1-2ns per UUID.
- **source_ts**: Set `Instant::now()` at batch start for latency tracking.
- **source_offset**: Set when the external system provides a resumable offset
  (Kafka offset, file line number, Redis stream ID).

---

## 6. Step-by-Step: Building a Custom Sink

This section builds a **CounterSink** that counts received outputs and tracks
delivery status per delivery strategy.

### Step 1: Create a SinkConfig Struct

```rust
use aeon_types::DeliveryStrategy;

/// Configuration for `CounterSink`.
pub struct CounterSinkConfig {
    /// Delivery strategy -- controls ack behavior.
    pub strategy: DeliveryStrategy,
}

impl CounterSinkConfig {
    pub fn new() -> Self {
        Self {
            strategy: DeliveryStrategy::default(), // OrderedBatch
        }
    }

    /// Set the delivery strategy.
    pub fn with_strategy(mut self, strategy: DeliveryStrategy) -> Self {
        self.strategy = strategy;
        self
    }
}

impl Default for CounterSinkConfig {
    fn default() -> Self {
        Self::new()
    }
}
```

### Step 2: Implement the Sink Trait

```rust
use aeon_types::{AeonError, BatchResult, Output, Sink};

/// A sink that counts outputs without writing them anywhere.
///
/// Useful for testing pipeline throughput and verifying delivery semantics.
pub struct CounterSink {
    config: CounterSinkConfig,
    delivered: u64,
    pending_ids: Vec<uuid::Uuid>,
}

impl CounterSink {
    pub fn new(config: CounterSinkConfig) -> Self {
        Self {
            config,
            delivered: 0,
            pending_ids: Vec::new(),
        }
    }

    /// Total outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

impl Sink for CounterSink {
    async fn write_batch(
        &mut self,
        outputs: Vec<Output>,
    ) -> Result<BatchResult, AeonError> {
        let event_ids: Vec<_> = outputs
            .iter()
            .filter_map(|o| o.source_event_id)
            .collect();

        let count = outputs.len() as u64;

        match self.config.strategy {
            DeliveryStrategy::PerEvent => {
                // Process one at a time, all immediately "delivered."
                self.delivered += count;
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::OrderedBatch => {
                // Process the whole batch, all immediately "delivered."
                self.delivered += count;
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::UnorderedBatch => {
                // Enqueue -- mark as pending. Resolve in flush().
                self.pending_ids.extend(event_ids.iter().copied());
                self.delivered += count;
                Ok(BatchResult::all_pending(event_ids))
            }
        }
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // In a real sink, this would collect pending ack futures.
        // For CounterSink, just clear the pending list.
        self.pending_ids.clear();
        Ok(())
    }
}
```

### Key Points

- **Extract event IDs first**: Before any processing, collect
  `source_event_id` from each output for the `BatchResult`.
- **Match on DeliveryStrategy**: The sink must behave differently depending
  on the strategy. See [Section 8](#8-delivery-strategy-integration).
- **flush()**: For blocking strategies (`PerEvent`, `OrderedBatch`), `flush()`
  is typically a no-op since acks are already resolved in `write_batch()`. For
  `UnorderedBatch`, `flush()` resolves all pending deliveries.
- **Never panic**: Use `map_err(|e| AeonError::connection(...))` for I/O errors.

---

## 7. Push-Based Sources

Some external systems push data to Aeon rather than being polled (WebSocket,
MQTT, RabbitMQ, webhook endpoints). These require a background task that
receives data and a bounded buffer that `next_batch()` drains.

### The PushBuffer Pattern

Aeon provides a `PushBuffer` abstraction in
`crates/aeon-connectors/src/push_buffer.rs` with three-phase backpressure:

1. **Phase 1 -- In-memory**: Bounded `tokio::sync::mpsc` channel. Fast path.
2. **Phase 2 -- Spill pressure**: When the channel is full, a spill counter
   increments and the sender blocks until space is available.
3. **Phase 3 -- Protocol flow control**: When spill count exceeds a threshold,
   `is_overloaded()` returns `true`. The connector should apply protocol-level
   backpressure (stop reading from socket, pause MQTT subscription, etc.).

### Using PushBuffer

```rust
use crate::push_buffer::{push_buffer, PushBufferConfig, PushBufferTx, PushBufferRx};
use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;

pub struct MyPushSourceConfig {
    pub address: String,
    pub source_name: Arc<str>,
    pub buffer_config: PushBufferConfig,
}

pub struct MyPushSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    // The background task handle (kept alive by the source).
    _task: tokio::task::JoinHandle<()>,
}

impl MyPushSource {
    pub async fn new(config: MyPushSourceConfig) -> Result<Self, AeonError> {
        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = Arc::clone(&config.source_name);

        // Spawn a background task that receives from the external system
        // and pushes events into the buffer.
        let task = tokio::spawn(async move {
            // Connect to the external system here.
            // In a loop, receive data and push to the buffer:
            loop {
                // let raw_data = external_system.recv().await;
                let raw_data = Bytes::from_static(b"example");

                let event = Event::new(
                    uuid::Uuid::now_v7(),
                    0,
                    Arc::clone(&source_name),
                    PartitionId::new(0),
                    raw_data,
                );

                // PushBuffer handles backpressure automatically.
                // If the channel is full, this blocks until space is available.
                if tx.send(event).await.is_err() {
                    break; // Channel closed, source dropped.
                }

                // Apply protocol-level backpressure when overloaded.
                if tx.is_overloaded() {
                    // Stop reading from external system until buffer drains.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        });

        Ok(Self {
            rx,
            poll_timeout: Duration::from_secs(1),
            _task: task,
        })
    }
}

impl Source for MyPushSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // PushBufferRx handles batch draining with timeout.
        self.rx.next_batch(self.poll_timeout).await
    }
}
```

### When to Use PushBuffer

| Source type | Model | Example |
|-------------|-------|---------|
| Kafka/Redpanda | Pull | `consumer.recv()` inside `next_batch()` |
| File | Pull | `reader.read_line()` inside `next_batch()` |
| HTTP Polling | Pull | `client.get()` inside `next_batch()` |
| WebSocket | Push | Background task + PushBuffer |
| MQTT | Push | Background task + PushBuffer |
| RabbitMQ | Push | Background task + PushBuffer |
| HTTP Webhook | Push | Axum handler + PushBuffer |

---

## 8. Delivery Strategy Integration

Every sink should respect the `DeliveryStrategy` configured for the pipeline.
The strategy controls how `write_batch()` and `flush()` behave.

### Three Strategies

```rust
pub enum DeliveryStrategy {
    PerEvent,       // Send + await each event individually
    OrderedBatch,   // Send all in order, await all at batch end (DEFAULT)
    UnorderedBatch, // Enqueue all, return immediately, await in flush()
}
```

### How Sinks Should Behave

#### PerEvent

```rust
DeliveryStrategy::PerEvent => {
    for output in &outputs {
        // Send to external system and wait for ack.
        external_system.send(output).await?;
    }
    // All delivered.
    Ok(BatchResult::all_delivered(event_ids))
}
```

- Strictest guarantee. Lowest throughput.
- Every event is confirmed before moving to the next.
- `flush()` is a no-op.

#### OrderedBatch (Default)

```rust
DeliveryStrategy::OrderedBatch => {
    let mut futures = Vec::with_capacity(outputs.len());
    for output in &outputs {
        // Send to external system (non-blocking enqueue).
        futures.push(external_system.send(output));
    }
    // Await all at batch boundary.
    for future in futures {
        future.await?;
    }
    Ok(BatchResult::all_delivered(event_ids))
}
```

- Ordering preserved within and across batches.
- Uses the downstream's native ordering mechanism (Kafka idempotent producer,
  database transactions, Redis `MULTI`/`EXEC`).
- `flush()` is a no-op (acks collected in `write_batch()`).

#### UnorderedBatch

```rust
DeliveryStrategy::UnorderedBatch => {
    for output in &outputs {
        // Enqueue into the external system's internal buffer.
        external_system.enqueue(output)?;
    }
    // Return immediately -- acks are pending.
    Ok(BatchResult::all_pending(event_ids))
}
```

- Highest throughput. No ordering guarantee.
- `flush()` must collect all pending acks:

```rust
async fn flush(&mut self) -> Result<(), AeonError> {
    // Wait for all enqueued messages to be delivered.
    self.external_system.flush(self.flush_timeout).await
        .map_err(|e| AeonError::connection(format!("flush failed: {e}")))?;
    Ok(())
}
```

### Reference Implementations

See these files for real delivery strategy implementations:

- `crates/aeon-connectors/src/file/sink.rs` -- File-based (fsync per event
  vs. fsync per batch vs. deferred fsync).
- `crates/aeon-connectors/src/kafka/sink.rs` -- Kafka/Redpanda (idempotent
  producer, batch produce futures, `producer.flush()`).
- `crates/aeon-connectors/src/nats/sink.rs` -- NATS JetStream (per-event
  publish+ack, batch ack collection, deferred ack futures).

---

## 9. Feature Gating

Every connector must be behind a Cargo feature flag. This keeps compile times
fast and binary sizes small.

### Step 1: Add Feature to Cargo.toml

In `crates/aeon-connectors/Cargo.toml`:

```toml
[dependencies]
# Add your optional dependency
my-client-lib = { version = "1.0", optional = true }

[features]
# Add your feature, pulling in the dependency and tokio if async
my-connector = ["dep:my-client-lib", "dep:tokio"]
```

### Step 2: Gate the Module in lib.rs

In `crates/aeon-connectors/src/lib.rs`:

```rust
#[cfg(feature = "my-connector")]
pub mod my_connector;

// Re-export at crate root
#[cfg(feature = "my-connector")]
pub use my_connector::{MySource, MySink};
```

### Step 3: Gate the Module Contents

In your source file, no additional gating is needed -- the entire module is
already behind the feature flag from `lib.rs`.

### Step 4: Add to Push Buffer Gate (If Push-Based)

If your source is push-based and uses `PushBuffer`, add your feature to the
`push_buffer` module gate in `lib.rs`:

```rust
#[cfg(any(
    feature = "websocket",
    feature = "http",
    feature = "mqtt",
    // ... existing features ...
    feature = "my-connector",  // Add here
))]
pub mod push_buffer;
```

### Existing Feature Flags

| Feature | Dependencies | Description |
|---------|-------------|-------------|
| `memory` | (none) | In-memory source/sink for testing |
| `blackhole` | (none) | Drop-all sink for benchmarking |
| `stdout` | (none) | Debug print sink |
| `kafka` | rdkafka, tokio | Kafka/Redpanda |
| `file` | tokio | File source/sink |
| `nats` | async-nats, futures-util, tokio | NATS JetStream |
| `mqtt` | rumqttc, tokio | MQTT v5 |
| `redis-streams` | redis, tokio | Redis Streams |
| `websocket` | tokio-tungstenite, futures-util, tokio | WebSocket |
| `http` | axum, reqwest, tokio | HTTP webhook + polling |
| `rabbitmq` | lapin, futures-util, tokio | RabbitMQ/AMQP |
| `quic` | quinn, rustls, rcgen, tokio, futures-util | Raw QUIC |
| `webtransport` | wtransport, tokio, futures-util | WebTransport |
| `postgres-cdc` | tokio-postgres, tokio, serde, serde_json | PostgreSQL CDC |
| `mysql-cdc` | mysql_async, tokio, serde, serde_json | MySQL CDC |
| `mongodb-cdc` | mongodb, tokio, futures-util, serde, serde_json | MongoDB CDC |

---

## 10. Testing

### Unit Testing with MemorySource and MemorySink

The `memory` connector is the standard test fixture. Use `MemorySource` to feed
events and `MemorySink` to capture outputs.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{Event, Output, PartitionId, Sink, Source};
    use bytes::Bytes;
    use std::sync::Arc;

    /// Helper to create test events.
    fn make_events(count: usize) -> Vec<Event> {
        (0..count)
            .map(|i| {
                Event::new(
                    uuid::Uuid::now_v7(),
                    i as i64,
                    Arc::from("test"),
                    PartitionId::new(0),
                    Bytes::from(format!("event-{i}")),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn source_yields_all_events() {
        let events = make_events(10);
        let mut source = MySource::new(MySourceConfig::new(/* ... */));

        let mut total = Vec::new();
        loop {
            let batch = source.next_batch().await.unwrap();
            if batch.is_empty() {
                break;
            }
            total.extend(batch);
        }
        assert_eq!(total.len(), 10);
    }

    #[tokio::test]
    async fn sink_delivers_all_outputs() {
        let mut sink = MySink::new(MySinkConfig::new(/* ... */));
        let outputs = vec![
            Output::new(Arc::from("dest"), Bytes::from_static(b"a")),
            Output::new(Arc::from("dest"), Bytes::from_static(b"b")),
        ];

        let result = sink.write_batch(outputs).await.unwrap();
        assert!(result.all_succeeded());
        sink.flush().await.unwrap();
    }
}
```

### Testing Batch Semantics

Verify that your source respects batch sizes:

```rust
#[tokio::test]
async fn respects_batch_size() {
    let mut source = MySource::new(
        MySourceConfig::new(/* ... */).with_batch_size(4),
    );

    let batch = source.next_batch().await.unwrap();
    assert!(batch.len() <= 4);
}
```

### Testing Delivery Strategies

Test each strategy separately:

```rust
#[tokio::test]
async fn ordered_batch_delivers_all() {
    let config = MySinkConfig::new()
        .with_strategy(DeliveryStrategy::OrderedBatch);
    let mut sink = MySink::new(config);

    let result = sink.write_batch(outputs).await.unwrap();
    assert!(result.all_succeeded());
    assert!(result.pending.is_empty());
}

#[tokio::test]
async fn unordered_batch_returns_pending() {
    let config = MySinkConfig::new()
        .with_strategy(DeliveryStrategy::UnorderedBatch);
    let mut sink = MySink::new(config);

    let result = sink.write_batch(outputs).await.unwrap();
    assert!(!result.pending.is_empty());

    // After flush, pending should resolve.
    sink.flush().await.unwrap();
}
```

### Testing Error Handling

```rust
#[tokio::test]
async fn handles_connection_error() {
    let mut source = MySource::new(
        MySourceConfig::new("invalid://address"),
    );

    let result = source.next_batch().await;
    assert!(result.is_err());

    // Verify it is a retryable connection error.
    if let Err(e) = result {
        assert!(e.is_retryable());
    }
}
```

### Integration Testing

For connectors that talk to external systems, use integration tests in
`crates/aeon-connectors/tests/` or `crates/aeon-engine/tests/`. Gate them
behind feature flags and use `#[ignore]` for tests requiring infrastructure:

```rust
#[tokio::test]
#[ignore] // Requires running instance of the external system
async fn roundtrip_through_external_system() {
    let source_config = MySourceConfig::new("localhost:9999");
    let sink_config = MySinkConfig::new("localhost:9999");

    let mut sink = MySink::new(sink_config).await.unwrap();
    let mut source = MySource::new(source_config).await.unwrap();

    // Write via sink
    let outputs = vec![
        Output::new(Arc::from("dest"), Bytes::from_static(b"roundtrip")),
    ];
    sink.write_batch(outputs).await.unwrap();
    sink.flush().await.unwrap();

    // Read via source
    let events = source.next_batch().await.unwrap();
    assert!(!events.is_empty());
    assert_eq!(events[0].payload.as_ref(), b"roundtrip");
}
```

### Running Tests

```bash
# Unit tests for a specific crate
cargo test -p aeon-connectors

# With a specific feature
cargo test -p aeon-connectors --features kafka

# All workspace tests
cargo test --workspace

# Including ignored integration tests
cargo test --workspace -- --ignored
```

---

## 11. Registration

To wire a connector into the pipeline engine, follow these steps.

### Module File Structure

Place your connector under `crates/aeon-connectors/src/`:

```
crates/aeon-connectors/src/
    my_connector/
        mod.rs          # Re-exports
        source.rs       # MySource implementation
        sink.rs         # MySink implementation
```

Or for simple connectors, a single file:

```
crates/aeon-connectors/src/
    my_connector.rs     # Both source and sink in one file
```

### Re-export from mod.rs

```rust
// crates/aeon-connectors/src/my_connector/mod.rs
mod source;
mod sink;

pub use source::{MySource, MySourceConfig};
pub use sink::{MySink, MySinkConfig};
```

### Gate and Re-export from lib.rs

As described in [Section 9](#9-feature-gating), add the cfg-gated module and
re-export to `lib.rs`.

### Pipeline Integration

The pipeline engine uses generic type parameters for Source and Sink. Your
connector is usable anywhere a `Source` or `Sink` trait bound is required:

```rust
use aeon_connectors::my_connector::{MySource, MySourceConfig, MySink, MySinkConfig};
use aeon_types::{Source, Sink};

let mut source = MySource::new(MySourceConfig::new(/* ... */));
let mut sink = MySink::new(MySinkConfig::new(/* ... */));

// Pipeline engine accepts any impl Source / impl Sink
// engine.run(source, processor, sink).await?;
```

Because Aeon uses **static dispatch** on the hot path (generics, not
`dyn Trait`), your connector gets monomorphized at compile time with zero
virtual dispatch overhead.

---

## 12. Best Practices

### Zero-Copy

- Use `Bytes` for payloads. Never `Vec<u8>` on the hot path.
- Use `Bytes::copy_from_slice()` when the upstream library owns the buffer
  (rdkafka, async-nats). This copies once into a reference-counted buffer
  that is then zero-copy for all downstream consumers.
- Use `Bytes::from(vec)` when you already own the `Vec<u8>`.
- Use `Bytes::from_static(b"literal")` for compile-time constant payloads.

### Arc\<str\> for String Identifiers

- Source names, destination names, and metadata keys/values must use `Arc<str>`.
- Intern strings once at initialization, then clone by pointer:

```rust
// At init:
let source_name: Arc<str> = Arc::from("my-source");

// In next_batch() -- pointer copy, not string copy:
let event_source = Arc::clone(&self.config.source_name);
```

### Batch-First

- `next_batch()` returns `Vec<Event>`, not a single event.
- `write_batch()` accepts `Vec<Output>`, not a single output.
- Pre-allocate vectors with `Vec::with_capacity(batch_size)`.

### No Panics

```rust
// FORBIDDEN:
.unwrap()
.expect("should never fail")
panic!("unexpected state")

// REQUIRED:
.map_err(|e| AeonError::connection(format!("my connector: {e}")))?
.ok_or_else(|| AeonError::state("reader not initialized"))?
```

### Error Mapping

Use the appropriate `AeonError` variant:

| Error type | When to use |
|------------|-------------|
| `AeonError::connection(msg)` | Network/I/O errors (retryable by default) |
| `AeonError::serialization(msg)` | Malformed data, encoding errors |
| `AeonError::config(msg)` | Invalid configuration |
| `AeonError::timeout(msg)` | Operation timed out (retryable) |
| `AeonError::state(msg)` | Internal state error (not initialized, etc.) |
| `AeonError::not_found(msg)` | Resource not found |

### Lazy Initialization

Open connections and files lazily on first use, not in the constructor. This
lets the pipeline configure connectors before the I/O event loop starts.

```rust
async fn ensure_open(&mut self) -> Result<(), AeonError> {
    if self.connection.is_none() {
        let conn = connect(&self.config.address).await
            .map_err(|e| AeonError::connection(format!("connect failed: {e}")))?;
        self.connection = Some(conn);
    }
    Ok(())
}
```

### Logging

Use `tracing` for structured logging. Log at `info` for lifecycle events
(connect, disconnect) and `debug`/`trace` for per-batch details:

```rust
tracing::info!(address = %self.config.address, "MySource connected");
tracing::debug!(batch_size = events.len(), "next_batch produced");
```

### Consistent Config Pattern

Follow the established convention: a `Config` struct with `new()` providing
defaults and `with_*` builder methods for each option. Store the `DeliveryStrategy`
in sink configs. Store batch size, source name, and partition in source configs.

---

## Quick Reference

### Minimal Source

```rust
use aeon_types::{AeonError, Event, Source};

pub struct MySource { /* fields */ }

impl Source for MySource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // Return events or empty vec.
        Ok(Vec::new())
    }
}
```

### Minimal Sink

```rust
use aeon_types::{AeonError, BatchResult, Output, Sink};

pub struct MySink { /* fields */ }

impl Sink for MySink {
    async fn write_batch(
        &mut self,
        outputs: Vec<Output>,
    ) -> Result<BatchResult, AeonError> {
        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
        // Deliver outputs here.
        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        Ok(())
    }
}
```

### Files to Reference

| File | What to learn |
|------|---------------|
| `crates/aeon-types/src/traits.rs` | Trait definitions |
| `crates/aeon-types/src/event.rs` | Event and Output structs |
| `crates/aeon-types/src/delivery.rs` | DeliveryStrategy, BatchResult |
| `crates/aeon-types/src/error.rs` | AeonError variants |
| `crates/aeon-connectors/src/memory/source.rs` | Simplest Source |
| `crates/aeon-connectors/src/memory/sink.rs` | Simplest Sink |
| `crates/aeon-connectors/src/blackhole/mod.rs` | Simplest Sink (discard) |
| `crates/aeon-connectors/src/file/sink.rs` | Delivery strategy example |
| `crates/aeon-connectors/src/kafka/source.rs` | Complex pull-based Source |
| `crates/aeon-connectors/src/kafka/sink.rs` | Complex strategy-aware Sink |
| `crates/aeon-connectors/src/push_buffer.rs` | Push-source backpressure |
| `crates/aeon-connectors/src/lib.rs` | Feature gating pattern |
| `crates/aeon-connectors/Cargo.toml` | Feature flag definitions |
