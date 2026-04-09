//! MQTT connector — Source and Sink implementations.
//!
//! `MqttSource`: Subscribes to an MQTT topic and receives messages as events.
//!   Push-source with three-phase backpressure via shared PushBuffer.
//!
//! `MqttSink`: Publishes outputs to an MQTT topic.

mod sink;
mod source;

pub use sink::{MqttSink, MqttSinkConfig};
pub use source::{MqttSource, MqttSourceConfig};
