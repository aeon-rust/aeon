//! QUIC endpoint management with connection pooling.

use std::net::SocketAddr;
use std::sync::Arc;

use aeon_types::{AeonError, BackoffPolicy};
use dashmap::DashMap;
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig};

use crate::types::{NodeAddress, NodeId};

/// QUIC endpoint wrapping quinn::Endpoint with connection pooling.
pub struct QuicEndpoint {
    endpoint: Endpoint,
    client_config: ClientConfig,
    /// Pool of established connections by node ID.
    connections: Arc<DashMap<NodeId, Connection>>,
}

impl QuicEndpoint {
    /// Create a QUIC endpoint bound to the given address.
    pub fn bind(
        bind_addr: SocketAddr,
        server_config: ServerConfig,
        client_config: ClientConfig,
    ) -> Result<Self, AeonError> {
        let endpoint =
            Endpoint::server(server_config, bind_addr).map_err(|e| AeonError::Connection {
                message: format!("failed to bind QUIC endpoint on {bind_addr}: {e}"),
                source: None,
                retryable: false,
            })?;

        Ok(Self {
            endpoint,
            client_config,
            connections: Arc::new(DashMap::new()),
        })
    }

    /// Connect to a remote node, reusing existing connections.
    pub async fn connect(
        &self,
        node_id: NodeId,
        addr: &NodeAddress,
    ) -> Result<Connection, AeonError> {
        // Check pool first
        if let Some(conn) = self.connections.get(&node_id) {
            if conn.close_reason().is_none() {
                return Ok(conn.clone());
            }
            // Connection is closed, remove it
            drop(conn);
            self.connections.remove(&node_id);
        }

        // Resolve hostname to socket address
        let socket_addr = tokio::net::lookup_host(addr.to_string())
            .await
            .map_err(|e| AeonError::Connection {
                message: format!("failed to resolve {addr}: {e}"),
                source: None,
                retryable: true,
            })?
            .next()
            .ok_or_else(|| AeonError::Connection {
                message: format!("no addresses found for {addr}"),
                source: None,
                retryable: false,
            })?;

        let connecting = self
            .endpoint
            .connect_with(self.client_config.clone(), socket_addr, &addr.host)
            .map_err(|e| AeonError::Connection {
                message: format!("failed to initiate QUIC connection to {addr}: {e}"),
                source: None,
                retryable: true,
            })?;

        let connection = connecting.await.map_err(|e| AeonError::Connection {
            message: format!("QUIC handshake failed with {addr}: {e}"),
            source: None,
            retryable: true,
        })?;

        self.connections.insert(node_id, connection.clone());
        tracing::debug!(node_id, %addr, "QUIC connection established");

        Ok(connection)
    }

    /// Connect to a remote node without touching the connection pool.
    ///
    /// G16: used by the seed-join flow where the caller iterates over multiple
    /// seed addresses but does not know their real Raft ids. Passing a
    /// placeholder id (e.g. `0`) through the pooled `connect()` causes the
    /// first seed's connection to be cached under that placeholder and every
    /// subsequent seed attempt to silently re-use it — so retries against the
    /// actual leader never leave node A. The uncached path does a fresh
    /// resolve + handshake for each call and lets the returned `Connection`
    /// drop naturally when the one-shot RPC completes.
    pub async fn connect_uncached(
        &self,
        addr: &NodeAddress,
    ) -> Result<Connection, AeonError> {
        let socket_addr = tokio::net::lookup_host(addr.to_string())
            .await
            .map_err(|e| AeonError::Connection {
                message: format!("failed to resolve {addr}: {e}"),
                source: None,
                retryable: true,
            })?
            .next()
            .ok_or_else(|| AeonError::Connection {
                message: format!("no addresses found for {addr}"),
                source: None,
                retryable: false,
            })?;

        let connecting = self
            .endpoint
            .connect_with(self.client_config.clone(), socket_addr, &addr.host)
            .map_err(|e| AeonError::Connection {
                message: format!("failed to initiate QUIC connection to {addr}: {e}"),
                source: None,
                retryable: true,
            })?;

        let connection = connecting.await.map_err(|e| AeonError::Connection {
            message: format!("QUIC handshake failed with {addr}: {e}"),
            source: None,
            retryable: true,
        })?;

        tracing::debug!(%addr, "QUIC connection established (uncached)");
        Ok(connection)
    }

    /// Connect with retry on retryable errors (TR-3).
    ///
    /// Intended for bootstrap/join flows where the target may not be ready
    /// yet (e.g., the seed node is still starting, DNS hasn't propagated).
    /// Retries up to `max_attempts` times with exponential backoff + jitter
    /// per `policy`, returning the final error if all attempts fail.
    ///
    /// **Not** intended for the openraft RPC path — openraft has its own
    /// retry logic at the protocol level, and blocking a client task here
    /// would delay heartbeats. Use plain `connect()` for those.
    pub async fn connect_with_backoff(
        &self,
        node_id: NodeId,
        addr: &NodeAddress,
        policy: BackoffPolicy,
        max_attempts: usize,
    ) -> Result<Connection, AeonError> {
        let attempts = max_attempts.max(1);
        let mut backoff = policy.iter();
        let mut last_err: Option<AeonError> = None;
        for attempt in 1..=attempts {
            match self.connect(node_id, addr).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    let retryable = matches!(
                        &e,
                        AeonError::Connection { retryable: true, .. }
                    );
                    if !retryable || attempt == attempts {
                        return Err(e);
                    }
                    let delay = backoff.next_delay();
                    tracing::warn!(
                        node_id,
                        %addr,
                        attempt,
                        max_attempts = attempts,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        "connect failed; backing off before retry"
                    );
                    last_err = Some(e);
                    tokio::time::sleep(delay).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| AeonError::Connection {
            message: format!("connect_with_backoff exhausted {attempts} attempts"),
            source: None,
            retryable: true,
        }))
    }

    /// Accept an incoming connection.
    pub async fn accept(&self) -> Option<quinn::Incoming> {
        self.endpoint.accept().await
    }

    /// Remove a connection from the pool.
    pub fn disconnect(&self, node_id: NodeId) {
        if let Some((_, conn)) = self.connections.remove(&node_id) {
            conn.close(0u32.into(), b"disconnect");
        }
    }

    /// Check if a node has an active connection.
    pub fn is_connected(&self, node_id: NodeId) -> bool {
        self.connections
            .get(&node_id)
            .is_some_and(|c| c.close_reason().is_none())
    }

    /// Get the local address this endpoint is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, AeonError> {
        self.endpoint
            .local_addr()
            .map_err(|e| AeonError::Connection {
                message: format!("failed to get local address: {e}"),
                source: None,
                retryable: false,
            })
    }

    /// Close the endpoint.
    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::tls::{dev_quic_configs, dev_quic_configs_insecure};

    #[tokio::test]
    async fn endpoint_bind_and_local_addr() {
        let (server_cfg, client_cfg) = dev_quic_configs();
        let endpoint =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let addr = endpoint.local_addr().unwrap();
        assert_eq!(addr.ip(), std::net::Ipv4Addr::LOCALHOST);
        assert_ne!(addr.port(), 0); // OS assigned a port
        endpoint.close();
    }

    #[tokio::test]
    async fn endpoint_connect_and_pool() {
        // Use insecure configs to avoid IPv6/SAN issues on Windows
        let (server_cfg, client_cfg) = dev_quic_configs_insecure();

        // Start a "server" endpoint
        let server = QuicEndpoint::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_cfg.clone(),
            client_cfg.clone(),
        )
        .unwrap();
        let server_addr = server.local_addr().unwrap();

        // Spawn server accept loop (must accept connections for handshake to complete)
        let server_handle = tokio::spawn(async move {
            if let Some(incoming) = server.accept().await {
                let _conn = incoming.await.ok();
            }
            server
        });

        // Start a "client" endpoint using same certs
        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();

        // Use 127.0.0.1 to avoid IPv6 resolution on Windows (localhost → ::1 first)
        let target = NodeAddress::new("127.0.0.1", server_addr.port());

        // Connect
        let conn = client.connect(1, &target).await.unwrap();
        assert!(client.is_connected(1));

        // Second connect should reuse
        let conn2 = client.connect(1, &target).await.unwrap();
        assert_eq!(
            conn.stable_id(),
            conn2.stable_id(),
            "should reuse existing connection"
        );

        // Disconnect
        client.disconnect(1);
        assert!(!client.is_connected(1));

        client.close();
        let server = server_handle.await.unwrap();
        server.close();
    }

    #[tokio::test]
    async fn connect_with_backoff_returns_quickly_on_success() {
        let (server_cfg, client_cfg) = dev_quic_configs_insecure();

        let server = QuicEndpoint::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_cfg.clone(),
            client_cfg.clone(),
        )
        .unwrap();
        let server_addr = server.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            if let Some(incoming) = server.accept().await {
                let _conn = incoming.await.ok();
            }
            server
        });

        let client =
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap();
        let target = NodeAddress::new("127.0.0.1", server_addr.port());

        let start = std::time::Instant::now();
        let conn = client
            .connect_with_backoff(1, &target, BackoffPolicy::default(), 3)
            .await
            .unwrap();
        // Success on first attempt should not sleep.
        assert!(start.elapsed() < std::time::Duration::from_millis(500));
        assert!(conn.close_reason().is_none());

        client.close();
        let server = server_handle.await.unwrap();
        server.close();
    }

    // Note: an exhaust-path integration test (connect against a silent port
    // and assert retry exhaustion) takes ~90 s against quinn's default
    // handshake timeout of ~30 s per attempt — too slow for CI. The retry
    // mechanism's timing is already covered by `BackoffPolicy` unit tests
    // in aeon-types (11 tests: monotonic growth, cap, jitter, reset).
}
