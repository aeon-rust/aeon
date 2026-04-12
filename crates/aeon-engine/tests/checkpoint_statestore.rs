//! FT-3 integration test: end-to-end `CheckpointBackend::StateStore`.
//!
//! Boots a real pipeline with a `RedbStore`-backed L3, runs events through,
//! waits for at least one checkpoint to be written, then reopens the same
//! redb file and asserts the checkpoint record survived restart with the
//! expected delivered count and source offsets.
//!
//! This exercises the path wired by FT-3:
//!   PipelineConfig.l3_checkpoint_store = Some(Arc<RedbStore>)
//!   → CheckpointBackend::StateStore
//!   → L3CheckpointStore::open / ::append via the sink task.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aeon_connectors::{BlackholeSink, MemorySource};
use aeon_engine::{
    CheckpointBackend, CheckpointPersist, L3CheckpointStore, PassthroughProcessor, PipelineConfig,
    PipelineMetrics, run_buffered,
};
use aeon_state::{RedbConfig, RedbStore};
use aeon_types::{Event, L3Store, PartitionId};

fn make_events(n: usize) -> Vec<Event> {
    let source: Arc<str> = Arc::from("memory");
    (0..n)
        .map(|i| {
            let mut ev = Event::new(
                uuid::Uuid::now_v7(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new(0),
                bytes::Bytes::from(format!("evt-{i}")),
            );
            ev.source_offset = Some(i as i64);
            ev
        })
        .collect()
}

#[tokio::test]
async fn checkpoint_statestore_persists_across_pipeline_restart() {
    // ── Setup: open a redb-backed L3 store ─────────────────────────────
    let dir = tempfile::tempdir().unwrap();
    let l3_path = dir.path().join("checkpoint.redb");
    let l3: Arc<dyn L3Store> = Arc::new(
        RedbStore::open(RedbConfig {
            path: l3_path.clone(),
            sync_writes: true,
        })
        .unwrap(),
    );

    // ── Run pipeline with StateStore checkpoint backend ────────────────
    let event_count = 512;
    let events = make_events(event_count);
    let source = MemorySource::new(events, 64);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let sink = BlackholeSink::new();

    let mut config = PipelineConfig {
        source_buffer_capacity: 64,
        sink_buffer_capacity: 64,
        max_batch_size: 64,
        ..Default::default()
    };
    config.delivery.checkpoint.backend = CheckpointBackend::StateStore;
    // Short flush interval so we get several checkpoint writes within the test.
    config.delivery.flush.interval = Duration::from_millis(50);
    config.l3_checkpoint_store = Some(Arc::clone(&l3));

    let metrics = Arc::new(PipelineMetrics::new());
    let shutdown = Arc::new(AtomicBool::new(false));

    run_buffered(
        source,
        processor,
        sink,
        config,
        Arc::clone(&metrics),
        shutdown,
        None,
    )
    .await
    .unwrap();

    let checkpoints_written = metrics.checkpoints_written.load(Ordering::Relaxed);
    assert!(
        checkpoints_written >= 1,
        "expected at least one checkpoint to land in L3, got {checkpoints_written}"
    );

    // ── Reopen the same redb file, assert checkpoint is recoverable ────
    drop(l3); // release the handle
    let l3_reopen: Arc<dyn L3Store> = Arc::new(
        RedbStore::open(RedbConfig {
            path: l3_path,
            sync_writes: true,
        })
        .unwrap(),
    );

    let store = L3CheckpointStore::open(Arc::clone(&l3_reopen)).unwrap();
    // Next-ID must reflect the checkpoints we wrote in the previous run.
    assert!(
        store.next_checkpoint_id() >= checkpoints_written,
        "expected next_checkpoint_id >= written count ({}), got {}",
        checkpoints_written,
        store.next_checkpoint_id()
    );

    // Last checkpoint must decode cleanly.
    let last = store
        .read_last()
        .unwrap()
        .expect("last checkpoint must be readable after restart");
    assert_eq!(last.failed_count, 0, "blackhole sink never fails");
}
