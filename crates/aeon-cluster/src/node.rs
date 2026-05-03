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
    use crate::types::{
        ClusterRequest, ClusterResponse, JoinRequest, NodeAddress, NodeId, TransferStatus,
    };

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
        /// Shutdown signal for the QUIC server task + transfer watch loop.
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        /// Shared read handle to the committed cluster state.
        shared_state: crate::store::SharedClusterState,
        /// Handle to the state machine's registry applier slot. Used by
        /// `install_registry_applier` to wire up the node-local engine after
        /// bootstrap, since openraft takes ownership of the state machine.
        applier_slot: crate::store::RegistryApplierSlot,
        /// G11.b — optional leader-side partition-transfer driver. Installed
        /// after bootstrap (the driver depends on engine-side installers).
        /// `propose_partition_transfer` calls `notify()` here so the watcher
        /// wakes immediately instead of polling. None on nodes that don't
        /// run the driver (e.g. single-node dev mode).
        transfer_driver: tokio::sync::RwLock<
            Option<Arc<crate::partition_driver::PartitionTransferDriver>>,
        >,
        /// CL-6c.4 — source-side provider slots serviced by the QUIC
        /// accept loop. Populated post-bootstrap by `aeon-cli` once the
        /// engine-side installers exist. `install_segment_provider` /
        /// `install_poh_provider` / `install_cutover_coordinator` delegate
        /// here. Multi-node bootstrap paths hand a clone to
        /// `server::serve_with_slots` so late installation becomes active
        /// without restarting the accept task.
        source_provider_slots: crate::transport::server::SourceProviderSlots,
        /// P5: receiver for the per-node owned-partitions watch. Subscribers
        /// (pipeline source loop) clone this via `watch_owned_partitions()`
        /// and select on `changed()` to re-assign the live source without
        /// tearing down the pipeline task on a CL-6 transfer commit.
        owned_partitions_rx: tokio::sync::watch::Receiver<Vec<PartitionId>>,
    }

    /// Install an `OwnedPartitionsWatch` into the state machine's slot and
    /// return the paired receiver. Shared helper used by every bootstrap
    /// path so the sender is in place before the initial partition-assignment
    /// proposals commit — if it weren't, those early applies would fire
    /// against an empty slot and subscribers would start out-of-sync.
    async fn install_owned_partitions_watch(
        slot: &crate::store::OwnedPartitionsWatchSlot,
        node_id: NodeId,
    ) -> tokio::sync::watch::Receiver<Vec<PartitionId>> {
        let (sender, rx) = tokio::sync::watch::channel(Vec::<PartitionId>::new());
        *slot.write().await = Some(crate::store::OwnedPartitionsWatch { node_id, sender });
        rx
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
            let owned_watch_slot = state_machine.owned_partitions_watch_slot();
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

            let owned_partitions_rx =
                install_owned_partitions_watch(&owned_watch_slot, config.node_id).await;

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
                transfer_driver: tokio::sync::RwLock::new(None),
                source_provider_slots: crate::transport::server::SourceProviderSlots::new_empty(),
                owned_partitions_rx,
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
            let owned_watch_slot = state_machine.owned_partitions_watch_slot();
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
            let owned_partitions_rx =
                install_owned_partitions_watch(&owned_watch_slot, config.node_id).await;

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
                transfer_driver: tokio::sync::RwLock::new(None),
                source_provider_slots: crate::transport::server::SourceProviderSlots::new_empty(),
                owned_partitions_rx,
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
            let owned_watch_slot = state_machine.owned_partitions_watch_slot();
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

            let owned_partitions_rx =
                install_owned_partitions_watch(&owned_watch_slot, config.node_id).await;

            // Start the QUIC RPC server
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let source_provider_slots =
                crate::transport::server::SourceProviderSlots::new_empty();
            let server_ep = Arc::clone(&endpoint);
            let server_raft = raft.clone();
            let server_node_id = config.node_id;
            let server_shutdown = Arc::clone(&shutdown);
            let server_slots = source_provider_slots.clone();
            tokio::spawn(async move {
                server::serve_with_slots(
                    server_ep,
                    server_raft,
                    server_node_id,
                    server_shutdown,
                    server_slots,
                )
                .await;
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
                transfer_driver: tokio::sync::RwLock::new(None),
                source_provider_slots,
                owned_partitions_rx,
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

        /// FT-1 + FT-2: persistent variant of [`Self::bootstrap_multi`].
        ///
        /// Identical control flow to `bootstrap_multi`, but the Raft log
        /// and state machine live on a caller-supplied `Arc<dyn L3Store>`
        /// (typically a `RedbStore` opened at
        /// `${artifact_dir}/raft.redb`). On pod restart the same backing
        /// file is reopened and `raft.initialize()` returns
        /// `InitializeError::NotAllowed` — we treat that as the recovery
        /// path and skip re-initialization. Without this, every pod
        /// restart would force a fresh election from term 1, which the
        /// other still-running pods reject by their higher terms,
        /// surfacing as the split-brain we hit during M2 testing.
        ///
        /// Partition assignment is *not* proposed here for the same
        /// reason as `bootstrap_multi`: only the elected leader proposes
        /// `InitialAssignment`, and the caller (cmd_serve) drives that
        /// after waiting for leader election. On a restart the
        /// partition_table is already populated in the recovered state
        /// machine, so the leader's "is the table empty?" check
        /// short-circuits and no proposal is made.
        pub async fn bootstrap_multi_persistent(
            config: ClusterConfig,
            endpoint: Arc<QuicEndpoint>,
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
            let state_machine = StateMachineStore::new_persistent(log_backend)?;
            let shared_state = state_machine.shared_state();
            let applier_slot = state_machine.applier_slot();
            let owned_watch_slot = state_machine.owned_partitions_watch_slot();
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

            let owned_partitions_rx =
                install_owned_partitions_watch(&owned_watch_slot, config.node_id).await;

            // Start the QUIC RPC server (same as bootstrap_multi).
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let source_provider_slots =
                crate::transport::server::SourceProviderSlots::new_empty();
            let server_ep = Arc::clone(&endpoint);
            let server_raft = raft.clone();
            let server_node_id = config.node_id;
            let server_shutdown = Arc::clone(&shutdown);
            let server_slots = source_provider_slots.clone();
            tokio::spawn(async move {
                server::serve_with_slots(
                    server_ep,
                    server_raft,
                    server_node_id,
                    server_shutdown,
                    server_slots,
                )
                .await;
            });

            // Build membership identically to bootstrap_multi.
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

            // FT-1 / FT-2 restart-survivability: NotAllowed = "log already
            // contains init entry from a prior boot" → recover silently
            // and trust the persisted state. Other init errors are still
            // hard failures.
            match raft.initialize(members).await {
                Ok(()) => {}
                Err(openraft::error::RaftError::APIError(
                    openraft::error::InitializeError::NotAllowed(_),
                )) => {
                    tracing::info!(
                        node_id = config.node_id,
                        "Raft log recovered from persistent store — skipping initialize"
                    );
                }
                Err(e) => {
                    return Err(AeonError::Cluster {
                        message: format!("failed to initialize Raft cluster: {e}"),
                        source: None,
                    });
                }
            }

            let node = Self {
                raft,
                config,
                endpoint: Some(endpoint),
                shutdown,
                shared_state,
                applier_slot,
                transfer_driver: tokio::sync::RwLock::new(None),
                source_provider_slots,
                owned_partitions_rx,
            };

            tracing::info!(
                node_id = node.config.node_id,
                peers = node.config.peers.len(),
                partitions = node.config.num_partitions,
                "Multi-node cluster bootstrapped with persistent Raft (FT-1 + FT-2)"
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
            let owned_watch_slot = state_machine.owned_partitions_watch_slot();
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

            let owned_partitions_rx =
                install_owned_partitions_watch(&owned_watch_slot, config.node_id).await;

            // Start the QUIC RPC server so the bootstrap leader can reach us
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let source_provider_slots =
                crate::transport::server::SourceProviderSlots::new_empty();
            let server_ep = Arc::clone(&endpoint);
            let server_raft = raft.clone();
            let server_node_id = config.node_id;
            let server_shutdown = Arc::clone(&shutdown);
            let server_slots = source_provider_slots.clone();
            tokio::spawn(async move {
                server::serve_with_slots(
                    server_ep,
                    server_raft,
                    server_node_id,
                    server_shutdown,
                    server_slots,
                )
                .await;
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
                transfer_driver: tokio::sync::RwLock::new(None),
                source_provider_slots,
                owned_partitions_rx,
            };

            Ok(node)
        }

        /// FT-1 + FT-2: persistent variant of [`Self::join`].
        ///
        /// Same join handshake (`AddNode` RPC to a seed) but the local
        /// Raft log + state machine live on a persistent
        /// `Arc<dyn L3Store>` instead of in-memory. On pod restart the
        /// persisted log is replayed to recover the node's view of
        /// cluster membership without re-issuing the join.
        ///
        /// `raft.initialize()` is intentionally NOT called here — the
        /// joining node is a learner first, the seed leader replicates
        /// the cluster's initial config + ongoing entries via
        /// AppendEntries. On restart, the recovered log already
        /// contains those entries, so AppendEntries from the current
        /// leader picks up cleanly from the last committed index.
        pub async fn join_persistent(
            config: ClusterConfig,
            endpoint: Arc<QuicEndpoint>,
            log_backend: Arc<dyn L3Store>,
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

            let log_store = L3RaftLogStore::open(Arc::clone(&log_backend))
                .await
                .map_err(|e| AeonError::Cluster {
                    message: format!("failed to open persistent Raft log: {e}"),
                    source: None,
                })?;
            let state_machine = StateMachineStore::new_persistent(log_backend)?;
            let shared_state = state_machine.shared_state();
            let applier_slot = state_machine.applier_slot();
            let owned_watch_slot = state_machine.owned_partitions_watch_slot();
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

            let owned_partitions_rx =
                install_owned_partitions_watch(&owned_watch_slot, config.node_id).await;

            // Start the QUIC RPC server before joining (peers must be
            // able to reach us when the seed elevates us to voter).
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let source_provider_slots =
                crate::transport::server::SourceProviderSlots::new_empty();
            let server_ep = Arc::clone(&endpoint);
            let server_raft = raft.clone();
            let server_node_id = config.node_id;
            let server_shutdown = Arc::clone(&shutdown);
            let server_slots = source_provider_slots.clone();
            tokio::spawn(async move {
                server::serve_with_slots(
                    server_ep,
                    server_raft,
                    server_node_id,
                    server_shutdown,
                    server_slots,
                )
                .await;
            });

            // Send the join RPC. Note: a restarted node already has the
            // membership in its persisted log, so this is a no-op on the
            // leader's side (member already present); openraft handles
            // the duplicate-add gracefully.
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

            // Mirror the existing `join()` retry loop: try each seed
            // until one accepts. Restart-path is fine here — openraft
            // tolerates duplicate AddNode RPCs (member already present).
            let mut joined = false;
            for seed in &config.seed_nodes {
                tracing::info!(
                    seed = %seed,
                    "attempting to join (persistent Raft) via seed"
                );
                match crate::transport::network::send_join_request(
                    &endpoint,
                    0, // placeholder — uncached path does not read this
                    seed,
                    &join_req,
                )
                .await
                {
                    Ok(resp) if resp.success => {
                        tracing::info!(
                            node_id = config.node_id,
                            leader = ?resp.leader_id,
                            "join_persistent: succeeded via seed {seed}"
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
                    }
                    Err(e) => {
                        tracing::warn!(
                            seed = %seed,
                            error = %e,
                            "failed to contact seed"
                        );
                    }
                }
            }
            if !joined {
                tracing::warn!(
                    "join_persistent: no seed accepted the AddNode RPC; \
                     proceeding — restart path may resync via persisted log"
                );
            }

            let node = Self {
                raft,
                config,
                endpoint: Some(endpoint),
                shutdown,
                shared_state,
                applier_slot,
                transfer_driver: tokio::sync::RwLock::new(None),
                source_provider_slots,
                owned_partitions_rx,
            };

            tracing::info!(
                node_id = node.config.node_id,
                seeds = node.config.seed_nodes.len(),
                "Joined cluster with persistent Raft (FT-1 + FT-2)"
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
                heartbeat_interval: config.raft_timing.heartbeat_ms,
                election_timeout_min: config.raft_timing.election_min_ms,
                election_timeout_max: config.raft_timing.election_max_ms,
                ..Config::default()
            });

            let log_store = MemLogStore::new();
            let state_machine = StateMachineStore::new();
            let applier_slot = state_machine.applier_slot();
            let owned_watch_slot = state_machine.owned_partitions_watch_slot();
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

            let owned_partitions_rx =
                install_owned_partitions_watch(&owned_watch_slot, config.node_id).await;

            // Start the QUIC RPC server so peers can reach us
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let source_provider_slots =
                crate::transport::server::SourceProviderSlots::new_empty();
            let server_ep = Arc::clone(&endpoint);
            let server_raft = raft.clone();
            let server_node_id = config.node_id;
            let server_shutdown = Arc::clone(&shutdown);
            let server_slots = source_provider_slots.clone();
            tokio::spawn(async move {
                server::serve_with_slots(
                    server_ep,
                    server_raft,
                    server_node_id,
                    server_shutdown,
                    server_slots,
                )
                .await;
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
                // Seed ids are unknown — send_join_request uses the uncached
                // QUIC path (G16) so each seed gets its own real handshake.
                tracing::info!(seed = %seed, "attempting to join cluster via seed node");

                match crate::transport::network::send_join_request(
                    &endpoint,
                    0, // placeholder — uncached path does not read this
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
                transfer_driver: tokio::sync::RwLock::new(None),
                source_provider_slots,
                owned_partitions_rx,
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
        ///
        /// G8 — if the node has just bootstrapped and self-election is still
        /// in-flight, `client_write` would return `ForwardToLeader { None, None }`.
        /// Block up to `leader_wait_budget()` for a leader to be known before
        /// submitting, so callers (including REST `create_pipeline` issued
        /// seconds after `aeon server` start) don't see a transient
        /// "has to forward request to: None, None" 500.
        ///
        /// 2026-04-25 — when this node is a *follower*, `client_write` returns
        /// `ForwardToLeader { Some(leader_id), Some(leader_node) }` rather
        /// than auto-forwarding the proposal. Callers in driver loops
        /// (partition-driver `CompleteTransfer` / `AbortTransfer`) hit this
        /// every time the source pod isn't also the leader. We catch that
        /// shape here and re-issue the proposal as a
        /// `MessageType::ProposeForwardRequest` over the existing cluster
        /// QUIC transport so the leader runs `client_write` locally and
        /// returns the same `ClusterResponse` shape. One hop only — if the
        /// leader's own propose returns ForwardToLeader (leadership
        /// changed mid-flight) the caller sees an error and retries.
        pub async fn propose(&self, request: ClusterRequest) -> Result<ClusterResponse, AeonError> {
            if self.raft.metrics().borrow().current_leader.is_none() {
                let _ = self.wait_for_leader(self.leader_wait_budget()).await;
            }

            match self.raft.client_write(request.clone()).await {
                Ok(response) => Ok(response.data),
                Err(e) => {
                    // openraft's client_write returns
                    // `RaftError<NID, ClientWriteError<NID, N>>`. Reach
                    // for the ForwardToLeader inside.
                    if let Some(ftl) = e.forward_to_leader() {
                        if let (Some(leader_id), Some(leader_node)) =
                            (ftl.leader_id, ftl.leader_node.as_ref())
                        {
                            return self
                                .forward_propose_to_leader(
                                    leader_id,
                                    leader_node,
                                    request,
                                )
                                .await;
                        }
                    }
                    Err(AeonError::Cluster {
                        message: format!("Raft proposal failed: {e}"),
                        source: None,
                    })
                }
            }
        }

        /// Re-issue a `ClusterRequest` against the named leader via the
        /// `MessageType::ProposeForwardRequest` RPC. Helper for
        /// [`propose`] — split out so the call-site stays readable and
        /// the test surface is narrow.
        async fn forward_propose_to_leader(
            &self,
            leader_id: NodeId,
            leader_node: &NodeAddress,
            request: ClusterRequest,
        ) -> Result<ClusterResponse, AeonError> {
            let request_bytes = bincode::serialize(&request).map_err(|e| {
                AeonError::Serialization {
                    message: format!(
                        "forward_propose: serialize ClusterRequest: {e}"
                    ),
                    source: None,
                }
            })?;

            let endpoint = self.endpoint.as_ref().ok_or_else(|| AeonError::Cluster {
                message: "forward_propose: no QUIC endpoint installed (single-node mode?)"
                    .to_string(),
                source: None,
            })?;

            let req = crate::types::ProposeForwardRequest { request_bytes };
            let resp = crate::transport::network::send_propose_forward(
                endpoint,
                leader_id,
                leader_node,
                &req,
            )
            .await
            .map_err(|e| AeonError::Cluster {
                message: format!("forward_propose RPC to leader {leader_id} failed: {e}"),
                source: None,
            })?;

            if !resp.success {
                return Err(AeonError::Cluster {
                    message: format!(
                        "forward_propose: leader {leader_id} rejected proposal: {}",
                        resp.message
                    ),
                    source: None,
                });
            }

            let cluster_resp: ClusterResponse = bincode::deserialize(&resp.response_bytes)
                .map_err(|e| AeonError::Serialization {
                    message: format!(
                        "forward_propose: deserialize ClusterResponse from leader: {e}"
                    ),
                    source: None,
                })?;

            Ok(cluster_resp)
        }

        /// Budget for waiting on self-election / leader-discovery before a
        /// proposal. Scaled off the configured election timeout (3x the max,
        /// floored at 2s and capped at 10s) so a tightened `raft_timing`
        /// preset shortens the wait and a loose one doesn't hang forever.
        fn leader_wait_budget(&self) -> std::time::Duration {
            let scaled = self
                .config
                .raft_timing
                .election_max_ms
                .saturating_mul(3);
            let ms = scaled.clamp(2_000, 10_000);
            std::time::Duration::from_millis(ms)
        }

        /// Wait until `raft.metrics().current_leader` is `Some(_)` or the
        /// timeout expires. Returns the observed leader id on success.
        ///
        /// Used by [`propose`] to smooth the single-node self-election race
        /// (G8) and available to callers that want to block before issuing a
        /// first write after startup.
        pub async fn wait_for_leader(
            &self,
            timeout: std::time::Duration,
        ) -> Result<NodeId, AeonError> {
            if let Some(id) = self.raft.metrics().borrow().current_leader {
                return Ok(id);
            }

            let metrics = self
                .raft
                .wait(Some(timeout))
                .metrics(
                    |m| m.current_leader.is_some(),
                    "wait_for_leader: current_leader set",
                )
                .await
                .map_err(|e| AeonError::Cluster {
                    message: format!(
                        "no Raft leader elected within {}ms: {e}",
                        timeout.as_millis()
                    ),
                    source: None,
                })?;

            metrics.current_leader.ok_or_else(|| AeonError::Cluster {
                message: "wait_for_leader observed metrics with no current_leader".to_string(),
                source: None,
            })
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

        /// G11.b — install the leader-side partition-transfer driver.
        /// Called by `aeon-cli` once the engine-side installers are built.
        /// Idempotent: re-installing replaces the existing driver.
        pub async fn install_transfer_driver(
            &self,
            driver: Arc<crate::partition_driver::PartitionTransferDriver>,
        ) {
            *self.transfer_driver.write().await = Some(driver);
        }

        /// CL-6a — install the source-side L2 segment transfer provider.
        /// Serves inbound `PartitionTransferRequest` streams. Called by
        /// `aeon-cli` once the engine-side `L2SegmentTransferProvider` is
        /// built over the live `L2BodyStore`. Idempotent.
        ///
        /// Has no effect on single-node bootstrap paths because they don't
        /// run a QUIC server — the slot is still populated for API
        /// uniformity.
        pub async fn install_segment_provider(
            &self,
            provider: Arc<dyn crate::transport::partition_transfer::PartitionTransferProvider>,
        ) {
            self.source_provider_slots
                .install_segment_provider(provider)
                .await;
        }

        /// CL-6b — install the source-side PoH chain export provider.
        /// Serves inbound `PohChainTransferRequest` streams. Called by
        /// `aeon-cli` once the engine-side `PohChainExportProvider` is
        /// built over the live per-partition `PohChain`. Idempotent.
        pub async fn install_poh_provider(
            &self,
            provider: Arc<dyn crate::transport::poh_transfer::PohChainProvider>,
        ) {
            self.source_provider_slots
                .install_poh_provider(provider)
                .await;
        }

        /// CL-6c.4 — install the source-side cutover coordinator.
        /// Serves inbound `PartitionCutoverRequest` streams: drains
        /// in-flight writes, flips the `WriteGate` to `Frozen`, and
        /// reports the final source offset + PoH sequence so the target
        /// can resume without gap.
        ///
        /// Called by `aeon-cli` with an `EngineCutoverCoordinator`
        /// wrapping the supervisor's `WriteGateRegistry`. Idempotent.
        /// Without this hook, CL-6 handovers complete the bulk-sync
        /// phase but fail at cutover (source writes never freeze).
        pub async fn install_cutover_coordinator(
            &self,
            coordinator: Arc<dyn crate::transport::cutover::CutoverCoordinator>,
        ) {
            self.source_provider_slots
                .install_cutover_coordinator(coordinator)
                .await;
        }

        /// Shutdown flag shared with `ClusterNode`'s QUIC server. Exposed so
        /// callers can run the driver's `watch_loop` tied to the same lifecycle
        /// (`ClusterNode::shutdown()` will flip this bit).
        pub fn shutdown_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
            Arc::clone(&self.shutdown)
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

        /// G9 — build the REST URL of the current leader for auto-forwarding
        /// cluster-write requests from a follower.
        ///
        /// Returns `Some(url)` only when all of these hold:
        ///   - `ClusterConfig::rest_api_port` is set (operator opted in);
        ///   - there is a known current leader that is NOT this node;
        ///   - the leader's `NodeAddress` is recorded in Raft membership so
        ///     we can use its host string.
        ///
        /// Returns `None` on a leaderless quorum, when self is leader, or
        /// when auto-forward has not been configured — the caller should
        /// fall back to the pre-G9 behaviour in those cases.
        pub fn leader_rest_url(&self, path_and_query: &str) -> Option<String> {
            let port = self.config.rest_api_port?;
            let metrics = self.raft.metrics().borrow().clone();
            let leader_id = metrics.current_leader?;
            if leader_id == self.config.node_id {
                return None;
            }
            let leader_addr = metrics
                .membership_config
                .membership()
                .get_node(&leader_id)
                .cloned()?;
            let scheme = self
                .config
                .rest_scheme
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("http");
            let path = if path_and_query.starts_with('/') {
                path_and_query.to_string()
            } else {
                format!("/{path_and_query}")
            };
            Some(format!("{scheme}://{host}:{port}{path}", host = leader_addr.host))
        }

        /// Initiate a partition handover from the current owner to `target`.
        ///
        /// Validates on the caller side (must be leader, partition exists, not
        /// already transferring, target differs from source) and then submits
        /// `ClusterRequest::BeginTransfer` through Raft. The state-machine
        /// re-validates on apply so racing callers can't corrupt the table.
        ///
        /// Intended for operator-driven handover (REST/CLI) — the Row 5
        /// Gate 2 cutover test in `docs/GATE2-ACCEPTANCE-PLAN.md §11.5` needs
        /// a way to measure transfer latency without waiting for rebalance.
        ///
        /// Returns [`TransferStatus`] indicating whether the begin-transfer was
        /// accepted, rejected, or would be a no-op. Only raises `AeonError`
        /// for plumbing failures (Raft proposal errored, etc).
        pub async fn propose_partition_transfer(
            &self,
            partition: PartitionId,
            target: NodeId,
        ) -> Result<TransferStatus, AeonError> {
            if !self.is_leader().await {
                return Ok(TransferStatus::NotLeader {
                    current_leader: self.current_leader().await,
                });
            }

            let source = {
                let state = self.shared_state.read().await;
                match state.partition_table.get(partition) {
                    None => return Ok(TransferStatus::UnknownPartition),
                    Some(crate::types::PartitionOwnership::Transferring { source, target: t }) => {
                        return Ok(TransferStatus::AlreadyTransferring {
                            source: *source,
                            target: *t,
                        });
                    }
                    Some(crate::types::PartitionOwnership::Owned(owner)) => *owner,
                }
            };

            if source == target {
                return Ok(TransferStatus::NoChange { owner: source });
            }

            match self
                .propose(ClusterRequest::BeginTransfer {
                    partition,
                    source,
                    target,
                })
                .await?
            {
                ClusterResponse::Ok => {
                    // G11.b — wake the watch loop so it picks up the fresh
                    // Transferring entry without waiting for the poll tick.
                    // `notify` is best-effort; a missing driver (e.g. on a
                    // single-node test node) just falls back to the poll
                    // interval the driver configures for itself.
                    if let Some(driver) = self.transfer_driver.read().await.clone() {
                        driver.notify();
                    }
                    Ok(TransferStatus::Accepted { source, target })
                }
                ClusterResponse::Error(msg) => Ok(TransferStatus::Rejected(msg)),
                ClusterResponse::Registry(_) => Ok(TransferStatus::Rejected(
                    "unexpected Registry response to BeginTransfer".to_string(),
                )),
            }
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

        /// P5: subscribe to this node's owned-partitions changes.
        ///
        /// Returns a `watch::Receiver` that is notified every time a Raft
        /// commit (or snapshot install) changes the slice of partitions
        /// assigned to this node id. The engine's pipeline source loop
        /// clones this handle and selects on `changed()` so a CL-6 transfer
        /// can re-assign a live source (e.g. KafkaSource's `consumer.assign`)
        /// without tearing down the pipeline task.
        pub fn watch_owned_partitions(
            &self,
        ) -> tokio::sync::watch::Receiver<Vec<PartitionId>> {
            self.owned_partitions_rx.clone()
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

        /// G14 — relinquish leadership ahead of graceful shutdown.
        ///
        /// If this node is the current Raft leader, disables heartbeats so
        /// followers election-timeout and elect a new leader while our
        /// process is still alive and able to respond to `AppendEntries` /
        /// `RequestVote`. Waits up to `timeout` for `metrics.current_leader`
        /// to flip away from this node and returns.
        ///
        /// No-op if we are not leader, or if the cluster is a single-node
        /// bootstrap (no quorum to hand off to).
        ///
        /// The caller is expected to follow up with `shutdown()` shortly
        /// after this returns — re-enabling heartbeats is NOT attempted on
        /// the success path because we're about to tear the node down. On
        /// timeout, heartbeats are re-enabled so we don't leave the cluster
        /// silently without a leader if shutdown is aborted.
        pub async fn relinquish_leadership(
            &self,
            timeout: std::time::Duration,
        ) -> Result<(), AeonError> {
            let self_id = self.config.node_id;

            let metrics = self.raft.metrics().borrow().clone();
            if metrics.current_leader != Some(self_id) {
                return Ok(());
            }

            let voters = metrics
                .membership_config
                .membership()
                .voter_ids()
                .filter(|id| *id != self_id)
                .count();
            if voters == 0 {
                // Single-node cluster — no one to hand off to.
                return Ok(());
            }

            tracing::info!(
                node_id = self_id,
                timeout_ms = timeout.as_millis() as u64,
                "relinquishing Raft leadership before shutdown"
            );

            self.raft.runtime_config().heartbeat(false);

            let res = self
                .raft
                .wait(Some(timeout))
                .metrics(
                    |m| m.current_leader != Some(self_id),
                    "relinquish_leadership: leader moved",
                )
                .await;

            match res {
                Ok(m) => {
                    tracing::info!(
                        node_id = self_id,
                        new_leader = ?m.current_leader,
                        "leadership relinquished"
                    );
                    Ok(())
                }
                Err(e) => {
                    // Re-enable heartbeats: if shutdown is aborted we don't
                    // want the cluster stuck without a leader.
                    self.raft.runtime_config().heartbeat(true);
                    Err(AeonError::Cluster {
                        message: format!(
                            "leadership did not move within {}ms: {e}",
                            timeout.as_millis()
                        ),
                        source: None,
                    })
                }
            }
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
        async fn relinquish_leadership_single_node_is_noop() {
            // G14: single-node cluster has no one to hand off to, so
            // relinquish must short-circuit as Ok without disabling
            // heartbeats. Leadership stays on node 1.
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            let before = node.current_leader().await;
            assert_eq!(before, Some(1));

            node.relinquish_leadership(std::time::Duration::from_millis(100))
                .await
                .unwrap();

            let after = node.current_leader().await;
            assert_eq!(after, Some(1), "single-node must remain leader");

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn wait_for_leader_returns_self_on_single_node_g8() {
            // G8: on a freshly-bootstrapped single-node cluster, wait_for_leader
            // must return Self as the elected leader within the bounded budget,
            // and a subsequent propose must not hit "forward request to: None, None".
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            let leader = node
                .wait_for_leader(std::time::Duration::from_secs(2))
                .await
                .expect("single-node must self-elect within 2s");
            assert_eq!(leader, 1);

            // A write issued immediately after wait_for_leader must succeed.
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

        #[tokio::test]
        async fn propose_partition_transfer_accepts_on_leader() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // P0 is owned by node 1 after bootstrap — transfer to node 2.
            let status = node
                .propose_partition_transfer(PartitionId::new(0), 2)
                .await
                .unwrap();
            assert_eq!(
                status,
                TransferStatus::Accepted {
                    source: 1,
                    target: 2,
                }
            );

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn propose_partition_transfer_noop_when_target_is_owner() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            let status = node
                .propose_partition_transfer(PartitionId::new(0), 1)
                .await
                .unwrap();
            assert_eq!(status, TransferStatus::NoChange { owner: 1 });

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn propose_partition_transfer_unknown_partition() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // Partition 99 is out of range — not in the table.
            let status = node
                .propose_partition_transfer(PartitionId::new(99), 2)
                .await
                .unwrap();
            assert_eq!(status, TransferStatus::UnknownPartition);

            node.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn propose_partition_transfer_already_transferring() {
            let config = ClusterConfig::single_node(1, 4);
            let node = ClusterNode::bootstrap_single(config).await.unwrap();

            // First transfer goes through.
            node.propose_partition_transfer(PartitionId::new(0), 2)
                .await
                .unwrap();

            // Second attempt observes the in-flight transfer.
            let status = node
                .propose_partition_transfer(PartitionId::new(0), 3)
                .await
                .unwrap();
            assert_eq!(
                status,
                TransferStatus::AlreadyTransferring {
                    source: 1,
                    target: 2,
                }
            );

            node.shutdown().await.unwrap();
        }
    }
}
