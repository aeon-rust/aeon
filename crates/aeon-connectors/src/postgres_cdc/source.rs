//! PostgreSQL CDC source — logical replication via pgoutput.
//!
//! Uses tokio-postgres to consume WAL changes via SQL-level replication functions.
//! Each change is emitted as an Event with a JSON payload describing the operation.
//!
//! Uses `pg_logical_slot_get_binary_changes()` for polling-based CDC.
//! For streaming replication protocol, a future version will use
//! START_REPLICATION with a walsender connection.

use aeon_types::{AeonError, Event, PartitionId, Source};
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
        }
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
    client: Client,
    config: PostgresCdcSourceConfig,
    _connection_handle: tokio::task::JoinHandle<()>,
}

impl PostgresCdcSource {
    /// Connect to PostgreSQL and set up the replication slot.
    pub async fn new(config: PostgresCdcSourceConfig) -> Result<Self, AeonError> {
        let (client, connection) = tokio_postgres::connect(&config.connection_string, NoTls)
            .await
            .map_err(|e| AeonError::connection(format!("postgres connect failed: {e}")))?;

        let conn_handle = tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!(error = %e, "postgres connection error");
            }
        });

        // Create logical replication slot if configured
        if config.create_slot {
            let create_sql = format!(
                "SELECT pg_create_logical_replication_slot('{}', 'pgoutput')",
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

        Ok(Self {
            client,
            config,
            _connection_handle: conn_handle,
        })
    }
}

impl Source for PostgresCdcSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // Use pg_logical_slot_get_changes to consume changes
        // This returns text-format changes from the pgoutput plugin
        let query = format!(
            "SELECT lsn::text, xid, data FROM pg_logical_slot_get_changes('{}', NULL, {}, 'proto_version', '1', 'publication_names', '{}')",
            self.config.slot_name, self.config.batch_size, self.config.publication,
        );

        let rows = self.client.simple_query(&query).await.map_err(|e| {
            AeonError::connection(format!("pg_logical_slot_get_changes failed: {e}"))
        })?;

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
