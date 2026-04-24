//! CL-6c.3 — End-to-end handover integration test over real QUIC.
//!
//! Exercises the full target-side handover sequence on a single
//! connection:
//!   1. `drive_partition_transfer` — bulk L2 sync (CL-6a)
//!   2. `drive_poh_chain_transfer` — PoH chain state snapshot (CL-6b)
//!   3. `drive_partition_cutover`  — drain + freeze handshake (CL-6c)
//!
//! The server-side dispatcher peeks the first frame's `MessageType` and
//! routes to the matching service — same shape as `server::serve()` but
//! without the Raft plumbing so the test stays focused on transport +
//! orchestration composition.

#![cfg(feature = "cluster")]
#![cfg(feature = "auto-tls")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use aeon_cluster::transfer::{TransferState, TransferTracker};
use aeon_cluster::transport::cutover::{
    CutoverCoordinator, CutoverOffsets, drive_partition_cutover,
    serve_partition_cutover_with_request,
};
use aeon_cluster::transport::endpoint::QuicEndpoint;
use aeon_cluster::transport::framing::{self, MessageType};
use aeon_cluster::transport::partition_transfer::{
    ChunkIter, PartitionTransferProvider, drive_partition_transfer,
    serve_partition_transfer_with_request,
};
use aeon_cluster::transport::poh_transfer::{
    PohChainProvider, drive_poh_chain_transfer, serve_poh_chain_transfer_with_request,
};
use aeon_cluster::transport::tls::dev_quic_configs_insecure;
use aeon_cluster::types::{
    NodeAddress, PartitionCutoverRequest, PartitionTransferRequest, PohChainTransferRequest,
};
use aeon_crypto::poh::{PohChain, PohChainState};
use aeon_types::{AeonError, PartitionId, SegmentChunk, SegmentEntry, SegmentManifest};
use bytes::Bytes;

const MAX_RECENT: usize = 64;

// ── Providers ────────────────────────────────────────────────────────

struct StubTransferProvider {
    manifest: SegmentManifest,
    chunks: Vec<SegmentChunk>,
}

impl PartitionTransferProvider for StubTransferProvider {
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

struct LivePohProvider {
    chain: std::sync::Mutex<PohChain>,
}

impl PohChainProvider for LivePohProvider {
    fn export_state<'a>(
        &'a self,
        _req: &'a PohChainTransferRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<u8>, AeonError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let chain = self
                .chain
                .lock()
                .map_err(|e| AeonError::state(format!("poh chain mutex poisoned: {e}")))?;
            chain
                .export_state()
                .to_bytes()
                .map_err(|e| AeonError::Serialization {
                    message: format!("PohChainState::to_bytes: {e}"),
                    source: None,
                })
        })
    }
}

struct StubCutoverCoordinator {
    calls: AtomicU32,
    offsets: CutoverOffsets,
    /// Captured request from the most recent call — lets tests verify
    /// the source sees the pipeline/partition the target asked about.
    seen: std::sync::Mutex<Option<PartitionCutoverRequest>>,
}

impl CutoverCoordinator for StubCutoverCoordinator {
    fn drain_and_freeze<'a>(
        &'a self,
        req: &'a PartitionCutoverRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CutoverOffsets, AeonError>> + Send + 'a>,
    > {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let offsets = self.offsets;
        let req = req.clone();
        Box::pin(async move {
            *self
                .seen
                .lock()
                .map_err(|e| AeonError::state(format!("lock: {e}")))? = Some(req);
            Ok(offsets)
        })
    }
}

// ── Dispatching server ──────────────────────────────────────────────

/// Minimal server that dispatches the three CL-6 message types — same
/// routing shape as `server::serve()` but without Raft.
async fn run_handover_server(
    server: Arc<QuicEndpoint>,
    transfer: Arc<StubTransferProvider>,
    poh: Arc<LivePohProvider>,
    cutover: Arc<StubCutoverCoordinator>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let incoming = tokio::select! {
            i = server.accept() => match i {
                Some(i) => i,
                None => break,
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => continue,
        };
        let transfer = Arc::clone(&transfer);
        let poh = Arc::clone(&poh);
        let cutover = Arc::clone(&cutover);
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => return,
            };
            loop {
                let (mut send, mut recv) = match conn.accept_bi().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let transfer = Arc::clone(&transfer);
                let poh = Arc::clone(&poh);
                let cutover = Arc::clone(&cutover);
                tokio::spawn(async move {
                    let (mt, payload) = match framing::read_frame(&mut recv).await {
                        Ok(f) => f,
                        Err(_) => return,
                    };
                    match mt {
                        MessageType::PartitionTransferRequest => {
                            let req: PartitionTransferRequest =
                                match bincode::deserialize(&payload) {
                                    Ok(r) => r,
                                    Err(_) => return,
                                };
                            let _ = serve_partition_transfer_with_request(
                                &*transfer,
                                send,
                                &req,
                            )
                            .await;
                        }
                        MessageType::PohChainTransferRequest => {
                            let req: PohChainTransferRequest =
                                match bincode::deserialize(&payload) {
                                    Ok(r) => r,
                                    Err(_) => return,
                                };
                            let _ = serve_poh_chain_transfer_with_request(
                                &*poh,
                                send,
                                &req,
                            )
                            .await;
                        }
                        MessageType::PartitionCutoverRequest => {
                            let req: PartitionCutoverRequest =
                                match bincode::deserialize(&payload) {
                                    Ok(r) => r,
                                    Err(_) => return,
                                };
                            let _ = serve_partition_cutover_with_request(
                                &*cutover,
                                send,
                                &req,
                            )
                            .await;
                        }
                        _ => {
                            let _ = send.finish();
                        }
                    }
                });
            }
        });
    }
}

// ── The test ────────────────────────────────────────────────────────

#[tokio::test]
async fn full_handover_walks_bulksync_poh_and_cutover_over_real_quic() {
    // Source-side state: a PoH chain with 3 real batches + two L2
    // segments to bulk-sync. The payload values are arbitrary; what
    // matters is that all three services are hit and the target's
    // post-handover state matches expectations.
    let partition = PartitionId::new(7);
    let mut source_chain = PohChain::new(partition, MAX_RECENT);
    for i in 0..3u64 {
        let a = format!("e{i}a");
        let b = format!("e{i}b");
        let payloads: [&[u8]; 2] = [a.as_bytes(), b.as_bytes()];
        let ts = 1_700_000_000_000_000_000_i64 + (i as i64 * 1_000_000);
        source_chain.append_batch(&payloads, ts, None).unwrap();
    }
    let expected_poh_hash = *source_chain.current_hash();
    let expected_poh_seq = source_chain.sequence();

    // L2 payloads — two segments totalling 8 bytes.
    let data0 = Bytes::from_static(b"aeon");
    let data1 = Bytes::from_static(b"-fts");
    let transfer = Arc::new(StubTransferProvider {
        manifest: SegmentManifest {
            entries: vec![
                SegmentEntry {
                    start_seq: 0,
                    size_bytes: data0.len() as u64,
                    crc32: 0x11111111,
                },
                SegmentEntry {
                    start_seq: 100,
                    size_bytes: data1.len() as u64,
                    crc32: 0x22222222,
                },
            ],
        },
        chunks: vec![
            SegmentChunk {
                start_seq: 0,
                offset: 0,
                data: data0.clone(),
                is_last: true,
            },
            SegmentChunk {
                start_seq: 100,
                offset: 0,
                data: data1.clone(),
                is_last: true,
            },
        ],
    });
    let expected_total_bytes = transfer.manifest.total_bytes();

    let poh = Arc::new(LivePohProvider {
        chain: std::sync::Mutex::new(source_chain),
    });
    let cutover = Arc::new(StubCutoverCoordinator {
        calls: AtomicU32::new(0),
        offsets: CutoverOffsets {
            final_source_offset: 5_555,
            final_poh_sequence: expected_poh_seq,
        },
        seen: std::sync::Mutex::new(None),
    });

    // Stand up the server with all three services wired.
    let (server_cfg, client_cfg) = dev_quic_configs_insecure();
    let server = Arc::new(
        QuicEndpoint::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_cfg.clone(),
            client_cfg.clone(),
        )
        .unwrap(),
    );
    let server_addr = server.local_addr().unwrap();

    let shutdown = Arc::new(AtomicBool::new(false));
    let server_task = tokio::spawn(run_handover_server(
        Arc::clone(&server),
        Arc::clone(&transfer),
        Arc::clone(&poh),
        Arc::clone(&cutover),
        Arc::clone(&shutdown),
    ));

    // Client / target side.
    let client = QuicEndpoint::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_cfg.clone(),
        client_cfg.clone(),
    )
    .unwrap();
    let target = NodeAddress::new("127.0.0.1", server_addr.port());
    let conn = client.connect(99, &target).await.unwrap();

    let mut tracker = TransferTracker::new(partition);

    // Phase 1: Bulk sync (Idle → BulkSync → Cutover).
    let mut received_bytes: u64 = 0;
    let xfer_req = PartitionTransferRequest {
        pipeline: "pl-handover".to_string(),
        partition,
    };
    drive_partition_transfer(&mut tracker, 11, 22, &conn, &xfer_req, None, |chunk| {
        received_bytes += chunk.data.len() as u64;
        Ok(())
    })
    .await
    .expect("bulk sync must succeed");
    assert_eq!(received_bytes, expected_total_bytes);
    assert!(
        matches!(
            tracker.state,
            TransferState::Cutover { source: 11, target: 22 }
        ),
        "tracker must be Cutover{{11,22}} after bulk sync, got {:?}",
        tracker.state
    );

    // Phase 2: PoH chain transfer — target installs the chain.
    let poh_req = PohChainTransferRequest {
        pipeline: "pl-handover".to_string(),
        partition,
    };
    let mut restored_chain: Option<PohChain> = None;
    drive_poh_chain_transfer(&conn, &poh_req, |bytes| {
        let state = PohChainState::from_bytes(&bytes).map_err(|e| AeonError::Serialization {
            message: format!("PohChainState::from_bytes: {e}"),
            source: None,
        })?;
        restored_chain = Some(PohChain::from_state(state, MAX_RECENT));
        Ok(())
    })
    .await
    .expect("poh transfer must succeed");
    let restored = restored_chain.expect("callback populates chain");
    assert_eq!(restored.current_hash(), &expected_poh_hash);
    assert_eq!(restored.sequence(), expected_poh_seq);

    // Phase 3: Cutover handshake — source drains + freezes, returns
    // watermarks. Tracker must remain in Cutover.
    let cut_req = PartitionCutoverRequest {
        pipeline: "pl-handover".to_string(),
        partition,
    };
    let offsets = drive_partition_cutover(&mut tracker, &conn, &cut_req)
        .await
        .expect("cutover handshake must succeed");
    assert_eq!(offsets.final_source_offset, 5_555);
    assert_eq!(offsets.final_poh_sequence, expected_poh_seq);
    assert!(
        matches!(
            tracker.state,
            TransferState::Cutover { source: 11, target: 22 }
        ),
        "tracker must remain Cutover after handshake, got {:?}",
        tracker.state
    );

    // drain_and_freeze must have been invoked exactly once and seen
    // the same pipeline+partition the target asked about.
    assert_eq!(
        cutover.calls.load(Ordering::Relaxed),
        1,
        "coordinator must be invoked exactly once across the full handover"
    );
    let seen = cutover.seen.lock().unwrap().clone().unwrap();
    assert_eq!(seen.pipeline, "pl-handover");
    assert_eq!(seen.partition, partition);

    // Final state: target has PoH head + final offsets; caller would
    // now drive the Raft ownership flip and call tracker.complete().
    // That transition is exercised by the unit tests for TransferTracker
    // (`transfer_happy_path`) and isn't the subject of this integration
    // test — cluster crate stays openraft-commit-agnostic.
    let new_owner = tracker.complete().unwrap();
    assert_eq!(new_owner, 22);
    assert!(matches!(
        tracker.state,
        TransferState::Complete { new_owner: 22 }
    ));

    shutdown.store(true, Ordering::Relaxed);
    client.close();
    server.close();
    let _ = server_task.await;
}
