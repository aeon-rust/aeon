//! Source and Sink connector implementations for Aeon.
//!
//! Each connector is feature-gated. Default features include
//! `memory`, `blackhole`, and `stdout` for testing and debugging.
//!
//! **Phase 11a — Streaming connectors:**
//! `kafka`, `file`, `http`, `websocket`, `redis-streams`, `nats`, `mqtt`, `rabbitmq`
//!
//! **Phase 11b — Advanced connectors:**
//! `quic`, `webtransport`, `postgres-cdc`, `mysql-cdc`, `mongodb-cdc`

// FT-10: no-panic policy. Production code in this crate must not use
// `.unwrap()` or `.expect()` except for explicitly-documented startup-time
// invariants, which must carry an `#[allow(...)]` attribute with rationale.
// Test modules and benches are exempt (`cfg(not(test))`).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

// Push-source backpressure buffer (shared by push-source connectors)
#[cfg(any(
    feature = "websocket",
    feature = "http",
    feature = "mqtt",
    feature = "rabbitmq",
    feature = "quic",
    feature = "webtransport",
    feature = "mongodb-cdc",
))]
pub mod push_buffer;

// ─── Phase 11a — Streaming Connectors ──────────────────────────────────

#[cfg(feature = "memory")]
pub mod memory;

#[cfg(feature = "blackhole")]
pub mod blackhole;

#[cfg(feature = "stdout")]
pub mod stdout;

#[cfg(feature = "kafka")]
pub mod kafka;

#[cfg(feature = "file")]
pub mod file;

#[cfg(feature = "http")]
pub mod http;

#[cfg(feature = "websocket")]
pub mod websocket;

#[cfg(feature = "redis-streams")]
pub mod redis_streams;

#[cfg(feature = "nats")]
pub mod nats;

#[cfg(feature = "mqtt")]
pub mod mqtt;

#[cfg(feature = "rabbitmq")]
pub mod rabbitmq;

// ─── Phase 11b — Advanced Connectors ───────────────────────────────────

#[cfg(feature = "quic")]
pub mod quic;

#[cfg(feature = "webtransport")]
pub mod webtransport;

#[cfg(feature = "postgres-cdc")]
pub mod postgres_cdc;

#[cfg(feature = "mysql-cdc")]
pub mod mysql_cdc;

#[cfg(feature = "mongodb-cdc")]
pub mod mongodb_cdc;

// ─── Re-exports ────────────────────────────────────────────────────────

#[cfg(feature = "memory")]
pub use memory::{MemorySink, MemorySource};

#[cfg(feature = "blackhole")]
pub use blackhole::BlackholeSink;

#[cfg(feature = "stdout")]
pub use stdout::StdoutSink;

#[cfg(feature = "kafka")]
pub use kafka::{KafkaSink, KafkaSource};

#[cfg(feature = "file")]
pub use file::{FileSink, FileSource};

#[cfg(feature = "http")]
pub use http::{HttpPollingSource, HttpSink, HttpWebhookSource};

#[cfg(feature = "websocket")]
pub use websocket::{WebSocketSink, WebSocketSource};

#[cfg(feature = "redis-streams")]
pub use redis_streams::{RedisSink as RedisStreamsSink, RedisSource as RedisStreamsSource};

#[cfg(feature = "nats")]
pub use nats::{NatsSink, NatsSource};

#[cfg(feature = "mqtt")]
pub use mqtt::{MqttSink, MqttSource};

#[cfg(feature = "rabbitmq")]
pub use rabbitmq::{RabbitMqSink, RabbitMqSource};

#[cfg(feature = "quic")]
pub use quic::{QuicSink, QuicSource};

#[cfg(feature = "webtransport")]
pub use webtransport::{WebTransportDatagramSource, WebTransportSink, WebTransportSource};

#[cfg(feature = "postgres-cdc")]
pub use postgres_cdc::PostgresCdcSource;

#[cfg(feature = "mysql-cdc")]
pub use mysql_cdc::MysqlCdcSource;

#[cfg(feature = "mongodb-cdc")]
pub use mongodb_cdc::MongoDbCdcSource;
