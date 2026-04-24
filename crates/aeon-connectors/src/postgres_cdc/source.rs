//! PostgreSQL CDC source — logical replication via pgoutput.
//!
//! Uses tokio-postgres to consume WAL changes via SQL-level replication functions.
//! Each change is emitted as an Event with a JSON payload describing the operation.
//!
//! Uses `pg_logical_slot_get_binary_changes()` for polling-based CDC.
//! For streaming replication protocol, a future version will use
//! START_REPLICATION with a walsender connection.

use aeon_types::{
    AeonError, BackoffPolicy, CoreLocalUuidGenerator, Event, OutboundAuthSigner, PartitionId,
    Source,
};
use bytes::Bytes;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

/// Configuration for `PostgresCdcSource`.
pub struct PostgresCdcSourceConfig {
    /// PostgreSQL connection string (e.g., "host=localhost user=aeon dbname=aeon").
    pub connection_string: String,
    /// Replication slot name.
    pub slot_name: String,
    /// Publication name (tables to capture).
    pub publication: String,
    /// Whether to create the replication slot if it doesn't exist.
    pub create_slot: bool,
    /// Maximum changes per `next_batch()`.
    pub batch_size: usize,
    /// Poll interval for checking new WAL data.
    pub poll_interval: Duration,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Reconnect backoff policy (TR-3). Applied when the connection or
    /// `pg_logical_slot_get_changes()` query fails. Exponential + jitter
    /// prevents reconnect storms against a flaky Postgres primary.
    pub backoff: BackoffPolicy,
    /// S10 outbound auth. When `Some`, the signer's mode drives how the
    /// `tokio_postgres::Config` is built from `connection_string`.
    pub auth: Option<Arc<OutboundAuthSigner>>,
}

impl PostgresCdcSourceConfig {
    /// Create a config for PostgreSQL CDC.
    pub fn new(
        connection_string: impl Into<String>,
        slot_name: impl Into<String>,
        publication: impl Into<String>,
    ) -> Self {
        Self {
            connection_string: connection_string.into(),
            slot_name: slot_name.into(),
            publication: publication.into(),
            create_slot: true,
            batch_size: 1024,
            poll_interval: Duration::from_millis(100),
            source_name: Arc::from("postgres-cdc"),
            backoff: BackoffPolicy::default(),
            auth: None,
        }
    }

    /// S10: attach an outbound-auth signer.
    pub fn with_auth(mut self, signer: Arc<OutboundAuthSigner>) -> Self {
        self.auth = Some(signer);
        self
    }

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Set batch size.
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Disable automatic slot creation.
    pub fn without_create_slot(mut self) -> Self {
        self.create_slot = false;
        self
    }

    /// Set poll interval.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
}

/// PostgreSQL CDC event source.
///
/// Uses logical decoding via `pg_logical_slot_get_changes()` to capture
/// INSERT, UPDATE, DELETE changes. Each change becomes an Event with a
/// JSON payload describing the operation.
///
/// Requires:
/// - `wal_level = logical` in postgresql.conf
/// - A publication on the target tables
/// - Sufficient replication slots configured
pub struct PostgresCdcSource {
    /// `None` when the connection has been dropped after an error and a
    /// reconnect is pending; the next `next_batch()` will attempt to rebuild.
    client: Option<Client>,
    config: PostgresCdcSourceConfig,
    _connection_handle: Option<tokio::task::JoinHandle<()>>,
    /// TR-3 reconnect backoff. Advanced on query/connection errors; reset on
    /// every successful query (even when the result set is empty).
    backoff: aeon_types::Backoff,
    uuid_gen: Mutex<CoreLocalUuidGenerator>,
}

impl PostgresCdcSource {
    /// Connect to PostgreSQL and set up the replication slot.
    pub async fn new(config: PostgresCdcSourceConfig) -> Result<Self, AeonError> {
        let (client, conn_handle) = establish(&config).await?;
        let backoff = config.backoff.iter();
        Ok(Self {
            client: Some(client),
            config,
            _connection_handle: Some(conn_handle),
            backoff,
            uuid_gen: Mutex::new(CoreLocalUuidGenerator::new(0)),
        })
    }
}

/// Establish a fresh tokio-postgres connection and (optionally) create the
/// replication slot. Returns the client and the driver task handle.
///
/// TLS handling: `auth::resolve_config` returns an optional
/// `rustls::ClientConfig` alongside the parsed `Config`. When `Some`, we
/// wrap it in a `tokio_postgres_rustls::MakeRustlsConnect` and hand that
/// to `Config::connect` so client-side mTLS is honoured. When `None`, we
/// fall back to `NoTls`.
async fn establish(
    config: &PostgresCdcSourceConfig,
) -> Result<(Client, tokio::task::JoinHandle<()>), AeonError> {
    let (pg_config, tls) =
        super::auth::resolve_config(&config.connection_string, config.auth.as_ref())?;

    let (client, conn_handle) = if let Some(rustls_config) = tls {
        let tls_connector = tokio_postgres_rustls::MakeRustlsConnect::new(rustls_config);
        let (client, connection) = pg_config.connect(tls_connector).await.map_err(|e| {
            AeonError::connection(format!("postgres mTLS connect failed: {e}"))
        })?;
        let conn_handle = tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!(error = %e, "postgres connection error");
            }
        });
        (client, conn_handle)
    } else {
        let (client, connection) = pg_config.connect(NoTls).await.map_err(|e| {
            AeonError::connection(format!("postgres connect failed: {e}"))
        })?;
        let conn_handle = tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!(error = %e, "postgres connection error");
            }
        });
        (client, conn_handle)
    };

    if config.create_slot {
        let create_sql = format!(
            "SELECT pg_create_logical_replication_slot('{}', 'test_decoding')",
            config.slot_name
        );
        match client.simple_query(&create_sql).await {
            Ok(_) => {
                tracing::info!(slot = %config.slot_name, "created replication slot");
            }
            Err(e) => {
                let msg = format!("{e}");
                if !msg.contains("already exists") {
                    return Err(AeonError::connection(format!(
                        "create replication slot failed: {e}"
                    )));
                }
            }
        }
    }

    tracing::info!(
        slot = %config.slot_name,
        publication = %config.publication,
        "PostgresCdcSource connected"
    );

    Ok((client, conn_handle))
}

impl Source for PostgresCdcSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // Reconnect if the client was dropped after a prior error (TR-3).
        // Returning Ok(empty) with a backoff sleep keeps the pipeline alive
        // rather than bubbling connection failures all the way up.
        if self.client.is_none() {
            match establish(&self.config).await {
                Ok((client, handle)) => {
                    self.client = Some(client);
                    self._connection_handle = Some(handle);
                    self.backoff.reset();
                }
                Err(e) => {
                    let delay = self.backoff.next_delay();
                    tracing::warn!(
                        error = %e,
                        delay_ms = delay.as_millis() as u64,
                        "postgres reconnect failed; backing off before retry"
                    );
                    tokio::time::sleep(delay).await;
                    return Ok(Vec::new());
                }
            }
        }

        // Use pg_logical_slot_get_changes with test_decoding plugin.
        // Returns text-format change descriptions (BEGIN, COMMIT,
        // table ... INSERT/UPDATE/DELETE with column values).
        let query = format!(
            "SELECT lsn::text, xid, data FROM pg_logical_slot_get_changes('{}', NULL, {})",
            self.config.slot_name, self.config.batch_size,
        );

        let client = self
            .client
            .as_ref()
            .ok_or_else(|| AeonError::state("postgres client missing after establish"))?;

        let rows = match client.simple_query(&query).await {
            Ok(rows) => rows,
            Err(e) => {
                // Drop the client so the next next_batch() rebuilds it, back
                // off, and return empty instead of Err so the pipeline stays
                // alive through the outage.
                let delay = self.backoff.next_delay();
                tracing::warn!(
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "pg_logical_slot_get_changes failed; dropping connection and backing off"
                );
                self.client = None;
                self._connection_handle = None;
                tokio::time::sleep(delay).await;
                return Ok(Vec::new());
            }
        };

        // Successful query — reset backoff so the next outage starts again
        // at `initial_ms`.
        self.backoff.reset();

        let mut events = Vec::new();
        let now = std::time::Instant::now();

        for msg in &rows {
            if let SimpleQueryMessage::Row(row) = msg {
                let lsn_str = row.get(0).unwrap_or("0/0");
                let xid = row.get(1).unwrap_or("0");
                let data = row.get(2).unwrap_or("");

                let payload = serde_json::json!({
                    "lsn": lsn_str,
                    "xid": xid,
                    "data": data,
                    "source": "postgres-cdc",
                    "slot": self.config.slot_name,
                });

                let payload_bytes = Bytes::from(serde_json::to_vec(&payload).unwrap_or_default());

                let event_id = self
                    .uuid_gen
                    .lock()
                    .map_err(|_| AeonError::connection("UUID generator mutex poisoned"))?
                    .next_uuid();
                let mut event = Event::new(
                    event_id,
                    0,
                    Arc::clone(&self.config.source_name),
                    PartitionId::new(0),
                    payload_bytes,
                );
                event = event.with_source_ts(now);
                // B2: pg_lsn format is "X/Y" where X and Y are hex u32s
                // representing the high/low halves of a 64-bit WAL offset.
                // Pack into a single i64 for checkpoint ordering.
                if let Some(offset) = parse_pg_lsn_to_i64(lsn_str) {
                    event = event.with_source_offset(offset);
                }
                event
                    .metadata
                    .push((Arc::from("pg.lsn"), Arc::from(lsn_str)));

                events.push(event);
            }
        }

        if events.is_empty() {
            tokio::time::sleep(self.config.poll_interval).await;
        }

        Ok(events)
    }
}

/// Parse a PostgreSQL LSN string `"X/Y"` (hex-high/hex-low halves of a
/// 64-bit WAL offset) into an `i64`. The sign bit is cleared so the
/// result stays non-negative for the life of the database (64-bit
/// LSN addresses 16 EiB of WAL — running out is not a concern).
///
/// Returns `None` on malformed input.
fn parse_pg_lsn_to_i64(lsn: &str) -> Option<i64> {
    let (high_s, low_s) = lsn.split_once('/')?;
    let high = u64::from_str_radix(high_s.trim(), 16).ok()?;
    let low = u64::from_str_radix(low_s.trim(), 16).ok()?;
    let packed = (high << 32) | (low & 0xFFFF_FFFF);
    Some((packed & 0x7FFF_FFFF_FFFF_FFFF) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lsn_basic() {
        // "0/16B2468" → (0 << 32) | 0x16B2468 = 23_799_400
        let v = parse_pg_lsn_to_i64("0/16B2468").unwrap();
        assert_eq!(v, 0x16B2468);
    }

    #[test]
    fn parse_lsn_monotonic_across_halves() {
        let a = parse_pg_lsn_to_i64("0/FFFFFFFF").unwrap();
        let b = parse_pg_lsn_to_i64("1/0").unwrap();
        assert!(b > a, "high-half increment strictly greater than max low");
    }

    #[test]
    fn parse_lsn_malformed_returns_none() {
        assert!(parse_pg_lsn_to_i64("nope").is_none());
        assert!(parse_pg_lsn_to_i64("0/ZZZ").is_none());
        assert!(parse_pg_lsn_to_i64("123").is_none());
    }
}
