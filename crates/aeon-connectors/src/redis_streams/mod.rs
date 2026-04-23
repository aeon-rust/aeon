//! Redis Streams connector — Source and Sink implementations.
//!
//! `RedisSource`: Reads from a Redis Stream using XREADGROUP (consumer group).
//!   Pull-source: `next_batch()` issues XREADGROUP and returns events.
//!
//! `RedisSink`: Writes to a Redis Stream using XADD.
//!   Each output becomes a stream entry with the payload as the field value.

mod auth;
mod sink;
mod source;

pub use sink::{RedisSink, RedisSinkConfig};
pub use source::{RedisSource, RedisSourceConfig};
