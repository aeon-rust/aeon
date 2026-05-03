//! P1.1h — 3-node loopback E2E for the CL-6 partition transfer driver.
//!
//! Walks one real partition handover end-to-end across three QUIC-linked
//! Raft nodes on `127.0.0.1`:
//!
//!   1. Form the cluster: three Raft nodes, one leader, replicated log.
//!   2. Seed: leader proposes `AssignPartition(P0 -> node 2)` so every
//!      node's partition table has `Owned(2)`.
//!   3. Wire CL-6 providers *on node 2 only* — it is the source, so it
//!      serves the bulk segment / PoH chain / cutover streams.
//!   4. Leader proposes `BeginTransfer(P0, source=2, target=3)`.
//!   5. Node 3 runs a `PartitionTransferDriver` backed by a
//!      `RaftNodeResolver` (real membership). `drive_one` pulls the
//!      payload off node 2, invokes the local installers, completes the
//!      cutover handshake, and proposes `CompleteTransfer` through Raft.
//!   6. Assert every replica observes `PartitionOwnership::Owned(3)`
//!      after the Raft apply propagates — the "post-cutover ownership
//!      flip" acceptance criterion for P1.1h.
//!
//! Write-freeze is exercised indirectly via the cutover coordinator: the
//! coordinator's `drain_and_freeze` is the interface point where a real
//! engine would slam the `WriteGate`. The test asserts that the source
//! coordinator saw exactly one call, which is the cluster-layer contract
//! we can test without pulling the engine crate in.

#![cfg(feature = "cluster")]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use aeon_cluster::raft_config::AeonRaftConfig;
use aeon_cluster::store::{MemLogStore, SharedClusterState, StateMachineStore};
use aeon_cluster::transport::cutover::{CutoverCoordinator, CutoverOffsets};
use aeon_cluster::transport::endpoint::QuicEndpoint;
use aeon_cluster::transport::network::QuicNetworkFactory;
use aeon_cluster::transport::partition_transfer::{ChunkIter, PartitionTransferProvider};
use aeon_cluster::transport::poh_transfer::PohChainProvider;
use aeon_cluster::transport::server;
use aeon_cluster::transport::tls::dev_quic_configs_insecure;
use aeon_cluster::{
    ClusterRequest, ClusterResponse, NodeAddress, NodeResolver, PartitionCutoverRequest,
    PartitionOwnership, PartitionTransferDriver, PartitionTransferEnd, PartitionTransferRequest,
    PohChainInstaller, PohChainTransferRequest, RaftNodeResolver, SegmentInstaller,
};
use aeon_types::{AeonError, PartitionId, SegmentChunk, SegmentEntry, SegmentManifest};
use bytes::Bytes;
use openraft::{Config, Raft};

// ── QUIC / Raft helpers ────────────────────────────────────────────────

fn dev_quic_configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
    dev_quic_configs_insecure()
}

fn create_endpoint(
    server_cfg: quinn::ServerConfig,
    client_cfg: quinn::ClientConfig,
) -> Arc<QuicEndpoint> {
    Arc::new(
        QuicEndpoint::bind(
            "127.0.0.1:0".parse().expect("parse loopback"),
            server_cfg,
            client_cfg,
        )
        .expect("bind quic endpoint"),
    )
}

/// Per-node handles we keep alive for the duration of the test.
struct NodeHandles {
    raft: Raft<AeonRaftConfig>,
    endpoint: Arc<QuicEndpoint>,
    shared: SharedClusterState,
}

/// Spin up one Raft node with its own QUIC endpoint + state machine.
/// Returns the node handles; the caller wires `server::serve` separately
/// so the source node can attach CL-6 providers while the other two run
/// with `None` providers (they only need the Raft RPC surface).
async fn build_node(
    node_id: u64,
    server_cfg: quinn::ServerConfig,
    client_cfg: quinn::ClientConfig,
) -> NodeHandles {
    let endpoint = create_endpoint(server_cfg, client_cfg);

    let raft_config = Arc::new(Config {
        cluster_name: "aeon-p1.1h".to_string(),
        heartbeat_interval: 150,
        election_timeout_min: 400,
        election_timeout_max: 800,
        ..Config::default()
    });

    let log_store = MemLogStore::new();
    let state_machine = StateMachineStore::new();
    let shared = state_machine.shared_state();
    let network = QuicNetworkFactory::new(Arc::clone(&endpoint));

    let raft = Raft::new(node_id, raft_config, network, log_store, state_machine)
        .await
        .expect("raft new");

    NodeHandles {
        raft,
        endpoint,
        shared,
    }
}

async fn wait_for_leader(raft: &Raft<AeonRaftConfig>) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(leader) = raft.current_leader().await {
            return leader;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no leader elected within 10s");
}

/// Propose on whichever node is currently leader, refreshing the leader
/// on each retry. Two binaries running in parallel (see the cluster test
/// suite) put the loopback port pool + CPU under enough contention that
/// a transient re-election can happen between `wait_for_leader` and the
/// first client_write — this helper absorbs that by re-asking each node
/// for `current_leader` and retrying up to `deadline`.
async fn propose_on_leader(
    nodes: &[(u64, &NodeHandles)],
    req: ClusterRequest,
    deadline: Duration,
) -> ClusterResponse {
    let end = Instant::now() + deadline;
    let mut last_err: Option<String> = None;
    while Instant::now() < end {
        let mut leader_id: Option<u64> = None;
        for (_, n) in nodes {
            if let Some(id) = n.raft.current_leader().await {
                leader_id = Some(id);
                break;
            }
        }
        if let Some(id) = leader_id {
            if let Some((_, leader_node)) = nodes.iter().find(|(nid, _)| *nid == id) {
                match leader_node.raft.client_write(req.clone()).await {
                    Ok(resp) => return resp.data,
                    Err(e) => last_err = Some(e.to_string()),
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "propose_on_leader timed out after {:?}; last error: {:?}",
        deadline, last_err
    );
}

/// Poll every replica until it observes the expected ownership for
/// `partition` (or time out). Returns the final snapshot observed on
/// each node so the caller can make hard assertions with a clear diff.
async fn wait_for_ownership(
    nodes: &[(u64, &NodeHandles)],
    partition: PartitionId,
    expected: PartitionOwnership,
    deadline: Duration,
) -> Vec<(u64, Option<PartitionOwnership>)> {
    let end = Instant::now() + deadline;
    loop {
        let mut latest = Vec::with_capacity(nodes.len());
        let mut all_match = true;
        for (id, n) in nodes {
            let snap = n.shared.read().await.clone();
            let got = snap.partition_table.get(partition).cloned();
            if got.as_ref() != Some(&expected) {
                all_match = false;
            }
            latest.push((*id, got));
        }
        if all_match {
            return latest;
        }
        if Instant::now() >= end {
            return latest;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ── CL-6 providers (source side — node 2) ──────────────────────────────

fn sample_manifest_and_chunks() -> (SegmentManifest, Vec<SegmentChunk>) {
    let d0 = Bytes::from_static(b"hello");
    let d1 = Bytes::from_static(b"aeon");
    let manifest = SegmentManifest {
        entries: vec![
            SegmentEntry {
                start_seq: 0,
                size_bytes: d0.len() as u64,
                crc32: 0xAAAA_BBBB,
            },
            SegmentEntry {
                start_seq: 100,
                size_bytes: d1.len() as u64,
                crc32: 0xCCCC_DDDD,
            },
        ],
    };
    let chunks = vec![
        SegmentChunk {
            start_seq: 0,
            offset: 0,
            data: d0,
            is_last: true,
        },
        SegmentChunk {
            start_seq: 100,
            offset: 0,
            data: d1,
            is_last: true,
        },
    ];
    (manifest, chunks)
}

struct FixedBulkProvider {
    manifest: SegmentManifest,
    chunks: Vec<SegmentChunk>,
}

impl PartitionTransferProvider for FixedBulkProvider {
    fn serve(
        &self,
        _req: &PartitionTransferRequest,
    ) -> Result<(SegmentManifest, ChunkIter<'static>), AeonError> {
        let manifest = self.manifest.clone();
        let chunks = self.chunks.clone();
        let mut idx = 0usize;
        let iter: ChunkIter<'static> = Box::new(move || {
            if idx >= chunks.len() {
                return Ok(None);
            }
            let c = chunks[idx].clone();
            idx += 1;
            Ok(Some(c))
        });
        Ok((manifest, iter))
    }
}

struct FixedPohProvider {
    payload: Vec<u8>,
}

impl PohChainProvider for FixedPohProvider {
    fn export_state<'a>(
        &'a self,
        _req: &'a PohChainTransferRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, AeonError>> + Send + 'a>> {
        let payload = self.payload.clone();
        Box::pin(async move { Ok(payload) })
    }
}

/// Cutover coordinator stub. Records the single drain+freeze call so the
/// test can assert the write-freeze interface was invoked exactly once
/// on the source — the cluster-layer proxy for "engine WriteGate closed".
struct RecordingCutoverCoordinator {
    offsets: CutoverOffsets,
    calls: AtomicU32,
}

impl CutoverCoordinator for RecordingCutoverCoordinator {
    fn drain_and_freeze<'a>(
        &'a self,
        _req: &'a PartitionCutoverRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CutoverOffsets, AeonError>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let offsets = self.offsets;
        Box::pin(async move { Ok(offsets) })
    }
}

// ── CL-6 installers (target side — node 3) ─────────────────────────────

#[derive(Default)]
struct RecordingSegmentInstaller {
    manifest: StdMutex<Option<SegmentManifest>>,
    chunks: StdMutex<Vec<SegmentChunk>>,
    ended: StdMutex<Option<PartitionTransferEnd>>,
}

impl SegmentInstaller for RecordingSegmentInstaller {
    fn begin(
        &self,
        _req: &PartitionTransferRequest,
        manifest: SegmentManifest,
    ) -> Result<(), AeonError> {
        *self
            .manifest
            .lock()
            .map_err(|e| AeonError::state(format!("lock: {e}")))? = Some(manifest);
        Ok(())
    }

    fn apply_chunk(
        &self,
        _req: &PartitionTransferRequest,
        chunk: SegmentChunk,
    ) -> Result<(), AeonError> {
        self.chunks
            .lock()
            .map_err(|e| AeonError::state(format!("lock: {e}")))?
            .push(chunk);
        Ok(())
    }

    fn finish(
        &self,
        _req: &PartitionTransferRequest,
        end: PartitionTransferEnd,
    ) -> Result<(), AeonError> {
        *self
            .ended
            .lock()
            .map_err(|e| AeonError::state(format!("lock: {e}")))? = Some(end);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingPohInstaller {
    bytes: StdMutex<Option<Vec<u8>>>,
}

impl PohChainInstaller for RecordingPohInstaller {
    fn install(
        &self,
        _req: &PohChainTransferRequest,
        state_bytes: Vec<u8>,
    ) -> Result<(), AeonError> {
        *self
            .bytes
            .lock()
            .map_err(|e| AeonError::state(format!("lock: {e}")))? = Some(state_bytes);
        Ok(())
    }
}

// ── Test: 3-node E2E partition handover ────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_partition_transfer_walks_cl6_and_flips_ownership() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    // Build three Raft nodes. Each owns its own QUIC endpoint.
    let n1 = build_node(1, server_cfg.clone(), client_cfg.clone()).await;
    let n2 = build_node(2, server_cfg.clone(), client_cfg.clone()).await;
    let n3 = build_node(3, server_cfg.clone(), client_cfg.clone()).await;

    let addr1 = n1.endpoint.local_addr().expect("ep1 local_addr");
    let addr2 = n2.endpoint.local_addr().expect("ep2 local_addr");
    let addr3 = n3.endpoint.local_addr().expect("ep3 local_addr");

    let shutdown = Arc::new(AtomicBool::new(false));

    // Node 1 & 3: plain Raft RPC server. Node 2: plain Raft server plus
    // CL-6 providers so it can serve the partition-transfer pull.
    let (manifest, chunks) = sample_manifest_and_chunks();
    let bulk_provider: Arc<dyn PartitionTransferProvider> =
        Arc::new(FixedBulkProvider { manifest, chunks });
    let poh_provider: Arc<dyn PohChainProvider> = Arc::new(FixedPohProvider {
        payload: b"poh-chain-state-bytes-3node".to_vec(),
    });
    let cutover_coord = Arc::new(RecordingCutoverCoordinator {
        offsets: CutoverOffsets {
            final_source_offset: 4242,
            final_poh_sequence: 77,
        },
        calls: AtomicU32::new(0),
    });
    let cutover_dyn: Arc<dyn CutoverCoordinator> = cutover_coord.clone();

    {
        let ep = Arc::clone(&n1.endpoint);
        let r = n1.raft.clone();
        let sd = Arc::clone(&shutdown);
        tokio::spawn(async move { server::serve(ep, r, 1, sd, None, None, None).await });
    }
    {
        let ep = Arc::clone(&n2.endpoint);
        let r = n2.raft.clone();
        let sd = Arc::clone(&shutdown);
        let bp = Arc::clone(&bulk_provider);
        let pp = Arc::clone(&poh_provider);
        let cc = Arc::clone(&cutover_dyn);
        tokio::spawn(
            async move { server::serve(ep, r, 2, sd, Some(bp), Some(pp), Some(cc)).await },
        );
    }
    {
        let ep = Arc::clone(&n3.endpoint);
        let r = n3.raft.clone();
        let sd = Arc::clone(&shutdown);
        tokio::spawn(async move { server::serve(ep, r, 3, sd, None, None, None).await });
    }

    // Bring the cluster up.
    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    members.insert(2u64, NodeAddress::new("127.0.0.1", addr2.port()));
    members.insert(3u64, NodeAddress::new("127.0.0.1", addr3.port()));

    n1.raft.initialize(members.clone()).await.expect("init n1");
    n2.raft.initialize(members.clone()).await.expect("init n2");
    n3.raft.initialize(members).await.expect("init n3");

    let _ = wait_for_leader(&n1.raft).await;
    let nodes_ref: [(u64, &NodeHandles); 3] = [(1, &n1), (2, &n2), (3, &n3)];

    // Seed: partition 0 owned by node 2.
    let assign = propose_on_leader(
        &nodes_ref,
        ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 2,
        },
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(assign, ClusterResponse::Ok);

    // Make sure every replica has observed the assignment before kicking
    // off the handover — the driver's AbortTransfer path depends on the
    // source's partition table being populated.
    let seeded = wait_for_ownership(
        &nodes_ref,
        PartitionId::new(0),
        PartitionOwnership::Owned(2),
        Duration::from_secs(5),
    )
    .await;
    for (id, own) in &seeded {
        assert_eq!(
            own.as_ref(),
            Some(&PartitionOwnership::Owned(2)),
            "node {id} never observed Owned(2) after seeding, saw {own:?}",
        );
    }

    // Begin the handover: leader proposes Transferring(P0, 2 → 3).
    let begin = propose_on_leader(
        &nodes_ref,
        ClusterRequest::BeginTransfer {
            partition: PartitionId::new(0),
            source: 2,
            target: 3,
        },
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(begin, ClusterResponse::Ok);

    // Drive the handover on node 3. Use the real RaftNodeResolver so
    // address resolution walks committed membership, not a stub.
    let resolver: Arc<dyn NodeResolver> = Arc::new(RaftNodeResolver::new(n3.raft.clone()));
    let segment_installer = Arc::new(RecordingSegmentInstaller::default());
    let poh_installer = Arc::new(RecordingPohInstaller::default());

    let driver = PartitionTransferDriver::new(
        Arc::clone(&n3.endpoint),
        n3.raft.clone(),
        n3.shared.clone(),
        resolver,
        3,
        "pl",
        segment_installer.clone(),
        poh_installer.clone(),
    );

    driver
        .drive_one(PartitionId::new(0), 2, 3)
        .await
        .expect("drive_one ok");

    // Every replica must observe Owned(3) once CompleteTransfer commits.
    let final_state = wait_for_ownership(
        &nodes_ref,
        PartitionId::new(0),
        PartitionOwnership::Owned(3),
        Duration::from_secs(5),
    )
    .await;
    for (id, own) in &final_state {
        assert_eq!(
            own.as_ref(),
            Some(&PartitionOwnership::Owned(3)),
            "node {id} did not flip to Owned(3) after CompleteTransfer, saw {own:?}",
        );
    }

    // Installer-side hooks must have run on node 3.
    assert!(
        segment_installer.manifest.lock().expect("lock").is_some(),
        "segment installer never saw manifest"
    );
    assert_eq!(
        segment_installer.chunks.lock().expect("lock").len(),
        2,
        "segment installer must see both chunks",
    );
    let end = segment_installer
        .ended
        .lock()
        .expect("lock")
        .clone()
        .expect("end frame");
    assert!(end.success, "end frame must be success=true");
    let poh_bytes = poh_installer
        .bytes
        .lock()
        .expect("lock")
        .clone()
        .expect("poh install");
    assert_eq!(poh_bytes, b"poh-chain-state-bytes-3node");

    // Source cutover coordinator must have been asked to drain+freeze
    // exactly once — this is the cluster-layer interface where a real
    // engine slams its per-partition WriteGate.
    assert_eq!(
        cutover_coord.calls.load(Ordering::Relaxed),
        1,
        "source must have been asked to drain+freeze exactly once",
    );

    // Teardown.
    shutdown.store(true, Ordering::Relaxed);
    let _ = n1.raft.shutdown().await;
    let _ = n2.raft.shutdown().await;
    let _ = n3.raft.shutdown().await;
    n1.endpoint.close();
    n2.endpoint.close();
    n3.endpoint.close();
}

// ── Test: watcher loop drives the handover without a direct drive_one call

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_watcher_picks_up_transferring_entry_and_completes() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    let n1 = build_node(1, server_cfg.clone(), client_cfg.clone()).await;
    let n2 = build_node(2, server_cfg.clone(), client_cfg.clone()).await;
    let n3 = build_node(3, server_cfg.clone(), client_cfg.clone()).await;

    let addr1 = n1.endpoint.local_addr().expect("ep1 local_addr");
    let addr2 = n2.endpoint.local_addr().expect("ep2 local_addr");
    let addr3 = n3.endpoint.local_addr().expect("ep3 local_addr");

    let shutdown = Arc::new(AtomicBool::new(false));

    let (manifest, chunks) = sample_manifest_and_chunks();
    let bulk_provider: Arc<dyn PartitionTransferProvider> =
        Arc::new(FixedBulkProvider { manifest, chunks });
    let poh_provider: Arc<dyn PohChainProvider> = Arc::new(FixedPohProvider {
        payload: b"poh-watcher".to_vec(),
    });
    let cutover_coord = Arc::new(RecordingCutoverCoordinator {
        offsets: CutoverOffsets {
            final_source_offset: 1,
            final_poh_sequence: 1,
        },
        calls: AtomicU32::new(0),
    });
    let cutover_dyn: Arc<dyn CutoverCoordinator> = cutover_coord.clone();

    {
        let ep = Arc::clone(&n1.endpoint);
        let r = n1.raft.clone();
        let sd = Arc::clone(&shutdown);
        tokio::spawn(async move { server::serve(ep, r, 1, sd, None, None, None).await });
    }
    {
        let ep = Arc::clone(&n2.endpoint);
        let r = n2.raft.clone();
        let sd = Arc::clone(&shutdown);
        let bp = Arc::clone(&bulk_provider);
        let pp = Arc::clone(&poh_provider);
        let cc = Arc::clone(&cutover_dyn);
        tokio::spawn(
            async move { server::serve(ep, r, 2, sd, Some(bp), Some(pp), Some(cc)).await },
        );
    }
    {
        let ep = Arc::clone(&n3.endpoint);
        let r = n3.raft.clone();
        let sd = Arc::clone(&shutdown);
        tokio::spawn(async move { server::serve(ep, r, 3, sd, None, None, None).await });
    }

    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    members.insert(2u64, NodeAddress::new("127.0.0.1", addr2.port()));
    members.insert(3u64, NodeAddress::new("127.0.0.1", addr3.port()));

    n1.raft.initialize(members.clone()).await.expect("init n1");
    n2.raft.initialize(members.clone()).await.expect("init n2");
    n3.raft.initialize(members).await.expect("init n3");

    let _ = wait_for_leader(&n1.raft).await;
    let nodes_ref: [(u64, &NodeHandles); 3] = [(1, &n1), (2, &n2), (3, &n3)];

    let _ = propose_on_leader(
        &nodes_ref,
        ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 2,
        },
        Duration::from_secs(10),
    )
    .await;

    // Spin up the driver + watcher BEFORE proposing the transfer so the
    // watch_loop observes the Transferring entry on its first post-wake
    // poll tick (via `notify()`).
    let resolver: Arc<dyn NodeResolver> = Arc::new(RaftNodeResolver::new(n3.raft.clone()));
    let segment_installer = Arc::new(RecordingSegmentInstaller::default());
    let poh_installer = Arc::new(RecordingPohInstaller::default());

    let driver = Arc::new(
        PartitionTransferDriver::new(
            Arc::clone(&n3.endpoint),
            n3.raft.clone(),
            n3.shared.clone(),
            resolver,
            3,
            "pl",
            segment_installer.clone(),
            poh_installer.clone(),
        )
        .with_poll_interval(Duration::from_millis(50)),
    );

    let watcher_shutdown = Arc::new(AtomicBool::new(false));
    let watcher = tokio::spawn({
        let d = Arc::clone(&driver);
        let sd = Arc::clone(&watcher_shutdown);
        async move { d.watch_loop(sd).await }
    });

    // Wait for every replica to observe Owned(2), then kick off the
    // transfer + wake the watcher.
    let _ = wait_for_ownership(
        &nodes_ref,
        PartitionId::new(0),
        PartitionOwnership::Owned(2),
        Duration::from_secs(5),
    )
    .await;

    let _ = propose_on_leader(
        &nodes_ref,
        ClusterRequest::BeginTransfer {
            partition: PartitionId::new(0),
            source: 2,
            target: 3,
        },
        Duration::from_secs(10),
    )
    .await;
    driver.notify();

    // The watcher should drive to completion without any further prods.
    let final_state = wait_for_ownership(
        &nodes_ref,
        PartitionId::new(0),
        PartitionOwnership::Owned(3),
        Duration::from_secs(10),
    )
    .await;
    for (id, own) in &final_state {
        assert_eq!(
            own.as_ref(),
            Some(&PartitionOwnership::Owned(3)),
            "node {id} did not flip to Owned(3) via watcher, saw {own:?}",
        );
    }
    assert_eq!(
        cutover_coord.calls.load(Ordering::Relaxed),
        1,
        "watcher must not double-drive the same Transferring entry",
    );

    watcher_shutdown.store(true, Ordering::Relaxed);
    // `watch_loop` returns once it sees the flag on its next poll.
    let _ = tokio::time::timeout(Duration::from_secs(2), watcher).await;

    shutdown.store(true, Ordering::Relaxed);
    let _ = n1.raft.shutdown().await;
    let _ = n2.raft.shutdown().await;
    let _ = n3.raft.shutdown().await;
    n1.endpoint.close();
    n2.endpoint.close();
    n3.endpoint.close();
}
