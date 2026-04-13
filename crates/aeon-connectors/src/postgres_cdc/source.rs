//! PostgreSQL CDC source — logical replication via pgoutput.
//!
//! Uses tokio-postgres to consume WAL changes via SQL-level replication functions.
//! Each change is emitted as an Event with a JSON payload describing the operation.
//!
//! Uses `pg_logical_slot_get_binary_changes()` for polling-based CDC.
//! For streaming replication protocol, a future version will use
//! START_REPLICATION with a walsender connection.

use aeon_types::{AeonError, BackoffPolicy, Event, PartitionId, Source};
use bytes::Bytes;
use std::sync::Arc;
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
        }
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
        })
    }
}

/// Establish a fresh tokio-postgres connection and (optionally) create the
/// replication slot. Returns the client and the driver task handle.
async fn establish(
    config: &PostgresCdcSourceConfig,
) -> Result<(Client, tokio::task::JoinHandle<()>), AeonError> {
    let (client, connection) = tokio_postgres::connect(&config.connection_string, NoTls)
        .await
        .map_err(|e| AeonError::connection(format!("postgres connect failed: {e}")))?;

    let conn_handle = tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!(error = %e, "postgres connection error");
        }
    });

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

                let mut event = Event::new(
                    uuid::Uuid::nil(),
                    0,
                    Arc::clone(&self.config.source_name),
                    PartitionId::new(0),
                    payload_bytes,
                );
                event = event.with_source_ts(now);
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
