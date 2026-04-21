//! Core traits — Gate 1 only.
//!
//! Traits are defined BEFORE implementations. Always.
//! Code against the trait, not the concrete type.
//!
//! Gate 2 and post-Gate 2 traits are defined when their phase begins.

use crate::delivery::BatchResult;
use crate::error::AeonError;
use crate::event::{Event, Output};
use crate::processor_transport::{ProcessorHealth, ProcessorInfo};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Callback invoked by a sink each time deliveries are confirmed by the
/// downstream system (broker ack, HTTP 2xx, fsync return, etc.).
///
/// The argument is the number of outputs newly acked since the last call.
/// Sinks fire this from inside `write_batch()` (for tiers that ack inline)
/// or `flush()` (for tiers that ack asynchronously, e.g. Kafka
/// `UnorderedBatch`). The callback must be cheap and non-blocking — it is
/// called on the sink's hot path. The engine installs it to drive the
/// `outputs_acked_total` companion metric.
pub type SinkAckCallback = Arc<dyn Fn(usize) + Send + Sync>;

/// Classification of how a source obtains events from its upstream.
///
/// Drives downstream decisions about event-body durability (EO-2):
/// `Pull` sources have a durable replay position upstream (broker offset,
/// CDC LSN, file byte offset) so Aeon does not persist event bodies to L2.
/// `Push` and `Poll` sources have no durable upstream position and require
/// L2 persistence when `durability != none`.
///
/// See `docs/EO-2-DURABILITY-DESIGN.md` §3 for the full source-kind matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Aeon polls; upstream has a durable replay position.
    /// Examples: Kafka, Redis Streams, Postgres/MySQL/Mongo CDC, file.
    /// Identity: deterministic UUIDv7 from `(source, partition, offset)`.
    /// L2 write: skipped — broker is buffer of record.
    Pull,

    /// Upstream pushes events; Aeon cannot pause the wire directly.
    /// Examples: WebSocket, WebTransport, webhook, MQTT, RabbitMQ push, QUIC.
    /// Identity: random UUIDv7 with sub-ms monotonic counter.
    /// L2 write: required when `durability != none`.
    Push,

    /// Aeon polls but upstream has no durable replay position.
    /// Examples: HTTP polling of REST endpoints, DNS poll, time-sampled sources.
    /// Identity: config-driven (cursor / etag / content_hash / compound).
    /// L2 write: required when `durability != none`.
    Poll,
}

/// Event ingestion source. Batch-first: returns `Vec<Event>` per poll.
///
/// Pull sources call the external system inside `next_batch()`.
/// Push sources drain an internal receive buffer inside `next_batch()`.
/// The engine does not know or care which model the source uses.
pub trait Source: Send + Sync {
    /// Poll for the next batch of events.
    /// Returns an empty vec during lulls (no events available).
    fn next_batch(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Vec<Event>, AeonError>> + Send;

    /// Declare how this source obtains events — drives EO-2 L2 write decisions.
    ///
    /// Default: `SourceKind::Pull`. Override in push and poll connectors so the
    /// pipeline runner enables L2 event-body persistence for them when
    /// `durability != none`. Pull sources never write L2 regardless.
    ///
    /// **Forgetting to override in a push connector is a silent data-loss bug
    /// under `durability != none`.** Every push/poll connector must override.
    fn source_kind(&self) -> SourceKind {
        SourceKind::Pull
    }

    /// Declare whether this source can supply an upstream-native timestamp
    /// for every event (Kafka record ts, CDC commit ts, file mtime, …).
    ///
    /// EO-2 P2: if the pipeline's `event_time` config is `Broker` but the
    /// connector returns `false` here, the pipeline **fails at start** —
    /// no silent fallback to `AeonIngest`. See
    /// `docs/EO-2-DURABILITY-DESIGN.md` §4.4.
    ///
    /// Default: `false`. Connectors that always carry an upstream timestamp
    /// must override to `true`.
    fn supports_broker_event_time(&self) -> bool {
        false
    }

    /// Pause the source — stop producing new events.
    ///
    /// After `pause()`, subsequent `next_batch()` calls should return empty
    /// batches (or block) until `resume()` is called. This allows the pipeline
    /// drain mechanism to quiesce in-flight events before a processor hot-swap.
    ///
    /// Default: no-op (source continues producing). Override for sources that
    /// can meaningfully pause (e.g., KafkaSource stops polling, push-sources
    /// stop draining their internal buffer).
    fn pause(&mut self) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    /// Resume the source after a pause.
    ///
    /// Resumes normal event production. Called after a processor swap completes.
    /// Default: no-op (matches the default no-op pause).
    fn resume(&mut self) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    /// EO-2 P5: apply a recovery plan derived from the last persisted
    /// checkpoint at pipeline start. `pull_offsets` carries per-partition
    /// upstream offsets (for `Seekable`-style pull connectors like Kafka);
    /// `replay_from_l2_seq` is the min-across-sinks ack cursor to replay
    /// from for push/poll connectors that know how to iterate the L2 body
    /// store. Default: no-op — connectors opt in by overriding.
    fn on_recovery_plan(
        &mut self,
        _pull_offsets: &std::collections::HashMap<crate::partition::PartitionId, i64>,
        _replay_from_l2_seq: u64,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send {
        async { Ok(()) }
    }

    /// P5: re-assign the partitions this source should be reading from.
    ///
    /// Called by the pipeline source loop when the cluster's replicated
    /// partition table records an ownership change for the local node
    /// (CL-6 transfer commit). The caller hands the new owned set; the
    /// source must rewire its upstream binding so subsequent `next_batch`
    /// reads from exactly those partitions — adding new ones, dropping
    /// ones that moved away, keeping unchanged ones with their current
    /// offsets.
    ///
    /// Default: no-op — single-partition and Push/Poll sources that don't
    /// model partitions ignore this safely. Partitioned pull connectors
    /// (Kafka today) override to re-issue `consumer.assign()` without
    /// tearing down the pipeline task.
    fn reassign_partitions(
        &mut self,
        _partitions: &[u16],
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send {
        async { Ok(()) }
    }
}

/// Event delivery sink. Batch-first: accepts `Vec<Output>` per flush.
///
/// `write_batch()` returns `BatchResult` which reports per-event delivery
/// status (delivered/pending/failed). The pipeline engine uses this with
/// `BatchFailurePolicy` to drive retry/DLQ/fail decisions.
///
/// How `write_batch()` behaves depends on the `DeliveryStrategy`:
/// - `PerEvent`: send + await each event individually, all returned as delivered
/// - `OrderedBatch`: send all in order, await all at batch end, all delivered
/// - `UnorderedBatch`: enqueue all, return immediately, all returned as pending
pub trait Sink: Send + Sync {
    /// Write a batch of outputs to the external system.
    ///
    /// Returns `BatchResult` indicating which events were delivered, which
    /// are pending (enqueued but not yet acked), and which failed.
    fn write_batch(
        &mut self,
        outputs: Vec<Output>,
    ) -> impl std::future::Future<Output = Result<BatchResult, AeonError>> + Send;

    /// Flush any buffered outputs, ensuring delivery.
    ///
    /// For `UnorderedBatch` mode, this collects pending acks and returns.
    /// For `PerEvent` and `OrderedBatch`, this is typically a no-op since
    /// acks are already collected in `write_batch()`.
    fn flush(&mut self) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;

    /// Install a callback fired each time downstream-confirmed deliveries
    /// land. Drives the engine's `outputs_acked_total` companion metric so
    /// operators can see the gap between `write_batch()` calls (counted as
    /// `outputs_sent`) and broker-confirmed acks. The argument is the
    /// number of outputs newly acked since the last call.
    ///
    /// Default: no-op. Connectors that can observe genuine downstream acks
    /// override this — Kafka fires on `delivered` increments, HTTP fires
    /// on 2xx response, etc. Fire-and-forget tiers (stdout, blackhole)
    /// keep the default since `sent == acked` trivially for them.
    ///
    /// The callback must be cheap and non-blocking — it runs on the sink's
    /// hot path. See `SinkAckCallback` for the type alias.
    fn on_ack_callback(&mut self, _cb: SinkAckCallback) {}
}

/// Event transformation processor.
///
/// Processors are the only component that may use `dyn Trait` (for Wasm runtime).
/// Native Rust processors implement this trait directly.
pub trait Processor: Send + Sync {
    /// Process a single event, producing zero or more outputs.
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError>;

    /// Process a batch of events. Default implementation calls `process()` per event.
    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        let mut outputs = Vec::with_capacity(events.len());
        for event in events {
            outputs.extend(self.process(event)?);
        }
        Ok(outputs)
    }

    /// Export host-side state so it can be transferred into the next processor
    /// instance on a hot-swap (TR-2).
    ///
    /// The engine calls this on the *outgoing* processor immediately before
    /// replacing it with a new one (drain-and-swap, blue-green cutover, canary
    /// completion). The returned `Vec<(key, value)>` is handed to the new
    /// processor's `restore_state()` so guest state survives the swap.
    ///
    /// Default: empty vec — processors are stateless by contract. Override
    /// only for runtimes that hold state on the host side (e.g. the Wasm
    /// runtime's per-instance `HostState.state` HashMap).
    #[allow(clippy::type_complexity)]
    fn snapshot_state(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>, AeonError> {
        Ok(Vec::new())
    }

    /// Import host-side state produced by a prior processor's `snapshot_state()`
    /// (TR-2). The engine calls this on the *incoming* processor right after it
    /// becomes active and before the first event is processed.
    ///
    /// Default: no-op. Override for stateful runtimes; implementations should
    /// replace existing state rather than merge, so that a fresh instance
    /// matches the outgoing one bit-for-bit.
    fn restore_state(&self, _snapshot: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), AeonError> {
        Ok(())
    }
}

/// Key-value state operations. Backed by the multi-tier state store.
pub trait StateOps: Send + Sync {
    fn get(
        &self,
        key: &[u8],
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, AeonError>> + Send;

    fn put(
        &self,
        key: &[u8],
        value: &[u8],
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;

    fn delete(&self, key: &[u8])
    -> impl std::future::Future<Output = Result<(), AeonError>> + Send;
}

/// Source that can rewind to a previous offset (e.g., for replay after crash).
pub trait Seekable: Source {
    fn seek(
        &mut self,
        offset: u64,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;
}

/// Sink that can deduplicate by event ID (for exactly-once delivery).
pub trait IdempotentSink: Sink {
    fn has_seen(
        &self,
        event_id: &uuid::Uuid,
    ) -> impl std::future::Future<Output = Result<bool, AeonError>> + Send;
}

/// EO-2: Which native atomic-commit primitive a sink exposes.
///
/// Each tier corresponds to a commit primitive the pipeline engine can drive
/// from `TransactionalSink::commit()`. The tier is fixed per connector type
/// (not per-instance config) and governs how the engine brackets a batch of
/// writes with a two-phase L3 checkpoint record (`pending → committed`).
///
/// See `docs/THROUGHPUT-VALIDATION.md` §7 for the full tier table and
/// `docs/FAULT-TOLERANCE-ANALYSIS.md` EO-2 for the design rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkEosTier {
    /// T1 — Transactional DBs (Postgres, MySQL, SQLite, MongoDB).
    /// Commit primitive: `COMMIT` on an in-progress SQL transaction,
    /// with `ON CONFLICT DO NOTHING` (or equivalent) supplying idempotence.
    TransactionalDb,
    /// T2 — Transactional streams (Kafka, Redpanda).
    /// Commit primitive: `commit_transaction` + `send_offsets_to_transaction`
    /// on the producer epoch established via `init_transactions`.
    TransactionalStream,
    /// T3 — Atomic-rename files.
    /// Commit primitive: write to `*.tmp`, then `rename()` into place.
    AtomicRenameFile,
    /// T4 — Dedup-keyed stores (Redis Streams, NATS JetStream).
    /// Commit primitive: pipeline EXEC (Redis) or ack-all + msg-id (JetStream).
    /// Idempotence comes from the sink's per-event dedup key.
    DedupKeyed,
    /// T5 — Idempotency-Key receivers (webhook, WS, WT).
    /// Commit primitive: HTTP request carrying `Idempotency-Key: <event_id>`.
    /// The server is responsible for deduplication.
    IdempotencyKey,
    /// T6 — Fire-and-forget (stdout, blackhole).
    /// No commit primitive. EOS is not meaningful — present so the capability
    /// probe is total.
    FireAndForget,
}

/// EO-2: Sink that supports transactional commit around a batch.
///
/// The pipeline engine drives the commit lifecycle:
///
/// ```text
///   begin()                              ← pipeline opens a txn
///   write_batch()  (1..N times)          ← normal sink writes
///   L3[pipeline/partition] = Pending {   ← phase 1 of two-phase L3 record
///     offset, pending_event_ids }
///   commit()                             ← sink's native atomic primitive
///   L3[pipeline/partition] = Committed   ← phase 2
/// ```
///
/// On crash after `Pending` but before `Committed`, recovery replays the
/// pending batch through the sink and relies on the sink's `IdempotentSink`
/// primitive (EO-1/EO-3) to suppress duplicates. A sink implementing
/// `TransactionalSink` must therefore also implement `IdempotentSink`
/// unless its tier provides commit-atomicity sufficient on its own
/// (Kafka transactions, atomic rename).
///
/// Tier selection (`eos_tier()`) is advisory metadata for the engine —
/// it lets the engine pick tier-specific recovery strategies (e.g. for
/// `TransactionalStream` the engine can call `abort()` on restart;
/// for `DedupKeyed` it just replays because the dedup keys will match).
///
/// Sinks that do not support atomic commit (e.g. plain fire-and-forget)
/// should NOT implement this trait — the engine falls back to the
/// at-least-once checkpoint path.
pub trait TransactionalSink: Sink {
    /// Tier the sink belongs to. See [`SinkEosTier`].
    ///
    /// Pure metadata — the engine calls this once at pipeline start.
    fn eos_tier(&self) -> SinkEosTier;

    /// Begin a transactional commit window.
    ///
    /// The sink transitions from idle to "in-transaction" — subsequent
    /// `write_batch()` calls are associated with this transaction. Must be
    /// idempotent within the same generation so that replays on recovery
    /// do not double-open.
    fn begin(&mut self) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;

    /// Commit the in-progress transaction atomically.
    ///
    /// After `commit()` returns `Ok(())`, every `write_batch()` since the
    /// matching `begin()` is durably visible to downstream consumers. After
    /// this returns, the sink is back in the idle state.
    fn commit(&mut self) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;

    /// Abort the in-progress transaction.
    ///
    /// Used on recovery when the last L3 record is `Pending` but the engine
    /// decides to replay from the prior `Committed` checkpoint instead of
    /// finishing the pending batch. Must leave the sink in the idle state.
    fn abort(&mut self) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;
}

/// Async transport abstraction for all processor tiers (T1–T4).
///
/// The pipeline calls this trait exclusively — it never knows whether the
/// processor is in-process (T1/T2) or out-of-process (T3/T4).
///
/// Unlike `Source`/`Sink`/`Processor` which use APIT (`impl Future`) for
/// static dispatch, this trait uses `Pin<Box<dyn Future>>` to support
/// dynamic dispatch via `&dyn ProcessorTransport` (Decision D2). The
/// per-batch Box allocation (~20ns) is negligible vs processing (240ns+).
pub trait ProcessorTransport: Send + Sync {
    /// Send a batch of events to the processor, receive outputs.
    ///
    /// For T1/T2 (in-process): resolves synchronously, no real await point.
    /// For T3/T4 (network): awaits network round-trip.
    /// Event identity propagation is handled by the implementation.
    fn call_batch(
        &self,
        events: Vec<Event>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Output>, AeonError>> + Send + '_>>;

    /// Check processor health.
    ///
    /// For T1/T2: always returns healthy.
    /// For T3/T4: checks heartbeat and connection state.
    fn health(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessorHealth, AeonError>> + Send + '_>>;

    /// Graceful drain — stop accepting new batches, flush in-flight work.
    ///
    /// For T1/T2: no-op (in-process, nothing to drain).
    /// For T3/T4: sends drain signal on control stream, awaits acknowledgment.
    fn drain(&self) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + '_>>;

    /// Processor metadata (sync — always available without I/O).
    fn info(&self) -> ProcessorInfo;
}

/// Replicates checkpoint source offsets to a cluster-wide store (e.g., Raft).
///
/// Implementations submit per-partition source offsets at checkpoint boundaries.
/// On failover, the new owner reads the replicated offsets to resume from the
/// correct position without data loss or duplication.
pub trait CheckpointReplicator: Send + Sync {
    /// Submit checkpoint source offsets for cross-node replication.
    ///
    /// `source_offsets` maps partition ID (as u16) to the source-anchor offset
    /// (e.g., Kafka consumer offset). Only offsets ahead of the currently
    /// replicated value are applied.
    fn submit_checkpoint(
        &self,
        source_offsets: std::collections::HashMap<u16, i64>,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::PartitionId;
    use bytes::Bytes;
    use std::sync::Arc;

    /// A trivial passthrough processor for testing.
    struct PassthroughProcessor;

    impl Processor for PassthroughProcessor {
        fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
            Ok(vec![
                Output::new(Arc::from("output"), event.payload.clone())
                    .with_source_ts(event.source_ts),
            ])
        }
    }

    #[test]
    fn passthrough_processor_produces_output() {
        let proc = PassthroughProcessor;
        let event = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"hello"),
        );
        let outputs = proc.process(event).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].payload.as_ref(), b"hello");
    }

    #[test]
    fn process_batch_default_impl() {
        let proc = PassthroughProcessor;
        let events: Vec<Event> = (0..3)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i,
                    Arc::from("test"),
                    PartitionId::new(0),
                    Bytes::from(format!("event-{i}")),
                )
            })
            .collect();
        let outputs = proc.process_batch(events).unwrap();
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0].payload.as_ref(), b"event-0");
        assert_eq!(outputs[2].payload.as_ref(), b"event-2");
    }

    /// A filter processor that drops events without "keep" in payload.
    struct FilterProcessor;

    impl Processor for FilterProcessor {
        fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
            if event.payload.as_ref().windows(4).any(|w| w == b"keep") {
                Ok(vec![Output::new(
                    Arc::from("output"),
                    event.payload.clone(),
                )])
            } else {
                Ok(vec![])
            }
        }
    }

    /// Compile-time proof that ProcessorTransport is object-safe (dyn-compatible).
    /// If this compiles, `&dyn ProcessorTransport` works — required by Decision D2.
    #[allow(dead_code)]
    fn _assert_processor_transport_object_safe(_: &dyn ProcessorTransport) {}

    /// EO-2: SinkEosTier is a plain Copy/Eq enum — cheap to carry around and
    /// safe to use in match arms. These asserts fail at compile time if the
    /// bounds ever regress.
    #[allow(dead_code)]
    fn _assert_sink_eos_tier_bounds() {
        fn needs<T: Copy + Eq + std::fmt::Debug + Send + Sync + 'static>() {}
        needs::<SinkEosTier>();
    }

    #[test]
    fn sink_eos_tier_variants_distinct() {
        use SinkEosTier::*;
        let all = [
            TransactionalDb,
            TransactionalStream,
            AtomicRenameFile,
            DedupKeyed,
            IdempotencyKey,
            FireAndForget,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b, "variants {i} and {j} compare equal");
                }
            }
        }
    }

    #[test]
    fn filter_processor_drops_non_matching() {
        let proc = FilterProcessor;
        let keep = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"keep this"),
        );
        let drop = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"discard this"),
        );

        assert_eq!(proc.process(keep).unwrap().len(), 1);
        assert_eq!(proc.process(drop).unwrap().len(), 0);
    }
}
