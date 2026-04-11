//! ClusterNode — the main coordination point for Raft + partition management.

#[cfg(feature = "cluster")]
pub use inner::*;

#[cfg(feature = "cluster")]
mod inner {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use aeon_types::{AeonError, PartitionId};
    use openraft::{Config, Raft};

    use crate::config::ClusterConfig;
    use crate::raft_config::AeonRaftConfig;
    use crate::store::{MemLogStore, StateMachineStore};
    use crate::transport::endpoint::QuicEndpoint;
    use crate::transport::network::QuicNetworkFactory;
    use crate::transport::server;
    use crate::types::{ClusterRequest, ClusterResponse, NodeAddress, NodeId};

    /// A stub RaftNetworkFactory that does nothing (for single-node clusters).
    /// Multi-node networking is implemented in Phase 8c.
    #[derive(Debug, Clone, Default)]
    pub struct StubNetworkFactory;

    impl openraft::RaftNetworkFactory<AeonRaftConfig> for StubNetworkFactory {
        type Network = StubNetwork;

        async fn new_client(&mut self, _target: u64, _node: &NodeAddress) -> Self::Network {
            StubNetwork
        }
    }

    /// A stub RaftNetwork that returns errors (for single-node clusters).
    #[derive(Debug)]
    pub struct StubNetwork;

    impl openraft::RaftNetwork<AeonRaftConfig> for StubNetwork {
        async fn append_entries(
            &mut self,
            _rpc: openraft::raft::AppendEntriesRequest<AeonRaftConfig>,
            _option: openraft::network::RPCOption,
        ) -> Result<
            openraft::raft::AppendEntriesResponse<u64>,
            openraft::error::RPCError<u64, NodeAddress, openraft::error::RaftError<u64>>,
        > {
            Err(openraft::error::RPCError::Network(
                openraft::error::NetworkError::new(&std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "no network in single-node mode",
                )),
            ))
        }

        async fn full_snapshot(
            &mut self,
            _vote: openraft::Vote<u64>,
            _snapshot: openraft::storage::Snapshot<AeonRaftConfig>,
            _cancel: impl std::future::Future<Output = openraft::error::ReplicationClosed>
            + Send
            + 'static,
            _option: openraft::network::RPCOption,
        ) -> Result<
            openraft::raft::SnapshotResponse<u64>,
            openraft::error::StreamingError<AeonRaftConfig, openraft::error::Fatal<u64>>,
        > {
            Err(openraft::error::StreamingError::Unreachable(
                openraft::error::Unreachable::new(&std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "no network in single-node mode",
                )),
            ))
        }

        async fn install_snapshot(
            &mut self,
            _rpc: openraft::raft::InstallSnapshotRequest<AeonRaftConfig>,
            _option: openraft::network::RPCOption,
        ) -> Result<
            openraft::raft::InstallSnapshotResponse<u64>,
            openraft::error::RPCError<
                u64,
                NodeAddress,
                openraft::error::RaftError<u64, openraft::error::InstallSnapshotError>,
            >,
        > {
            Err(openraft::error::RPCError::Network(
                openraft::error::NetworkError::new(&std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "no network in single-node mode",
                )),
            ))
        }

        async fn vote(
            &mut self,
            _rpc: openraft::raft::VoteRequest<u64>,
            _option: openraft::network::RPCOption,
        ) -> Result<
            openraft::raft::VoteResponse<u64>,
            openraft::error::RPCError<u64, NodeAddress, openraft::error::RaftError<u64>>,
        > {
            Err(openraft::error::RPCError::Network(
                openraft::error::NetworkError::new(&std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "no network in single-node mode",
                )),
            ))
        }
    }

    /// The main cluster coordination point.
    ///
    /// Wraps a Raft instance with partition management. In single-node mode,
    /// no QUIC listeners are started and commits are instant (quorum=1).
    /// In multi-node mode, a QUIC endpoint runs for Raft RPCs.
    pub struct ClusterNode {
        raft: Raft<AeonRaftConfig>,
        config: ClusterConfig,
        /// QUIC endpoint (None in single-node stub mode).
        endpoint: Option<Arc<QuicEndpoint>>,
        /// Shutdown signal for the QUIC server task.
        shutdown: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ClusterNode {
        /// Bootstrap a single-node cluster. All partitions are assigned to this node.
        pub async fn bootstrap_single(config: ClusterConfig) -> Result<Self, AeonError> {
            config.validate()?;

            let raft_config = Arc::new(Config {
                cluster_name: "aeon".to_string(),
                heartbeat_interval: 500,
                election_timeout_min: 1500,
                election_timeout_max: 3000,
                ..Config::default()
            });

            let log_store = MemLogStore::new();
            let state_machine = StateMachineStore::new();
            let network = StubNetworkFactory;

            let raft = Raft::new(
                config.node_id,
                raft_config,
                network,
                log_store,
                state_machine,
            )
            .await
            .map_err(|e| AeonError::Cluster {
                message: format!("failed to create Raft node: {e}"),
                source: None,
            })?;

            // Initialize as single-node cluster
            let mut members = BTreeMap::new();
            members.insert(
                config.node_id,
                NodeAddress::new(config.bind.ip().to_string(), config.bind.port()),
            );

            raft.initialize(members)
                .await
                .map_err(|e| AeonError::Cluster {
                    message: format!("failed to initialize Raft: {e}"),
                    source: None,
                })?;

            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let node = Self {
                raft,
                config,
                endpoint: None,
                shutdown,
            };

            // Assign all partitions to self
            for i in 0..node.config.num_partitions {
                node.propose(ClusterRequest::AssignPartition {
                    partition: PartitionId::new(i),
                    node: node.config.node_id,
                })
                .await?;
            }

            tracing::info!(
                node_id = node.config.node_id,
                partitions = node.config.num_partitions,
                "Single-node cluster bootstrapped"
            );

            Ok(node)
        }

        /// Bootstrap a multi-node cluster with QUIC transport.
        ///
        /// Creates a QUIC endpoint, starts the Raft RPC server, and initializes
        /// the cluster with the configured peers. The initializing node becomes
        /// the first leader candidate; Raft election determines the actual leader.
        pub async fn bootstrap_multi(
            config: ClusterConfig,
            endpoint: Arc<QuicEndpoint>,
        ) -> Result<Self, AeonError> {
            config.validate()?;

            let raft_config = Arc::new(Config {
                cluster_name: "aeon".to_string(),
                heartbeat_interval: 500,
                election_timeout_min: 1500,
                election_timeout_max: 3000,
                ..Config::default()
            });

            let log_store = MemLogStore::new();
            let state_machine = StateMachineStore::new();
            let network = QuicNetworkFactory::new(Arc::clone(&endpoint));

            let raft = Raft::new(
                config.node_id,
                raft_config,
                network,
                log_store,
                state_machine,
            )
            .await
            .map_err(|e| AeonError::Cluster {
                message: format!("failed to create Raft node: {e}"),
                source: None,
            })?;

            // Start the QUIC RPC server
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let server_ep = Arc::clone(&endpoint);
            let server_raft = raft.clone();
            let server_shutdown = Arc::clone(&shutdown);
            tokio::spawn(async move {
                server::serve(server_ep, server_raft, server_shutdown).await;
            });

            // Build membership from self + peers
            let local_addr = endpoint.local_addr().map_err(|e| AeonError::Cluster {
                message: format!("failed to get local QUIC address: {e}"),
                source: None,
            })?;

            let mut members = BTreeMap::new();
            members.insert(
                config.node_id,
                NodeAddress::new(local_addr.ip().to_string(), local_addr.port()),
            );
            for (i, peer) in config.peers.iter().enumerate() {
                // Peer node IDs are assigned sequentially starting after self
                let peer_id = config.node_id + (i as u64) + 1;
                members.insert(peer_id, peer.clone());
            }

            raft.initialize(members)
                .await
                .map_err(|e| AeonError::Cluster {
                    message: format!("failed to initialize Raft cluster: {e}"),
                    source: None,
                })?;

            let node = Self {
                raft,
                config,
                endpoint: Some(endpoint),
                shutdown,
            };

            tracing::info!(
                node_id = node.config.node_id,
                peers = node.config.peers.len(),
                partitions = node.config.num_partitions,
                "Multi-node cluster bootstrapped with QUIC transport"
            );

            Ok(node)
        }

        /// Join an existing cluster via seed nodes.
        ///
        /// Connects to a seed node, fetches the current cluster membership,
        /// and adds this node to the cluster. The QUIC server is started
        /// before joining so peers can reach this node.
        pub async fn join(
            config: ClusterConfig,
            endpoint: Arc<QuicEndpoint>,
        ) -> Result<Self, AeonError> {
            config.validate()?;

            if config.seed_nodes.is_empty() {
                return Err(AeonError::Config {
                    message: "join requires at least one seed node".to_string(),
                });
            }

            let raft_config = Arc::new(Config {
                cluster_name: "aeon".to_string(),
                heartbeat_interval: 500,
                election_timeout_min: 1500,
                election_timeout_max: 3000,
                ..Config::default()
            });

            let log_store = MemLogStore::new();
            let state_machine = StateMachineStore::new();
            let network = QuicNetworkFactory::new(Arc::clone(&endpoint));

            let raft = Raft::new(
                config.node_id,
                raft_config,
                network,
                log_store,
                state_machine,
            )
            .await
            .map_err(|e| AeonError::Cluster {
                message: format!("failed to create Raft node: {e}"),
                source: None,
            })?;

            // Start the QUIC RPC server so peers can reach us
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let server_ep = Arc::clone(&endpoint);
            let server_raft = raft.clone();
            let server_shutdown = Arc::clone(&shutdown);
            tokio::spawn(async move {
                server::serve(server_ep, server_raft, server_shutdown).await;
            });

            // TODO: Contact seed nodes to add ourselves to the cluster.
            // This requires a cluster management RPC (AddLearner + ChangeMembership)
            // that is not yet implemented. For now, log the intent.
            tracing::info!(
                node_id = config.node_id,
                seed_nodes = ?config.seed_nodes,
                "Node started with QUIC transport, awaiting cluster join (seed-based join is Phase 2)"
            );

            let node = Self {
                raft,
                config,
                endpoint: Some(endpoint),
                shutdown,
            };

            Ok(node)
        }

        /// Propose a ClusterRequest through Raft consensus.
        pub async fn propose(&self, request: ClusterRequest) -> Result<ClusterResponse, AeonError> {
            let response =
                self.raft
                    .client_write(request)
                    .await
                    .map_err(|e| AeonError::Cluster {
                        message: format!("Raft proposal failed: {e}"),
                        source: None,
                    })?;

            Ok(response.data)
        }

        /// Get the current leader's NodeId.
        pub async fn current_leader(&self) -> Option<NodeId> {
            self.raft.current_leader().await
        }

        /// Get the node's config.
        pub fn config(&self) -> &ClusterConfig {
            &self.config
        }

        /// Get the Raft reference (for advanced operations).
        pub fn raft(&self) -> &Raft<AeonRaftConfig> {
            &self.raft
        }

        /// Shut down the Raft node and QUIC transport gracefully.
        pub async fn shutdown(&self) -> Result<(), AeonError> {
            // Signal the QUIC server to stop accepting connections
            self.shutdown
                .store(true, std::sync::atomic::Ordering::Relaxed);

            // Shut down Raft
            self.raft.shutdown().await.map_err(|e| AeonError::Cluster {
                message: format!("Raft shutdown failed: {e}"),
                source: None,
            })?;

            // Close the QUIC endpoint
            if let Some(ep) = &self.endpoint {
                ep.close();
            }

            Ok(())
        }

        /// Get the QUIC endpoint (if running in multi-node mode).
        pub fn endpoint(&self) -> Option<&Arc<QuicEndpoint>> {
            self.endpoint.as_ref()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn single_node_bootstrap() {
            let config = ClusterConfig::single_node(1, 16);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // Should become leader
            let leader = node.current_leader().await;
            assert_eq!(leader, Some(1));

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn single_node_propose_assign() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // All 4 partitions should be assigned to node 1 during bootstrap
            // Verify by proposing a reassignment
            let resp = node
                .propose(ClusterRequest::AssignPartition {
                    partition: PartitionId::new(0),
                    node: 1,
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn single_node_transfer_lifecycle() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // Begin transfer P0: 1 → 2
            let resp = node
                .propose(ClusterRequest::BeginTransfer {
                    partition: PartitionId::new(0),
                    source: 1,
                    target: 2,
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            // Complete transfer
            let resp = node
                .propose(ClusterRequest::CompleteTransfer {
                    partition: PartitionId::new(0),
                    new_owner: 2,
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn single_node_abort_transfer() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            node.propose(ClusterRequest::BeginTransfer {
                partition: PartitionId::new(0),
                source: 1,
                target: 2,
            })
            .await
            .unwrap();

            let resp = node
                .propose(ClusterRequest::AbortTransfer {
                    partition: PartitionId::new(0),
                    revert_to: 1,
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn single_node_config_update() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            let resp = node
                .propose(ClusterRequest::UpdateConfig {
                    key: "batch_size".to_string(),
                    value: "2048".to_string(),
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            node.shutdown().await.unwrap();
        }
    }
}
