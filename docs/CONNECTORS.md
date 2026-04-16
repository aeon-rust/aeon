# Aeon Connectors Reference

> Comprehensive guide to all Source and Sink connectors in the `aeon-connectors` crate.
>
> **Implementation status**: Kafka/Redpanda, Memory, Blackhole, Stdout, File, HTTP,
> and WebSocket connectors are implemented and E2E tested. NATS, Redis Streams, MQTT,
> and RabbitMQ connectors are implemented with infrastructure deployed on K3s for
> testing (E2E tests pending). QUIC and WebTransport source/sink connectors exist as
> code. CDC connectors (PostgreSQL, MySQL, MongoDB) are stubs for post-Gate 2.

---

## Table of Contents

1. [Overview](#overview)
2. [Connector Matrix](#connector-matrix)
3. [Streaming Connectors](#streaming-connectors)
4. [Advanced Connectors](#advanced-connectors)
5. [Feature Flags](#feature-flags)
6. [Configuration — Docker Compose Services](#configuration--docker-compose-services)
7. [EO-2 Durability — Migration Note](#eo-2-durability--migration-note)

---

## Overview

Aeon's connector architecture is built on two core traits defined in `aeon-types`:

```rust
pub trait Source: Send + Sync {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError>;
}

pub trait Sink: Send + Sync {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError>;
    async fn flush(&mut self) -> Result<(), AeonError>;
}
```

**Design principles:**

- **Batch-first APIs.** `Source::next_batch()` returns `Vec<Event>`, `Sink::write_batch()` accepts `Vec<Output>`. No per-event channel overhead.
- **Feature-gated.** Every connector is behind a Cargo feature flag. Only `memory`, `blackhole`, and `stdout` are enabled by default.
- **Three source kinds.** Every source is classified by its `SourceKind`, which determines backpressure behavior under EO-2 capacity limits:
  - **Pull** — issues fetch/read calls inside `next_batch()`. Backpressure pauses polling. (Kafka, File, Redis Streams, NATS)
  - **Push** — runs a background receive task that feeds a `PushBuffer`; `next_batch()` drains the buffer. Backpressure rejects new messages at the protocol level. (WebSocket, HTTP Webhook, MQTT, RabbitMQ, QUIC, WebTransport, MongoDB CDC)
  - **Poll** — periodically fetches from an external system. Backpressure skips the next poll cycle. (HTTP Polling, PostgreSQL CDC, MySQL CDC)
- **Three-phase backpressure** for push sources: (1) in-memory bounded channel, (2) spill counter when channel is full, (3) protocol-level flow control signal (`is_overloaded()`) that tells the connector to pause reading.
- **Delivery strategies** for sinks that support them: `PerEvent`, `OrderedBatch` (default), `UnorderedBatch`. Controls how and when write acknowledgments are awaited.

---

## Connector Matrix

| Connector | Source | Sink | Source Kind | Feature Flag | Crate Dependency | Status |
|-----------|--------|------|------------|-------------|------------------|--------|
| Memory | `MemorySource` | `MemorySink` | Pull | `memory` (default) | -- | Stable |
| Blackhole | -- | `BlackholeSink` | -- | `blackhole` (default) | -- | Stable |
| Stdout | -- | `StdoutSink` | -- | `stdout` (default) | -- | Stable |
| Kafka / Redpanda | `KafkaSource` | `KafkaSink` | Pull | `kafka` | `rdkafka` | Stable |
| File | `FileSource` | `FileSink` | Pull | `file` | `tokio` | Stable |
| HTTP Polling | `HttpPollingSource` | -- | Poll | `http` | `reqwest` | Phase 11a |
| HTTP Webhook | `HttpWebhookSource` | -- | Push | `http` | `axum` | Phase 11a |
| WebSocket | `WebSocketSource` | `WebSocketSink` | Push | `websocket` | `tokio-tungstenite` | Phase 11a |
| Redis Streams | `RedisStreamsSource` | `RedisStreamsSink` | Pull | `redis-streams` | `redis` | Phase 11a |
| NATS JetStream | `NatsSource` | `NatsSink` | Pull | `nats` | `async-nats` | Phase 11a |
| MQTT | `MqttSource` | `MqttSink` | Push | `mqtt` | `rumqttc` | Phase 11a |
| RabbitMQ | `RabbitMqSource` | `RabbitMqSink` | Push | `rabbitmq` | `lapin` | Phase 11a |
| QUIC | `QuicSource` | `QuicSink` | Push | `quic` | `quinn`, `rustls` | Phase 11b |
| WebTransport (Streams) | `WebTransportSource` | `WebTransportSink` | Push | `webtransport` | `wtransport` | Phase 11b |
| WebTransport (Datagrams) | `WebTransportDatagramSource` | -- | Push | `webtransport` | `wtransport` | Phase 11b |
| PostgreSQL CDC | `PostgresCdcSource` | -- | Poll | `postgres-cdc` | `tokio-postgres` | Phase 11b |
| MySQL CDC | `MysqlCdcSource` | -- | Poll | `mysql-cdc` | `mysql_async` | Phase 11b |
| MongoDB CDC | `MongoDbCdcSource` | -- | Push | `mongodb-cdc` | `mongodb` | Phase 11b |

---

## Streaming Connectors

### Kafka / Redpanda

**Feature flag:** `kafka`

The primary production connector. Uses `rdkafka` (librdkafka) with manual partition assignment -- no consumer group rebalancing. Aeon manages partition ownership directly.

#### KafkaSource

Pull source. Uses `StreamConsumer` for async tokio integration. Batch polling: waits for the first message (up to `poll_timeout`), then drains available messages within `drain_timeout` up to `batch_max_messages`.

```rust
use aeon_connectors::kafka::source::{KafkaSource, KafkaSourceConfig};

let config = KafkaSourceConfig::new("localhost:19092", "my-topic")
    .with_partitions(vec![0, 1, 2, 3])
    .with_batch_max(2048)
    .with_poll_timeout(Duration::from_secs(1))
    .with_drain_timeout(Duration::from_millis(10))
    .with_group_id("aeon-manual")
    .with_max_empty_polls(10)
    .with_config("fetch.min.bytes", "1024");

let mut source = KafkaSource::new(config)?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `brokers` | `String` | -- | Broker addresses (e.g., `"localhost:19092"`) |
| `topic` | `String` | -- | Topic to consume from |
| `partitions` | `Vec<i32>` | `[0]` | Partitions to manually assign |
| `batch_max_messages` | `usize` | `1024` | Max messages per `next_batch()` |
| `poll_timeout` | `Duration` | `1s` | Timeout for first message |
| `drain_timeout` | `Duration` | `10ms` | Timeout for draining additional messages |
| `source_name` | `Arc<str>` | `"kafka"` | Source identifier in events |
| `group_id` | `String` | `"aeon-manual"` | Consumer group ID (required by rdkafka) |
| `max_empty_polls` | `u32` | `10` | Consecutive empty polls before exhaustion signal |
| `config_overrides` | `Vec<(String, String)>` | `[]` | Additional rdkafka config |

#### KafkaSink

Uses `FutureProducer` with three delivery strategies:

- **PerEvent**: Each output enqueued and delivery future awaited individually.
- **OrderedBatch** (default): All outputs enqueued in order, all delivery futures awaited together. Idempotent producer enabled for ordering guarantee.
- **UnorderedBatch**: Outputs enqueued into rdkafka internal buffer; `flush()` awaits pending deliveries. Highest throughput, downstream sorts by UUIDv7.

```rust
use aeon_connectors::kafka::sink::{KafkaSink, KafkaSinkConfig};
use aeon_types::DeliveryStrategy;

let config = KafkaSinkConfig::new("localhost:19092", "output-topic")
    .with_strategy(DeliveryStrategy::OrderedBatch)
    .with_produce_timeout(Duration::from_secs(5))
    .with_flush_timeout(Duration::from_secs(30))
    .with_config("batch.num.messages", "10000");

let sink = KafkaSink::new(config)?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `brokers` | `String` | -- | Broker addresses |
| `default_topic` | `String` | -- | Default destination topic |
| `produce_timeout` | `Duration` | `5s` | Per-message produce timeout |
| `flush_timeout` | `Duration` | `30s` | How long `flush()` waits for pending deliveries |
| `strategy` | `DeliveryStrategy` | `OrderedBatch` | Delivery strategy |
| `config_overrides` | `Vec<(String, String)>` | `[]` | Additional rdkafka config |

**Key rdkafka defaults set by Aeon:** `queue.buffering.max.messages=100000`, `queue.buffering.max.kbytes=1048576` (1GB), `batch.num.messages=10000`, `linger.ms=5`. Idempotent producer is enabled when strategy preserves order.

---

### Memory

**Feature flag:** `memory` (default)

In-memory connectors for testing and benchmarking. No external dependencies.

#### MemorySource

Serves events from a pre-loaded `Vec<Event>` in configurable batch sizes. Returns empty batches once exhausted. Supports `reset()` for repeated benchmark runs.

```rust
use aeon_connectors::MemorySource;

let source = MemorySource::new(events, 1024); // batch_size = 1024
```

#### MemorySink

Collects all outputs into a `Vec<Output>` for assertion. Inspect via `outputs()` or consume via `take_outputs()`.

```rust
use aeon_connectors::MemorySink;

let mut sink = MemorySink::new();
// ... run pipeline ...
assert_eq!(sink.len(), expected_count);
let results = sink.take_outputs();
```

---

### Blackhole

**Feature flag:** `blackhole` (default)

Discards all outputs. Tracks count atomically. Used for benchmarking Aeon's internal processing ceiling without external I/O overhead.

```rust
use aeon_connectors::BlackholeSink;

let sink = BlackholeSink::new();
// ... run pipeline ...
println!("Processed: {}", sink.count());
```

---

### Stdout

**Feature flag:** `stdout` (default)

Prints each output as `[destination] payload` to stdout. For debugging pipelines. Not suitable for hot-path benchmarking.

```rust
use aeon_connectors::StdoutSink;

let sink = StdoutSink::new();
```

---

### File

**Feature flag:** `file`

Reads and writes newline-delimited records. Suitable for log files, JSONL, CSV, or any line-oriented format. Uses tokio async I/O.

#### FileSource

Pull source. Reads lines from a file, each line becomes an Event payload. Opens lazily on first `next_batch()`. Returns empty batches once the file is fully read.

```rust
use aeon_connectors::file::source::{FileSource, FileSourceConfig};

let config = FileSourceConfig::new("/path/to/input.jsonl")
    .with_batch_size(2048)
    .with_source_name("access-log");

let mut source = FileSource::new(config);
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | `PathBuf` | -- | Path to the file |
| `batch_size` | `usize` | `1024` | Max lines per batch |
| `source_name` | `Arc<str>` | `"file"` | Source identifier in events |
| `partition` | `PartitionId` | `0` | Partition ID for all events |

#### FileSink

Writes output payloads as newline-delimited records. Opens lazily. Supports append and truncate modes. Supports all three delivery strategies controlling flush behavior.

```rust
use aeon_connectors::file::sink::{FileSink, FileSinkConfig};
use aeon_types::DeliveryStrategy;

let config = FileSinkConfig::new("/path/to/output.jsonl")
    .with_append(true)
    .with_strategy(DeliveryStrategy::OrderedBatch);

let mut sink = FileSink::new(config);
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | `PathBuf` | -- | Output file path |
| `append` | `bool` | `false` | Append mode (true) vs truncate (false) |
| `strategy` | `DeliveryStrategy` | `OrderedBatch` | Controls flush behavior |

**Delivery strategy behavior:**

| Strategy | Behavior |
|----------|----------|
| `PerEvent` | Write + fsync per event (strict durability) |
| `OrderedBatch` | Write all in order, single fsync at batch end |
| `UnorderedBatch` | Write to BufWriter, defer fsync to `flush()` |

---

### HTTP

**Feature flag:** `http`

Two source types for HTTP-based ingestion. No HTTP sink is provided (use webhooks outbound via the processor).

#### HttpPollingSource

Pull source. Periodically issues HTTP GET requests and returns the response body as a single Event. Natural flow control via polling interval.

```rust
use aeon_connectors::http::polling_source::{HttpPollingSource, HttpPollingSourceConfig};

let config = HttpPollingSourceConfig::new("https://api.example.com/data")
    .with_interval(Duration::from_secs(10))
    .with_timeout(Duration::from_secs(30))
    .with_header("Authorization", "Bearer token123");

let mut source = HttpPollingSource::new(config)?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | `String` | -- | URL to poll |
| `interval` | `Duration` | `10s` | Polling interval |
| `timeout` | `Duration` | `30s` | Request timeout |
| `source_name` | `Arc<str>` | `"http-poll"` | Source identifier |
| `headers` | `Vec<(String, String)>` | `[]` | Headers to include in requests |

#### HttpWebhookSource

Push source. Runs an axum HTTP server. Each POST request body becomes an Event. Uses three-phase push buffer backpressure.

```rust
use aeon_connectors::http::webhook_source::{HttpWebhookSource, HttpWebhookSourceConfig};

let config = HttpWebhookSourceConfig::new("0.0.0.0:8088".parse().unwrap())
    .with_path("/webhook")
    .with_poll_timeout(Duration::from_secs(1))
    .with_channel_capacity(8192);

let mut source = HttpWebhookSource::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bind_addr` | `SocketAddr` | -- | Address to bind the HTTP server |
| `path` | `String` | `"/webhook"` | HTTP path for webhooks |
| `source_name` | `Arc<str>` | `"http-webhook"` | Source identifier |
| `buffer_config` | `PushBufferConfig` | default | Push buffer settings |
| `poll_timeout` | `Duration` | `1s` | Timeout for first event in `next_batch()` |

---

### WebSocket

**Feature flag:** `websocket`

Bidirectional WebSocket connectivity using `tokio-tungstenite`.

#### WebSocketSource

Push source. Spawns a background task that reads messages from a WebSocket connection and feeds them into the push buffer. `next_batch()` drains the buffer.

```rust
use aeon_connectors::websocket::source::{WebSocketSource, WebSocketSourceConfig};

let config = WebSocketSourceConfig::new("ws://localhost:8080/ws")
    .with_poll_timeout(Duration::from_secs(1))
    .with_channel_capacity(4096);

let mut source = WebSocketSource::new(config).await?;
```

#### WebSocketSink

Maintains a persistent WebSocket connection. Each output is sent as a binary message. Errors propagated to the engine for retry -- no automatic reconnection.

```rust
use aeon_connectors::websocket::sink::{WebSocketSink, WebSocketSinkConfig};

let config = WebSocketSinkConfig::new("ws://localhost:8080/ws");
let mut sink = WebSocketSink::new(config).await?;
```

---

### Redis Streams

**Feature flag:** `redis-streams`

Uses Redis Streams (XREADGROUP/XADD) for message passing.

#### RedisStreamsSource

Pull source. Issues XREADGROUP with COUNT and BLOCK for batch consumption. Uses consumer groups for reliable delivery with acknowledgment.

```rust
use aeon_connectors::redis_streams::source::{RedisSource, RedisSourceConfig};

let config = RedisSourceConfig::new(
    "redis://localhost:6379",
    "my-stream",
    "my-group",
    "consumer-1",
)
.with_batch_size(1024)
.with_block_ms(1000);

let mut source = RedisSource::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | `String` | -- | Redis connection URL |
| `stream_key` | `String` | -- | Redis Stream key |
| `group` | `String` | -- | Consumer group name |
| `consumer` | `String` | -- | Consumer name within the group |
| `batch_size` | `usize` | `1024` | Max messages per batch |
| `block_ms` | `usize` | `1000` | XREADGROUP block timeout (0 = no block) |
| `source_name` | `Arc<str>` | `"redis"` | Source identifier |

#### RedisStreamsSink

Uses XADD to append entries. Each output's payload is stored as the `"data"` field. Output headers become additional fields. Supports approximate max stream length via MAXLEN ~.

```rust
use aeon_connectors::redis_streams::sink::{RedisSink, RedisSinkConfig};

let config = RedisSinkConfig::new("redis://localhost:6379", "output-stream")
    .with_max_len(100_000);

let mut sink = RedisSink::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | `String` | -- | Redis connection URL |
| `stream_key` | `String` | -- | Redis Stream key to write to |
| `max_len` | `usize` | `0` (unbounded) | Approximate max stream length |

---

### NATS JetStream

**Feature flag:** `nats`

Durable messaging via NATS JetStream. Pull-based consumer for source, JetStream publish for sink.

#### NatsSource

Pull source. Uses JetStream pull-based consumer for durable, at-least-once delivery. Messages are acknowledged after being returned from `next_batch()`.

```rust
use aeon_connectors::nats::source::{NatsSource, NatsSourceConfig};

let config = NatsSourceConfig::new("nats://localhost:4222", "my-stream", "events.>")
    .with_consumer("aeon-consumer")
    .with_batch_size(1024);

let mut source = NatsSource::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | `String` | -- | NATS server URL |
| `stream` | `String` | -- | JetStream stream name |
| `consumer` | `String` | `"aeon"` | Durable consumer name |
| `subject` | `String` | -- | Subject filter |
| `batch_size` | `usize` | `1024` | Max messages per batch |
| `fetch_timeout` | `Duration` | `1s` | Fetch timeout |
| `source_name` | `Arc<str>` | `"nats"` | Source identifier |

#### NatsSink

Supports both core NATS (fire-and-forget) and JetStream (persistent) publishing. JetStream is the default. Supports all three delivery strategies controlling how acks are handled.

```rust
use aeon_connectors::nats::sink::{NatsSink, NatsSinkConfig};
use aeon_types::DeliveryStrategy;

let config = NatsSinkConfig::new("nats://localhost:4222", "events.processed")
    .with_strategy(DeliveryStrategy::OrderedBatch);

// Use core NATS instead of JetStream:
// let config = NatsSinkConfig::new(...).with_core_nats();

let mut sink = NatsSink::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | `String` | -- | NATS server URL |
| `subject` | `String` | -- | Subject to publish to |
| `jetstream` | `bool` | `true` | Use JetStream (persistent) or core NATS |
| `strategy` | `DeliveryStrategy` | `OrderedBatch` | Delivery strategy for JetStream acks |

**Delivery strategy behavior (JetStream mode):**

| Strategy | Behavior |
|----------|----------|
| `PerEvent` | Publish + await ack per message |
| `OrderedBatch` | Publish all in order, await all acks at batch end |
| `UnorderedBatch` | Publish and store ack futures, await in `flush()` |

---

### MQTT

**Feature flag:** `mqtt`

Pub/sub messaging via MQTT using `rumqttc`.

#### MqttSource

Push source. A background task reads from the MQTT EventLoop and pushes events into a PushBuffer. `next_batch()` drains the buffer. Phase 3 backpressure pauses event loop polling.

```rust
use aeon_connectors::mqtt::source::{MqttSource, MqttSourceConfig};

let config = MqttSourceConfig::new("localhost", 1883, "sensor/data")
    .with_client_id("aeon-sensor-reader")
    .with_qos(rumqttc::QoS::AtLeastOnce);

let mut source = MqttSource::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | `String` | -- | MQTT broker host |
| `port` | `u16` | -- | MQTT broker port |
| `client_id` | `String` | `"aeon-mqtt-{pid}"` | Client ID |
| `topic` | `String` | -- | Topic to subscribe to |
| `qos` | `QoS` | `AtLeastOnce` | QoS level (0, 1, or 2) |
| `buffer_config` | `PushBufferConfig` | default | Push buffer settings |
| `poll_timeout` | `Duration` | `1s` | Timeout for first event |
| `source_name` | `Arc<str>` | `"mqtt"` | Source identifier |
| `keep_alive` | `Duration` | `30s` | Keep alive interval |
| `cap` | `usize` | `256` | MQTT event loop channel capacity |

#### MqttSink

Publishes each output as an MQTT message. A background task polls the event loop for protocol-level communication (PingResp, etc.).

```rust
use aeon_connectors::mqtt::sink::{MqttSink, MqttSinkConfig};

let config = MqttSinkConfig::new("localhost", 1883, "output/topic")
    .with_qos(rumqttc::QoS::AtLeastOnce);

let mut sink = MqttSink::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | `String` | -- | MQTT broker host |
| `port` | `u16` | -- | MQTT broker port |
| `client_id` | `String` | `"aeon-mqtt-sink-{pid}"` | Client ID |
| `topic` | `String` | -- | Default publish topic |
| `qos` | `QoS` | `AtLeastOnce` | QoS level |
| `keep_alive` | `Duration` | `30s` | Keep alive interval |
| `cap` | `usize` | `256` | Event loop channel capacity |

---

### RabbitMQ

**Feature flag:** `rabbitmq`

AMQP 0-9-1 messaging via `lapin`.

#### RabbitMqSource

Push source. A background task consumes from a queue and pushes events into a PushBuffer. Messages are acknowledged after being pushed. Phase 3 backpressure uses `basic.cancel` to stop delivery.

```rust
use aeon_connectors::rabbitmq::source::{RabbitMqSource, RabbitMqSourceConfig};

let config = RabbitMqSourceConfig::new("amqp://aeon:password@localhost:5672", "my-queue")
    .with_prefetch_count(256)
    .with_consumer_tag("aeon-consumer");

let mut source = RabbitMqSource::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `uri` | `String` | -- | AMQP URI |
| `queue` | `String` | -- | Queue to consume from |
| `consumer_tag` | `String` | `"aeon-{pid}"` | Consumer tag |
| `prefetch_count` | `u16` | `256` | QoS prefetch count |
| `buffer_config` | `PushBufferConfig` | default | Push buffer settings |
| `poll_timeout` | `Duration` | `1s` | Timeout for first event |
| `source_name` | `Arc<str>` | `"rabbitmq"` | Source identifier |
| `declare_queue` | `bool` | `true` | Auto-declare queue if not exists |

#### RabbitMqSink

Publishes outputs to a RabbitMQ exchange. Supports publisher confirms. Two factory methods: `new()` for exchange-based routing, `direct_to_queue()` for default exchange with queue name as routing key.

```rust
use aeon_connectors::rabbitmq::sink::{RabbitMqSink, RabbitMqSinkConfig};

// Publish to a named exchange:
let config = RabbitMqSinkConfig::new("amqp://aeon:password@localhost:5672", "my-exchange")
    .with_routing_key("events");

// Or publish directly to a queue (default exchange):
let config = RabbitMqSinkConfig::direct_to_queue(
    "amqp://aeon:password@localhost:5672",
    "output-queue",
);

let mut sink = RabbitMqSink::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `uri` | `String` | -- | AMQP URI |
| `exchange` | `String` | -- | Exchange to publish to (empty = default) |
| `routing_key` | `String` | `""` | Default routing key |
| `declare_exchange` | `bool` | `false` | Auto-declare exchange |
| `exchange_type` | `String` | `"direct"` | Exchange type if declaring |
| `publisher_confirms` | `bool` | `true` | Enable publisher confirms |

---

## Advanced Connectors

### QUIC

**Feature flag:** `quic`

Raw QUIC transport via `quinn`. Length-prefixed binary messages on bidirectional streams. Protocol: `[length: u32 LE][payload]` per message.

#### QuicSource

Push source. Binds a QUIC endpoint and accepts incoming connections. Each bidirectional stream reads length-prefixed messages and pushes them into the push buffer. Requires TLS configuration via `quinn::ServerConfig`.

```rust
use aeon_connectors::quic::source::{QuicSource, QuicSourceConfig};

let server_config = /* quinn::ServerConfig with TLS */;
let config = QuicSourceConfig::new("0.0.0.0:4463".parse().unwrap(), server_config)
    .with_source_name("quic-ingest");

let mut source = QuicSource::new(config).await?;
```

#### QuicSink

Maintains a persistent QUIC connection. Opens a new bidirectional stream per batch and sends length-prefixed messages.

```rust
use aeon_connectors::quic::sink::{QuicSink, QuicSinkConfig};

let client_config = /* quinn::ClientConfig with TLS */;
let config = QuicSinkConfig::new(
    "127.0.0.1:4463".parse().unwrap(),
    "aeon-server",
    client_config,
);

let mut sink = QuicSink::new(config).await?;
```

---

### WebTransport

**Feature flag:** `webtransport`

HTTP/3-based WebTransport via `wtransport`. Two source modes: reliable streams and unreliable datagrams.

#### WebTransportSource (Streams)

Push source. Accepts WebTransport sessions via HTTP/3. Each bidirectional stream carries length-prefixed messages. Reliable and ordered delivery.

```rust
use aeon_connectors::webtransport::source::{WebTransportSource, WebTransportSourceConfig};

let server_config = /* wtransport::ServerConfig */;
let config = WebTransportSourceConfig::new("0.0.0.0:4472".parse().unwrap(), server_config);

let mut source = WebTransportSource::new(config).await?;
```

#### WebTransportDatagramSource (Datagrams)

Push source. Reads datagrams from WebTransport sessions. Datagrams may be dropped, reordered, or duplicated. Requires explicit `accept_loss: true` to acknowledge lossy semantics. Use cases: real-time telemetry, sensor data, game state.

```rust
use aeon_connectors::webtransport::datagram_source::{
    WebTransportDatagramSource, WebTransportDatagramSourceConfig,
};

let server_config = /* wtransport::ServerConfig */;
let config = WebTransportDatagramSourceConfig::new(server_config, true /* accept_loss */);

let mut source = WebTransportDatagramSource::new(config).await?;
```

#### WebTransportSink

Connects to a WebTransport server and sends outputs as length-prefixed messages on bidirectional streams.

```rust
use aeon_connectors::webtransport::sink::{WebTransportSink, WebTransportSinkConfig};

let client_config = /* wtransport::ClientConfig */;
let config = WebTransportSinkConfig::new("https://localhost:4472", client_config);

let mut sink = WebTransportSink::new(config).await?;
```

---

### PostgreSQL CDC

**Feature flag:** `postgres-cdc`

Logical replication via `pgoutput`. Uses `tokio-postgres` to consume WAL changes. Each change is emitted as an Event with a JSON payload describing the operation. Currently uses polling via `pg_logical_slot_get_binary_changes()`.

```rust
use aeon_connectors::postgres_cdc::source::{PostgresCdcSource, PostgresCdcSourceConfig};

let config = PostgresCdcSourceConfig::new(
    "host=localhost user=aeon dbname=aeon",
    "aeon_slot",
    "aeon_pub",
)
.with_batch_size(1024)
.without_create_slot(); // if slot already exists

let mut source = PostgresCdcSource::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `connection_string` | `String` | -- | PostgreSQL connection string |
| `slot_name` | `String` | -- | Replication slot name |
| `publication` | `String` | -- | Publication name (tables to capture) |
| `create_slot` | `bool` | `true` | Auto-create replication slot |
| `batch_size` | `usize` | `1024` | Max changes per batch |
| `poll_interval` | `Duration` | `100ms` | Poll interval for new WAL data |
| `source_name` | `Arc<str>` | `"postgres-cdc"` | Source identifier |

**Requirements:** PostgreSQL must be configured with `wal_level=logical`, and a publication must exist for the target tables.

---

### MySQL CDC

**Feature flag:** `mysql-cdc`

Binlog-based change data capture via `mysql_async`. Each change is emitted as an Event with a JSON payload. Uses polling of binlog via `SHOW BINLOG EVENTS`.

```rust
use aeon_connectors::mysql_cdc::source::{MysqlCdcSource, MysqlCdcSourceConfig};

let config = MysqlCdcSourceConfig::new("mysql://root:password@localhost:3306/mydb")
    .with_server_id(1001)
    .with_table("orders");

let mut source = MysqlCdcSource::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | `String` | -- | MySQL connection URL |
| `server_id` | `u32` | `1000` | Replication slave server ID (must be unique) |
| `tables` | `Vec<String>` | `[]` (all) | Tables to watch |
| `batch_size` | `usize` | `1024` | Max changes per batch |
| `poll_interval` | `Duration` | `500ms` | Poll interval for new binlog data |
| `source_name` | `Arc<str>` | `"mysql-cdc"` | Source identifier |
| `binlog_file` | `Option<String>` | `None` (current) | Starting binlog file |
| `binlog_position` | `Option<u64>` | `None` (current) | Starting binlog position |

**Requirements:** MySQL must have `binlog-format=ROW`, `binlog-row-image=FULL`, and GTID mode enabled.

---

### MongoDB CDC

**Feature flag:** `mongodb-cdc`

Change Streams via the official MongoDB driver. Push source with a background task reading changes into a PushBuffer. Requires a MongoDB replica set (change streams are not available on standalone instances).

```rust
use aeon_connectors::mongodb_cdc::source::{MongoDbCdcSource, MongoDbCdcSourceConfig};

let config = MongoDbCdcSourceConfig::new("mongodb://localhost:27017", "mydb")
    .with_collection("orders")
    .with_source_name("mongo-orders");

let mut source = MongoDbCdcSource::new(config).await?;
```

**Config fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `uri` | `String` | -- | MongoDB connection URI |
| `database` | `String` | -- | Database name |
| `collection` | `Option<String>` | `None` (whole database) | Collection to watch |
| `buffer_config` | `PushBufferConfig` | default | Push buffer settings |
| `poll_timeout` | `Duration` | `1s` | Timeout for first event |
| `source_name` | `Arc<str>` | `"mongodb-cdc"` | Source identifier |
| `full_document` | `bool` | `true` | Include full document on update events |
| `pipeline` | `Vec<Document>` | `[]` | Aggregation pipeline for filtering changes |

**Requirements:** MongoDB must run as a replica set. The `docker-compose.yml` starts MongoDB with `--replSet rs0` and an init container that calls `rs.initiate()`.

---

## Feature Flags

Enable connectors in your `Cargo.toml`:

```toml
[dependencies]
aeon-connectors = { path = "crates/aeon-connectors", features = [
    "memory",           # default — in-memory (testing)
    "blackhole",        # default — discard sink (benchmarks)
    "stdout",           # default — print sink (debugging)
    "kafka",            # Kafka/Redpanda (rdkafka)
    "file",             # File source/sink (tokio)
    "http",             # HTTP polling + webhook (axum, reqwest)
    "websocket",        # WebSocket (tokio-tungstenite)
    "redis-streams",    # Redis Streams (redis)
    "nats",             # NATS JetStream (async-nats)
    "mqtt",             # MQTT (rumqttc)
    "rabbitmq",         # RabbitMQ/AMQP (lapin)
    "quic",             # Raw QUIC (quinn, rustls)
    "webtransport",     # WebTransport streams + datagrams (wtransport)
    "postgres-cdc",     # PostgreSQL CDC (tokio-postgres)
    "mysql-cdc",        # MySQL CDC (mysql_async)
    "mongodb-cdc",      # MongoDB Change Streams (mongodb)
] }
```

The `default` feature enables only `memory`, `blackhole`, and `stdout`. All other connectors are opt-in. This keeps compile times low and avoids pulling in unnecessary native libraries (e.g., librdkafka, openssl).

---

## Configuration -- Docker Compose Services

The project includes a `docker-compose.yml` with all required infrastructure. Start services selectively based on which connectors you use.

### Service Reference

| Connector | Docker Compose Service(s) | Ports | Notes |
|-----------|--------------------------|-------|-------|
| Kafka/Redpanda | `redpanda`, `redpanda-console`, `redpanda-init` | 19092 (Kafka), 18082 (HTTP), 18081 (Schema Registry), 8080 (Console) | `redpanda-init` pre-creates topics |
| Redis Streams | `redis` | 6379 | Runs with AOF persistence |
| NATS JetStream | `nats` | 4222 (client), 8222 (monitoring) | Started with `--jetstream` |
| MQTT | `mosquitto` | 1883 (MQTT), 9001 (MQTT over WS) | Requires `docker/mosquitto.conf` |
| RabbitMQ | `rabbitmq` | 5672 (AMQP), 15672 (Management UI) | Default user set via env vars |
| PostgreSQL CDC | `postgres` | 5432 | `wal_level=logical`, 4 replication slots |
| MySQL CDC | `mysql` | 3306 | ROW binlog format, GTID enabled |
| MongoDB CDC | `mongodb`, `mongodb-init` | 27017 | Replica set `rs0`, init container runs `rs.initiate()` |

### Quick Start Commands

```bash
# Minimal (Redpanda only — Scenario 1):
docker compose up -d redpanda redpanda-console redpanda-init

# Redpanda + Redis:
docker compose up -d redpanda redpanda-init redis

# All messaging:
docker compose up -d redpanda redpanda-init redis nats mosquitto rabbitmq

# All CDC databases:
docker compose up -d postgres mysql mongodb mongodb-init

# Everything:
docker compose up -d
```

### Environment Variables

The following must be set (via `.env` file or shell):

| Variable | Required By | Example |
|----------|-------------|---------|
| `POSTGRES_PASSWORD` | `postgres` | `aeon_dev` |
| `MYSQL_ROOT_PASSWORD` | `mysql` | `aeon_dev` |
| `MYSQL_PASSWORD` | `mysql` | `aeon_dev` |
| `RABBITMQ_PASSWORD` | `rabbitmq` | `aeon_dev` |
| `GRAFANA_ADMIN_PASSWORD` | `grafana` | `admin` |

---

## EO-2 Durability — Migration Note

Aeon's EO-2 durability layer (L2 body store, L3 checkpoint, capacity-based backpressure) is opt-in per pipeline. Existing pipeline manifests that omit the `durability` key default to `durability: none` — no L2 writes, no capacity tracking, no backpressure gating. This preserves backwards compatibility and zero overhead for pipelines that don't need exactly-once guarantees.

When `durability` is enabled, the `source_kind` classification (Pull / Push / Poll) determines how backpressure is applied when L2 capacity limits are reached:

| Source Kind | Backpressure Remedy | Effect |
|-------------|---------------------|--------|
| **Pull** | `PullPause` | Source task sleeps until capacity is reclaimed by GC |
| **Push** | `PushReject` | Protocol-level rejection (e.g., HTTP 429, MQTT disconnect) |
| **Poll** | `PollSkip` | Next poll cycle is skipped, timer continues |

Connectors that don't override `fn source_kind()` default to `SourceKind::Pull`. See `aeon-types` for the trait definition and `eo2_backpressure` in `aeon-engine` for the capacity hierarchy.
