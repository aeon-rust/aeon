//! Source and Sink connector implementations for Aeon.
//!
//! Each connector is feature-gated. Default features include
//! `memory`, `blackhole`, and `stdout` for testing and debugging.
//!
//! Enable `kafka` feature for Kafka/Redpanda connectors.

#[cfg(feature = "memory")]
pub mod memory;

#[cfg(feature = "blackhole")]
pub mod blackhole;

#[cfg(feature = "stdout")]
pub mod stdout;

#[cfg(feature = "kafka")]
pub mod kafka;

// Re-exports
#[cfg(feature = "memory")]
pub use memory::{MemorySink, MemorySource};

#[cfg(feature = "blackhole")]
pub use blackhole::BlackholeSink;

#[cfg(feature = "stdout")]
pub use stdout::StdoutSink;

#[cfg(feature = "kafka")]
pub use kafka::{KafkaSink, KafkaSource};
