//! NATS / JetStream connector — Source and Sink implementations.
//!
//! `NatsSource`: JetStream pull consumer for durable, at-least-once delivery.
//!   Pull-source: `next_batch()` fetches messages from the consumer.
//!
//! `NatsSink`: Publishes outputs to a NATS subject (optionally via JetStream
//!   for persistence guarantees).

mod sink;
mod source;

pub use sink::{NatsSink, NatsSinkConfig};
pub use source::{NatsSource, NatsSourceConfig};
