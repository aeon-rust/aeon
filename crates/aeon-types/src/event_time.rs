//! Per-source `event_time` configuration — how Aeon stamps `Event.timestamp`.
//!
//! Three modes, explicit per connector, no silent fallbacks. See
//! `docs/EO-2-DURABILITY-DESIGN.md` §4.4.
//!
//! - `Broker`      — timestamp supplied by upstream (Kafka ts, CDC ts, …).
//!                    Zero cost. Requires `Source::supports_broker_event_time()`
//!                    to return `true`; pipeline fails at start otherwise.
//! - `AeonIngest`  — `Instant::now()` at batch ingestion (~20 ns). Loses
//!                    cross-replay determinism — pull-source UUIDv7
//!                    derivation will therefore be non-deterministic.
//! - `Header(name)` — parse from a metadata header the upstream sets. ~50 ns.

use serde::{Deserialize, Serialize};

/// How `Event.timestamp` is populated for a given source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EventTime {
    /// Upstream-provided timestamp (Kafka record ts, CDC commit ts, …).
    /// Default for connectors that expose it. Requires the connector to
    /// override `Source::supports_broker_event_time()` to `true`.
    Broker,

    /// Aeon microtime at ingest. Loses cross-replay determinism.
    AeonIngest,

    /// Extract from a named metadata header. Value must parse as Unix-epoch
    /// nanoseconds (i64). Missing / unparsable → pipeline fails the batch.
    Header {
        /// Header name, e.g. `"X-Event-Time"`.
        name: String,
    },
}

impl Default for EventTime {
    /// Default is `Broker`. Poll / push connectors that cannot supply a
    /// broker timestamp must override the per-pipeline config, otherwise
    /// pipeline start validation fails.
    fn default() -> Self {
        Self::Broker
    }
}

impl EventTime {
    /// Short token for logs and metrics labels.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Broker => "broker",
            Self::AeonIngest => "aeon_ingest",
            Self::Header { .. } => "header",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_broker() {
        assert_eq!(EventTime::default(), EventTime::Broker);
    }

    #[test]
    fn json_roundtrip_broker() {
        let j = r#"{"mode":"broker"}"#;
        let v: EventTime = serde_json::from_str(j).unwrap();
        assert_eq!(v, EventTime::Broker);
    }

    #[test]
    fn json_roundtrip_header() {
        let j = r#"{"mode":"header","name":"X-Event-Time"}"#;
        let v: EventTime = serde_json::from_str(j).unwrap();
        assert_eq!(
            v,
            EventTime::Header {
                name: "X-Event-Time".into()
            }
        );
    }

    #[test]
    fn tags_distinct() {
        assert_ne!(EventTime::Broker.tag(), EventTime::AeonIngest.tag());
        assert_ne!(
            EventTime::Broker.tag(),
            EventTime::Header { name: "x".into() }.tag()
        );
    }
}
