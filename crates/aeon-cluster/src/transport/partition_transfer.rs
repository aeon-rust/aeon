//! CL-6a.2 — QUIC streaming protocol for L2 partition transfer.
//!
//! Client-side `request_partition_transfer` opens a single bi-di stream
//! on an established connection, sends a [`PartitionTransferRequest`],
//! then reads back:
//!
//! 1. one `PartitionTransferManifestFrame` carrying a [`SegmentManifest`]
//! 2. zero or more `PartitionTransferChunkFrame`s carrying [`SegmentChunk`]s
//! 3. one terminal `PartitionTransferEndFrame` carrying [`PartitionTransferEnd`]
//!
//! Server-side `serve_partition_transfer_stream` reads the request,
//! invokes caller-provided closures to fetch the manifest + chunks, and
//! writes them back in order. The closure-based interface keeps the
//! cluster crate independent of `aeon-engine`; the caller plugs in a
//! source (typically `aeon_engine::l2_transfer::SegmentReader`).
//!
//! Wiring this into the `serve()` RPC dispatch loop lives in CL-6a.3.

use aeon_types::{AeonError, SegmentChunk, SegmentManifest};

use crate::transport::framing::{self, MessageType};
use crate::transport::throttle::TransferThrottle;
use crate::transport::transfer_metrics::{PartitionTransferMetrics, TransferRole};
use crate::types::{PartitionTransferEnd, PartitionTransferRequest};

use crate::transfer::TransferTracker;
use crate::types::NodeId;

/// Callback returning the next `SegmentChunk` for a partition transfer.
///
/// Return `Ok(None)` to signal end-of-stream (the server will emit the
/// terminal end frame with `success = true`). Return `Err` to abort the
/// transfer (the server emits an end frame with `success = false` and
/// the error message).
pub type ChunkIter<'a> = Box<dyn FnMut() -> Result<Option<SegmentChunk>, AeonError> + Send + 'a>;

/// Callback driving the per-request server response. Given the received
/// `PartitionTransferRequest`, produce `(manifest, next_chunk)` — the
/// chunk iterator is consumed to exhaustion, then the stream closes with
/// a success end frame. Any error returned anywhere converts into a
/// failure end frame and stream close.
///
/// Split into `manifest` + `chunks` rather than a single combined struct
/// so the caller can cheaply answer "what do you have?" (list files) and
/// defer the expensive per-chunk reads until after the manifest is on
/// the wire.
pub trait PartitionTransferProvider: Send + Sync {
    fn serve(
        &self,
        req: &PartitionTransferRequest,
    ) -> Result<(SegmentManifest, ChunkIter<'static>), AeonError>;

    /// Optional source-side bandwidth throttle applied per chunk write.
    ///
    /// Default `None` — writes proceed at line rate. CL-6d: engines plug
    /// in a `TransferThrottle` sized to a fraction (30% per roadmap) of
    /// observed link capacity so a partition bulk-sync can't starve the
    /// live pipeline traffic sharing the same QUIC connection.
    fn throttle(&self) -> Option<&TransferThrottle> {
        None
    }

    /// Optional source-side progress metrics sink. When wired, the serve
    /// loop emits `role=source` samples for every transfer. The engine
    /// typically shares the same `PartitionTransferMetrics` instance
    /// with the target-side `drive_partition_transfer` call so operators
    /// see both halves of the transfer on `/metrics`.
    fn metrics(&self) -> Option<&PartitionTransferMetrics> {
        None
    }
}

/// Client side — run the full request/receive loop on `connection`.
///
/// `on_manifest` is called once, before any chunks, so the caller can
/// pre-allocate / open target files. `on_chunk` is called for every
/// `SegmentChunk`, in arrival order. The returned value of `on_manifest`
/// is threaded into each `on_chunk` call so the caller can stash state
/// (typically a `SegmentWriter`) without an external cell.
pub async fn request_partition_transfer<W, FM, FC, FF>(
    connection: &quinn::Connection,
    req: &PartitionTransferRequest,
    mut on_manifest: FM,
    mut on_chunk: FC,
    mut on_finish: FF,
) -> Result<(), AeonError>
where
    FM: FnMut(SegmentManifest) -> Result<W, AeonError>,
    FC: FnMut(&mut W, SegmentChunk) -> Result<(), AeonError>,
    FF: FnMut(W, PartitionTransferEnd) -> Result<(), AeonError>,
{
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| AeonError::Connection {
            message: format!("partition-transfer: open_bi: {e}"),
            source: None,
            retryable: true,
        })?;

    let req_bytes = bincode::serialize(req).map_err(|e| AeonError::Serialization {
        message: format!("serialize PartitionTransferRequest: {e}"),
        source: None,
    })?;
    framing::write_frame(&mut send, MessageType::PartitionTransferRequest, &req_bytes).await?;
    let _ = send.finish();

    // Manifest arrives first.
    let (mt, payload) = framing::read_frame(&mut recv).await?;
    if mt != MessageType::PartitionTransferManifestFrame {
        return Err(AeonError::Connection {
            message: format!("partition-transfer: expected manifest frame, got {mt:?}"),
            source: None,
            retryable: false,
        });
    }
    let manifest: SegmentManifest =
        bincode::deserialize(&payload).map_err(|e| AeonError::Serialization {
            message: format!("deserialize SegmentManifest: {e}"),
            source: None,
        })?;
    let mut writer_state = on_manifest(manifest)?;

    // Chunks until end frame.
    loop {
        let (mt, payload) = framing::read_frame(&mut recv).await?;
        match mt {
            MessageType::PartitionTransferChunkFrame => {
                let chunk: SegmentChunk =
                    bincode::deserialize(&payload).map_err(|e| AeonError::Serialization {
                        message: format!("deserialize SegmentChunk: {e}"),
                        source: None,
                    })?;
                on_chunk(&mut writer_state, chunk)?;
            }
            MessageType::PartitionTransferEndFrame => {
                let end: PartitionTransferEnd =
                    bincode::deserialize(&payload).map_err(|e| AeonError::Serialization {
                        message: format!("deserialize PartitionTransferEnd: {e}"),
                        source: None,
                    })?;
                on_finish(writer_state, end)?;
                return Ok(());
            }
            other => {
                return Err(AeonError::Connection {
                    message: format!(
                        "partition-transfer: unexpected frame {other:?} in chunk stream"
                    ),
                    source: None,
                    retryable: false,
                });
            }
        }
    }
}

/// Server side — service one accepted bidirectional stream.
///
/// Reads the `PartitionTransferRequest`, invokes `provider.serve(&req)`,
/// then streams manifest → chunks → end frame back. On any provider or
/// IO error, the terminal frame carries `success = false` + the error
/// message so the client can abort + surface context.
pub async fn serve_partition_transfer_stream<P>(
    provider: &P,
    send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<(), AeonError>
where
    P: PartitionTransferProvider + ?Sized,
{
    let (mt, payload) = framing::read_frame(&mut recv).await?;
    if mt != MessageType::PartitionTransferRequest {
        return Err(AeonError::Connection {
            message: format!("partition-transfer: expected request frame, got {mt:?}"),
            source: None,
            retryable: false,
        });
    }
    let req: PartitionTransferRequest =
        bincode::deserialize(&payload).map_err(|e| AeonError::Serialization {
            message: format!("deserialize PartitionTransferRequest: {e}"),
            source: None,
        })?;
    serve_partition_transfer_with_request(provider, send, &req).await
}

/// Same as `serve_partition_transfer_stream`, but for callers that have
/// already read and parsed the initial request frame (e.g. the cluster
/// `serve()` RPC dispatcher, which peeks the first frame to route by
/// `MessageType`).
pub async fn serve_partition_transfer_with_request<P>(
    provider: &P,
    mut send: quinn::SendStream,
    req: &PartitionTransferRequest,
) -> Result<(), AeonError>
where
    P: PartitionTransferProvider + ?Sized,
{
    match provider.serve(req) {
        Ok((manifest, mut chunks)) => {
            let total_bytes = manifest.total_bytes();
            let manifest_bytes =
                bincode::serialize(&manifest).map_err(|e| AeonError::Serialization {
                    message: format!("serialize SegmentManifest: {e}"),
                    source: None,
                })?;
            framing::write_frame(
                &mut send,
                MessageType::PartitionTransferManifestFrame,
                &manifest_bytes,
            )
            .await?;

            let throttle = provider.throttle();
            let metrics = provider.metrics();
            if let Some(m) = metrics {
                m.record_total(
                    &req.pipeline,
                    req.partition,
                    TransferRole::Source,
                    total_bytes,
                );
            }
            let mut stream_err: Option<String> = None;
            loop {
                match chunks() {
                    Ok(Some(chunk)) => {
                        let b =
                            bincode::serialize(&chunk).map_err(|e| AeonError::Serialization {
                                message: format!("serialize SegmentChunk: {e}"),
                                source: None,
                            })?;
                        if let Some(t) = throttle {
                            t.acquire(b.len() as u64).await;
                        }
                        if let Err(e) = framing::write_frame(
                            &mut send,
                            MessageType::PartitionTransferChunkFrame,
                            &b,
                        )
                        .await
                        {
                            stream_err = Some(e.to_string());
                            break;
                        }
                        if let Some(m) = metrics {
                            m.add_transferred(
                                &req.pipeline,
                                req.partition,
                                TransferRole::Source,
                                chunk.data.len() as u64,
                            );
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        stream_err = Some(e.to_string());
                        break;
                    }
                }
            }
            write_end(&mut send, stream_err).await?;
            if let Some(m) = metrics {
                m.clear(&req.pipeline, req.partition, TransferRole::Source);
            }
        }
        Err(e) => {
            write_end(&mut send, Some(e.to_string())).await?;
        }
    }

    let _ = send.finish();
    Ok(())
}

async fn write_end(send: &mut quinn::SendStream, err: Option<String>) -> Result<(), AeonError> {
    let end = match err {
        None => PartitionTransferEnd {
            success: true,
            message: String::new(),
        },
        Some(m) => PartitionTransferEnd {
            success: false,
            message: m,
        },
    };
    let b = bincode::serialize(&end).map_err(|e| AeonError::Serialization {
        message: format!("serialize PartitionTransferEnd: {e}"),
        source: None,
    })?;
    framing::write_frame(send, MessageType::PartitionTransferEndFrame, &b).await?;
    Ok(())
}

/// Drive a partition transfer from start to `Cutover` while keeping the
/// `TransferTracker` state machine in sync with on-wire byte progress.
///
/// This is CL-6a.3's orchestrator: it begins the transfer (Idle → BulkSync),
/// streams segments from `connection` using `request_partition_transfer`,
/// reports per-chunk byte progress into the tracker, and on a successful
/// end frame transitions BulkSync → Cutover. The *final* Cutover → Complete
/// transition is performed by the caller once the Raft ownership flip has
/// committed — we don't do that here because this function has no access
/// to the consensus layer.
///
/// On any error (transport, provider, writer), the tracker is moved to
/// `Aborted { reverted_to: source }` and the error is returned.
///
/// `writer` is invoked once per received chunk with `(tracker, chunk)`.
/// The caller is responsible for actually persisting the chunk (typically
/// via a `SegmentWriter` in `aeon-engine`). The tracker reference lets
/// the writer read progress without us threading a second argument.
///
/// `metrics` is an optional shared registry updated with `role=target`
/// samples so operators can watch receive-side progress on `/metrics`.
/// Pass `None` in tests/contexts that don't observe metrics.
pub async fn drive_partition_transfer<F>(
    tracker: &mut TransferTracker,
    source: NodeId,
    target: NodeId,
    connection: &quinn::Connection,
    req: &PartitionTransferRequest,
    metrics: Option<&PartitionTransferMetrics>,
    mut writer: F,
) -> Result<(), AeonError>
where
    F: FnMut(&SegmentChunk) -> Result<(), AeonError>,
{
    tracker
        .begin(source, target)
        .map_err(|e| AeonError::State {
            message: format!("partition-transfer: begin: {e}"),
            source: None,
        })?;

    // Local accumulators — we can't borrow `tracker` through the async
    // request because the inner closures are synchronous and we need the
    // state updates visible after each chunk.
    let mut total_bytes: u64 = 0;
    let mut progress: u64 = 0;
    let mut end_state: Option<PartitionTransferEnd> = None;

    let result = request_partition_transfer(
        connection,
        req,
        |manifest| {
            total_bytes = manifest.total_bytes();
            if let Some(m) = metrics {
                m.record_total(
                    &req.pipeline,
                    req.partition,
                    TransferRole::Target,
                    total_bytes,
                );
            }
            Ok::<(), AeonError>(())
        },
        |_, chunk| {
            let n = chunk.data.len() as u64;
            progress = progress.saturating_add(n);
            // Record receipt before handing off to the persistence
            // writer: the bytes are already in RAM and we're about to
            // commit them, so the gauge is "bytes received" rather than
            // "bytes durably persisted". On writer error, the transfer
            // aborts and the entry is cleared anyway.
            if let Some(m) = metrics {
                m.add_transferred(&req.pipeline, req.partition, TransferRole::Target, n);
            }
            writer(&chunk)?;
            Ok(())
        },
        |_, end| {
            end_state = Some(end);
            Ok(())
        },
    )
    .await;

    // Reflect what actually made it to disk before deciding next state.
    tracker.update_progress(progress, total_bytes);

    if let Err(e) = result {
        let reason = format!("partition-transfer stream error: {e}");
        let _ = tracker.abort(reason.clone());
        if let Some(m) = metrics {
            m.clear(&req.pipeline, req.partition, TransferRole::Target);
        }
        return Err(AeonError::Cluster {
            message: reason,
            source: None,
        });
    }

    let end = end_state.ok_or_else(|| AeonError::Connection {
        message: "partition-transfer: stream closed without end frame".to_string(),
        source: None,
        retryable: false,
    })?;
    if !end.success {
        let reason = format!(
            "partition-transfer: remote reported failure: {}",
            end.message
        );
        let _ = tracker.abort(reason.clone());
        if let Some(m) = metrics {
            m.clear(&req.pipeline, req.partition, TransferRole::Target);
        }
        return Err(AeonError::Cluster {
            message: reason,
            source: None,
        });
    }

    tracker.begin_cutover().map_err(|e| AeonError::State {
        message: format!("partition-transfer: begin_cutover: {e}"),
        source: None,
    })?;

    // Successful handover into Cutover — target-side bookkeeping is
    // done; drop the entry so completed transfers don't linger on
    // /metrics as stale gauges.
    if let Some(m) = metrics {
        m.clear(&req.pipeline, req.partition, TransferRole::Target);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::endpoint::QuicEndpoint;
    use crate::transport::tls::dev_quic_configs_insecure;
    use crate::types::NodeAddress;

    use aeon_types::{PartitionId, SegmentEntry};
    use bytes::Bytes;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A deterministic in-memory `PartitionTransferProvider` for tests —
    /// holds one manifest + a precomputed chunk list per (pipeline, partition).
    struct StubProvider {
        manifest: SegmentManifest,
        chunks: Vec<SegmentChunk>,
    }

    impl PartitionTransferProvider for StubProvider {
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

    /// A provider that fails mid-stream — used to verify the end-frame
    /// error path reaches the client.
    struct FailingProvider;
    impl PartitionTransferProvider for FailingProvider {
        fn serve(
            &self,
            _req: &PartitionTransferRequest,
        ) -> Result<(SegmentManifest, ChunkIter<'static>), AeonError> {
            Err(AeonError::state("stub: intentional provider failure"))
        }
    }

    async fn run_server<P: PartitionTransferProvider + 'static>(
        server: Arc<QuicEndpoint>,
        provider: Arc<P>,
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
                        let _ = serve_partition_transfer_stream(&*provider, send, recv).await;
                    });
                }
            });
        }
    }

    fn make_stub_transfer() -> StubProvider {
        // Two segments: seg@0 of 5 bytes, seg@100 of 3 bytes.
        let data0 = Bytes::from_static(b"hello");
        let data1 = Bytes::from_static(b"aeo");
        StubProvider {
            manifest: SegmentManifest {
                entries: vec![
                    SegmentEntry {
                        start_seq: 0,
                        size_bytes: data0.len() as u64,
                        crc32: 0x11223344,
                    },
                    SegmentEntry {
                        start_seq: 100,
                        size_bytes: data1.len() as u64,
                        crc32: 0x55667788,
                    },
                ],
            },
            chunks: vec![
                SegmentChunk {
                    start_seq: 0,
                    offset: 0,
                    data: data0,
                    is_last: true,
                },
                SegmentChunk {
                    start_seq: 100,
                    offset: 0,
                    data: data1,
                    is_last: true,
                },
            ],
        }
    }

    #[tokio::test]
    async fn roundtrip_happy_path_preserves_manifest_and_chunks() {
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

        let provider = Arc::new(make_stub_transfer());
        let expected_manifest = provider.manifest.clone();
        let expected_chunks = provider.chunks.clone();

        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&provider),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let req = PartitionTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };

        let mut received_manifest: Option<SegmentManifest> = None;
        let mut received_chunks: Vec<SegmentChunk> = Vec::new();
        let mut end_flag: Option<PartitionTransferEnd> = None;

        request_partition_transfer(
            &conn,
            &req,
            |m| {
                received_manifest = Some(m);
                Ok::<_, AeonError>(())
            },
            |_, c| {
                received_chunks.push(c);
                Ok(())
            },
            |_, end| {
                end_flag = Some(end);
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(received_manifest.as_ref(), Some(&expected_manifest));
        assert_eq!(received_chunks, expected_chunks);
        let end = end_flag.unwrap();
        assert!(end.success, "happy-path end frame must be success");
        assert!(end.message.is_empty());

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn provider_failure_surfaces_as_end_frame() {
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

        let provider = Arc::new(FailingProvider);
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&provider),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let req = PartitionTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };

        // When the provider errors before emitting a manifest, the server
        // closes the stream with an end frame; our client's on_manifest
        // closure is never invoked. Verify `request_partition_transfer`
        // surfaces that as an Err at read-time (we got an end frame
        // where a manifest was expected).
        let result = request_partition_transfer(
            &conn,
            &req,
            |_m| Ok::<(), AeonError>(()),
            |_, _| Ok(()),
            |_, _| Ok(()),
        )
        .await;
        assert!(
            result.is_err(),
            "client must surface the provider failure as an error, got: {:?}",
            result
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn empty_manifest_transfer_completes_cleanly() {
        // No segments on the source — expect manifest with 0 entries and
        // an immediate success end frame.
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

        struct EmptyProvider;
        impl PartitionTransferProvider for EmptyProvider {
            fn serve(
                &self,
                _req: &PartitionTransferRequest,
            ) -> Result<(SegmentManifest, ChunkIter<'static>), AeonError> {
                Ok((SegmentManifest { entries: vec![] }, Box::new(|| Ok(None))))
            }
        }

        let provider = Arc::new(EmptyProvider);
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&provider),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let req = PartitionTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        let mut manifest_seen: Option<SegmentManifest> = None;
        let mut chunk_count = 0usize;
        let mut end_seen = false;
        request_partition_transfer(
            &conn,
            &req,
            |m| {
                manifest_seen = Some(m);
                Ok::<_, AeonError>(())
            },
            |_, _| {
                chunk_count += 1;
                Ok(())
            },
            |_, end| {
                end_seen = end.success;
                Ok(())
            },
        )
        .await
        .unwrap();
        assert!(manifest_seen.unwrap().is_empty());
        assert_eq!(chunk_count, 0);
        assert!(end_seen);

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn drive_transfer_walks_tracker_through_bulksync_to_cutover() {
        use crate::transfer::{TransferState, TransferTracker};

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

        let provider = Arc::new(make_stub_transfer());
        let expected_total = provider.manifest.total_bytes();

        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&provider),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let req = PartitionTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(7),
        };
        let mut tracker = TransferTracker::new(PartitionId::new(7));
        assert_eq!(tracker.state, TransferState::Idle);

        let mut written_bytes: u64 = 0;
        drive_partition_transfer(&mut tracker, 11, 22, &conn, &req, None, |chunk| {
            written_bytes += chunk.data.len() as u64;
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(written_bytes, expected_total);
        assert!(
            matches!(
                tracker.state,
                TransferState::Cutover {
                    source: 11,
                    target: 22,
                }
            ),
            "expected Cutover {{11,22}}, got {:?}",
            tracker.state
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn drive_transfer_aborts_tracker_on_provider_failure() {
        use crate::transfer::{TransferState, TransferTracker};

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

        let provider = Arc::new(FailingProvider);
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&provider),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let req = PartitionTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(3),
        };
        let mut tracker = TransferTracker::new(PartitionId::new(3));
        let err =
            drive_partition_transfer(&mut tracker, 11, 22, &conn, &req, None, |_| Ok(())).await;
        assert!(err.is_err(), "drive must surface provider failure");
        assert!(
            matches!(
                &tracker.state,
                TransferState::Aborted {
                    reverted_to: 11,
                    ..
                }
            ),
            "expected Aborted with reverted_to=11, got {:?}",
            tracker.state
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    /// Provider wrapper that attaches a `TransferThrottle` — used to
    /// verify `provider.throttle()` is invoked from the chunk loop.
    struct ThrottledProvider {
        inner: StubProvider,
        throttle: TransferThrottle,
    }

    impl PartitionTransferProvider for ThrottledProvider {
        fn serve(
            &self,
            req: &PartitionTransferRequest,
        ) -> Result<(SegmentManifest, ChunkIter<'static>), AeonError> {
            self.inner.serve(req)
        }

        fn throttle(&self) -> Option<&TransferThrottle> {
            Some(&self.throttle)
        }
    }

    /// Provider wrapper that exposes a shared `PartitionTransferMetrics`
    /// — used to verify both source-side and target-side samples land
    /// in the same registry during a real transfer. Its `serve` wraps
    /// the stub's chunk iterator with a hook that records the *source*
    /// metric snapshot after each yield, which captures the accumulated
    /// source-side count for chunk[i-1] right before chunk[i] is pulled.
    struct MetricsProvider {
        inner: StubProvider,
        metrics: Arc<PartitionTransferMetrics>,
        source_snapshots: Arc<std::sync::Mutex<Vec<u64>>>,
    }

    impl PartitionTransferProvider for MetricsProvider {
        fn serve(
            &self,
            req: &PartitionTransferRequest,
        ) -> Result<(SegmentManifest, ChunkIter<'static>), AeonError> {
            let (manifest, mut inner_chunks) = self.inner.serve(req)?;
            let metrics = Arc::clone(&self.metrics);
            let snapshots = Arc::clone(&self.source_snapshots);
            let pipeline = req.pipeline.clone();
            let partition = req.partition;
            let iter: ChunkIter<'static> = Box::new(move || {
                // Pull the next chunk from the stub — calling this means
                // the serve loop finished write_frame + add_transferred
                // for the previous chunk, so the snapshot here captures
                // cumulative source-side bytes up through chunk[i-1].
                let snap = metrics
                    .snapshot(&pipeline, partition, TransferRole::Source)
                    .map(|(t, _)| t)
                    .unwrap_or(0);
                snapshots.lock().unwrap().push(snap);
                inner_chunks()
            });
            Ok((manifest, iter))
        }

        fn metrics(&self) -> Option<&PartitionTransferMetrics> {
            Some(&self.metrics)
        }
    }

    #[tokio::test]
    async fn provider_throttle_is_respected_during_transfer() {
        // Two 1 MiB chunks at 1 MiB/s: first drains the burst budget,
        // second must wait ~1 s for the refill. Proves the serve loop
        // actually awaits `provider.throttle()` between chunk writes.
        use std::time::Instant as StdInstant;

        let data0 = bytes::Bytes::from(vec![0xABu8; 1_000_000]);
        let data1 = bytes::Bytes::from(vec![0xCDu8; 1_000_000]);
        let inner = StubProvider {
            manifest: SegmentManifest {
                entries: vec![
                    SegmentEntry {
                        start_seq: 0,
                        size_bytes: data0.len() as u64,
                        crc32: 0,
                    },
                    SegmentEntry {
                        start_seq: data0.len() as u64,
                        size_bytes: data1.len() as u64,
                        crc32: 0,
                    },
                ],
            },
            chunks: vec![
                SegmentChunk {
                    start_seq: 0,
                    offset: 0,
                    data: data0,
                    is_last: true,
                },
                SegmentChunk {
                    start_seq: 1_000_000,
                    offset: 0,
                    data: data1,
                    is_last: true,
                },
            ],
        };
        let provider = Arc::new(ThrottledProvider {
            inner,
            throttle: TransferThrottle::new(1_000_000),
        });

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
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&provider),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let req = PartitionTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        let start = StdInstant::now();
        request_partition_transfer(
            &conn,
            &req,
            |_| Ok::<(), AeonError>(()),
            |_, _| Ok(()),
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "throttle must slow the second chunk, got {:?}",
            elapsed
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn both_sides_emit_to_shared_metrics_registry() {
        use crate::transfer::TransferTracker;

        let metrics = Arc::new(PartitionTransferMetrics::new());
        let inner = make_stub_transfer();
        let expected_total = inner.manifest.total_bytes();
        let chunk0_bytes = inner.chunks[0].data.len() as u64;

        let source_snapshots = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let provider = Arc::new(MetricsProvider {
            inner,
            metrics: Arc::clone(&metrics),
            source_snapshots: Arc::clone(&source_snapshots),
        });

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
        let server_task = tokio::spawn(run_server(
            Arc::clone(&server),
            Arc::clone(&provider),
            Arc::clone(&shutdown),
        ));

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let conn = client.connect(1, &target).await.unwrap();

        let partition = PartitionId::new(9);
        let req = PartitionTransferRequest {
            pipeline: "metrics-pl".to_string(),
            partition,
        };
        let mut tracker = TransferTracker::new(partition);

        // Target-side snapshots: the drive emits add_transferred for
        // chunk[i] *before* invoking the writer callback, so the
        // snapshot inside the callback captures the cumulative count
        // through chunk[i] inclusive.
        let target_snapshots: Arc<std::sync::Mutex<Vec<u64>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let snapshot_slot = Arc::clone(&target_snapshots);
        let metrics_for_cb = Arc::clone(&metrics);

        drive_partition_transfer(
            &mut tracker,
            11,
            22,
            &conn,
            &req,
            Some(&metrics),
            |_chunk| {
                let snap = metrics_for_cb
                    .snapshot("metrics-pl", partition, TransferRole::Target)
                    .map(|(t, _)| t)
                    .unwrap_or(0);
                snapshot_slot.lock().unwrap().push(snap);
                Ok(())
            },
        )
        .await
        .expect("transfer must succeed");

        // Target side: 2 chunks of {5, 3} bytes — snapshots must be
        // {5, 8} cumulative.
        let tgt = target_snapshots.lock().unwrap().clone();
        assert_eq!(
            tgt,
            vec![chunk0_bytes, expected_total],
            "target-side cumulative snapshots must match chunk arrivals, got {tgt:?}"
        );

        // Source side: the provider's chunk iterator snapshots *before*
        // yielding each chunk. Entry 0 is taken before chunk[0] is sent
        // (source count = 0). Entry 1 is taken before chunk[1] is sent,
        // by which time chunk[0]'s add_transferred has fired → count
        // = chunk0_bytes. There's also a trailing call returning None
        // after chunk[1] sent → count = expected_total.
        let src = source_snapshots.lock().unwrap().clone();
        assert_eq!(
            src,
            vec![0, chunk0_bytes, expected_total],
            "source-side cumulative snapshots must reflect chunk-by-chunk progress, got {src:?}"
        );

        // Post-transfer both entries cleared.
        assert_eq!(
            metrics.snapshot("metrics-pl", partition, TransferRole::Source),
            None,
            "source entry must be cleared after successful transfer"
        );
        assert_eq!(
            metrics.snapshot("metrics-pl", partition, TransferRole::Target),
            None,
            "target entry must be cleared after successful transfer"
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }
}
