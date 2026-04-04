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
    pub struct ClusterNode {
        raft: Raft<AeonRaftConfig>,
        config: ClusterConfig,
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

            let node = Self { raft, config };

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

        /// Shut down the Raft node gracefully.
        pub async fn shutdown(&self) -> Result<(), AeonError> {
            self.raft.shutdown().await.map_err(|e| AeonError::Cluster {
                message: format!("Raft shutdown failed: {e}"),
                source: None,
            })?;
            Ok(())
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
