//! Application-level health ping/pong for peer liveness (FT-6).
//!
//! Raft's AppendEntries heartbeats detect leader liveness, but they flow only
//! leader → follower and only while a leader exists. For observability —
//! dashboards, alerts, and pre-election staleness checks — every node needs to
//! know which peers are reachable, even when it is a follower or candidate.
//!
//! HealthPing (node_id, timestamp_ns) is sent periodically to each known peer.
//! The peer echoes a HealthPong carrying the ping's original timestamp plus its
//! own node_id; the sender derives RTT from `now - echoed_ts`.
//!
//! Failure to receive a pong within the timeout window marks the peer
//! `last_seen_stale`. State lives in [`SharedHealthState`] for scraping.
//!
//! Wire format (bincode, over QUIC bi-stream framed by [`super::framing`]):
//!   Client → Server: MessageType::HealthPing  + HealthPing { node_id, timestamp_ns }
//!   Server → Client: MessageType::HealthPong  + HealthPong { node_id, timestamp_ns }

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use aeon_types::AeonError;

use crate::transport::endpoint::QuicEndpoint;
use crate::transport::framing::{self, MessageType};
use crate::types::{NodeAddress, NodeId};

/// A health ping sent by a node to probe peer liveness and measure RTT.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthPing {
    /// The sender's node ID.
    pub node_id: NodeId,
    /// Sender's send time in nanoseconds since UNIX epoch.
    pub timestamp_ns: u128,
}

/// A health pong echoed by the receiver of a [`HealthPing`].
///
/// `timestamp_ns` is copied verbatim from the ping so the sender can compute
/// RTT locally (no clock-sync assumptions between nodes).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthPong {
    /// The responder's node ID.
    pub node_id: NodeId,
    /// Original ping timestamp, echoed unchanged.
    pub timestamp_ns: u128,
}

/// Per-peer health statistics, updated on each successful ping/pong.
#[derive(Debug, Clone, Copy, Default)]
pub struct HealthStats {
    /// UNIX-epoch nanos when the most recent pong was received.
    pub last_seen_ns: u128,
    /// Round-trip time from the most recent successful ping (nanoseconds).
    pub last_rtt_ns: u128,
    /// Total successful pong responses observed.
    pub successes: u64,
    /// Total ping attempts that timed out or errored.
    pub failures: u64,
}

impl HealthStats {
    /// Returns true if `last_seen_ns` is older than `staleness` relative to `now_ns`.
    pub fn is_stale(&self, now_ns: u128, staleness: Duration) -> bool {
        self.last_seen_ns == 0
            || now_ns.saturating_sub(self.last_seen_ns) > staleness.as_nanos()
    }
}

/// Shared map of peer node_id → [`HealthStats`]. Cheap to clone (Arc).
pub type SharedHealthState = Arc<RwLock<HashMap<NodeId, HealthStats>>>;

/// Construct an empty [`SharedHealthState`].
pub fn new_health_state() -> SharedHealthState {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Current UNIX-epoch time in nanoseconds. Saturates to 0 on pre-epoch clocks.
pub fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// ── Client side ─────────────────────────────────────────────────────

/// Send a single HealthPing to `target` and wait for the HealthPong.
/// Returns the measured RTT on success.
pub async fn send_health_ping(
    endpoint: &QuicEndpoint,
    self_id: NodeId,
    target_id: NodeId,
    target_addr: &NodeAddress,
) -> Result<Duration, AeonError> {
    let ping = HealthPing {
        node_id: self_id,
        timestamp_ns: now_ns(),
    };

    let conn = endpoint
        .connect(target_id, target_addr)
        .await
        .map_err(|e| AeonError::Connection {
            message: format!("health ping: connect to {target_id}@{target_addr} failed: {e}"),
            source: None,
            retryable: true,
        })?;

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| AeonError::Connection {
        message: format!("health ping: open_bi to node {target_id} failed: {e}"),
        source: None,
        retryable: true,
    })?;

    let payload = bincode::serialize(&ping).map_err(|e| AeonError::Serialization {
        message: format!("serialize HealthPing: {e}"),
        source: None,
    })?;

    framing::write_frame(&mut send, MessageType::HealthPing, &payload).await?;
    send.finish().map_err(|e| AeonError::Connection {
        message: format!("health ping: finish send failed: {e}"),
        source: None,
        retryable: false,
    })?;

    let (recv_type, resp_payload) = framing::read_frame(&mut recv).await?;
    if recv_type != MessageType::HealthPong {
        return Err(AeonError::Connection {
            message: format!("health ping: unexpected response type {:?}", recv_type),
            source: None,
            retryable: false,
        });
    }

    let pong: HealthPong =
        bincode::deserialize(&resp_payload).map_err(|e| AeonError::Serialization {
            message: format!("deserialize HealthPong: {e}"),
            source: None,
        })?;

    if pong.timestamp_ns != ping.timestamp_ns {
        return Err(AeonError::Connection {
            message: "health pong: echoed timestamp mismatch".to_string(),
            source: None,
            retryable: false,
        });
    }

    let rtt_ns = now_ns().saturating_sub(ping.timestamp_ns);
    Ok(Duration::from_nanos(rtt_ns.min(u64::MAX as u128) as u64))
}

/// Periodic pinger configuration.
#[derive(Debug, Clone)]
pub struct HealthPingerConfig {
    /// How often each peer is pinged.
    pub interval: Duration,
    /// How long to wait for a pong before marking the attempt a failure.
    pub timeout: Duration,
}

impl Default for HealthPingerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(1),
        }
    }
}

/// Spawn a background task that periodically pings every peer in `peers` and
/// updates `state`. Stops when `shutdown` becomes true.
pub fn spawn_health_pinger(
    endpoint: Arc<QuicEndpoint>,
    self_id: NodeId,
    peers: Vec<(NodeId, NodeAddress)>,
    state: SharedHealthState,
    config: HealthPingerConfig,
    shutdown: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(config.interval);
        // Skip the immediate tick fired by interval at t=0 — wait one period.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        while !shutdown.load(Ordering::Relaxed) {
            ticker.tick().await;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            for (peer_id, peer_addr) in &peers {
                let result = tokio::time::timeout(
                    config.timeout,
                    send_health_ping(&endpoint, self_id, *peer_id, peer_addr),
                )
                .await;

                let mut stats_map = state.write().await;
                let entry = stats_map.entry(*peer_id).or_default();
                match result {
                    Ok(Ok(rtt)) => {
                        entry.last_seen_ns = now_ns();
                        entry.last_rtt_ns = rtt.as_nanos();
                        entry.successes = entry.successes.saturating_add(1);
                    }
                    Ok(Err(e)) => {
                        entry.failures = entry.failures.saturating_add(1);
                        tracing::debug!(peer = peer_id, error = %e, "health ping failed");
                    }
                    Err(_) => {
                        entry.failures = entry.failures.saturating_add(1);
                        tracing::debug!(peer = peer_id, "health ping timed out");
                    }
                }
            }
        }
    })
}

// ── Server side ─────────────────────────────────────────────────────

/// Handle an inbound HealthPing on a QUIC stream by echoing a HealthPong.
pub async fn handle_health_ping(
    self_id: NodeId,
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), AeonError> {
    let ping: HealthPing =
        bincode::deserialize(payload).map_err(|e| AeonError::Serialization {
            message: format!("deserialize HealthPing: {e}"),
            source: None,
        })?;

    let pong = HealthPong {
        node_id: self_id,
        timestamp_ns: ping.timestamp_ns,
    };

    let resp =
        bincode::serialize(&pong).map_err(|e| AeonError::Serialization {
            message: format!("serialize HealthPong: {e}"),
            source: None,
        })?;

    framing::write_frame(send, MessageType::HealthPong, &resp).await?;
    let _ = send.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_ping_pong_serde_roundtrip() {
        let ping = HealthPing {
            node_id: 42,
            timestamp_ns: 1_700_000_000_000_000_000,
        };
        let bytes = bincode::serialize(&ping).unwrap();
        let decoded: HealthPing = bincode::deserialize(&bytes).unwrap();
        assert_eq!(ping, decoded);

        let pong = HealthPong {
            node_id: 7,
            timestamp_ns: ping.timestamp_ns,
        };
        let bytes = bincode::serialize(&pong).unwrap();
        let decoded: HealthPong = bincode::deserialize(&bytes).unwrap();
        assert_eq!(pong, decoded);
    }

    #[test]
    fn health_stats_is_stale_when_never_seen() {
        let stats = HealthStats::default();
        assert!(stats.is_stale(now_ns(), Duration::from_secs(5)));
    }

    #[test]
    fn health_stats_is_stale_past_threshold() {
        let now = now_ns();
        let stats = HealthStats {
            last_seen_ns: now.saturating_sub(Duration::from_secs(10).as_nanos()),
            ..Default::default()
        };
        assert!(stats.is_stale(now, Duration::from_secs(5)));
        assert!(!stats.is_stale(now, Duration::from_secs(30)));
    }

    #[tokio::test]
    async fn new_health_state_starts_empty() {
        let state = new_health_state();
        assert!(state.read().await.is_empty());
    }

    #[test]
    fn pinger_default_config() {
        let c = HealthPingerConfig::default();
        assert_eq!(c.interval, Duration::from_secs(2));
        assert_eq!(c.timeout, Duration::from_secs(1));
    }

    // ── Integration: real QUIC ping/pong roundtrip ──────────────────
    use crate::transport::endpoint::QuicEndpoint;
    use crate::transport::tls::dev_quic_configs_insecure;

    #[tokio::test]
    async fn health_ping_roundtrip_over_quic() {
        let (server_cfg, client_cfg) = dev_quic_configs_insecure();

        // Server endpoint
        let server = Arc::new(
            QuicEndpoint::bind(
                "127.0.0.1:0".parse().unwrap(),
                server_cfg.clone(),
                client_cfg.clone(),
            )
            .unwrap(),
        );
        let server_addr = server.local_addr().unwrap();

        // Spawn server accept loop: respond to any HealthPing with a HealthPong as node 99.
        // Loop on accept_bi so `conn` stays alive until the client closes — otherwise
        // dropping `conn` immediately after send.finish() aborts the stream before the
        // client reads the response.
        let server_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let incoming = match server.accept().await {
                    Some(i) => i,
                    None => return,
                };
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(_) => return,
                };
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let (mt, payload) = match framing::read_frame(&mut recv).await {
                        Ok(f) => f,
                        Err(_) => break,
                    };
                    assert_eq!(mt, MessageType::HealthPing);
                    handle_health_ping(99, &payload, &mut send).await.unwrap();
                }
            })
        };

        // Client endpoint
        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();

        let target = NodeAddress::new("127.0.0.1", server_addr.port());
        let rtt = send_health_ping(&client, 1, 99, &target).await.unwrap();

        // Loopback RTT must be well under a second.
        assert!(rtt < Duration::from_secs(1), "unexpectedly large RTT: {rtt:?}");

        server_task.await.unwrap();
        client.close();
        server.close();
    }

    #[tokio::test]
    async fn spawn_health_pinger_updates_stats() {
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

        // Server: answer pings as node 7 until the client disconnects.
        let server_clone = Arc::clone(&server);
        let server_task = tokio::spawn(async move {
            if let Some(incoming) = server_clone.accept().await {
                if let Ok(conn) = incoming.await {
                    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                        let payload = match framing::read_frame(&mut recv).await {
                            Ok((_, p)) => p,
                            Err(_) => break,
                        };
                        let _ = handle_health_ping(7, &payload, &mut send).await;
                    }
                }
            }
        });

        let client_ep = Arc::new(
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap(),
        );
        let state = new_health_state();
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = spawn_health_pinger(
            Arc::clone(&client_ep),
            1,
            vec![(7, NodeAddress::new("127.0.0.1", server_addr.port()))],
            Arc::clone(&state),
            HealthPingerConfig {
                interval: Duration::from_millis(50),
                timeout: Duration::from_secs(2),
            },
            Arc::clone(&shutdown),
        );

        // Wait up to 2s for the first success.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            {
                let s = state.read().await;
                if s.get(&7).map(|e| e.successes).unwrap_or(0) >= 1 {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("pinger did not record a success within timeout");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        shutdown.store(true, Ordering::Relaxed);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), server_task).await;

        let s = state.read().await;
        let stats = s.get(&7).unwrap();
        assert!(stats.successes >= 1);
        assert!(stats.last_seen_ns > 0);
        assert!(!stats.is_stale(now_ns(), Duration::from_secs(10)));

        client_ep.close();
        server.close();
    }
}
