//! CL-6c — QUIC request/response for partition cutover handshake.
//!
//! Cutover is the atomic "stop accepting writes for partition X on
//! node A, start on node B" step that follows a successful bulk sync
//! (CL-6a) and PoH chain transfer (CL-6b). The target initiates the
//! handshake by sending a `PartitionCutoverRequest` on a fresh
//! bidirectional stream; the source drains pending writes, freezes the
//! partition, and replies with a single terminal
//! `PartitionCutoverResponse` carrying the final source offset and PoH
//! sequence at the moment of freeze.
//!
//! After a successful response, the source guarantees no further writes
//! will be accepted until the caller either commits the Raft ownership
//! flip (authoritative handover) or aborts the transfer. The buffer-on-
//! target + replay-from-final-offset behaviour is engine-side
//! responsibility (CL-6c.4) — this module is purely the transport
//! primitive, kept crypto-agnostic and engine-agnostic like CL-6a/b.

use std::future::Future;
use std::pin::Pin;

use aeon_types::AeonError;

use crate::transfer::{TransferState, TransferTracker};
use crate::transport::framing::{self, MessageType};
use crate::types::{PartitionCutoverRequest, PartitionCutoverResponse};

/// Watermarks returned by the source at the moment it freezes the
/// partition. The target uses these to know when it has fully caught
/// up and can start accepting writes on its side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutoverOffsets {
    /// Source-anchor offset at freeze (`-1` = no offset, e.g. non-Kafka
    /// source or first cutover).
    pub final_source_offset: i64,
    /// PoH chain sequence at freeze.
    pub final_poh_sequence: u64,
}

/// Server-side hook: called on the source node when a cutover request
/// arrives. Must stop accepting new writes for `(pipeline, partition)`,
/// drain any in-flight writes, and return the final watermarks.
///
/// Returning `Err` surfaces as `success=false` on the wire so the target
/// can abort the transfer cleanly rather than seeing the stream drop.
///
/// Async via `Pin<Box<dyn Future>>` (not `impl Future`) so the trait stays
/// dyn-compatible — the transport dispatcher holds
/// `Arc<dyn CutoverCoordinator>` so the engine-crate implementor can be
/// installed without a generic parameter propagating through the server.
pub trait CutoverCoordinator: Send + Sync {
    fn drain_and_freeze<'a>(
        &'a self,
        req: &'a PartitionCutoverRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CutoverOffsets, AeonError>> + Send + 'a>>;
}

/// Client side — request cutover for a partition and return the
/// watermarks the target must reach before resuming writes.
///
/// On source failure, the remote sends `success=false` + message and
/// this function surfaces it as `AeonError::Cluster { message, .. }`.
pub async fn request_partition_cutover(
    connection: &quinn::Connection,
    req: &PartitionCutoverRequest,
) -> Result<CutoverOffsets, AeonError> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| AeonError::Connection {
            message: format!("cutover: open_bi: {e}"),
            source: None,
            retryable: true,
        })?;

    let req_bytes = bincode::serialize(req).map_err(|e| AeonError::Serialization {
        message: format!("serialize PartitionCutoverRequest: {e}"),
        source: None,
    })?;
    framing::write_frame(&mut send, MessageType::PartitionCutoverRequest, &req_bytes).await?;
    let _ = send.finish();

    let (mt, payload) = framing::read_frame(&mut recv).await?;
    if mt != MessageType::PartitionCutoverResponse {
        return Err(AeonError::Connection {
            message: format!("cutover: expected response frame, got {mt:?}"),
            source: None,
            retryable: false,
        });
    }
    let resp: PartitionCutoverResponse =
        bincode::deserialize(&payload).map_err(|e| AeonError::Serialization {
            message: format!("deserialize PartitionCutoverResponse: {e}"),
            source: None,
        })?;

    if !resp.success {
        return Err(AeonError::Cluster {
            message: format!("cutover: remote reported failure: {}", resp.message),
            source: None,
        });
    }
    Ok(CutoverOffsets {
        final_source_offset: resp.final_source_offset,
        final_poh_sequence: resp.final_poh_sequence,
    })
}

/// Server side — service one accepted bidirectional stream. Reads the
/// request, invokes `coordinator.drain_and_freeze(&req)`, and writes
/// back a single `PartitionCutoverResponse` frame.
pub async fn serve_partition_cutover_stream<C>(
    coordinator: &C,
    send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<(), AeonError>
where
    C: CutoverCoordinator + ?Sized,
{
    let (mt, payload) = framing::read_frame(&mut recv).await?;
    if mt != MessageType::PartitionCutoverRequest {
        return Err(AeonError::Connection {
            message: format!("cutover: expected request frame, got {mt:?}"),
            source: None,
            retryable: false,
        });
    }
    let req: PartitionCutoverRequest =
        bincode::deserialize(&payload).map_err(|e| AeonError::Serialization {
            message: format!("deserialize PartitionCutoverRequest: {e}"),
            source: None,
        })?;
    serve_partition_cutover_with_request(coordinator, send, &req).await
}

/// Same as `serve_partition_cutover_stream`, but for callers that have
/// already read and parsed the initial request frame (the cluster
/// `serve()` dispatcher peeks the first frame to route by `MessageType`).
pub async fn serve_partition_cutover_with_request<C>(
    coordinator: &C,
    mut send: quinn::SendStream,
    req: &PartitionCutoverRequest,
) -> Result<(), AeonError>
where
    C: CutoverCoordinator + ?Sized,
{
    let resp = match coordinator.drain_and_freeze(req).await {
        Ok(offsets) => PartitionCutoverResponse {
            success: true,
            final_source_offset: offsets.final_source_offset,
            final_poh_sequence: offsets.final_poh_sequence,
            message: String::new(),
        },
        Err(e) => PartitionCutoverResponse {
            success: false,
            final_source_offset: -1,
            final_poh_sequence: 0,
            message: e.to_string(),
        },
    };
    let b = bincode::serialize(&resp).map_err(|e| AeonError::Serialization {
        message: format!("serialize PartitionCutoverResponse: {e}"),
        source: None,
    })?;
    framing::write_frame(&mut send, MessageType::PartitionCutoverResponse, &b).await?;
    let _ = send.finish();
    Ok(())
}

/// CL-6c.2: orchestrate the cutover handshake from the target side.
///
/// Expects `tracker` to already be in `Cutover` — typically because the
/// caller has just returned successfully from `drive_partition_transfer`
/// (which transitions BulkSync → Cutover on the success end frame) and
/// optionally pulled the PoH chain state via `drive_poh_chain_transfer`.
/// On success the source has frozen the partition and returned its
/// final watermarks; the tracker stays in `Cutover` so the caller can
/// drive the Raft ownership flip before calling `tracker.complete()`.
///
/// On any failure — precondition violation, transport error, or source
/// reporting `success = false` — the tracker is moved to
/// `Aborted { reverted_to: source }` and the error is returned.
pub async fn drive_partition_cutover(
    tracker: &mut TransferTracker,
    connection: &quinn::Connection,
    req: &PartitionCutoverRequest,
) -> Result<CutoverOffsets, AeonError> {
    if !matches!(tracker.state, TransferState::Cutover { .. }) {
        return Err(AeonError::State {
            message: format!(
                "drive_partition_cutover: tracker must be in Cutover, got {:?}",
                tracker.state
            ),
            source: None,
        });
    }

    match request_partition_cutover(connection, req).await {
        Ok(offsets) => Ok(offsets),
        Err(e) => {
            let reason = format!("cutover handshake failed: {e}");
            let _ = tracker.abort(reason.clone());
            Err(AeonError::Cluster {
                message: reason,
                source: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::endpoint::QuicEndpoint;
    use crate::transport::tls::dev_quic_configs_insecure;
    use crate::types::NodeAddress;

    use aeon_types::PartitionId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// Records how many times `drain_and_freeze` was called, and returns
    /// a fixed set of watermarks so tests can verify propagation.
    struct StubCoordinator {
        calls: AtomicU32,
        offsets: CutoverOffsets,
    }

    impl CutoverCoordinator for StubCoordinator {
        fn drain_and_freeze<'a>(
            &'a self,
            _req: &'a PartitionCutoverRequest,
        ) -> Pin<Box<dyn Future<Output = Result<CutoverOffsets, AeonError>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let offsets = self.offsets;
            Box::pin(async move { Ok(offsets) })
        }
    }

    struct FailingCoordinator;
    impl CutoverCoordinator for FailingCoordinator {
        fn drain_and_freeze<'a>(
            &'a self,
            _req: &'a PartitionCutoverRequest,
        ) -> Pin<Box<dyn Future<Output = Result<CutoverOffsets, AeonError>> + Send + 'a>> {
            Box::pin(async move { Err(AeonError::state("stub: intentional cutover failure")) })
        }
    }

    async fn run_server<C: CutoverCoordinator + 'static>(
        server: Arc<QuicEndpoint>,
        coordinator: Arc<C>,
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
            let coordinator = Arc::clone(&coordinator);
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
                    let coordinator = Arc::clone(&coordinator);
                    tokio::spawn(async move {
                        let _ = serve_partition_cutover_stream(&*coordinator, send, recv).await;
                    });
                }
            });
        }
    }

    #[tokio::test]
    async fn roundtrip_happy_path_returns_offsets_and_invokes_coordinator() {
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

        let coordinator = Arc::new(StubCoordinator {
            calls: AtomicU32::new(0),
            offsets: CutoverOffsets {
                final_source_offset: 12_345,
                final_poh_sequence: 678,
            },
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&coordinator),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let req = PartitionCutoverRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        let got = request_partition_cutover(&conn, &req).await.unwrap();
        assert_eq!(got.final_source_offset, 12_345);
        assert_eq!(got.final_poh_sequence, 678);
        assert_eq!(
            coordinator.calls.load(Ordering::Relaxed),
            1,
            "drain_and_freeze must be invoked exactly once"
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn coordinator_failure_surfaces_as_cluster_error() {
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

        let coordinator = Arc::new(FailingCoordinator);
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&coordinator),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let req = PartitionCutoverRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(9),
        };
        let err = request_partition_cutover(&conn, &req).await;
        assert!(err.is_err(), "failing coordinator must surface as Err");
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("intentional cutover failure"),
            "error must carry coordinator message, got: {msg}"
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn drive_cutover_returns_offsets_and_keeps_tracker_in_cutover() {
        use crate::transfer::{TransferState, TransferTracker};
        use aeon_types::PartitionId as P;

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

        let coordinator = Arc::new(StubCoordinator {
            calls: AtomicU32::new(0),
            offsets: CutoverOffsets {
                final_source_offset: 9_999,
                final_poh_sequence: 42,
            },
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&coordinator),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        // Walk the tracker manually into Cutover (simulating a completed
        // bulk sync) so drive_partition_cutover's precondition passes.
        let mut tracker = TransferTracker::new(P::new(3));
        tracker.begin(7, 11).unwrap();
        tracker.begin_cutover().unwrap();
        assert!(matches!(
            tracker.state,
            TransferState::Cutover {
                source: 7,
                target: 11
            }
        ));

        let req = PartitionCutoverRequest {
            pipeline: "pl".to_string(),
            partition: P::new(3),
        };
        let got = drive_partition_cutover(&mut tracker, &conn, &req)
            .await
            .unwrap();
        assert_eq!(got.final_source_offset, 9_999);
        assert_eq!(got.final_poh_sequence, 42);
        assert!(
            matches!(
                tracker.state,
                TransferState::Cutover {
                    source: 7,
                    target: 11
                }
            ),
            "tracker must remain in Cutover after successful handshake, got {:?}",
            tracker.state
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn drive_cutover_aborts_tracker_on_coordinator_failure() {
        use crate::transfer::{TransferState, TransferTracker};
        use aeon_types::PartitionId as P;

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

        let coordinator = Arc::new(FailingCoordinator);
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&coordinator),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let mut tracker = TransferTracker::new(P::new(3));
        tracker.begin(7, 11).unwrap();
        tracker.begin_cutover().unwrap();

        let req = PartitionCutoverRequest {
            pipeline: "pl".to_string(),
            partition: P::new(3),
        };
        let err = drive_partition_cutover(&mut tracker, &conn, &req).await;
        assert!(err.is_err(), "failing coordinator must surface as Err");
        assert!(
            matches!(tracker.state, TransferState::Aborted { reverted_to: 7, .. }),
            "tracker must be Aborted with reverted_to=source, got {:?}",
            tracker.state
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn drive_cutover_rejects_wrong_tracker_state_without_wire_traffic() {
        use crate::transfer::TransferTracker;
        use aeon_types::PartitionId as P;

        // Real server + stub coordinator that counts calls. Pass an
        // Idle tracker and verify the precondition rejects before any
        // RPC hits the wire (coordinator.calls stays at 0).
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

        let coordinator = Arc::new(StubCoordinator {
            calls: AtomicU32::new(0),
            offsets: CutoverOffsets {
                final_source_offset: 0,
                final_poh_sequence: 0,
            },
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&coordinator),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let mut tracker = TransferTracker::new(P::new(0));
        // Tracker left in Idle — precondition must reject.

        let req = PartitionCutoverRequest {
            pipeline: "pl".to_string(),
            partition: P::new(0),
        };
        let err = drive_partition_cutover(&mut tracker, &conn, &req).await;
        assert!(
            err.is_err(),
            "Idle tracker must be rejected by precondition"
        );
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("must be in Cutover"),
            "error must explain the precondition, got: {msg}"
        );
        assert_eq!(
            coordinator.calls.load(Ordering::Relaxed),
            0,
            "precondition must reject before any wire traffic hits the source"
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn request_fields_are_propagated_to_coordinator() {
        // Verify the source side actually sees the pipeline + partition
        // we asked about (not just that the round-trip works).
        struct CapturingCoordinator {
            seen: std::sync::Mutex<Option<PartitionCutoverRequest>>,
        }
        impl CutoverCoordinator for CapturingCoordinator {
            fn drain_and_freeze<'a>(
                &'a self,
                req: &'a PartitionCutoverRequest,
            ) -> Pin<Box<dyn Future<Output = Result<CutoverOffsets, AeonError>> + Send + 'a>>
            {
                let req = req.clone();
                Box::pin(async move {
                    *self
                        .seen
                        .lock()
                        .map_err(|e| AeonError::state(format!("lock: {e}")))? = Some(req);
                    Ok(CutoverOffsets {
                        final_source_offset: 0,
                        final_poh_sequence: 0,
                    })
                })
            }
        }

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

        let coordinator = Arc::new(CapturingCoordinator {
            seen: std::sync::Mutex::new(None),
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&coordinator),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let req = PartitionCutoverRequest {
            pipeline: "my-pipeline".to_string(),
            partition: PartitionId::new(17),
        };
        let _ = request_partition_cutover(&conn, &req).await.unwrap();

        let seen = coordinator.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.pipeline, "my-pipeline");
        assert_eq!(seen.partition, PartitionId::new(17));

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }
}
