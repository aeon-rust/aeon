//! ClusterNode — the main coordination point for Raft + partition management.

#[cfg(feature = "cluster")]
pub use inner::*;

#[cfg(feature = "cluster")]
mod inner {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use aeon_types::{AeonError, L3Store, PartitionId};
    use openraft::{Config, Raft};

    use crate::config::ClusterConfig;
    use crate::log_store::L3RaftLogStore;
    use crate::raft_config::AeonRaftConfig;
    use crate::store::{MemLogStore, StateMachineStore};
    use crate::transport::endpoint::QuicEndpoint;
    use crate::transport::network::QuicNetworkFactory;
    use crate::transport::server;
    use crate::types::{ClusterRequest, ClusterResponse, JoinRequest, NodeAddress, NodeId};

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
        /// Shared read handle to the committed cluster state.
        shared_state: crate::store::SharedClusterState,
        /// Handle to the state machine's registry applier slot. Used by
        /// `install_registry_applier` to wire up the node-local engine after
        /// bootstrap, since openraft takes ownership of the state machine.
        applier_slot: crate::store::RegistryApplierSlot,
    }

    impl ClusterNode {
        /// Bootstrap a single-node cluster. All partitions are assigned to this node.
        pub async fn bootstrap_single(config: ClusterConfig) -> Result<Self, AeonError> {
            config.validate()?;

            let raft_config = Arc::new(Config {
                cluster_name: "aeon".to_string(),
                heartbeat_interval: config.raft_timing.heartbeat_ms,
                election_timeout_min: config.raft_timing.election_min_ms,
                election_timeout_max: config.raft_timing.election_max_ms,
                ..Config::default()
            });

            let log_store = MemLogStore::new();
            let state_machine = StateMachineStore::new();
            let shared_state = state_machine.shared_state();
            let applier_slot = state_machine.applier_slot();
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
                shared_state,
                applier_slot,
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

        /// Bootstrap a single-node cluster backed by a persistent Raft log (FT-1).
        ///
        /// Identical to [`Self::bootstrap_single`] but uses [`L3RaftLogStore`]
        /// over the caller-supplied `Arc<dyn L3Store>` instead of the in-memory
        /// `MemLogStore`. The caller owns the L3 handle (typically a
        /// `RedbStore` from `aeon-state`) so the same backing file can be
        /// reopened after a restart to recover log + vote state.
        ///
        /// Partition-initialization is skipped when the state machine reports
        /// partitions already assigned, so a restarted node doesn't duplicate
        /// the bootstrap proposals.
        pub async fn bootstrap_single_persistent(
            config: ClusterConfig,
            log_backend: Arc<dyn L3Store>,
        ) -> Result<Self, AeonError> {
            config.validate()?;

            let raft_config = Arc::new(Config {
                cluster_name: "aeon".to_string(),
                heartbeat_interval: config.raft_timing.heartbeat_ms,
                election_timeout_min: config.raft_timing.election_min_ms,
                election_timeout_max: config.raft_timing.election_max_ms,
                ..Config::default()
            });

            let log_store = L3RaftLogStore::open(Arc::clone(&log_backend))
                .await
                .map_err(|e| AeonError::Cluster {
                    message: format!("failed to open persistent Raft log: {e}"),
                    source: None,
                })?;
            // FT-2: share the same L3 backend for persistent snapshots so a
            // restart can recover from snapshot + tail log rather than full
            // log replay.
            let state_machine = StateMachineStore::new_persistent(log_backend)?;
            let shared_state = state_machine.shared_state();
            let applier_slot = state_machine.applier_slot();
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

            // initialize() is idempotent for single-node — safe to call on
            // both first boot and restart. If the log already contains an
            // initialization entry, openraft returns AlreadyInitialized,
            // which we treat as success.
            let mut members = BTreeMap::new();
            members.insert(
                config.node_id,
                NodeAddress::new(config.bind.ip().to_string(), config.bind.port()),
            );
            match raft.initialize(members).await {
                Ok(()) => {}
                Err(openraft::error::RaftError::APIError(
                    openraft::error::InitializeError::NotAllowed(_),
                )) => {
                    // Already initialized — restart path, no action needed.
                    tracing::info!(
                        node_id = config.node_id,
                        "Raft log recovered from persistent store — skipping initialize"
                    );
                }
                Err(e) => {
                    return Err(AeonError::Cluster {
                        message: format!("failed to initialize Raft: {e}"),
                        source: None,
                    });
                }
            }

            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let node = Self {
                raft,
                config,
                endpoint: None,
                shutdown,
                shared_state,
                applier_slot,
            };

            // Skip partition assignment on restart — the applied state machine
            // (rebuilt from the persisted log) already has the assignments.
            // On first boot the partition_table is empty, so we assign.
            let already_assigned = node
                .shared_state
                .read()
                .await
                .partition_table
                .iter()
                .next()
                .is_some();
            if !already_assigned {
                for i in 0..node.config.num_partitions {
                    node.propose(ClusterRequest::AssignPartition {
                        partition: PartitionId::new(i),
                        node: node.config.node_id,
                    })
                    .await?;
                }
            }

            tracing::info!(
                node_id = node.config.node_id,
                partitions = node.config.num_partitions,
                restart = already_assigned,
                "Single-node cluster bootstrapped with persistent Raft log"
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
                heartbeat_interval: config.raft_timing.heartbeat_ms,
                election_timeout_min: config.raft_timing.election_min_ms,
                election_timeout_max: config.raft_timing.election_max_ms,
                ..Config::default()
            });

            let log_store = MemLogStore::new();
            let state_machine = StateMachineStore::new();
            let shared_state = state_machine.shared_state();
            let applier_slot = state_machine.applier_slot();
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

            // Build membership — use pre-computed initial_members if available,
            // otherwise derive from local endpoint address + peers
            let members: BTreeMap<u64, NodeAddress> = if !config.initial_members.is_empty() {
                config.initial_members.iter().cloned().collect()
            } else {
                let local_addr = endpoint.local_addr().map_err(|e| AeonError::Cluster {
                    message: format!("failed to get local QUIC address: {e}"),
                    source: None,
                })?;
                let mut m = BTreeMap::new();
                m.insert(
                    config.node_id,
                    NodeAddress::new(local_addr.ip().to_string(), local_addr.port()),
                );
                for (i, peer) in config.peers.iter().enumerate() {
                    let peer_id = config.node_id + (i as u64) + 1;
                    m.insert(peer_id, peer.clone());
                }
                m
            };

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
                shared_state,
                applier_slot,
            };

            // NOTE: InitialAssignment is NOT proposed here — it must happen
            // after leader election completes, and only on the leader node.
            // The caller (main.rs) handles this after waiting for election.

            tracing::info!(
                node_id = node.config.node_id,
                peers = node.config.peers.len(),
                partitions = node.config.num_partitions,
                "Multi-node cluster bootstrapped with QUIC transport"
            );

            Ok(node)
        }

        /// Start as a peer node in a fresh cluster bootstrap.
        ///
        /// Unlike `bootstrap_multi()`, this does NOT call `raft.initialize()`.
        /// The bootstrap node (node 1) initializes the cluster with all members;
        /// peer nodes just start their Raft instance + QUIC server and wait for
        /// the leader to replicate the initial log entries via AppendEntries.
        pub async fn start_peer(
            config: ClusterConfig,
            endpoint: Arc<QuicEndpoint>,
        ) -> Result<Self, AeonError> {
            config.validate()?;

            let raft_config = Arc::new(Config {
                cluster_name: "aeon".to_string(),
                heartbeat_interval: config.raft_timing.heartbeat_ms,
                election_timeout_min: config.raft_timing.election_min_ms,
                election_timeout_max: config.raft_timing.election_max_ms,
                ..Config::default()
            });

            let log_store = MemLogStore::new();
            let state_machine = StateMachineStore::new();
            let applier_slot = state_machine.applier_slot();
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

            // Start the QUIC RPC server so the bootstrap leader can reach us
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let server_ep = Arc::clone(&endpoint);
            let server_raft = raft.clone();
            let server_shutdown = Arc::clone(&shutdown);
            tokio::spawn(async move {
                server::serve(server_ep, server_raft, server_shutdown).await;
            });

            let shared_state = crate::store::SharedClusterState::default();

            tracing::info!(
                node_id = config.node_id,
                peers = config.peers.len(),
                "Peer node started — awaiting cluster initialization from bootstrap node"
            );

            let node = Self {
                raft,
                config,
                endpoint: Some(endpoint),
                shutdown,
                shared_state,
                applier_slot,
            };

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
                heartbeat_interval: config.raft_timing.heartbeat_ms,
                election_timeout_min: config.raft_timing.election_min_ms,
                election_timeout_max: config.raft_timing.election_max_ms,
                ..Config::default()
            });

            let log_store = MemLogStore::new();
            let state_machine = StateMachineStore::new();
            let applier_slot = state_machine.applier_slot();
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

            // Use advertise_addr if set; otherwise fall back to bind address.
            // In K8s, advertise_addr should be the pod's DNS name.
            let self_addr = config
                .advertise_addr
                .clone()
                .unwrap_or_else(|| NodeAddress::new(
                    config.bind.ip().to_string(),
                    config.bind.port(),
                ));

            let join_req = JoinRequest {
                node_id: config.node_id,
                addr: self_addr,
            };

            // Try each seed node until one accepts the join.
            // If a seed is not the leader, it returns leader_id — retry against
            // the actual leader via any seed that knows the leader address.
            let mut last_err = String::from("no seed nodes responded");
            let mut joined = false;

            for seed in &config.seed_nodes {
                // Use a temporary node_id of 0 for the seed connection target
                // (we don't know the seed's Raft ID, but the connection only
                // needs the address for DNS resolution).
                tracing::info!(seed = %seed, "attempting to join cluster via seed node");

                match crate::transport::network::send_join_request(
                    &endpoint,
                    0, // target_id is only used for connection caching
                    seed,
                    &join_req,
                )
                .await
                {
                    Ok(resp) if resp.success => {
                        tracing::info!(
                            node_id = config.node_id,
                            leader = ?resp.leader_id,
                            "successfully joined cluster via seed {}",
                            seed
                        );
                        joined = true;
                        break;
                    }
                    Ok(resp) => {
                        tracing::warn!(
                            seed = %seed,
                            leader = ?resp.leader_id,
                            msg = %resp.message,
                            "seed rejected join, will try next"
                        );
                        last_err = resp.message;
                    }
                    Err(e) => {
                        tracing::warn!(seed = %seed, error = %e, "failed to contact seed");
                        last_err = e.to_string();
                    }
                }
            }

            if !joined {
                return Err(AeonError::Cluster {
                    message: format!("failed to join cluster via any seed node: {last_err}"),
                    source: None,
                });
            }

            let node = Self {
                raft,
                config,
                endpoint: Some(endpoint),
                shutdown,
                shared_state: crate::store::SharedClusterState::default(),
                applier_slot,
            };

            Ok(node)
        }

        /// Add a node to the cluster dynamically.
        ///
        /// This is a two-step process:
        /// 1. Add as learner — starts replication, blocks until caught up
        /// 2. Promote to voter — participates in elections and quorum
        ///
        /// Must be called on the current leader. If not leader, returns an error.
        pub async fn add_node(
            &self,
            node_id: NodeId,
            addr: NodeAddress,
        ) -> Result<(), AeonError> {
            // Step 1: Add as learner (blocking = true → waits for log catch-up)
            self.raft
                .add_learner(node_id, addr, true)
                .await
                .map_err(|e| AeonError::Cluster {
                    message: format!("failed to add learner node {node_id}: {e}"),
                    source: None,
                })?;

            tracing::info!(node_id, "node added as learner, promoting to voter");

            // Step 2: Promote to voter
            let mut voters = std::collections::BTreeSet::new();
            voters.insert(node_id);
            self.raft
                .change_membership(
                    openraft::ChangeMembers::AddVoterIds(voters),
                    false, // don't retain removed voters as learners
                )
                .await
                .map_err(|e| AeonError::Cluster {
                    message: format!("failed to promote node {node_id} to voter: {e}"),
                    source: None,
                })?;

            tracing::info!(node_id, "node promoted to voter");

            // Step 3: Rebalance partitions to include the new node
            let members = self.members().await?;
            let all_nodes: Vec<u64> = members.keys().copied().collect();
            if !all_nodes.is_empty() {
                self.propose(ClusterRequest::RebalancePartitions {
                    nodes: all_nodes.clone(),
                })
                .await?;
                tracing::info!(
                    node_id,
                    cluster_size = all_nodes.len(),
                    "partition rebalance proposed after node addition"
                );
            }

            Ok(())
        }

        /// Remove a node from the cluster dynamically.
        ///
        /// This is a two-step process:
        /// 1. Demote from voter — stops participating in quorum
        /// 2. Remove from cluster — stops replication
        ///
        /// Must be called on the current leader. The leader cannot remove itself.
        pub async fn remove_node(&self, node_id: NodeId) -> Result<(), AeonError> {
            if node_id == self.config.node_id {
                return Err(AeonError::Config {
                    message: "a node cannot remove itself from the cluster; \
                              transfer leadership first"
                        .to_string(),
                });
            }

            // Step 1: Demote from voter to learner
            let mut to_remove = std::collections::BTreeSet::new();
            to_remove.insert(node_id);
            self.raft
                .change_membership(
                    openraft::ChangeMembers::RemoveVoters(to_remove.clone()),
                    false,
                )
                .await
                .map_err(|e| AeonError::Cluster {
                    message: format!("failed to demote node {node_id} from voter: {e}"),
                    source: None,
                })?;

            tracing::info!(node_id, "node demoted from voter, removing from cluster");

            // Step 2: Remove from cluster entirely
            self.raft
                .change_membership(
                    openraft::ChangeMembers::RemoveNodes(to_remove),
                    false,
                )
                .await
                .map_err(|e| AeonError::Cluster {
                    message: format!("failed to remove node {node_id} from cluster: {e}"),
                    source: None,
                })?;

            // Disconnect QUIC connection to removed node
            if let Some(ep) = &self.endpoint {
                ep.disconnect(node_id);
            }

            tracing::info!(node_id, "node removed from cluster");

            // Step 3: Rebalance partitions away from the removed node
            let members = self.members().await?;
            let remaining_nodes: Vec<u64> = members.keys().copied().collect();
            if !remaining_nodes.is_empty() {
                self.propose(ClusterRequest::RebalancePartitions {
                    nodes: remaining_nodes.clone(),
                })
                .await?;
                tracing::info!(
                    removed = node_id,
                    remaining = remaining_nodes.len(),
                    "partition rebalance proposed after node removal"
                );
            }

            Ok(())
        }

        /// Get the current cluster members (voters and learners).
        pub async fn members(
            &self,
        ) -> Result<BTreeMap<NodeId, NodeAddress>, AeonError> {
            let metrics = self.raft.metrics().borrow().clone();
            let membership = metrics
                .membership_config
                .membership()
                .clone();

            // Collect all nodes (voters + learners)
            let mut result = BTreeMap::new();
            for (id, node) in membership.nodes() {
                result.insert(*id, node.clone());
            }
            Ok(result)
        }

        /// Check if this node is currently the Raft leader.
        pub async fn is_leader(&self) -> bool {
            self.raft
                .current_leader()
                .await
                .is_some_and(|leader| leader == self.config.node_id)
        }

        /// Get the current cluster size (number of voters).
        pub async fn voter_count(&self) -> usize {
            let metrics = self.raft.metrics().borrow().clone();
            metrics
                .membership_config
                .membership()
                .voter_ids()
                .count()
        }

        /// Format current Raft + cluster metrics as Prometheus exposition text (CL-4).
        ///
        /// Emits:
        /// - `aeon_raft_term` — current term (monotonic; increments on each election attempt)
        /// - `aeon_raft_last_log_index` — most recent log index
        /// - `aeon_raft_last_applied_index` — most recent applied log index
        /// - `aeon_raft_leader_id` — current leader's NodeId (0 if unknown)
        /// - `aeon_raft_is_leader` — 1 on the leader, 0 elsewhere
        /// - `aeon_raft_state` — 0=Learner, 1=Follower, 2=Candidate, 3=Leader, 4=Shutdown
        /// - `aeon_cluster_membership_size` — voter count
        /// - `aeon_cluster_node_id` — this node's NodeId (label for multi-node scrape)
        /// - `aeon_raft_millis_since_quorum_ack` — leader-only; elapsed ms since
        ///   last quorum ack (high values suggest leader partitioned from cluster)
        /// - `aeon_raft_replication_lag{follower="N"}` — leader-only; per-follower
        ///   replication lag in log entries (last_log_index - follower_matched_index)
        ///
        /// Monitoring hints:
        /// - A flapping `aeon_raft_term` + low `aeon_raft_is_leader` across the
        ///   cluster signals election storms (GAP 6 / pre-vote mitigations in FT-4).
        /// - A sustained non-zero `aeon_raft_replication_lag` on a follower
        ///   indicates catch-up pressure or follower lag.
        /// - `aeon_raft_millis_since_quorum_ack` above 2× election_max suggests
        ///   the leader has lost contact with a quorum.
        pub fn cluster_metrics_prometheus(&self) -> String {
            use openraft::ServerState;

            let metrics = self.raft.metrics().borrow().clone();

            let term = metrics.current_term;
            let last_log = metrics.last_log_index.unwrap_or(0);
            let last_applied = metrics.last_applied.map(|l| l.index).unwrap_or(0);
            let leader = metrics.current_leader.unwrap_or(0);
            let node_id = self.config.node_id;
            let is_leader: u64 = if metrics.current_leader == Some(node_id) { 1 } else { 0 };
            let state_code: u64 = match metrics.state {
                ServerState::Learner => 0,
                ServerState::Follower => 1,
                ServerState::Candidate => 2,
                ServerState::Leader => 3,
                ServerState::Shutdown => 4,
            };
            let membership_size = metrics
                .membership_config
                .membership()
                .voter_ids()
                .count() as u64;

            let mut out = String::with_capacity(2048);

            out.push_str("# HELP aeon_raft_term Current Raft term (monotonic; +1 on each election attempt)\n");
            out.push_str("# TYPE aeon_raft_term gauge\n");
            out.push_str(&format!("aeon_raft_term{{node_id=\"{node_id}\"}} {term}\n"));

            out.push_str("# HELP aeon_raft_last_log_index Most recent Raft log index\n");
            out.push_str("# TYPE aeon_raft_last_log_index gauge\n");
            out.push_str(&format!("aeon_raft_last_log_index{{node_id=\"{node_id}\"}} {last_log}\n"));

            out.push_str("# HELP aeon_raft_last_applied_index Most recent applied log index\n");
            out.push_str("# TYPE aeon_raft_last_applied_index gauge\n");
            out.push_str(&format!("aeon_raft_last_applied_index{{node_id=\"{node_id}\"}} {last_applied}\n"));

            out.push_str("# HELP aeon_raft_leader_id Current Raft leader NodeId (0 if unknown)\n");
            out.push_str("# TYPE aeon_raft_leader_id gauge\n");
            out.push_str(&format!("aeon_raft_leader_id{{node_id=\"{node_id}\"}} {leader}\n"));

            out.push_str("# HELP aeon_raft_is_leader 1 if this node is the current leader, 0 otherwise\n");
            out.push_str("# TYPE aeon_raft_is_leader gauge\n");
            out.push_str(&format!("aeon_raft_is_leader{{node_id=\"{node_id}\"}} {is_leader}\n"));

            out.push_str("# HELP aeon_raft_state Raft server state (0=Learner,1=Follower,2=Candidate,3=Leader)\n");
            out.push_str("# TYPE aeon_raft_state gauge\n");
            out.push_str(&format!("aeon_raft_state{{node_id=\"{node_id}\"}} {state_code}\n"));

            out.push_str("# HELP aeon_cluster_membership_size Number of voters in the current membership\n");
            out.push_str("# TYPE aeon_cluster_membership_size gauge\n");
            out.push_str(&format!("aeon_cluster_membership_size{{node_id=\"{node_id}\"}} {membership_size}\n"));

            out.push_str("# HELP aeon_cluster_node_id This node's configured NodeId\n");
            out.push_str("# TYPE aeon_cluster_node_id gauge\n");
            out.push_str(&format!("aeon_cluster_node_id {node_id}\n"));

            if let Some(ms) = metrics.millis_since_quorum_ack {
                out.push_str("# HELP aeon_raft_millis_since_quorum_ack Elapsed ms since last quorum ack (leader only)\n");
                out.push_str("# TYPE aeon_raft_millis_since_quorum_ack gauge\n");
                out.push_str(&format!(
                    "aeon_raft_millis_since_quorum_ack{{node_id=\"{node_id}\"}} {ms}\n"
                ));
            }

            if let Some(repl) = metrics.replication.as_ref() {
                out.push_str("# HELP aeon_raft_replication_lag Per-follower replication lag in log entries (leader only)\n");
                out.push_str("# TYPE aeon_raft_replication_lag gauge\n");
                for (follower_id, matched) in repl.iter() {
                    let matched_idx = matched.as_ref().map(|l| l.index).unwrap_or(0);
                    let lag = last_log.saturating_sub(matched_idx);
                    out.push_str(&format!(
                        "aeon_raft_replication_lag{{node_id=\"{node_id}\",follower=\"{follower_id}\"}} {lag}\n"
                    ));
                }
            }

            out
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

        /// Install (or replace) the registry applier that receives committed
        /// `ClusterRequest::Registry` entries. Typically called once at
        /// startup from `aeon-engine` after constructing the local
        /// PipelineManager / ProcessorRegistry.
        pub async fn install_registry_applier(
            &self,
            applier: Arc<dyn aeon_types::RegistryApplier>,
        ) {
            *self.applier_slot.write().await = Some(applier);
        }

        /// Propose a `RegistryCommand` through Raft and decode the response.
        ///
        /// Convenience wrapper that encodes the command into
        /// `ClusterRequest::Registry`, submits it via [`propose`], and
        /// decodes the `ClusterResponse::Registry` reply back into a
        /// `RegistryResponse`. Use this when the caller holds a
        /// `RegistryCommand` (e.g. REST handlers creating a pipeline) and
        /// wants the replicated apply to run on every node in the cluster.
        pub async fn propose_registry(
            &self,
            cmd: aeon_types::RegistryCommand,
        ) -> Result<aeon_types::RegistryResponse, AeonError> {
            let req = ClusterRequest::registry(&cmd)?;
            let resp = self.propose(req).await?;
            resp.into_registry()
        }

        /// Get the current leader's NodeId.
        pub async fn current_leader(&self) -> Option<NodeId> {
            self.raft.current_leader().await
        }

        /// Propose initial partition assignment via Raft.
        ///
        /// Must be called only on the leader node after election completes.
        /// Uses round-robin distribution across all nodes. Idempotent — if
        /// partitions are already assigned, the state machine ignores duplicates.
        pub async fn assign_initial_partitions(&self) -> Result<(), AeonError> {
            // Derive node IDs from initial_members (populated during K8s discovery).
            // Falls back to collecting from Raft membership metrics.
            let nodes: Vec<u64> = if !self.config.initial_members.is_empty() {
                let mut ids: Vec<u64> =
                    self.config.initial_members.iter().map(|(id, _)| *id).collect();
                ids.sort();
                ids
            } else {
                let metrics = self.raft.metrics().borrow().clone();
                let mut ids: Vec<u64> = metrics
                    .membership_config
                    .membership()
                    .voter_ids()
                    .collect();
                ids.sort();
                ids
            };
            self.propose(ClusterRequest::InitialAssignment {
                num_partitions: self.config.num_partitions,
                nodes,
            })
            .await?;
            Ok(())
        }

        /// Get a read handle to the committed cluster state.
        pub fn shared_state(&self) -> crate::store::SharedClusterState {
            Arc::clone(&self.shared_state)
        }

        /// Get the current partition table from the committed state.
        pub async fn partition_table_snapshot(
            &self,
        ) -> crate::types::PartitionTable {
            self.shared_state.read().await.partition_table.clone()
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

    impl aeon_types::CheckpointReplicator for ClusterNode {
        async fn submit_checkpoint(
            &self,
            source_offsets: std::collections::HashMap<u16, i64>,
        ) -> Result<(), aeon_types::AeonError> {
            self.propose(ClusterRequest::SubmitCheckpoint { source_offsets })
                .await
                .map(|_| ())
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

        #[tokio::test]
        async fn rebalance_partitions_via_raft() {
            let config = ClusterConfig::single_node(1, 8);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // All 8 partitions are on node 1. Rebalance to include nodes 1,2,3.
            let resp = node
                .propose(ClusterRequest::RebalancePartitions {
                    nodes: vec![1, 2, 3],
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn initial_assignment_via_raft() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // Override with a 6-partition assignment across 3 nodes
            let resp = node
                .propose(ClusterRequest::InitialAssignment {
                    num_partitions: 6,
                    nodes: vec![1, 2, 3],
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn rebalance_after_node_removal() {
            let config = ClusterConfig::single_node(1, 6);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // First distribute across 3 nodes
            let resp = node
                .propose(ClusterRequest::RebalancePartitions {
                    nodes: vec![1, 2, 3],
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            // Now node 3 is removed — rebalance to just nodes 1 and 2
            let resp = node
                .propose(ClusterRequest::RebalancePartitions {
                    nodes: vec![1, 2],
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn submit_checkpoint_offsets_via_raft() {
            use aeon_types::CheckpointReplicator;

            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            let mut offsets = std::collections::HashMap::new();
            offsets.insert(0u16, 100i64);
            offsets.insert(1u16, 200i64);
            offsets.insert(2u16, 300i64);

            // Submit via the CheckpointReplicator trait
            node.submit_checkpoint(offsets.clone()).await.unwrap();

            // Submit again with higher offsets — should advance
            let mut offsets2 = std::collections::HashMap::new();
            offsets2.insert(0u16, 150i64);
            offsets2.insert(1u16, 180i64); // lower than 200 — should NOT regress
            offsets2.insert(3u16, 50i64); // new partition
            node.submit_checkpoint(offsets2).await.unwrap();

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn rebalance_empty_nodes_is_noop() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // Rebalance with empty nodes list should succeed (no-op)
            let resp = node
                .propose(ClusterRequest::RebalancePartitions {
                    nodes: vec![],
                })
                .await
                .unwrap();
            assert_eq!(resp, ClusterResponse::Ok);

            node.shutdown().await.unwrap();
        }
    }
}
