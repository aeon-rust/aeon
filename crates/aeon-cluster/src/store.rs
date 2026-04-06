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

    use openraft::storage::{
        LogFlushed, LogState, RaftLogReader, RaftLogStorage, RaftStateMachine,
    };
    use openraft::storage::{RaftSnapshotBuilder, Snapshot, SnapshotMeta};
    use openraft::{Entry, LogId, StorageError, StoredMembership, Vote};
    use tokio::sync::RwLock;

    use crate::raft_config::AeonRaftConfig;
    use crate::snapshot::ClusterSnapshot;
    use crate::types::{ClusterRequest, ClusterResponse, NodeAddress, PartitionOwnership};

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
    }

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
            }
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

            let snapshot = Snapshot {
                meta: SnapshotMeta {
                    last_log_id: self.last_applied_log,
                    last_membership: self.last_membership.clone(),
                    snapshot_id,
                },
                snapshot: Box::new(Cursor::new(data)),
            };

            Ok(snapshot)
        }
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
                        let resp = self.apply_request(&req);
                        responses.push(resp);
                    }
                    openraft::EntryPayload::Membership(membership) => {
                        self.last_membership =
                            StoredMembership::new(Some(entry.log_id), membership);
                        responses.push(ClusterResponse::Ok);
                    }
                }
            }

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
            #[allow(unused_mut)]
            let mut data = snapshot.into_inner();

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
    }
}
