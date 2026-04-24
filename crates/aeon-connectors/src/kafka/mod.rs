//! Kafka / Redpanda connector — Source and Sink implementations.
//!
//! Uses manual partition assignment (`assign()`), not consumer groups (`subscribe()`).
//! Aeon's partition manager owns the mapping; Kafka's `__consumer_offsets` is not used.
//!
//! Both source and sink are batch-first:
//! - `KafkaSource::next_batch()` polls up to `batch_max_messages` per call
//! - `KafkaSink::write_batch()` sends all outputs in a single produce batch

mod auth;
mod sink;
mod source;

pub use sink::{KafkaSink, KafkaSinkConfig, redpanda_sink_config};
pub use source::{KafkaSource, KafkaSourceConfig, redpanda_source_config};
