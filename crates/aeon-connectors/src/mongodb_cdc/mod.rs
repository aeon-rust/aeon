//! MongoDB Change Streams connector — CDC source.
//!
//! `MongoDbCdcSource`: Watches a MongoDB collection (or database) for changes
//!   using Change Streams. Each change document becomes an Event with a JSON payload.
//!
//! Supports:
//! - Collection-level or database-level watching
//! - Resume token tracking for exactly-once semantics
//! - Pipeline filtering (aggregation stages)
//! - Full document lookup on update events
//! - Schema change handling (new fields are captured automatically)

mod source;

pub use source::{MongoDbCdcSource, MongoDbCdcSourceConfig};
