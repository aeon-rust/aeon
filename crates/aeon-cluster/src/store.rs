//! In-memory Raft log storage and state machine for Aeon cluster.
//!
//! The Raft log is intentionally small (no event data), so in-memory storage is sufficient.

#[cfg(feature = "cluster")]
pub use inner::*;

#[cfg(feature = "cluster")]
mod inner {
    use std::collections::BTreeMap;
    use std::fmt::Debug;
    use std::io::Cursor;
    use std::ops::RangeBounds;
    use std::sync::Arc;

    use aeon_types::{BatchOp, L3Store, RegistryApplier, RegistryCommand};
    use openraft::storage::{
        LogFlushed, LogState, RaftLogReader, RaftLogStorage, RaftStateMachine,
    };
    use openraft::storage::{RaftSnapshotBuilder, Snapshot, SnapshotMeta};
    use openraft::{Entry, LogId, StorageError, StoredMembership, Vote};
    use tokio::sync::RwLock;

    // ── FT-2: Raft snapshot persistence keys ──────────────────────────
    /// Serialized `SnapshotMeta<u64, NodeAddress>` for the most recent snapshot.
    const SNAPSHOT_META_KEY: &[u8] = b"raft/snapshot/meta";
    /// Raw snapshot payload (encrypted if encryption-at-rest is configured).
    const SNAPSHOT_DATA_KEY: &[u8] = b"raft/snapshot/data";

    use crate::partition_manager;
    use crate::raft_config::AeonRaftConfig;
    use crate::snapshot::ClusterSnapshot;
    use crate::types::{
        ClusterRequest, ClusterResponse, NodeAddress, NodeId, PartitionOwnership,
    };
    use aeon_types::PartitionId;

    // ─── Shared Log Data ──────────────────────────────────────────────

    /// Shared state between MemLogStore and its readers.
    #[derive(Debug, Default)]
    struct SharedLogData {
        vote: RwLock<Option<Vote<u64>>>,
        log: RwLock<BTreeMap<u64, Entry<AeonRaftConfig>>>,
        committed: RwLock<Option<LogId<u64>>>,
        last_purged: RwLock<Option<LogId<u64>>>,
    }

    // ─── In-Memory Log Store ───────────────────────────────────────────

    /// In-memory Raft log storage.
    ///
    /// Suitable for Aeon because the Raft log only contains partition table changes
    /// and membership — never event data.
    #[derive(Debug, Clone)]
    pub struct MemLogStore {
        data: Arc<SharedLogData>,
    }

    impl Default for MemLogStore {
        fn default() -> Self {
            Self {
                data: Arc::new(SharedLogData::default()),
            }
        }
    }

    impl MemLogStore {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl RaftLogReader<AeonRaftConfig> for MemLogStore {
        async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
            &mut self,
            range: RB,
        ) -> Result<Vec<Entry<AeonRaftConfig>>, StorageError<u64>> {
            let log = self.data.log.read().await;
            let entries: Vec<_> = log.range(range).map(|(_, e)| e.clone()).collect();
            Ok(entries)
        }
    }

    impl RaftLogStorage<AeonRaftConfig> for MemLogStore {
        type LogReader = Self;

        async fn get_log_state(&mut self) -> Result<LogState<AeonRaftConfig>, StorageError<u64>> {
            let log = self.data.log.read().await;
            let last_purged = self.data.last_purged.read().await;
            let last_log_id = log.values().last().map(|e| e.log_id);
            Ok(LogState {
                last_purged_log_id: *last_purged,
                last_log_id: last_log_id.or(*last_purged),
            })
        }

        async fn get_log_reader(&mut self) -> Self::LogReader {
            self.clone()
        }

        async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
            let mut v = self.data.vote.write().await;
            *v = Some(*vote);
            Ok(())
        }

        async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
            let v = self.data.vote.read().await;
            Ok(*v)
        }

        async fn save_committed(
            &mut self,
            committed: Option<LogId<u64>>,
        ) -> Result<(), StorageError<u64>> {
            let mut c = self.data.committed.write().await;
            *c = committed;
            Ok(())
        }

        async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
            let c = self.data.committed.read().await;
            Ok(*c)
        }

        async fn append<I>(
            &mut self,
            entries: I,
            callback: LogFlushed<AeonRaftConfig>,
        ) -> Result<(), StorageError<u64>>
        where
            I: IntoIterator<Item = Entry<AeonRaftConfig>> + Send,
            I::IntoIter: Send,
        {
            let mut log = self.data.log.write().await;
            for entry in entries {
                log.insert(entry.log_id.index, entry);
            }
            // In-memory store: data is immediately "flushed"
            callback.log_io_completed(Ok(()));
            Ok(())
        }

        async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
            let mut log = self.data.log.write().await;
            let keys_to_remove: Vec<u64> = log.range(log_id.index..).map(|(k, _)| *k).collect();
            for k in keys_to_remove {
                log.remove(&k);
            }
            Ok(())
        }

        async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
            let mut log = self.data.log.write().await;
            let mut last_purged = self.data.last_purged.write().await;
            let keys_to_remove: Vec<u64> = log.range(..=log_id.index).map(|(k, _)| *k).collect();
            for k in keys_to_remove {
                log.remove(&k);
            }
            *last_purged = Some(log_id);
            Ok(())
        }
    }

    // ─── State Machine ────────────────────────────────────────────────

    /// In-memory Raft state machine that applies ClusterRequests to a ClusterSnapshot.
    ///
    /// When `encryption_key` is set, snapshots are encrypted at rest using
    /// AES-256-CTR + HMAC-SHA-512 (Encrypt-then-MAC) via `aeon_crypto::EtmKey`.
    pub struct StateMachineStore {
        state: ClusterSnapshot,
        last_applied_log: Option<LogId<u64>>,
        last_membership: StoredMembership<u64, NodeAddress>,
        snapshot_idx: u64,
        /// Optional encryption key for snapshot encryption at rest.
        #[cfg(feature = "encryption-at-rest")]
        encryption_key: Option<aeon_crypto::encryption::EtmKey>,
        /// Shared read handle — cloned snapshot updated after each apply.
        /// External consumers (REST API, ClusterNode) can read from this
        /// without accessing the Raft internals.
        shared_state: Arc<tokio::sync::RwLock<ClusterSnapshot>>,
        /// FT-2: optional L3 backend for persistent snapshots. When set,
        /// [`build_snapshot`] and [`install_snapshot`] write the snapshot
        /// to `raft/snapshot/{meta,data}` so it survives restart and lets
        /// a rebooted node skip full log replay.
        snapshot_store: Option<Arc<dyn L3Store>>,
        /// Shared slot holding the node-local applier for
        /// `ClusterRequest::Registry` entries. Held behind an `Arc<RwLock>`
        /// so callers can install/replace the applier **after** the state
        /// machine has been handed to `Raft::new` (which takes ownership).
        ///
        /// When the slot is `None`, committed Registry entries are replicated
        /// silently — nodes that wire an applier later will converge via
        /// snapshot replay or via rebuilding state from stored definitions.
        registry_applier: RegistryApplierSlot,
        /// P5: optional per-node owned-partitions watch. When set, the apply
        /// loop pushes the latest owned-partitions slice to the watch after
        /// every committed entry so the engine can re-assign a live source
        /// without a pipeline restart. See [`OwnedPartitionsWatch`].
        owned_partitions_watch: OwnedPartitionsWatchSlot,
    }

    /// Cloneable handle to the state machine's registry applier slot.
    /// Install an applier via `*slot.write().await = Some(applier)`.
    pub type RegistryApplierSlot = Arc<tokio::sync::RwLock<Option<Arc<dyn RegistryApplier>>>>;

    /// Read-only handle to the cluster state machine snapshot.
    /// Can be cheaply cloned and shared across threads.
    pub type SharedClusterState = Arc<tokio::sync::RwLock<ClusterSnapshot>>;

    /// P5: per-node owned-partitions notifier.
    ///
    /// Installed into the state machine by `ClusterNode` on startup once the
    /// node knows its own `node_id`. After every committed entry (and after
    /// any `install_snapshot`), the state machine recomputes
    /// `partition_table.partitions_for_node(self.node_id)` and sends it on
    /// `sender` if the set changed. The pipeline source loop subscribes to
    /// `sender.subscribe()` so it can `reassign_partitions()` the live source
    /// without tearing down the task on a CL-6 transfer commit.
    ///
    /// `sender.send_if_modified` is the fast path — identical lists skip the
    /// channel push so receivers are only woken on real ownership churn.
    pub struct OwnedPartitionsWatch {
        pub node_id: NodeId,
        pub sender: tokio::sync::watch::Sender<Vec<PartitionId>>,
    }

    /// Cloneable slot for installing an `OwnedPartitionsWatch` after the
    /// state machine has been handed to `Raft::new`. Mirrors
    /// `RegistryApplierSlot`.
    pub type OwnedPartitionsWatchSlot =
        Arc<tokio::sync::RwLock<Option<OwnedPartitionsWatch>>>;

    // Manual Debug impl to avoid exposing encryption key
    impl Debug for StateMachineStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("StateMachineStore")
                .field("state", &self.state)
                .field("last_applied_log", &self.last_applied_log)
                .field("snapshot_idx", &self.snapshot_idx)
                .finish()
        }
    }

    impl StateMachineStore {
        pub fn new() -> Self {
            Self {
                state: ClusterSnapshot::default(),
                last_applied_log: None,
                last_membership: StoredMembership::new(None, openraft::Membership::new(vec![], ())),
                snapshot_idx: 0,
                #[cfg(feature = "encryption-at-rest")]
                encryption_key: None,
                shared_state: Arc::new(tokio::sync::RwLock::new(ClusterSnapshot::default())),
                snapshot_store: None,
                registry_applier: Arc::new(tokio::sync::RwLock::new(None)),
                owned_partitions_watch: Arc::new(tokio::sync::RwLock::new(None)),
            }
        }

        /// Returns a cloneable handle to the applier slot. External callers
        /// (typically `ClusterNode`) hold this handle and write an applier
        /// into it post-construction, since `Raft::new` takes ownership of
        /// the state machine itself.
        pub fn applier_slot(&self) -> RegistryApplierSlot {
            Arc::clone(&self.registry_applier)
        }

        /// P5: cloneable handle to the owned-partitions watch slot. The
        /// `ClusterNode` fills this in after bootstrap, once the node's
        /// own `node_id` is known and a `watch::Sender` has been paired
        /// with a receiver to hand out to subscribers.
        pub fn owned_partitions_watch_slot(&self) -> OwnedPartitionsWatchSlot {
            Arc::clone(&self.owned_partitions_watch)
        }

        /// P5: broadcast the current owned-partitions slice for the
        /// locally-installed node id, if any, and only when the set
        /// differs from the last value observed on the watch. Callers
        /// invoke this after any in-place mutation of `self.state`.
        async fn broadcast_owned_partitions(&self) {
            let guard = self.owned_partitions_watch.read().await;
            let Some(watch) = guard.as_ref() else { return };
            let mut owned: Vec<PartitionId> = self
                .state
                .partition_table
                .partitions_for_node(watch.node_id);
            owned.sort_unstable();
            watch.sender.send_if_modified(|current| {
                if *current == owned {
                    false
                } else {
                    *current = owned.clone();
                    true
                }
            });
        }

        /// FT-2: create a state machine backed by a persistent L3 snapshot store.
        ///
        /// On construction, reads `raft/snapshot/{meta,data}` from `l3`. If both
        /// keys are present, deserializes the snapshot and hydrates `state`,
        /// `last_applied_log`, and `last_membership` so the rebooted node can
        /// skip the bulk of log replay.
        ///
        /// Returns an `AeonError::Cluster` if decoding fails — a corrupt snapshot
        /// is a fatal startup error rather than a silent fallback.
        pub fn new_persistent(l3: Arc<dyn L3Store>) -> Result<Self, aeon_types::AeonError> {
            let mut sm = Self::new();
            sm.hydrate_from_l3(&l3)?;
            sm.snapshot_store = Some(l3);
            Ok(sm)
        }

        /// FT-2: as [`new_persistent`], but additionally configures encryption
        /// at rest for the snapshot payload.
        #[cfg(feature = "encryption-at-rest")]
        pub fn new_persistent_encrypted(
            l3: Arc<dyn L3Store>,
            key: aeon_crypto::encryption::EtmKey,
        ) -> Result<Self, aeon_types::AeonError> {
            let mut sm = Self::with_encryption_key(key);
            sm.hydrate_from_l3(&l3)?;
            sm.snapshot_store = Some(l3);
            Ok(sm)
        }

        /// FT-2: internal hydration helper — reads and decodes a snapshot
        /// previously written via [`persist_snapshot_bytes`].
        fn hydrate_from_l3(&mut self, l3: &Arc<dyn L3Store>) -> Result<(), aeon_types::AeonError> {
            let meta_bytes = l3.get(SNAPSHOT_META_KEY)?;
            let data_bytes = l3.get(SNAPSHOT_DATA_KEY)?;
            #[allow(unused_mut)]
            let (Some(meta_bytes), Some(mut data_bytes)) = (meta_bytes, data_bytes) else {
                return Ok(()); // no snapshot persisted yet — fresh start
            };

            let meta: SnapshotMeta<u64, NodeAddress> = bincode::deserialize(&meta_bytes)
                .map_err(|e| aeon_types::AeonError::Cluster {
                    message: format!("corrupt raft snapshot meta: {e}"),
                    source: None,
                })?;

            // Decrypt payload if encryption-at-rest is configured.
            #[cfg(feature = "encryption-at-rest")]
            if let Some(ref k) = self.encryption_key {
                data_bytes = k.decrypt(&data_bytes).map_err(|e| {
                    aeon_types::AeonError::Cluster {
                        message: format!("raft snapshot decryption failed: {e}"),
                        source: None,
                    }
                })?;
            }

            let state = ClusterSnapshot::from_bytes(&data_bytes).map_err(|e| {
                aeon_types::AeonError::Cluster {
                    message: format!("corrupt raft snapshot data: {e}"),
                    source: None,
                }
            })?;

            // `shared_state` is a brand-new `Arc<RwLock<_>>` created in
            // `Self::new()` above with no other clones yet, so `try_write`
            // always succeeds here; fall back to blocking_write defensively.
            if let Ok(mut guard) = self.shared_state.try_write() {
                *guard = state.clone();
            } else {
                *self.shared_state.blocking_write() = state.clone();
            }
            self.state = state;
            self.last_applied_log = meta.last_log_id;
            self.last_membership = meta.last_membership;
            Ok(())
        }

        /// Get a shared read handle to the cluster state.
        ///
        /// The returned Arc can be cloned and held by external consumers
        /// (ClusterNode, REST API) for read-only access to the latest
        /// committed state.
        pub fn shared_state(&self) -> SharedClusterState {
            Arc::clone(&self.shared_state)
        }

        /// Create a state machine with snapshot encryption at rest.
        #[cfg(feature = "encryption-at-rest")]
        pub fn with_encryption_key(key: aeon_crypto::encryption::EtmKey) -> Self {
            Self {
                state: ClusterSnapshot::default(),
                last_applied_log: None,
                last_membership: StoredMembership::new(None, openraft::Membership::new(vec![], ())),
                snapshot_idx: 0,
                encryption_key: Some(key),
                shared_state: Arc::new(tokio::sync::RwLock::new(ClusterSnapshot::default())),
                snapshot_store: None,
                registry_applier: Arc::new(tokio::sync::RwLock::new(None)),
                owned_partitions_watch: Arc::new(tokio::sync::RwLock::new(None)),
            }
        }

        /// Get a reference to the current cluster state.
        pub fn state(&self) -> &ClusterSnapshot {
            &self.state
        }

        /// Apply a single ClusterRequest to the state machine.
        fn apply_request(&mut self, req: &ClusterRequest) -> ClusterResponse {
            match req {
                ClusterRequest::AssignPartition { partition, node } => {
                    self.state.partition_table.assign(*partition, *node);
                    ClusterResponse::Ok
                }
                ClusterRequest::BeginTransfer {
                    partition,
                    source,
                    target,
                } => {
                    let current = self.state.partition_table.get(*partition);
                    match current {
                        Some(PartitionOwnership::Owned(owner)) if *owner == *source => {
                            self.state
                                .partition_table
                                .begin_transfer(*partition, *source, *target);
                            ClusterResponse::Ok
                        }
                        Some(PartitionOwnership::Transferring { .. }) => {
                            ClusterResponse::Error("partition already in transfer".to_string())
                        }
                        _ => ClusterResponse::Error(format!(
                            "partition {:?} not owned by node {}",
                            partition, source
                        )),
                    }
                }
                ClusterRequest::CompleteTransfer {
                    partition,
                    new_owner,
                } => {
                    let current = self.state.partition_table.get(*partition);
                    match current {
                        Some(PartitionOwnership::Transferring { target, .. })
                            if *target == *new_owner =>
                        {
                            self.state
                                .partition_table
                                .complete_transfer(*partition, *new_owner);
                            ClusterResponse::Ok
                        }
                        Some(PartitionOwnership::Owned(owner)) if *owner == *new_owner => {
                            // Idempotent: already completed
                            ClusterResponse::Ok
                        }
                        _ => ClusterResponse::Error(format!(
                            "no active transfer for partition {:?} to node {}",
                            partition, new_owner
                        )),
                    }
                }
                ClusterRequest::AbortTransfer {
                    partition,
                    revert_to,
                } => {
                    self.state.partition_table.assign(*partition, *revert_to);
                    ClusterResponse::Ok
                }
                ClusterRequest::UpdateConfig { key, value } => {
                    self.state
                        .config_overrides
                        .insert(key.clone(), value.clone());
                    ClusterResponse::Ok
                }
                ClusterRequest::SubmitCheckpoint { source_offsets } => {
                    // Merge incoming offsets into the cluster-wide source_anchor_offsets.
                    // Only update if the new offset is ahead of the existing one.
                    for (&partition_u16, &offset) in source_offsets {
                        let pid = aeon_types::PartitionId::new(partition_u16);
                        let entry = self
                            .state
                            .source_anchor_offsets
                            .entry(pid)
                            .or_insert(0);
                        if offset > *entry as i64 {
                            *entry = offset as u64;
                        }
                    }
                    ClusterResponse::Ok
                }
                ClusterRequest::RebalancePartitions { nodes } => {
                    let moves =
                        partition_manager::compute_rebalance(&self.state.partition_table, nodes);
                    for (partition, _from, to) in &moves {
                        self.state.partition_table.assign(*partition, *to);
                    }
                    ClusterResponse::Ok
                }
                ClusterRequest::Registry { .. } => {
                    // Handled upstream in the async `apply()` dispatcher, not
                    // here. This sync helper is preserved for entries that
                    // only touch the in-cluster ClusterSnapshot.
                    ClusterResponse::Error(
                        "ClusterRequest::Registry must be dispatched via async apply".into(),
                    )
                }
                ClusterRequest::InitialAssignment {
                    num_partitions,
                    nodes,
                } => {
                    // Idempotent: skip if partitions are already assigned.
                    if self.state.partition_table.iter().next().is_some() {
                        ClusterResponse::Ok
                    } else {
                        let assignments =
                            partition_manager::initial_assignment(*num_partitions, nodes);
                        for (partition, node) in assignments {
                            self.state.partition_table.assign(partition, node);
                        }
                        ClusterResponse::Ok
                    }
                }
            }
        }
    }

    impl Default for StateMachineStore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl RaftSnapshotBuilder<AeonRaftConfig> for Arc<StateMachineStore> {
        async fn build_snapshot(&mut self) -> Result<Snapshot<AeonRaftConfig>, StorageError<u64>> {
            let snapshot_id = format!(
                "snap-{}-{}",
                self.last_applied_log.map(|l| l.index).unwrap_or(0),
                self.snapshot_idx,
            );

            #[allow(unused_mut)]
            let mut data = self.state.to_bytes().map_err(|e| {
                StorageError::from_io_error(
                    openraft::ErrorSubject::StateMachine,
                    openraft::ErrorVerb::Read,
                    std::io::Error::other(e.to_string()),
                )
            })?;

            // Encrypt snapshot if encryption-at-rest is configured
            #[cfg(feature = "encryption-at-rest")]
            if let Some(ref key) = self.encryption_key {
                data = key.encrypt(&data).map_err(|e| {
                    StorageError::from_io_error(
                        openraft::ErrorSubject::StateMachine,
                        openraft::ErrorVerb::Write,
                        std::io::Error::other(format!("snapshot encryption failed: {e}")),
                    )
                })?;
            }

            let meta = SnapshotMeta {
                last_log_id: self.last_applied_log,
                last_membership: self.last_membership.clone(),
                snapshot_id,
            };

            // FT-2: persist snapshot to L3 if a backend is configured.
            // `data` is already encrypted (if encryption-at-rest is on), so we
            // write it verbatim.
            if let Some(ref l3) = self.snapshot_store {
                persist_snapshot_bytes(l3, &meta, &data).map_err(|e| {
                    StorageError::from_io_error(
                        openraft::ErrorSubject::Snapshot(Some(meta.signature())),
                        openraft::ErrorVerb::Write,
                        std::io::Error::other(format!("snapshot persistence failed: {e}")),
                    )
                })?;
            }

            let snapshot = Snapshot {
                meta,
                snapshot: Box::new(Cursor::new(data)),
            };

            Ok(snapshot)
        }
    }

    /// FT-2: atomically write snapshot meta + data to L3.
    ///
    /// Both keys are written in a single `write_batch` so the meta and data
    /// never disagree on disk. `data_bytes` is stored as-is — the caller is
    /// responsible for any encryption-at-rest transformation before this call.
    fn persist_snapshot_bytes(
        l3: &Arc<dyn L3Store>,
        meta: &SnapshotMeta<u64, NodeAddress>,
        data_bytes: &[u8],
    ) -> Result<(), aeon_types::AeonError> {
        let meta_bytes = bincode::serialize(meta).map_err(|e| aeon_types::AeonError::Cluster {
            message: format!("snapshot meta serialization failed: {e}"),
            source: None,
        })?;
        let ops = vec![
            (BatchOp::Put, SNAPSHOT_META_KEY.to_vec(), Some(meta_bytes)),
            (BatchOp::Put, SNAPSHOT_DATA_KEY.to_vec(), Some(data_bytes.to_vec())),
        ];
        l3.write_batch(&ops)?;
        l3.flush()?;
        Ok(())
    }

    impl RaftStateMachine<AeonRaftConfig> for StateMachineStore {
        type SnapshotBuilder = Arc<StateMachineStore>;

        async fn applied_state(
            &mut self,
        ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, NodeAddress>), StorageError<u64>>
        {
            Ok((self.last_applied_log, self.last_membership.clone()))
        }

        async fn apply<I>(&mut self, entries: I) -> Result<Vec<ClusterResponse>, StorageError<u64>>
        where
            I: IntoIterator<Item = Entry<AeonRaftConfig>> + Send,
            I::IntoIter: Send,
        {
            let mut responses = Vec::new();

            for entry in entries {
                self.last_applied_log = Some(entry.log_id);
                self.state.last_applied_log_index = entry.log_id.index;
                self.state.last_applied_log_term = entry.log_id.leader_id.term;

                match entry.payload {
                    openraft::EntryPayload::Blank => {
                        responses.push(ClusterResponse::Ok);
                    }
                    openraft::EntryPayload::Normal(req) => {
                        let resp = match &req {
                            ClusterRequest::Registry { payload } => {
                                match serde_json::from_slice::<RegistryCommand>(payload) {
                                    Ok(cmd) => {
                                        let applier = self.registry_applier.read().await.clone();
                                        if let Some(applier) = applier {
                                            let rresp = applier.apply(cmd).await;
                                            ClusterResponse::registry(&rresp).unwrap_or_else(
                                                |e| ClusterResponse::Error(e.to_string()),
                                            )
                                        } else {
                                            // No local applier installed — replicate
                                            // silently. Nodes that wire an applier later
                                            // will converge via snapshot replay.
                                            ClusterResponse::Ok
                                        }
                                    }
                                    Err(e) => ClusterResponse::Error(format!(
                                        "malformed Registry payload: {e}"
                                    )),
                                }
                            }
                            _ => self.apply_request(&req),
                        };
                        responses.push(resp);
                    }
                    openraft::EntryPayload::Membership(membership) => {
                        self.last_membership =
                            StoredMembership::new(Some(entry.log_id), membership);
                        responses.push(ClusterResponse::Ok);
                    }
                }
            }

            // Push committed state to the shared read handle.
            *self.shared_state.write().await = self.state.clone();

            // P5: notify subscribers if this node's owned-partitions slice
            // shifted as a result of this apply batch. Cheap when unchanged
            // (watch::send_if_modified) and a no-op when no subscriber is
            // installed.
            self.broadcast_owned_partitions().await;

            Ok(responses)
        }

        async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
            self.snapshot_idx += 1;
            // Arc allows the snapshot builder to work concurrently
            // We clone the state for a point-in-time snapshot
            Arc::new(StateMachineStore {
                state: self.state.clone(),
                last_applied_log: self.last_applied_log,
                last_membership: self.last_membership.clone(),
                snapshot_idx: self.snapshot_idx,
                #[cfg(feature = "encryption-at-rest")]
                encryption_key: self.encryption_key.clone(),
                shared_state: Arc::clone(&self.shared_state),
                snapshot_store: self.snapshot_store.clone(),
                registry_applier: self.registry_applier.clone(),
                owned_partitions_watch: Arc::clone(&self.owned_partitions_watch),
            })
        }

        async fn begin_receiving_snapshot(
            &mut self,
        ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
            Ok(Box::new(Cursor::new(Vec::new())))
        }

        async fn install_snapshot(
            &mut self,
            meta: &SnapshotMeta<u64, NodeAddress>,
            snapshot: Box<Cursor<Vec<u8>>>,
        ) -> Result<(), StorageError<u64>> {
            let raw = snapshot.into_inner();
            #[allow(unused_mut)]
            let mut data = raw.clone();

            // Decrypt snapshot if encryption-at-rest is configured
            #[cfg(feature = "encryption-at-rest")]
            if let Some(ref key) = self.encryption_key {
                data = key.decrypt(&data).map_err(|e| {
                    StorageError::from_io_error(
                        openraft::ErrorSubject::StateMachine,
                        openraft::ErrorVerb::Read,
                        std::io::Error::other(format!("snapshot decryption failed: {e}")),
                    )
                })?;
            }

            // FT-2: persist the on-wire (encrypted-if-applicable) bytes to L3
            // so the restart path can hydrate without log replay.
            if let Some(ref l3) = self.snapshot_store {
                persist_snapshot_bytes(l3, meta, &raw).map_err(|e| {
                    StorageError::from_io_error(
                        openraft::ErrorSubject::Snapshot(Some(meta.signature())),
                        openraft::ErrorVerb::Write,
                        std::io::Error::other(format!("snapshot persistence failed: {e}")),
                    )
                })?;
            }

            let state = ClusterSnapshot::from_bytes(&data).map_err(|e| {
                StorageError::from_io_error(
                    openraft::ErrorSubject::StateMachine,
                    openraft::ErrorVerb::Read,
                    std::io::Error::other(e.to_string()),
                )
            })?;

            self.state = state;
            self.last_applied_log = meta.last_log_id;
            self.last_membership = meta.last_membership.clone();

            // Push restored state to the shared read handle.
            *self.shared_state.write().await = self.state.clone();

            // P5: a snapshot install can jump ownership without going through
            // individual CompleteTransfer entries, so recompute + broadcast
            // the owned-partitions set for the local node here too.
            self.broadcast_owned_partitions().await;

            Ok(())
        }

        async fn get_current_snapshot(
            &mut self,
        ) -> Result<Option<Snapshot<AeonRaftConfig>>, StorageError<u64>> {
            if self.last_applied_log.is_none() {
                return Ok(None);
            }

            #[allow(unused_mut)]
            let mut data = self.state.to_bytes().map_err(|e| {
                StorageError::from_io_error(
                    openraft::ErrorSubject::StateMachine,
                    openraft::ErrorVerb::Read,
                    std::io::Error::other(e.to_string()),
                )
            })?;

            // Encrypt snapshot if encryption-at-rest is configured
            #[cfg(feature = "encryption-at-rest")]
            if let Some(ref key) = self.encryption_key {
                data = key.encrypt(&data).map_err(|e| {
                    StorageError::from_io_error(
                        openraft::ErrorSubject::StateMachine,
                        openraft::ErrorVerb::Write,
                        std::io::Error::other(format!("snapshot encryption failed: {e}")),
                    )
                })?;
            }

            Ok(Some(Snapshot {
                meta: SnapshotMeta {
                    last_log_id: self.last_applied_log,
                    last_membership: self.last_membership.clone(),
                    snapshot_id: format!(
                        "snap-{}-current",
                        self.last_applied_log.map(|l| l.index).unwrap_or(0),
                    ),
                },
                snapshot: Box::new(Cursor::new(data)),
            }))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use aeon_types::PartitionId;

        #[test]
        fn apply_assign_partition() {
            let mut sm = StateMachineStore::new();
            let resp = sm.apply_request(&ClusterRequest::AssignPartition {
                partition: PartitionId::new(0),
                node: 1,
            });
            assert_eq!(resp, ClusterResponse::Ok);
            assert_eq!(
                sm.state.partition_table.get(PartitionId::new(0)),
                Some(&PartitionOwnership::Owned(1))
            );
        }

        #[test]
        fn apply_transfer_lifecycle() {
            let mut sm = StateMachineStore::new();

            // Setup: assign P0 to node 1
            sm.apply_request(&ClusterRequest::AssignPartition {
                partition: PartitionId::new(0),
                node: 1,
            });

            // Begin transfer P0: 1 → 2
            let resp = sm.apply_request(&ClusterRequest::BeginTransfer {
                partition: PartitionId::new(0),
                source: 1,
                target: 2,
            });
            assert_eq!(resp, ClusterResponse::Ok);
            assert_eq!(
                sm.state.partition_table.get(PartitionId::new(0)),
                Some(&PartitionOwnership::Transferring {
                    source: 1,
                    target: 2
                })
            );

            // Complete transfer
            let resp = sm.apply_request(&ClusterRequest::CompleteTransfer {
                partition: PartitionId::new(0),
                new_owner: 2,
            });
            assert_eq!(resp, ClusterResponse::Ok);
            assert_eq!(
                sm.state.partition_table.get(PartitionId::new(0)),
                Some(&PartitionOwnership::Owned(2))
            );
        }

        #[test]
        fn apply_transfer_abort() {
            let mut sm = StateMachineStore::new();
            sm.apply_request(&ClusterRequest::AssignPartition {
                partition: PartitionId::new(0),
                node: 1,
            });
            sm.apply_request(&ClusterRequest::BeginTransfer {
                partition: PartitionId::new(0),
                source: 1,
                target: 2,
            });

            let resp = sm.apply_request(&ClusterRequest::AbortTransfer {
                partition: PartitionId::new(0),
                revert_to: 1,
            });
            assert_eq!(resp, ClusterResponse::Ok);
            assert_eq!(
                sm.state.partition_table.get(PartitionId::new(0)),
                Some(&PartitionOwnership::Owned(1))
            );
        }

        #[test]
        fn apply_begin_transfer_wrong_owner() {
            let mut sm = StateMachineStore::new();
            sm.apply_request(&ClusterRequest::AssignPartition {
                partition: PartitionId::new(0),
                node: 1,
            });

            // Try to begin transfer from node 2 (wrong owner)
            let resp = sm.apply_request(&ClusterRequest::BeginTransfer {
                partition: PartitionId::new(0),
                source: 2,
                target: 3,
            });
            assert!(matches!(resp, ClusterResponse::Error(_)));
        }

        #[test]
        fn apply_complete_transfer_idempotent() {
            let mut sm = StateMachineStore::new();
            sm.apply_request(&ClusterRequest::AssignPartition {
                partition: PartitionId::new(0),
                node: 2,
            });

            // Complete on already-owned partition (idempotent)
            let resp = sm.apply_request(&ClusterRequest::CompleteTransfer {
                partition: PartitionId::new(0),
                new_owner: 2,
            });
            assert_eq!(resp, ClusterResponse::Ok);
        }

        #[test]
        fn apply_update_config() {
            let mut sm = StateMachineStore::new();
            let resp = sm.apply_request(&ClusterRequest::UpdateConfig {
                key: "batch_size".to_string(),
                value: "2048".to_string(),
            });
            assert_eq!(resp, ClusterResponse::Ok);
            assert_eq!(
                sm.state.config_overrides.get("batch_size"),
                Some(&"2048".to_string())
            );
        }

        #[test]
        fn apply_double_begin_transfer_fails() {
            let mut sm = StateMachineStore::new();
            sm.apply_request(&ClusterRequest::AssignPartition {
                partition: PartitionId::new(0),
                node: 1,
            });
            sm.apply_request(&ClusterRequest::BeginTransfer {
                partition: PartitionId::new(0),
                source: 1,
                target: 2,
            });

            // Try to begin another transfer while one is in progress
            let resp = sm.apply_request(&ClusterRequest::BeginTransfer {
                partition: PartitionId::new(0),
                source: 1,
                target: 3,
            });
            assert!(matches!(resp, ClusterResponse::Error(_)));
        }

        #[tokio::test]
        async fn snapshot_roundtrip() {
            let mut sm = StateMachineStore::new();
            sm.apply_request(&ClusterRequest::AssignPartition {
                partition: PartitionId::new(0),
                node: 1,
            });
            sm.apply_request(&ClusterRequest::AssignPartition {
                partition: PartitionId::new(1),
                node: 2,
            });

            // Build snapshot
            let mut builder = sm.get_snapshot_builder().await;
            let snapshot = builder.build_snapshot().await.unwrap();

            // Install into fresh state machine
            let mut sm2 = StateMachineStore::new();
            let data = snapshot.snapshot.into_inner();
            sm2.install_snapshot(&snapshot.meta, Box::new(Cursor::new(data)))
                .await
                .unwrap();

            assert_eq!(
                sm2.state.partition_table.get(PartitionId::new(0)),
                Some(&PartitionOwnership::Owned(1))
            );
            assert_eq!(
                sm2.state.partition_table.get(PartitionId::new(1)),
                Some(&PartitionOwnership::Owned(2))
            );
        }

        // ─── FT-2: persistent snapshot tests ────────────────────────────
        #[derive(Default)]
        struct MemL3 {
            data: std::sync::Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>,
        }

        impl aeon_types::L3Store for MemL3 {
            fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, aeon_types::AeonError> {
                Ok(self.data.lock().unwrap().get(key).cloned())
            }
            fn put(&self, key: &[u8], value: &[u8]) -> Result<(), aeon_types::AeonError> {
                self.data
                    .lock()
                    .unwrap()
                    .insert(key.to_vec(), value.to_vec());
                Ok(())
            }
            fn delete(&self, key: &[u8]) -> Result<(), aeon_types::AeonError> {
                self.data.lock().unwrap().remove(key);
                Ok(())
            }
            fn write_batch(
                &self,
                ops: &[aeon_types::BatchEntry],
            ) -> Result<(), aeon_types::AeonError> {
                let mut d = self.data.lock().unwrap();
                for (op, k, v) in ops {
                    match op {
                        aeon_types::BatchOp::Put => {
                            d.insert(k.clone(), v.clone().unwrap_or_default());
                        }
                        aeon_types::BatchOp::Delete => {
                            d.remove(k);
                        }
                    }
                }
                Ok(())
            }
            fn scan_prefix(
                &self,
                prefix: &[u8],
            ) -> Result<aeon_types::KvPairs, aeon_types::AeonError> {
                let d = self.data.lock().unwrap();
                Ok(d.range(prefix.to_vec()..)
                    .take_while(|(k, _)| k.starts_with(prefix))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect())
            }
            fn flush(&self) -> Result<(), aeon_types::AeonError> {
                Ok(())
            }
            fn len(&self) -> Result<usize, aeon_types::AeonError> {
                Ok(self.data.lock().unwrap().len())
            }
        }

        #[tokio::test]
        async fn persistent_snapshot_roundtrip_across_reopen() {
            let l3: Arc<dyn L3Store> = Arc::new(MemL3::default());

            // Boot a persistent state machine, apply a few requests,
            // and build a snapshot — which should persist to `l3`.
            {
                let mut sm = StateMachineStore::new_persistent(Arc::clone(&l3)).unwrap();
                sm.apply_request(&ClusterRequest::AssignPartition {
                    partition: PartitionId::new(0),
                    node: 1,
                });
                sm.apply_request(&ClusterRequest::UpdateConfig {
                    key: "k".into(),
                    value: "v".into(),
                });
                // Set last_applied_log so the snapshot meta is non-empty.
                sm.last_applied_log = Some(openraft::LogId::new(
                    openraft::CommittedLeaderId::new(1, 0),
                    7,
                ));

                let mut builder = sm.get_snapshot_builder().await;
                let _snap = builder.build_snapshot().await.unwrap();
            }

            // L3 must now contain both snapshot keys.
            assert!(l3.get(SNAPSHOT_META_KEY).unwrap().is_some());
            assert!(l3.get(SNAPSHOT_DATA_KEY).unwrap().is_some());

            // Re-open from the same L3 — state should be hydrated.
            let sm2 = StateMachineStore::new_persistent(Arc::clone(&l3)).unwrap();
            assert_eq!(
                sm2.state.partition_table.get(PartitionId::new(0)),
                Some(&PartitionOwnership::Owned(1))
            );
            assert_eq!(sm2.state.config_overrides.get("k"), Some(&"v".to_string()));
            assert_eq!(sm2.last_applied_log.map(|l| l.index), Some(7));
        }

        #[tokio::test]
        async fn persistent_new_on_empty_l3_is_fresh() {
            let l3: Arc<dyn L3Store> = Arc::new(MemL3::default());
            let sm = StateMachineStore::new_persistent(l3).unwrap();
            assert!(sm.last_applied_log.is_none());
            assert_eq!(sm.state.partition_table.iter().count(), 0);
        }

        #[tokio::test]
        async fn log_store_vote_roundtrip() {
            let mut store = MemLogStore::new();
            assert!(store.read_vote().await.unwrap().is_none());

            let vote = Vote::new(1, 42);
            store.save_vote(&vote).await.unwrap();
            assert_eq!(store.read_vote().await.unwrap(), Some(vote));
        }

        #[tokio::test]
        async fn log_store_append_and_read() {
            let mut store = MemLogStore::new();

            let state = store.get_log_state().await.unwrap();
            assert!(state.last_log_id.is_none());
            assert!(state.last_purged_log_id.is_none());
        }

        // ── P5: owned-partitions watch tests ────────────────────────────
        #[tokio::test]
        async fn owned_partitions_watch_broadcasts_on_change() {
            let sm = StateMachineStore::new();
            let slot = sm.owned_partitions_watch_slot();

            let (sender, mut rx) =
                tokio::sync::watch::channel(Vec::<PartitionId>::new());
            *slot.write().await = Some(OwnedPartitionsWatch {
                node_id: 7,
                sender,
            });

            // Mutate state so node 7 owns P0 and P2; P1 goes elsewhere.
            let mut sm = sm;
            sm.state.partition_table.assign(PartitionId::new(0), 7);
            sm.state.partition_table.assign(PartitionId::new(1), 3);
            sm.state.partition_table.assign(PartitionId::new(2), 7);

            sm.broadcast_owned_partitions().await;
            assert!(rx.has_changed().unwrap());
            let owned = rx.borrow_and_update().clone();
            assert_eq!(
                owned,
                vec![PartitionId::new(0), PartitionId::new(2)]
            );
        }

        #[tokio::test]
        async fn owned_partitions_watch_noop_when_unchanged() {
            let sm = StateMachineStore::new();
            let slot = sm.owned_partitions_watch_slot();
            let (sender, mut rx) =
                tokio::sync::watch::channel(Vec::<PartitionId>::new());
            *slot.write().await = Some(OwnedPartitionsWatch {
                node_id: 7,
                sender,
            });

            let mut sm = sm;
            sm.state.partition_table.assign(PartitionId::new(0), 7);
            sm.broadcast_owned_partitions().await;
            assert!(rx.has_changed().unwrap());
            rx.borrow_and_update();

            // Second broadcast with no state change must not fire.
            sm.broadcast_owned_partitions().await;
            assert!(!rx.has_changed().unwrap());
        }

        #[tokio::test]
        async fn owned_partitions_watch_noop_without_subscriber() {
            let mut sm = StateMachineStore::new();
            sm.state.partition_table.assign(PartitionId::new(0), 7);
            // No slot installed — broadcast must be safe and silent.
            sm.broadcast_owned_partitions().await;
        }
    }
}
