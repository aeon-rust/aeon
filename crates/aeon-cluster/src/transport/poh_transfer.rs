//! CL-6b — QUIC request/response for PoH chain state transfer.
//!
//! Unlike CL-6a's L2 segment transfer (which is streamed because bodies
//! can reach hundreds of MiB), a `PohChainState` is small (current hash,
//! sequence, and MMR — typically well under 16 KiB), so a single
//! round-trip is sufficient. The client opens a bidirectional stream,
//! sends one `PohChainTransferRequest` frame, and reads one terminal
//! `PohChainTransferResponse` frame.
//!
//! State payload is carried as opaque `Vec<u8>` (bincode-encoded
//! `aeon_crypto::poh::PohChainState`) — the transport layer stays
//! crypto-agnostic and the caller plugs in serialize/deserialize via
//! the `PohChainState::to_bytes` / `PohChainState::from_bytes` helpers.

use aeon_types::AeonError;

use crate::transport::framing::{self, MessageType};
use crate::types::{PohChainTransferRequest, PohChainTransferResponse};

/// Server-side source of PoH chain state for a given `(pipeline, partition)`.
///
/// Returning `Err` surfaces as a failure response to the client with the
/// error message — the server doesn't drop the stream silently.
///
/// The method is async because production providers (e.g. the engine's
/// `PohChainExportProvider`) need to await a `tokio::sync::Mutex` on the
/// live `PohChain` handle. Sync stubs return `Box::pin(async move { ... })`.
pub trait PohChainProvider: Send + Sync {
    fn export_state<'a>(
        &'a self,
        req: &'a PohChainTransferRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, AeonError>> + Send + 'a>>;
}

/// Client side — request the PoH chain state for a partition and return
/// the raw bytes of the (bincode-serialized) `PohChainState`.
///
/// The caller is responsible for decoding via `PohChainState::from_bytes`.
/// On provider failure, the remote sends `success=false` + message and
/// this function surfaces it as `AeonError::Cluster { message, .. }`.
pub async fn request_poh_chain_transfer(
    connection: &quinn::Connection,
    req: &PohChainTransferRequest,
) -> Result<Vec<u8>, AeonError> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| AeonError::Connection {
            message: format!("poh-transfer: open_bi: {e}"),
            source: None,
            retryable: true,
        })?;

    let req_bytes = bincode::serialize(req).map_err(|e| AeonError::Serialization {
        message: format!("serialize PohChainTransferRequest: {e}"),
        source: None,
    })?;
    framing::write_frame(&mut send, MessageType::PohChainTransferRequest, &req_bytes).await?;
    let _ = send.finish();

    let (mt, payload) = framing::read_frame(&mut recv).await?;
    if mt != MessageType::PohChainTransferResponse {
        return Err(AeonError::Connection {
            message: format!("poh-transfer: expected response frame, got {mt:?}"),
            source: None,
            retryable: false,
        });
    }
    let resp: PohChainTransferResponse =
        bincode::deserialize(&payload).map_err(|e| AeonError::Serialization {
            message: format!("deserialize PohChainTransferResponse: {e}"),
            source: None,
        })?;

    if !resp.success {
        return Err(AeonError::Cluster {
            message: format!("poh-transfer: remote reported failure: {}", resp.message),
            source: None,
        });
    }
    Ok(resp.state_bytes)
}

/// Server side — service one accepted bidirectional stream. Reads the
/// `PohChainTransferRequest`, invokes `provider.export_state(&req)`, and
/// writes back a single `PohChainTransferResponse` frame.
pub async fn serve_poh_chain_transfer_stream<P>(
    provider: &P,
    send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<(), AeonError>
where
    P: PohChainProvider + ?Sized,
{
    let (mt, payload) = framing::read_frame(&mut recv).await?;
    if mt != MessageType::PohChainTransferRequest {
        return Err(AeonError::Connection {
            message: format!("poh-transfer: expected request frame, got {mt:?}"),
            source: None,
            retryable: false,
        });
    }
    let req: PohChainTransferRequest =
        bincode::deserialize(&payload).map_err(|e| AeonError::Serialization {
            message: format!("deserialize PohChainTransferRequest: {e}"),
            source: None,
        })?;
    serve_poh_chain_transfer_with_request(provider, send, &req).await
}

/// Same as `serve_poh_chain_transfer_stream`, but for callers that have
/// already read and parsed the initial request frame (the cluster
/// `serve()` dispatcher peeks the first frame to route by `MessageType`).
pub async fn serve_poh_chain_transfer_with_request<P>(
    provider: &P,
    mut send: quinn::SendStream,
    req: &PohChainTransferRequest,
) -> Result<(), AeonError>
where
    P: PohChainProvider + ?Sized,
{
    let resp = match provider.export_state(req).await {
        Ok(state_bytes) => PohChainTransferResponse {
            success: true,
            state_bytes,
            message: String::new(),
        },
        Err(e) => PohChainTransferResponse {
            success: false,
            state_bytes: Vec::new(),
            message: e.to_string(),
        },
    };
    let b = bincode::serialize(&resp).map_err(|e| AeonError::Serialization {
        message: format!("serialize PohChainTransferResponse: {e}"),
        source: None,
    })?;
    framing::write_frame(&mut send, MessageType::PohChainTransferResponse, &b).await?;
    let _ = send.finish();
    Ok(())
}

/// Orchestrate a PoH chain transfer: pull the `(pipeline, partition)`
/// state bytes from `connection`, then hand them to `on_state` for the
/// caller to decode via `PohChainState::from_bytes` and install into
/// their local `PohChain` (typically via `PohChain::from_state`).
///
/// Pairs with CL-6a's `drive_partition_transfer`: after a successful L2
/// bulk sync, the target node drives this to pick up the PoH chain head
/// before flipping ownership via Raft. The decode + verification step
/// is caller-owned so `aeon-cluster` stays free of a hard `aeon-crypto`
/// dependency.
pub async fn drive_poh_chain_transfer<F>(
    connection: &quinn::Connection,
    req: &PohChainTransferRequest,
    mut on_state: F,
) -> Result<(), AeonError>
where
    F: FnMut(Vec<u8>) -> Result<(), AeonError>,
{
    let bytes = request_poh_chain_transfer(connection, req).await?;
    on_state(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::endpoint::QuicEndpoint;
    use crate::transport::tls::dev_quic_configs_insecure;
    use crate::types::NodeAddress;

    use aeon_types::PartitionId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct StubProvider {
        payload: Vec<u8>,
    }

    impl PohChainProvider for StubProvider {
        fn export_state<'a>(
            &'a self,
            _req: &'a PohChainTransferRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<u8>, AeonError>> + Send + 'a>,
        > {
            let payload = self.payload.clone();
            Box::pin(async move { Ok(payload) })
        }
    }

    struct FailingProvider;
    impl PohChainProvider for FailingProvider {
        fn export_state<'a>(
            &'a self,
            _req: &'a PohChainTransferRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<u8>, AeonError>> + Send + 'a>,
        > {
            Box::pin(async move { Err(AeonError::state("stub: intentional poh failure")) })
        }
    }

    async fn run_server<P: PohChainProvider + 'static>(
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
                        let _ = serve_poh_chain_transfer_stream(&*provider, send, recv).await;
                    });
                }
            });
        }
    }

    #[tokio::test]
    async fn roundtrip_happy_path_preserves_payload() {
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

        let expected_payload = (0u8..37).collect::<Vec<u8>>();
        let provider = Arc::new(StubProvider {
            payload: expected_payload.clone(),
        });

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

        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        let got = request_poh_chain_transfer(&conn, &req).await.unwrap();
        assert_eq!(got, expected_payload);

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn provider_failure_surfaces_as_cluster_error() {
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

        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        let err = request_poh_chain_transfer(&conn, &req).await;
        assert!(err.is_err(), "failing provider must surface as Err");
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("intentional poh failure"),
            "error must carry provider message, got: {msg}"
        );

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn drive_poh_chain_transfer_hands_bytes_to_callback() {
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

        let expected = b"phony-chain-state".to_vec();
        let provider = Arc::new(StubProvider {
            payload: expected.clone(),
        });
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

        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        let mut captured: Option<Vec<u8>> = None;
        drive_poh_chain_transfer(&conn, &req, |bytes| {
            captured = Some(bytes);
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(captured.as_deref(), Some(expected.as_slice()));

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn empty_state_payload_roundtrips() {
        // A zero-sized state_bytes (edge case: brand-new partition with
        // no appends) must still succeed and return an empty Vec.
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

        let provider = Arc::new(StubProvider { payload: vec![] });
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

        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(7),
        };
        let got = request_poh_chain_transfer(&conn, &req).await.unwrap();
        assert!(got.is_empty());

        shutdown.store(true, Ordering::Relaxed);
        client.close();
        server.close();
        let _ = server_task.await;
    }
}
