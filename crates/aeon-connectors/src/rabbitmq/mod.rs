//! RabbitMQ / AMQP connector — Source and Sink implementations.
//!
//! `RabbitMqSource`: Consumes from a RabbitMQ queue via AMQP basic.consume.
//!   Push-source with three-phase backpressure via shared PushBuffer.
//!
//! `RabbitMqSink`: Publishes to a RabbitMQ exchange via AMQP basic.publish.

mod sink;
mod source;

pub use sink::{RabbitMqSink, RabbitMqSinkConfig};
pub use source::{RabbitMqSource, RabbitMqSourceConfig};
