//! CL-6b.3 — Real-network PoH chain transfer integration test.
//!
//! Exercises the full round-trip: populate a `PohChain` on the source
//! node with a real sequence of event batches, transfer the chain state
//! over QUIC to the target node via `drive_poh_chain_transfer`, rebuild
//! the chain from the received bytes via `PohChain::from_state`, and
//! verify that the restored chain is functionally equivalent — same
//! current hash, sequence, MMR root — and can continue appending.

#![cfg(feature = "cluster")]
#![cfg(feature = "auto-tls")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aeon_cluster::transport::endpoint::QuicEndpoint;
use aeon_cluster::transport::poh_transfer::{
    PohChainProvider, drive_poh_chain_transfer, serve_poh_chain_transfer_stream,
};
use aeon_cluster::transport::tls::dev_quic_configs_insecure;
use aeon_cluster::types::{NodeAddress, PohChainTransferRequest};
use aeon_crypto::poh::{PohChain, PohChainState};
use aeon_types::{AeonError, PartitionId};

const MAX_RECENT: usize = 64;

/// Provider that exports the live `PohChainState` of a real chain
/// protected by a Mutex (single-partition harness).
struct LivePohProvider {
    chain: std::sync::Mutex<PohChain>,
}

impl PohChainProvider for LivePohProvider {
    fn export_state<'a>(
        &'a self,
        _req: &'a PohChainTransferRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, AeonError>> + Send + 'a>>
    {
        Box::pin(async move {
            let chain = self
                .chain
                .lock()
                .map_err(|e| AeonError::state(format!("poh chain mutex poisoned: {e}")))?;
            let state = chain.export_state();
            state.to_bytes().map_err(|e| AeonError::Serialization {
                message: format!("PohChainState::to_bytes: {e}"),
                source: None,
            })
        })
    }
}

async fn run_server(
    server: Arc<QuicEndpoint>,
    provider: Arc<LivePohProvider>,
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
        let provider = Arc::clone(&provider);
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => return,
            };
            loop {
                let (send, recv) = match conn.accept_bi().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let provider = Arc::clone(&provider);
                tokio::spawn(async move {
                    let _ = serve_poh_chain_transfer_stream(&*provider, send, recv).await;
                });
            }
        });
    }
}

#[tokio::test]
async fn poh_chain_transfers_over_real_quic_and_preserves_head() {
    // --- Build a real PoH chain on the "source" side with several batches.
    let partition = PartitionId::new(42);
    let mut source_chain = PohChain::new(partition, MAX_RECENT);

    // Append 5 real batches with deterministic payloads + timestamps.
    for i in 0..5u64 {
        let p0 = format!("evt-{i}-a");
        let p1 = format!("evt-{i}-b");
        let p2 = format!("evt-{i}-c");
        let payloads: [&[u8]; 3] = [p0.as_bytes(), p1.as_bytes(), p2.as_bytes()];
        let ts = 1_700_000_000_000_000_000_i64 + (i as i64 * 1_000_000);
        let entry = source_chain
            .append_batch(&payloads, ts, None)
            .expect("append should succeed for non-empty payloads");
        assert_eq!(entry.sequence, i);
    }
    let expected_hash = *source_chain.current_hash();
    let expected_seq = source_chain.sequence();
    let expected_mmr_root = source_chain.mmr_root();
    assert_eq!(expected_seq, 5);

    // --- Stand up a real QUIC server holding the chain as the provider.
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

    let provider = Arc::new(LivePohProvider {
        chain: std::sync::Mutex::new(source_chain),
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_task = tokio::spawn(run_server(
        Arc::clone(&server),
        Arc::clone(&provider),
        Arc::clone(&shutdown),
    ));

    // --- Client connects and drives the transfer.
    let client = QuicEndpoint::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_cfg.clone(),
        client_cfg.clone(),
    )
    .unwrap();
    let target = NodeAddress::new("127.0.0.1", server_addr.port());
    let conn = client.connect(99, &target).await.unwrap();

    let req = PohChainTransferRequest {
        pipeline: "pl-integration".to_string(),
        partition,
    };

    let mut restored_chain: Option<PohChain> = None;
    drive_poh_chain_transfer(&conn, &req, |bytes| {
        let state = PohChainState::from_bytes(&bytes).map_err(|e| AeonError::Serialization {
            message: format!("PohChainState::from_bytes: {e}"),
            source: None,
        })?;
        restored_chain = Some(PohChain::from_state(state, MAX_RECENT));
        Ok(())
    })
    .await
    .expect("drive_poh_chain_transfer should succeed over real QUIC");

    // --- Verify the target-side chain is functionally equivalent.
    let restored = restored_chain.expect("callback must populate the chain");
    assert_eq!(restored.partition(), partition);
    assert_eq!(
        restored.current_hash(),
        &expected_hash,
        "restored chain head must match source chain head byte-for-byte"
    );
    assert_eq!(
        restored.sequence(),
        expected_seq,
        "restored sequence must match source sequence"
    );
    assert_eq!(
        restored.mmr_root(),
        expected_mmr_root,
        "restored MMR root must match source MMR root"
    );

    // --- Continuity: appending on the restored chain produces the same
    // next-hash as appending on the source would have.
    let mut restored_for_continue = restored;
    let mut source_clone = {
        let state_bytes = provider
            .export_state(&req)
            .await
            .expect("re-export must succeed");
        let state = PohChainState::from_bytes(&state_bytes).unwrap();
        PohChain::from_state(state, MAX_RECENT)
    };
    let payload: [&[u8]; 1] = [b"post-transfer-event"];
    let ts = 1_700_000_005_000_000_000_i64;
    let src_entry = source_clone.append_batch(&payload, ts, None).unwrap();
    let tgt_entry = restored_for_continue
        .append_batch(&payload, ts, None)
        .unwrap();
    assert_eq!(
        src_entry.hash, tgt_entry.hash,
        "post-transfer append must produce identical next hash on both sides"
    );
    assert_eq!(src_entry.sequence, tgt_entry.sequence);
    assert_eq!(src_entry.merkle_root, tgt_entry.merkle_root);

    shutdown.store(true, Ordering::Relaxed);
    client.close();
    server.close();
    let _ = server_task.await;
}

#[tokio::test]
async fn poh_transfer_of_fresh_chain_yields_genesis_state() {
    // A chain with zero appends still has a valid state — partition id +
    // genesis hash + sequence=0 + empty MMR. Exercise that edge.
    let partition = PartitionId::new(0);
    let source_chain = PohChain::new(partition, MAX_RECENT);
    let expected_hash = *source_chain.current_hash();
    let expected_seq = source_chain.sequence();

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

    let provider = Arc::new(LivePohProvider {
        chain: std::sync::Mutex::new(source_chain),
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_task = tokio::spawn(run_server(
        Arc::clone(&server),
        Arc::clone(&provider),
        Arc::clone(&shutdown),
    ));

    let client = QuicEndpoint::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_cfg.clone(),
        client_cfg.clone(),
    )
    .unwrap();
    let target = NodeAddress::new("127.0.0.1", server_addr.port());
    let conn = client.connect(99, &target).await.unwrap();

    let req = PohChainTransferRequest {
        pipeline: "pl-fresh".to_string(),
        partition,
    };

    let mut restored_chain: Option<PohChain> = None;
    drive_poh_chain_transfer(&conn, &req, |bytes| {
        let state = PohChainState::from_bytes(&bytes).map_err(|e| AeonError::Serialization {
            message: format!("PohChainState::from_bytes: {e}"),
            source: None,
        })?;
        restored_chain = Some(PohChain::from_state(state, MAX_RECENT));
        Ok(())
    })
    .await
    .unwrap();

    let restored = restored_chain.unwrap();
    assert_eq!(restored.current_hash(), &expected_hash);
    assert_eq!(restored.sequence(), expected_seq);
    assert_eq!(restored.sequence(), 0);

    shutdown.store(true, Ordering::Relaxed);
    client.close();
    server.close();
    let _ = server_task.await;
}
