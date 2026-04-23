//! EO-2 runtime primitives — `L2WritingSource` adapter, `PipelineL2Registry`,
//! and `AckSeqTracker`. Kept separate from `pipeline.rs` so the invasive hot-
//! path wiring is incremental and individually testable.
//!
//! Phase P4 of `docs/EO-2-DURABILITY-DESIGN.md`.

use crate::delivery::L2BodyStoreConfig;
use crate::l2_body::{L2BodyConfig, L2BodyStore};
use aeon_crypto::kek::KekHandle;
use aeon_types::{AeonError, DurabilityMode, Event, PartitionId, Source, SourceKind};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ── PipelineL2Registry ──────────────────────────────────────────────────

/// Shared per-(pipeline, partition) L2 body store registry.
///
/// The source-side adapter and the sink-side GC both resolve the same
/// `L2BodyStore` handle through this registry, so they always agree on which
/// segment directory is authoritative for a given partition.
type L2RegistryMap = HashMap<(String, PartitionId), Arc<Mutex<L2BodyStore>>>;

#[derive(Clone, Default)]
pub struct PipelineL2Registry {
    inner: Arc<Mutex<L2RegistryMap>>,
    root: Option<PathBuf>,
    segment_bytes: u64,
    /// Data-context KEK for per-segment at-rest encryption. `None`
    /// means segments are written plaintext (S3 `at_rest=off`).
    kek: Option<Arc<KekHandle>>,
    /// S5: retention hold applied to acked segments before `gc_up_to`
    /// physically deletes them. `ZERO` is the pre-S5 ack-immediate
    /// behaviour.
    gc_min_hold: std::time::Duration,
}

impl PipelineL2Registry {
    /// Build a registry from a `L2BodyStoreConfig`. If `root` is `None` a
    /// per-process temp dir is used (dev-only default).
    pub fn new(config: L2BodyStoreConfig) -> Self {
        let root = config.root.or_else(|| {
            Some(std::env::temp_dir().join(format!("aeon-l2body-{}", std::process::id())))
        });
        Self {
            inner: Arc::default(),
            root,
            segment_bytes: config.segment_bytes,
            kek: None,
            gc_min_hold: std::time::Duration::ZERO,
        }
    }

    /// Install the data-context KEK used to wrap per-segment DEKs. Must
    /// be called before any partition store is opened; stores already
    /// resolved through the registry retain their original cipher state.
    pub fn with_kek(mut self, kek: Arc<KekHandle>) -> Self {
        self.kek = Some(kek);
        self
    }

    /// S5: install the minimum hold applied before `gc_up_to` physically
    /// deletes a segment that has become eligible for reclaim. Must be
    /// called before any partition store is opened.
    pub fn with_gc_min_hold(mut self, hold: std::time::Duration) -> Self {
        self.gc_min_hold = hold;
        self
    }

    /// Get-or-create the `L2BodyStore` for `(pipeline, partition)`.
    pub fn open(
        &self,
        pipeline: &str,
        partition: PartitionId,
    ) -> Result<Arc<Mutex<L2BodyStore>>, AeonError> {
        let key = (pipeline.to_owned(), partition);
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| AeonError::state("l2 registry: poisoned"))?;
        if let Some(store) = guard.get(&key) {
            return Ok(Arc::clone(store));
        }
        // Startup-time disk setup; panics not acceptable on hot path, but
        // here we are on the pipeline-start cold path.
        let root = self
            .root
            .clone()
            .ok_or_else(|| AeonError::state("l2 registry: no root configured"))?;
        let dir = root
            .join(pipeline)
            .join(format!("p{:05}", partition.as_u16()));
        let store = L2BodyStore::open(
            dir,
            L2BodyConfig {
                segment_bytes: self.segment_bytes,
                kek: self.kek.clone(),
                gc_min_hold: self.gc_min_hold,
            },
        )?;
        let handle = Arc::new(Mutex::new(store));
        guard.insert(key, Arc::clone(&handle));
        Ok(handle)
    }

    /// Registered (pipeline, partition) pairs — for tests and observability.
    pub fn keys(&self) -> Vec<(String, PartitionId)> {
        self.inner
            .lock()
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Directory the `L2BodyStore` for `(pipeline, partition)` lives in.
    /// Returns `None` only when the registry was constructed without a
    /// root (an uninitialised default). The path is computed
    /// deterministically — it does not require the store to be opened.
    /// Used by CL-6c.4's `L2SegmentTransferProvider` to walk segments
    /// even when the partition is currently quiesced.
    pub fn partition_dir(
        &self,
        pipeline: &str,
        partition: PartitionId,
    ) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|r| r.join(pipeline).join(format!("p{:05}", partition.as_u16())))
    }
}

// ── L2WritingSource adapter ─────────────────────────────────────────────

/// Wraps any `Source`. When the inner source is `Push` or `Poll` and the
/// configured `DurabilityMode` requires L2, every event emitted by
/// `next_batch` is appended to the per-partition `L2BodyStore` before being
/// handed to the pipeline runner. Pull sources short-circuit to a direct
/// passthrough with zero overhead.
///
/// The seq assigned by `L2BodyStore::append` is stashed on a parallel
/// `last_seqs` vec so the sink-side ack path can update
/// `per_sink_ack_seq` without reparsing event bodies.
pub struct L2WritingSource<S: Source> {
    inner: S,
    pipeline: String,
    registry: PipelineL2Registry,
    mode: DurabilityMode,
    /// Populated by `next_batch` — seq assigned to each event, parallel to
    /// the returned `Vec<Event>`. Consumed by `take_last_seqs`.
    last_seqs: Vec<(PartitionId, u64)>,
    /// EO-2 P7: optional capacity tracker — `adjust(+bytes)` after each L2 append.
    capacity: Option<crate::eo2_backpressure::PipelineCapacity>,
}

impl<S: Source> L2WritingSource<S> {
    pub fn new(
        inner: S,
        pipeline: impl Into<String>,
        registry: PipelineL2Registry,
        mode: DurabilityMode,
    ) -> Self {
        Self {
            inner,
            pipeline: pipeline.into(),
            registry,
            mode,
            last_seqs: Vec::new(),
            capacity: None,
        }
    }

    /// Attach a `PipelineCapacity` for L2 byte accounting.
    pub fn with_capacity(mut self, capacity: crate::eo2_backpressure::PipelineCapacity) -> Self {
        self.capacity = Some(capacity);
        self
    }

    /// Take the (partition, seq) pairs assigned to the most recent batch.
    /// Empty for pull sources or `DurabilityMode::None`.
    pub fn take_last_seqs(&mut self) -> Vec<(PartitionId, u64)> {
        std::mem::take(&mut self.last_seqs)
    }

    /// Whether this adapter is an effective pass-through (no L2 writes).
    pub fn is_passthrough(&self) -> bool {
        !self.mode.requires_l2_body_store() || matches!(self.inner.source_kind(), SourceKind::Pull)
    }
}

#[allow(clippy::manual_async_fn)]
impl<S: Source> Source for L2WritingSource<S> {
    fn next_batch(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Vec<Event>, AeonError>> + Send {
        async move {
            let mut batch = self.inner.next_batch().await?;
            self.last_seqs.clear();

            if self.is_passthrough() || batch.is_empty() {
                return Ok(batch);
            }

            // Group indices by partition so we hold each partition lock once.
            let mut by_part: HashMap<PartitionId, Vec<usize>> = HashMap::new();
            for (i, ev) in batch.iter().enumerate() {
                by_part.entry(ev.partition).or_default().push(i);
            }

            self.last_seqs.resize(batch.len(), (PartitionId::new(0), 0));

            for (part, idxs) in by_part {
                let store = self.registry.open(&self.pipeline, part)?;
                let mut guard = store
                    .lock()
                    .map_err(|_| AeonError::state("l2 store lock poisoned"))?;
                for i in idxs {
                    let bytes_before = guard.disk_bytes();
                    let seq = guard.append(&batch[i])?;
                    batch[i].l2_seq = Some(seq);
                    self.last_seqs[i] = (part, seq);
                    if let Some(ref cap) = self.capacity {
                        let delta = guard.disk_bytes().saturating_sub(bytes_before);
                        if delta > 0 {
                            cap.adjust(part.as_u16(), delta as i64);
                        }
                    }
                }
                if self.mode.fsync_per_event() {
                    guard.fsync()?;
                }
            }

            Ok(batch)
        }
    }

    fn source_kind(&self) -> SourceKind {
        self.inner.source_kind()
    }

    fn supports_broker_event_time(&self) -> bool {
        self.inner.supports_broker_event_time()
    }

    fn pause(&mut self) -> impl std::future::Future<Output = ()> + Send {
        self.inner.pause()
    }

    fn resume(&mut self) -> impl std::future::Future<Output = ()> + Send {
        self.inner.resume()
    }

    fn on_recovery_plan(
        &mut self,
        pull_offsets: &HashMap<PartitionId, i64>,
        replay_from_l2_seq: u64,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send {
        self.inner.on_recovery_plan(pull_offsets, replay_from_l2_seq)
    }

    fn broker_coordinated_partitions(&self) -> bool {
        self.inner.broker_coordinated_partitions()
    }
}

// ── MaybeL2Wrapped ──────────────────────────────────────────────────────

/// Runtime wrapper the pipeline uses to apply `L2WritingSource` conditionally
/// without changing the function signature or type of the source argument.
///
/// `run_buffered` calls `MaybeL2Wrapped::wrap(source, cfg)` once up-front;
/// after that the source task uses the enum as an ordinary `Source`. The
/// match in `next_batch` monomorphises to a single branch per call-site
/// invocation so the indirection is near-free on the hot path.
pub enum MaybeL2Wrapped<S: Source> {
    Direct(S),
    Wrapped(L2WritingSource<S>),
}

impl<S: Source> MaybeL2Wrapped<S> {
    /// Wrap `source` in an `L2WritingSource` when a registry is provided
    /// AND the durability mode actually requires L2 body persistence.
    /// Otherwise pass through untouched.
    pub fn wrap(
        source: S,
        pipeline_name: &str,
        registry: Option<&PipelineL2Registry>,
        mode: DurabilityMode,
        capacity: Option<&crate::eo2_backpressure::PipelineCapacity>,
    ) -> Self {
        match registry {
            Some(reg) if mode.requires_l2_body_store() => {
                let mut l2 = L2WritingSource::new(
                    source,
                    pipeline_name,
                    reg.clone(),
                    mode,
                );
                if let Some(c) = capacity {
                    l2 = l2.with_capacity(c.clone());
                }
                Self::Wrapped(l2)
            }
            _ => Self::Direct(source),
        }
    }

    /// Drain and return the last batch's `(partition, seq)` assignments.
    /// Empty for the `Direct` variant or pull sources.
    pub fn take_last_seqs(&mut self) -> Vec<(PartitionId, u64)> {
        match self {
            Self::Direct(_) => Vec::new(),
            Self::Wrapped(w) => w.take_last_seqs(),
        }
    }
}

#[allow(clippy::manual_async_fn)]
impl<S: Source> Source for MaybeL2Wrapped<S> {
    fn next_batch(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Vec<Event>, AeonError>> + Send {
        async move {
            match self {
                Self::Direct(s) => s.next_batch().await,
                Self::Wrapped(w) => w.next_batch().await,
            }
        }
    }

    fn source_kind(&self) -> SourceKind {
        match self {
            Self::Direct(s) => s.source_kind(),
            Self::Wrapped(w) => w.source_kind(),
        }
    }

    fn supports_broker_event_time(&self) -> bool {
        match self {
            Self::Direct(s) => s.supports_broker_event_time(),
            Self::Wrapped(w) => w.supports_broker_event_time(),
        }
    }

    fn pause(&mut self) -> impl std::future::Future<Output = ()> + Send {
        async move {
            match self {
                Self::Direct(s) => s.pause().await,
                Self::Wrapped(w) => w.pause().await,
            }
        }
    }

    fn resume(&mut self) -> impl std::future::Future<Output = ()> + Send {
        async move {
            match self {
                Self::Direct(s) => s.resume().await,
                Self::Wrapped(w) => w.resume().await,
            }
        }
    }

    fn on_recovery_plan(
        &mut self,
        pull_offsets: &HashMap<PartitionId, i64>,
        replay_from_l2_seq: u64,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send {
        async move {
            match self {
                Self::Direct(s) => s.on_recovery_plan(pull_offsets, replay_from_l2_seq).await,
                Self::Wrapped(w) => w.on_recovery_plan(pull_offsets, replay_from_l2_seq).await,
            }
        }
    }

    fn broker_coordinated_partitions(&self) -> bool {
        match self {
            Self::Direct(s) => s.broker_coordinated_partitions(),
            Self::Wrapped(w) => w.broker_coordinated_partitions(),
        }
    }
}

// ── AckSeqTracker ───────────────────────────────────────────────────────

/// Per-sink highest contiguously-acked delivery sequence number.
///
/// The sink task calls `record_ack(sink, seq)` for every successful
/// delivery; the tracker snapshots `min()` across all sinks on demand so the
/// GC sweep can safely drop L2 segments with `max_seq < snapshot`.
#[derive(Clone, Default)]
pub struct AckSeqTracker {
    inner: Arc<Mutex<HashMap<String, u64>>>,
}

impl AckSeqTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `sink` has acked delivery sequence `seq`. Monotonically
    /// increasing — stale updates are ignored.
    pub fn record_ack(&self, sink: &str, seq: u64) {
        if let Ok(mut g) = self.inner.lock() {
            let e = g.entry(sink.to_string()).or_insert(0);
            if seq > *e {
                *e = seq;
            }
        }
    }

    /// Declare a sink as active with an initial ack of 0. Required so
    /// `min_across_sinks()` returns 0 (not an unbounded max) before the
    /// first ack lands.
    pub fn register_sink(&self, sink: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.entry(sink.to_string()).or_insert(0);
        }
    }

    /// Snapshot the current per-sink map — for `CheckpointRecord` writes.
    pub fn snapshot(&self) -> HashMap<String, u64> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// `min()` across registered sinks. Returns `None` if no sink is
    /// registered (GC must not run in that case).
    pub fn min_across_sinks(&self) -> Option<u64> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.values().min().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::Arc as StdArc;

    fn ev(part: u16, n: u64) -> Event {
        Event::new(
            uuid::Uuid::now_v7(),
            n as i64,
            StdArc::from("src"),
            PartitionId::new(part),
            Bytes::from(format!("p{part}-{n}")),
        )
    }

    struct VecSource {
        batches: std::vec::IntoIter<Vec<Event>>,
        kind: SourceKind,
    }
    impl Source for VecSource {
        fn next_batch(
            &mut self,
        ) -> impl std::future::Future<Output = Result<Vec<Event>, AeonError>> + Send {
            let b = self.batches.next().unwrap_or_default();
            async move { Ok(b) }
        }
        fn source_kind(&self) -> SourceKind {
            self.kind
        }
    }

    fn registry() -> PipelineL2Registry {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        PipelineL2Registry::new(L2BodyStoreConfig {
            root: Some(root),
            segment_bytes: 4096,
        })
    }

    #[tokio::test]
    async fn pull_source_is_passthrough() {
        let reg = registry();
        let src = VecSource {
            batches: vec![vec![ev(0, 0), ev(0, 1)]].into_iter(),
            kind: SourceKind::Pull,
        };
        let mut adapter = L2WritingSource::new(src, "pipe", reg.clone(), DurabilityMode::OrderedBatch);
        assert!(adapter.is_passthrough());
        let batch = adapter.next_batch().await.unwrap();
        assert_eq!(batch.len(), 2);
        assert!(adapter.take_last_seqs().is_empty());
        assert!(reg.keys().is_empty(), "pull must not touch registry");
    }

    #[tokio::test]
    async fn push_source_writes_l2() {
        let reg = registry();
        let src = VecSource {
            batches: vec![vec![ev(0, 0), ev(0, 1), ev(1, 0)]].into_iter(),
            kind: SourceKind::Push,
        };
        let mut adapter =
            L2WritingSource::new(src, "pipe", reg.clone(), DurabilityMode::OrderedBatch);
        let batch = adapter.next_batch().await.unwrap();
        assert_eq!(batch.len(), 3);
        let seqs = adapter.take_last_seqs();
        assert_eq!(seqs.len(), 3);
        // Partition 0 got two events with seq 0,1 (within that partition's store).
        let p0: Vec<u64> = seqs
            .iter()
            .filter(|(p, _)| *p == PartitionId::new(0))
            .map(|(_, s)| *s)
            .collect();
        assert_eq!(p0, vec![0, 1]);
        let p1: Vec<u64> = seqs
            .iter()
            .filter(|(p, _)| *p == PartitionId::new(1))
            .map(|(_, s)| *s)
            .collect();
        assert_eq!(p1, vec![0]);
        let keys = reg.keys();
        assert_eq!(keys.len(), 2);
    }

    #[tokio::test]
    async fn durability_none_is_passthrough_even_for_push() {
        let reg = registry();
        let src = VecSource {
            batches: vec![vec![ev(0, 0)]].into_iter(),
            kind: SourceKind::Push,
        };
        let mut adapter = L2WritingSource::new(src, "pipe", reg.clone(), DurabilityMode::None);
        assert!(adapter.is_passthrough());
        adapter.next_batch().await.unwrap();
        assert!(reg.keys().is_empty());
    }

    #[tokio::test]
    async fn poll_source_writes_l2() {
        let reg = registry();
        let src = VecSource {
            batches: vec![vec![ev(0, 0)]].into_iter(),
            kind: SourceKind::Poll,
        };
        let mut adapter =
            L2WritingSource::new(src, "pipe", reg.clone(), DurabilityMode::UnorderedBatch);
        adapter.next_batch().await.unwrap();
        assert_eq!(adapter.take_last_seqs().len(), 1);
    }

    #[test]
    fn registry_dedupes_by_key() {
        let reg = registry();
        let a = reg.open("p", PartitionId::new(0)).unwrap();
        let b = reg.open("p", PartitionId::new(0)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let c = reg.open("p", PartitionId::new(1)).unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
    }

    #[test]
    fn ack_tracker_monotonic() {
        let t = AckSeqTracker::new();
        t.register_sink("s1");
        t.record_ack("s1", 5);
        t.record_ack("s1", 3); // stale — ignored
        assert_eq!(t.snapshot().get("s1").copied(), Some(5));
    }

    #[test]
    fn ack_tracker_min_across_sinks() {
        let t = AckSeqTracker::new();
        t.register_sink("a");
        t.register_sink("b");
        assert_eq!(t.min_across_sinks(), Some(0));
        t.record_ack("a", 10);
        t.record_ack("b", 3);
        assert_eq!(t.min_across_sinks(), Some(3));
        t.record_ack("b", 11);
        assert_eq!(t.min_across_sinks(), Some(10));
    }

    #[test]
    fn ack_tracker_no_sink_returns_none() {
        let t = AckSeqTracker::new();
        assert_eq!(t.min_across_sinks(), None);
    }

    #[test]
    fn ack_tracker_snapshot_is_owned() {
        let t = AckSeqTracker::new();
        t.register_sink("s");
        t.record_ack("s", 7);
        let snap = t.snapshot();
        // Mutate after snapshot — snap unaffected.
        t.record_ack("s", 99);
        assert_eq!(snap.get("s").copied(), Some(7));
    }
}
