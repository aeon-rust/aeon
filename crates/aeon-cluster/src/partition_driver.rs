//! Leader-side / target-side partition transfer driver (CL-6 / P1.1c).
//!
//! Where CL-6a, CL-6b, CL-6c each implement one wire step of the
//! partition handover protocol, this module ties them together into a
//! single end-to-end orchestrator and bolts on the Raft-side commit.
//! For a given `(pipeline, partition, source, target)` tuple where
//! `target == self`, [`PartitionTransferDriver::drive_one`]:
//!
//! 1. Opens a QUIC connection to `source` (if not already pooled).
//! 2. CL-6a — pulls the L2 segment spine with manifest-aware install
//!    hooks ([`SegmentInstaller`]), tracking tracker state Idle →
//!    BulkSync → (pending cutover).
//! 3. CL-6b — pulls the PoH chain state and installs it via
//!    [`PohChainInstaller`].
//! 4. CL-6c — sends the cutover request; source drains + freezes and
//!    returns final watermarks. Tracker transitions BulkSync → Cutover.
//! 5. Proposes [`ClusterRequest::CompleteTransfer`] through Raft so
//!    every node observes the ownership flip. On success, tracker
//!    transitions Cutover → Complete.
//!
//! Any failure along the way aborts the tracker (`reverted_to = source`)
//! and proposes [`ClusterRequest::AbortTransfer`] so the cluster's
//! partition table reverts to `Owned(source)`.
//!
//! The watcher side — finding `PartitionOwnership::Transferring { .. }`
//! entries in committed cluster state where we are the target — is
//! served by [`PartitionTransferDriver::watch_loop`]. It polls
//! `shared_state` with dedup (one inflight `drive_one` per partition)
//! and can be woken early by [`PartitionTransferDriver::notify`].
//!
//! Engine-side persistence hooks ([`SegmentInstaller`],
//! [`PohChainInstaller`]) live close to the engine and are injected at
//! construction so this crate stays free of a hard engine dependency.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aeon_types::{AeonError, PartitionId, SegmentChunk, SegmentManifest};
use openraft::Raft;
use tokio::sync::{Mutex, Notify};

use crate::raft_config::AeonRaftConfig;
use crate::store::SharedClusterState;
use crate::transfer::TransferTracker;
use crate::transport::cutover::request_partition_cutover;
use crate::transport::endpoint::QuicEndpoint;
use crate::transport::partition_transfer::request_partition_transfer;
use crate::transport::poh_transfer::request_poh_chain_transfer;
use crate::types::{
    ClusterRequest, ClusterResponse, NodeAddress, NodeId, PartitionCutoverRequest,
    PartitionOwnership, PartitionTransferEnd, PartitionTransferRequest, PohChainTransferRequest,
};

/// Target-side installer for L2 segment bytes received during a
/// CL-6a partition transfer. Called in order: `begin` → `apply_chunk`*
/// → `finish`. Any `Err` aborts the transfer.
///
/// Engines typically back this with an `aeon_engine::l2_transfer::
/// SegmentWriter` per-partition. Held behind `&self` (not `&mut self`)
/// so the driver can hand the same `Arc<dyn SegmentInstaller>` to
/// concurrent transfers; implementations keep per-partition state
/// behind their own locking if needed.
pub trait SegmentInstaller: Send + Sync {
    /// Called once, before any chunks, with the full segment manifest.
    /// The installer should allocate its per-partition target state
    /// here (open files, pre-size CRC state, etc).
    fn begin(
        &self,
        req: &PartitionTransferRequest,
        manifest: SegmentManifest,
    ) -> Result<(), AeonError>;

    /// Called once per received chunk in arrival order. Chunks arrive
    /// grouped by segment but the installer should not assume strict
    /// ordering across segments.
    fn apply_chunk(
        &self,
        req: &PartitionTransferRequest,
        chunk: SegmentChunk,
    ) -> Result<(), AeonError>;

    /// Called once after the terminal end frame. `end.success == false`
    /// means the source aborted mid-stream; the installer should delete
    /// any partial files it wrote.
    fn finish(
        &self,
        req: &PartitionTransferRequest,
        end: PartitionTransferEnd,
    ) -> Result<(), AeonError>;
}

/// Target-side installer for a CL-6b PoH chain state snapshot.
/// `state_bytes` is the bincode-encoded `PohChainState` the source
/// returned; the installer decodes it and wires it into the node-local
/// `PohChain` for the partition.
pub trait PohChainInstaller: Send + Sync {
    fn install(&self, req: &PohChainTransferRequest, state_bytes: Vec<u8>)
    -> Result<(), AeonError>;
}

/// Maps a `NodeId` to the `NodeAddress` the driver should connect to
/// when pulling segments / PoH state / requesting cutover. Typically
/// backed by the live Raft membership (see [`RaftNodeResolver`]) but
/// abstracted here so tests can inject phantom nodes without adding
/// them to Raft.
pub trait NodeResolver: Send + Sync {
    fn resolve(&self, node_id: NodeId) -> Option<NodeAddress>;
}

/// [`NodeResolver`] backed by an openraft `Raft` handle's committed
/// membership snapshot. Production wiring uses this.
pub struct RaftNodeResolver {
    raft: Raft<AeonRaftConfig>,
}

impl RaftNodeResolver {
    pub fn new(raft: Raft<AeonRaftConfig>) -> Self {
        Self { raft }
    }
}

impl NodeResolver for RaftNodeResolver {
    fn resolve(&self, node_id: NodeId) -> Option<NodeAddress> {
        let metrics = self.raft.metrics().borrow().clone();
        metrics
            .membership_config
            .membership()
            .nodes()
            .find(|(id, _)| **id == node_id)
            .map(|(_, addr)| addr.clone())
    }
}

/// Orchestrates the three-step CL-6 partition handover on the target
/// node and then commits the Raft ownership flip. See module docs.
///
/// The driver is cheaply `Arc`-shareable; spawning the watcher loop
/// takes `Arc<Self>` so multiple partitions can drive concurrently.
pub struct PartitionTransferDriver {
    endpoint: Arc<QuicEndpoint>,
    raft: Raft<AeonRaftConfig>,
    shared_state: SharedClusterState,
    resolver: Arc<dyn NodeResolver>,
    my_id: NodeId,
    /// Single-pipeline deployments use this name for all wire requests.
    /// Multi-pipeline will need a resolver (partition → pipeline) in a
    /// follow-up; the cluster partition table currently has no pipeline
    /// dimension so this is correct for the single-pipeline default.
    pipeline: String,
    segment_installer: Arc<dyn SegmentInstaller>,
    poh_installer: Arc<dyn PohChainInstaller>,
    /// Per-partition trackers + dedup set. A partition is in this map
    /// for the full lifetime of its drive attempt; entries are removed
    /// on success or abort so a retry can re-enter.
    trackers: Mutex<HashMap<PartitionId, TransferTracker>>,
    /// Wake signal for the watcher — `propose_partition_transfer` calls
    /// `notify()` so we don't wait a poll interval to observe the new
    /// `Transferring` entry.
    wake: Notify,
    /// Watcher poll interval. 500ms is short enough for responsive
    /// handover in tests, long enough to avoid hot-looping an idle node.
    poll_interval: Duration,
}

impl PartitionTransferDriver {
    /// Construct a driver. `pipeline` is the single pipeline this node
    /// hosts — all wire requests (`PartitionTransferRequest`,
    /// `PohChainTransferRequest`, `PartitionCutoverRequest`) will carry
    /// this name. Multi-pipeline support is a follow-up (P5).
    #[allow(clippy::too_many_arguments)] // All args are required deps; grouping would only shuffle complexity.
    pub fn new(
        endpoint: Arc<QuicEndpoint>,
        raft: Raft<AeonRaftConfig>,
        shared_state: SharedClusterState,
        resolver: Arc<dyn NodeResolver>,
        my_id: NodeId,
        pipeline: impl Into<String>,
        segment_installer: Arc<dyn SegmentInstaller>,
        poh_installer: Arc<dyn PohChainInstaller>,
    ) -> Self {
        Self {
            endpoint,
            raft,
            shared_state,
            resolver,
            my_id,
            pipeline: pipeline.into(),
            segment_installer,
            poh_installer,
            trackers: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            poll_interval: Duration::from_millis(500),
        }
    }

    /// Override the watcher poll interval. Primarily a test seam —
    /// tests use very short intervals to keep runtime low.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Wake the watcher loop early. Safe to call from any thread.
    /// Intended for `ClusterNode::propose_partition_transfer` so a
    /// freshly-accepted transfer starts without the poll delay.
    pub fn notify(&self) {
        self.wake.notify_one();
    }

    /// Resolve `node_id` to its current address via the injected
    /// [`NodeResolver`]. Returns `Err` if the node is unknown —
    /// typically means the membership update hasn't propagated yet and
    /// the caller should retry after the next apply.
    fn resolve_address(&self, node_id: NodeId) -> Result<NodeAddress, AeonError> {
        self.resolver.resolve(node_id).ok_or_else(|| {
            AeonError::state(format!(
                "partition-driver: no membership entry for node_id={node_id}"
            ))
        })
    }

    /// Run the full handover for one partition. Blocks until the
    /// transfer commits (target owns the partition) or aborts (source
    /// keeps the partition). Independent of the watcher loop —
    /// integration tests call this directly.
    pub async fn drive_one(
        &self,
        partition: PartitionId,
        source: NodeId,
        target: NodeId,
    ) -> Result<(), AeonError> {
        if target != self.my_id {
            return Err(AeonError::state(format!(
                "partition-driver: drive_one called on node {} for transfer target {}",
                self.my_id, target
            )));
        }

        // Insert a fresh tracker — or bail if a drive is already in
        // flight for this partition. The watcher relies on this to
        // avoid spawning duplicate drives for the same Transferring
        // entry on successive poll ticks.
        {
            let mut trackers = self.trackers.lock().await;
            if trackers.contains_key(&partition) {
                return Err(AeonError::state(format!(
                    "partition-driver: transfer already in flight for partition {}",
                    partition.as_u16()
                )));
            }
            trackers.insert(partition, TransferTracker::new(partition));
        }

        let result = self.drive_one_inner(partition, source, target).await;

        // Always clean up the tracker entry so a subsequent retry
        // (e.g. after an abort-and-reassign) can re-enter.
        self.trackers.lock().await.remove(&partition);
        result
    }

    async fn drive_one_inner(
        &self,
        partition: PartitionId,
        source: NodeId,
        target: NodeId,
    ) -> Result<(), AeonError> {
        // Resolve source address + open connection.
        let source_addr = self.resolve_address(source)?;
        let conn = self
            .endpoint
            .connect(source, &source_addr)
            .await
            .map_err(|e| AeonError::Cluster {
                message: format!("partition-driver: connect to source {source}@{source_addr}: {e}"),
                source: None,
            })?;

        // CL-6a: bulk sync. Use `request_partition_transfer` directly
        // (not `drive_partition_transfer`) because the installer needs
        // manifest access that the latter does not expose. Manage the
        // tracker state inline.
        self.transition_begin(partition, source, target).await?;

        let transfer_req = PartitionTransferRequest {
            pipeline: self.pipeline.clone(),
            partition,
        };
        let installer = Arc::clone(&self.segment_installer);
        let req_for_manifest = transfer_req.clone();
        let req_for_chunk = transfer_req.clone();
        let req_for_finish = transfer_req.clone();
        let bulk_result = request_partition_transfer(
            &conn,
            &transfer_req,
            {
                let installer = Arc::clone(&installer);
                move |manifest| installer.begin(&req_for_manifest, manifest)
            },
            {
                let installer = Arc::clone(&installer);
                move |_, chunk| installer.apply_chunk(&req_for_chunk, chunk)
            },
            {
                let installer = Arc::clone(&installer);
                move |_, end| installer.finish(&req_for_finish, end)
            },
        )
        .await;

        if let Err(e) = bulk_result {
            self.abort_with_reason(partition, source, format!("bulk sync failed: {e}"))
                .await;
            return Err(e);
        }

        // Flip tracker: BulkSync → Cutover.
        self.transition_begin_cutover(partition, source).await?;

        // CL-6b: PoH chain state. Single round-trip.
        let poh_req = PohChainTransferRequest {
            pipeline: self.pipeline.clone(),
            partition,
        };
        let poh_bytes = match request_poh_chain_transfer(&conn, &poh_req).await {
            Ok(b) => b,
            Err(e) => {
                self.abort_with_reason(
                    partition,
                    source,
                    format!("poh-chain transfer failed: {e}"),
                )
                .await;
                return Err(e);
            }
        };
        // Empty response is the source-side sentinel for "this pipeline
        // has no PoH leg on this partition" — see
        // `aeon_engine::engine_providers::PohChainExportProvider::export_state`.
        // Skip the install call in that case; the target has nothing to
        // seed a local chain from, and installing an empty payload would
        // just surface a decode error. Non-empty payloads go through as
        // before.
        if poh_bytes.is_empty() {
            tracing::debug!(
                partition = partition.as_u16(),
                source = ?source,
                "partition-driver: source signalled no PoH leg for this partition; skipping install"
            );
        } else if let Err(e) = self.poh_installer.install(&poh_req, poh_bytes) {
            self.abort_with_reason(partition, source, format!("poh-chain install failed: {e}"))
                .await;
            return Err(e);
        }

        // CL-6c: cutover — source drains, freezes, returns final offsets.
        let cutover_req = PartitionCutoverRequest {
            pipeline: self.pipeline.clone(),
            partition,
        };
        let _offsets = match request_partition_cutover(&conn, &cutover_req).await {
            Ok(o) => o,
            Err(e) => {
                self.abort_with_reason(partition, source, format!("cutover handshake failed: {e}"))
                    .await;
                return Err(e);
            }
        };

        // Raft commit: propose CompleteTransfer so every node observes
        // the ownership flip. Uses propose_with_forward so a follower
        // node (the source pod when it isn't also the Raft leader) can
        // still complete the proposal — the helper detects
        // ForwardToLeader and re-issues over the QUIC transport. On
        // Raft failure we still abort to keep the cluster consistent —
        // the source is still frozen from the cutover handshake above,
        // so we must revert ownership to free it (the source's write
        // gate auto-releases on the abort-flip via the engine's
        // ClusterRegistryApplier).
        let propose_result = self
            .propose_with_forward(ClusterRequest::CompleteTransfer {
                partition,
                new_owner: target,
            })
            .await;

        match propose_result {
            Ok(ClusterResponse::Ok) => {
                self.transition_complete(partition).await;
                Ok(())
            }
            Ok(ClusterResponse::Error(msg)) => {
                self.abort_with_reason(
                    partition,
                    source,
                    format!("CompleteTransfer rejected by state-machine: {msg}"),
                )
                .await;
                Err(AeonError::Cluster {
                    message: format!("partition-driver: CompleteTransfer rejected: {msg}"),
                    source: None,
                })
            }
            Ok(ClusterResponse::Registry(_)) => {
                self.abort_with_reason(
                    partition,
                    source,
                    "unexpected Registry response to CompleteTransfer".to_string(),
                )
                .await;
                Err(AeonError::state(
                    "partition-driver: unexpected Registry response to CompleteTransfer",
                ))
            }
            Err(e) => {
                let msg = format!("CompleteTransfer propose failed: {e}");
                self.abort_with_reason(partition, source, msg.clone()).await;
                Err(AeonError::Cluster {
                    message: format!("partition-driver: {msg}"),
                    source: None,
                })
            }
        }
    }

    async fn transition_begin(
        &self,
        partition: PartitionId,
        source: NodeId,
        target: NodeId,
    ) -> Result<(), AeonError> {
        let mut trackers = self.trackers.lock().await;
        let tracker = trackers.get_mut(&partition).ok_or_else(|| {
            AeonError::state(format!(
                "partition-driver: no tracker slot for partition {}",
                partition.as_u16()
            ))
        })?;
        tracker
            .begin(source, target)
            .map_err(|e| AeonError::state(format!("partition-driver: tracker.begin: {e}")))
    }

    async fn transition_begin_cutover(
        &self,
        partition: PartitionId,
        _source: NodeId,
    ) -> Result<(), AeonError> {
        let mut trackers = self.trackers.lock().await;
        let tracker = trackers.get_mut(&partition).ok_or_else(|| {
            AeonError::state(format!(
                "partition-driver: no tracker slot for partition {}",
                partition.as_u16()
            ))
        })?;
        tracker
            .begin_cutover()
            .map_err(|e| AeonError::state(format!("partition-driver: tracker.begin_cutover: {e}")))
    }

    async fn transition_complete(&self, partition: PartitionId) {
        let mut trackers = self.trackers.lock().await;
        if let Some(tracker) = trackers.get_mut(&partition) {
            let _ = tracker.complete();
        }
    }

    /// Transition tracker to Aborted and propose
    /// `ClusterRequest::AbortTransfer` so the partition table reverts
    /// to `Owned(source)`. Best-effort on the Raft side — if the
    /// propose fails we log and surface the original error; the Raft
    /// apply path will re-converge on the next committed entry.
    async fn abort_with_reason(&self, partition: PartitionId, source: NodeId, reason: String) {
        {
            let mut trackers = self.trackers.lock().await;
            if let Some(tracker) = trackers.get_mut(&partition) {
                let _ = tracker.abort(reason.clone());
            }
        }
        let abort = ClusterRequest::AbortTransfer {
            partition,
            revert_to: source,
        };
        if let Err(e) = self.propose_with_forward(abort).await {
            tracing::warn!(
                partition = partition.as_u16(),
                source,
                error = %e,
                "partition-driver: AbortTransfer propose failed"
            );
        } else {
            tracing::info!(
                partition = partition.as_u16(),
                source,
                reason = %reason,
                "partition-driver: transfer aborted, ownership reverted"
            );
        }
    }

    /// Propose a `ClusterRequest` and, when this node is a follower,
    /// forward the proposal to the leader via the
    /// `MessageType::ProposeForwardRequest` RPC. Mirrors
    /// [`crate::node::ClusterNode::propose`]'s 2026-04-25 fix for the
    /// driver's own propose call sites (CompleteTransfer / AbortTransfer)
    /// that previously failed with `ForwardToLeader` whenever the
    /// transfer source pod wasn't also the Raft leader.
    ///
    /// Returns the leader's `ClusterResponse` on either path. One hop
    /// only — if the leader's own propose returns `ForwardToLeader`
    /// (mid-flight election) we surface the error and let the caller
    /// abort the transfer.
    async fn propose_with_forward(
        &self,
        request: ClusterRequest,
    ) -> Result<ClusterResponse, AeonError> {
        match self.raft.client_write(request.clone()).await {
            Ok(response) => Ok(response.data),
            Err(e) => {
                if let Some(ftl) = e.forward_to_leader() {
                    if let (Some(leader_id), Some(leader_node)) =
                        (ftl.leader_id, ftl.leader_node.as_ref())
                    {
                        return self.forward_propose(leader_id, leader_node, request).await;
                    }
                }
                Err(AeonError::Cluster {
                    message: format!("Raft proposal failed: {e}"),
                    source: None,
                })
            }
        }
    }

    /// RPC the proposal to the named leader. Companion of
    /// [`Self::propose_with_forward`]. Kept as a method so tests can
    /// stub the endpoint dial behavior without re-exporting the whole
    /// transport surface.
    async fn forward_propose(
        &self,
        leader_id: NodeId,
        leader_node: &NodeAddress,
        request: ClusterRequest,
    ) -> Result<ClusterResponse, AeonError> {
        let request_bytes = bincode::serialize(&request).map_err(|e| AeonError::Serialization {
            message: format!("forward_propose: serialize ClusterRequest: {e}"),
            source: None,
        })?;

        let req = crate::types::ProposeForwardRequest { request_bytes };
        let resp = crate::transport::network::send_propose_forward(
            &self.endpoint,
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

        bincode::deserialize::<ClusterResponse>(&resp.response_bytes).map_err(|e| {
            AeonError::Serialization {
                message: format!("forward_propose: deserialize ClusterResponse from leader: {e}"),
                source: None,
            }
        })
    }

    /// Watcher loop — polls `shared_state` for `Transferring` entries
    /// where `target == self.my_id` and spawns a `drive_one` task for
    /// each fresh one. Dedup via the `trackers` map so repeated polls
    /// don't spawn duplicates.
    ///
    /// Call `notify()` to wake the loop between polls. Set
    /// `shutdown` to `true` to stop the loop; the call returns once
    /// the outer loop sees the flag — in-flight `drive_one` tasks run
    /// to completion (they're not cancelled).
    pub async fn watch_loop(self: Arc<Self>, shutdown: Arc<AtomicBool>) {
        while !shutdown.load(Ordering::Relaxed) {
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let pending: Vec<(PartitionId, NodeId, NodeId)> = {
                let state = self.shared_state.read().await;
                let in_flight = self.trackers.lock().await;
                state
                    .partition_table
                    .iter()
                    .filter_map(|(p, own)| match own {
                        PartitionOwnership::Transferring { source, target }
                            if *target == self.my_id && !in_flight.contains_key(p) =>
                        {
                            Some((*p, *source, *target))
                        }
                        _ => None,
                    })
                    .collect()
            };

            for (partition, source, target) in pending {
                let driver = Arc::clone(&self);
                tokio::spawn(async move {
                    if let Err(e) = driver.drive_one(partition, source, target).await {
                        tracing::warn!(
                            partition = partition.as_u16(),
                            source,
                            target,
                            error = %e,
                            "partition-driver: drive_one failed"
                        );
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::cutover::{CutoverCoordinator, CutoverOffsets};
    use crate::transport::endpoint::QuicEndpoint;
    use crate::transport::framing::{self, MessageType};
    use crate::transport::partition_transfer::{ChunkIter, PartitionTransferProvider};
    use crate::transport::poh_transfer::PohChainProvider;
    use crate::transport::tls::dev_quic_configs_insecure;

    use aeon_types::{SegmentEntry, SegmentManifest};
    use bytes::Bytes;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicU32;

    /// Collects every call to the three install methods so tests can
    /// assert the driver flows the right data through the hooks.
    #[derive(Default)]
    struct RecordingSegmentInstaller {
        manifest: StdMutex<Option<SegmentManifest>>,
        chunks: StdMutex<Vec<SegmentChunk>>,
        ended: StdMutex<Option<PartitionTransferEnd>>,
        fail_on_begin: AtomicBool,
    }

    impl SegmentInstaller for RecordingSegmentInstaller {
        fn begin(
            &self,
            _req: &PartitionTransferRequest,
            manifest: SegmentManifest,
        ) -> Result<(), AeonError> {
            if self.fail_on_begin.load(Ordering::Relaxed) {
                return Err(AeonError::state("recorder: forced begin failure"));
            }
            *self
                .manifest
                .lock()
                .map_err(|e| AeonError::state(format!("lock: {e}")))? = Some(manifest);
            Ok(())
        }
        fn apply_chunk(
            &self,
            _req: &PartitionTransferRequest,
            chunk: SegmentChunk,
        ) -> Result<(), AeonError> {
            self.chunks
                .lock()
                .map_err(|e| AeonError::state(format!("lock: {e}")))?
                .push(chunk);
            Ok(())
        }
        fn finish(
            &self,
            _req: &PartitionTransferRequest,
            end: PartitionTransferEnd,
        ) -> Result<(), AeonError> {
            *self
                .ended
                .lock()
                .map_err(|e| AeonError::state(format!("lock: {e}")))? = Some(end);
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingPohInstaller {
        bytes: StdMutex<Option<Vec<u8>>>,
        fail: AtomicBool,
    }
    impl PohChainInstaller for RecordingPohInstaller {
        fn install(
            &self,
            _req: &PohChainTransferRequest,
            state_bytes: Vec<u8>,
        ) -> Result<(), AeonError> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(AeonError::state("recorder: forced poh install failure"));
            }
            *self
                .bytes
                .lock()
                .map_err(|e| AeonError::state(format!("lock: {e}")))? = Some(state_bytes);
            Ok(())
        }
    }

    /// Fixed-payload provider for CL-6a tests (happy path).
    struct FixedProvider {
        manifest: SegmentManifest,
        chunks: Vec<SegmentChunk>,
    }
    impl PartitionTransferProvider for FixedProvider {
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

    /// Provider that errors out before returning any data.
    struct FailingBulkProvider;
    impl PartitionTransferProvider for FailingBulkProvider {
        fn serve(
            &self,
            _req: &PartitionTransferRequest,
        ) -> Result<(SegmentManifest, ChunkIter<'static>), AeonError> {
            Err(AeonError::state("stub: bulk provider failure"))
        }
    }

    struct FixedPohProvider {
        payload: Vec<u8>,
    }
    impl PohChainProvider for FixedPohProvider {
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

    struct FixedCutoverCoordinator {
        offsets: CutoverOffsets,
        calls: AtomicU32,
    }
    impl CutoverCoordinator for FixedCutoverCoordinator {
        fn drain_and_freeze<'a>(
            &'a self,
            _req: &'a PartitionCutoverRequest,
        ) -> Pin<Box<dyn Future<Output = Result<CutoverOffsets, AeonError>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let offsets = self.offsets;
            Box::pin(async move { Ok(offsets) })
        }
    }

    /// Tiny server that muxes CL-6a / CL-6b / CL-6c frames off one
    /// endpoint by peeking the first frame type. Mirrors the
    /// real `handle_stream` dispatch loop but without the Raft RPCs we
    /// don't need here.
    async fn run_source_endpoint(
        endpoint: Arc<QuicEndpoint>,
        bulk: Arc<dyn PartitionTransferProvider>,
        poh: Arc<dyn PohChainProvider>,
        cutover: Arc<dyn CutoverCoordinator>,
        shutdown: Arc<AtomicBool>,
    ) {
        while !shutdown.load(Ordering::Relaxed) {
            let incoming = tokio::select! {
                i = endpoint.accept() => match i {
                    Some(i) => i,
                    None => break,
                },
                _ = tokio::time::sleep(Duration::from_millis(25)) => continue,
            };
            let bulk = Arc::clone(&bulk);
            let poh = Arc::clone(&poh);
            let cutover = Arc::clone(&cutover);
            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(_) => return,
                };
                loop {
                    let (send, mut recv) = match conn.accept_bi().await {
                        Ok(s) => s,
                        Err(_) => break,
                    };
                    let bulk = Arc::clone(&bulk);
                    let poh = Arc::clone(&poh);
                    let cutover = Arc::clone(&cutover);
                    tokio::spawn(async move {
                        // Peek first frame to route.
                        let (mt, payload) = match framing::read_frame(&mut recv).await {
                            Ok(x) => x,
                            Err(_) => return,
                        };
                        match mt {
                            MessageType::PartitionTransferRequest => {
                                // Need to re-inject the payload; the
                                // real server uses `_with_request` for
                                // this, so we do the same.
                                let req: PartitionTransferRequest =
                                    match bincode::deserialize(&payload) {
                                        Ok(r) => r,
                                        Err(_) => return,
                                    };
                                let _ = crate::transport::partition_transfer::
                                    serve_partition_transfer_with_request(
                                        &*bulk, send, &req,
                                    )
                                    .await;
                            }
                            MessageType::PohChainTransferRequest => {
                                let req: PohChainTransferRequest =
                                    match bincode::deserialize(&payload) {
                                        Ok(r) => r,
                                        Err(_) => return,
                                    };
                                let _ = crate::transport::poh_transfer::
                                    serve_poh_chain_transfer_with_request(
                                        &*poh, send, &req,
                                    )
                                    .await;
                            }
                            MessageType::PartitionCutoverRequest => {
                                let req: PartitionCutoverRequest =
                                    match bincode::deserialize(&payload) {
                                        Ok(r) => r,
                                        Err(_) => return,
                                    };
                                let _ = crate::transport::cutover::
                                    serve_partition_cutover_with_request(
                                        &*cutover, send, &req,
                                    )
                                    .await;
                            }
                            _ => {
                                // Swallow unknown types — the driver
                                // never sends them.
                                let _ = send;
                            }
                        }
                    });
                }
            });
        }
    }

    fn sample_manifest_and_chunks() -> (SegmentManifest, Vec<SegmentChunk>) {
        let d0 = Bytes::from_static(b"hello");
        let d1 = Bytes::from_static(b"aeon");
        let manifest = SegmentManifest {
            entries: vec![
                SegmentEntry {
                    start_seq: 0,
                    size_bytes: d0.len() as u64,
                    crc32: 0xAAAA_BBBB,
                },
                SegmentEntry {
                    start_seq: 100,
                    size_bytes: d1.len() as u64,
                    crc32: 0xCCCC_DDDD,
                },
            ],
        };
        let chunks = vec![
            SegmentChunk {
                start_seq: 0,
                offset: 0,
                data: d0,
                is_last: true,
            },
            SegmentChunk {
                start_seq: 100,
                offset: 0,
                data: d1,
                is_last: true,
            },
        ];
        (manifest, chunks)
    }

    /// Bring up a single-node cluster + a separate "source" QUIC
    /// endpoint, then drive a fake transfer `source(2) → target(self=1)`
    /// and assert every hook fired + Raft committed CompleteTransfer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_one_happy_path_commits_complete_transfer() {
        let (server_cfg, client_cfg) = dev_quic_configs_insecure();

        // Source-side QUIC endpoint that speaks CL-6a/b/c.
        let source_ep = Arc::new(
            QuicEndpoint::bind(
                "127.0.0.1:0".parse().expect("parse"),
                server_cfg.clone(),
                client_cfg.clone(),
            )
            .expect("bind source"),
        );
        let source_addr = source_ep.local_addr().expect("local_addr");
        let (manifest, chunks) = sample_manifest_and_chunks();
        let bulk_provider: Arc<dyn PartitionTransferProvider> =
            Arc::new(FixedProvider { manifest, chunks });
        let poh_provider: Arc<dyn PohChainProvider> = Arc::new(FixedPohProvider {
            payload: b"poh-chain-state-bytes".to_vec(),
        });
        let cutover_coord: Arc<dyn CutoverCoordinator> = Arc::new(FixedCutoverCoordinator {
            offsets: CutoverOffsets {
                final_source_offset: 42,
                final_poh_sequence: 7,
            },
            calls: AtomicU32::new(0),
        });
        let src_shutdown = Arc::new(AtomicBool::new(false));
        let src_task = tokio::spawn(run_source_endpoint(
            Arc::clone(&source_ep),
            Arc::clone(&bulk_provider),
            Arc::clone(&poh_provider),
            Arc::clone(&cutover_coord),
            Arc::clone(&src_shutdown),
        ));

        // Target-side: single-node Raft (immediate leader) + a
        // separate QUIC endpoint for outbound connections.
        let (node, target_ep) = new_test_node().await;
        let resolver: Arc<dyn NodeResolver> = Arc::new(StubResolver(NodeAddress::new(
            "127.0.0.1",
            source_addr.port(),
        )));
        let segment_installer = Arc::new(RecordingSegmentInstaller::default());
        let poh_installer = Arc::new(RecordingPohInstaller::default());

        // Seed the partition table with Transferring(P0, 2 → 1) so
        // CompleteTransfer's apply has something to complete.
        node.propose(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 2,
        })
        .await
        .expect("assign p0 to source node 2");
        node.propose(ClusterRequest::BeginTransfer {
            partition: PartitionId::new(0),
            source: 2,
            target: 1,
        })
        .await
        .expect("begin transfer");

        let driver = PartitionTransferDriver::new(
            target_ep,
            node.raft().clone(),
            node.shared_state(),
            resolver,
            1,
            "pl",
            segment_installer.clone(),
            poh_installer.clone(),
        );

        driver
            .drive_one(PartitionId::new(0), 2, 1)
            .await
            .expect("drive_one ok");

        // Segment installer must have observed the manifest + both chunks + end.
        assert!(
            segment_installer.manifest.lock().expect("lock").is_some(),
            "segment installer never saw manifest"
        );
        assert_eq!(
            segment_installer.chunks.lock().expect("lock").len(),
            2,
            "segment installer must see both chunks"
        );
        let end = segment_installer
            .ended
            .lock()
            .expect("lock")
            .clone()
            .expect("end frame");
        assert!(end.success, "end frame must be success=true");

        // PoH installer must have received the source bytes.
        let poh_bytes = poh_installer
            .bytes
            .lock()
            .expect("lock")
            .clone()
            .expect("poh install");
        assert_eq!(poh_bytes, b"poh-chain-state-bytes");

        // Partition table must reflect ownership flip to node 1.
        let snap = node.shared_state().read().await.clone();
        assert_eq!(
            snap.partition_table.get(PartitionId::new(0)),
            Some(&PartitionOwnership::Owned(1)),
            "CompleteTransfer apply must flip ownership to target"
        );

        src_shutdown.store(true, Ordering::Relaxed);
        source_ep.close();
        let _ = src_task.await;
        node.shutdown().await.expect("node shutdown");
    }

    /// Happy path except the bulk provider fails mid-stream: the
    /// driver must propose AbortTransfer so the partition table
    /// reverts to Owned(source).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_one_bulk_failure_proposes_abort_transfer() {
        let (server_cfg, client_cfg) = dev_quic_configs_insecure();

        let source_ep = Arc::new(
            QuicEndpoint::bind(
                "127.0.0.1:0".parse().expect("parse"),
                server_cfg.clone(),
                client_cfg.clone(),
            )
            .expect("bind source"),
        );
        let source_addr = source_ep.local_addr().expect("local_addr");
        let bulk_provider: Arc<dyn PartitionTransferProvider> = Arc::new(FailingBulkProvider);
        let poh_provider: Arc<dyn PohChainProvider> = Arc::new(FixedPohProvider {
            payload: Vec::new(),
        });
        let cutover_coord: Arc<dyn CutoverCoordinator> = Arc::new(FixedCutoverCoordinator {
            offsets: CutoverOffsets {
                final_source_offset: 0,
                final_poh_sequence: 0,
            },
            calls: AtomicU32::new(0),
        });
        let src_shutdown = Arc::new(AtomicBool::new(false));
        let src_task = tokio::spawn(run_source_endpoint(
            Arc::clone(&source_ep),
            Arc::clone(&bulk_provider),
            Arc::clone(&poh_provider),
            Arc::clone(&cutover_coord),
            Arc::clone(&src_shutdown),
        ));

        let (node, target_ep) = new_test_node().await;
        let resolver: Arc<dyn NodeResolver> = Arc::new(StubResolver(NodeAddress::new(
            "127.0.0.1",
            source_addr.port(),
        )));
        let segment_installer = Arc::new(RecordingSegmentInstaller::default());
        let poh_installer = Arc::new(RecordingPohInstaller::default());

        node.propose(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 2,
        })
        .await
        .expect("assign");
        node.propose(ClusterRequest::BeginTransfer {
            partition: PartitionId::new(0),
            source: 2,
            target: 1,
        })
        .await
        .expect("begin transfer");

        let driver = PartitionTransferDriver::new(
            target_ep,
            node.raft().clone(),
            node.shared_state(),
            resolver,
            1,
            "pl",
            segment_installer,
            poh_installer,
        );

        let _err = driver
            .drive_one(PartitionId::new(0), 2, 1)
            .await
            .expect_err("bulk failure must surface as Err");
        // The exact error string depends on whether the failure surfaces
        // as a failure end frame (→ "remote reported failure") or as a
        // missing manifest frame (→ "expected manifest frame"). We don't
        // pin the wording; the key behavior is the abort-on-Raft below.

        // Partition table must have reverted to Owned(source=2).
        let snap = node.shared_state().read().await.clone();
        assert_eq!(
            snap.partition_table.get(PartitionId::new(0)),
            Some(&PartitionOwnership::Owned(2)),
            "AbortTransfer apply must revert ownership to source"
        );

        src_shutdown.store(true, Ordering::Relaxed);
        source_ep.close();
        let _ = src_task.await;
        node.shutdown().await.expect("node shutdown");
    }

    /// A second `drive_one` for a partition that already has a live
    /// tracker must be rejected (dedup gate). This is a focused unit
    /// test of the entry guard: we pre-populate `trackers` directly
    /// (we're in the same module) rather than racing two real drives
    /// through a flaky barrier — the dedup check is the first thing
    /// `drive_one` does after `target == my_id` validation, so no QUIC
    /// or Raft machinery is touched on the rejected path.
    #[tokio::test]
    async fn drive_one_rejects_concurrent_same_partition() {
        let (node, target_ep) = new_test_node().await;
        let resolver: Arc<dyn NodeResolver> =
            Arc::new(StubResolver(NodeAddress::new("127.0.0.1", 1)));
        let driver = PartitionTransferDriver::new(
            target_ep,
            node.raft().clone(),
            node.shared_state(),
            resolver,
            1,
            "pl",
            Arc::new(RecordingSegmentInstaller::default()),
            Arc::new(RecordingPohInstaller::default()),
        );

        // Simulate a first drive that has already acquired the tracker
        // slot (by inserting directly — same module has field access).
        driver.trackers.lock().await.insert(
            PartitionId::new(0),
            TransferTracker::new(PartitionId::new(0)),
        );

        let err = driver
            .drive_one(PartitionId::new(0), 2, 1)
            .await
            .expect_err("second drive_one must reject for same partition");
        let msg = format!("{err}");
        assert!(
            msg.contains("already in flight"),
            "rejection must name the reason, got: {msg}"
        );

        // Drop the synthetic tracker and confirm a subsequent drive
        // would not be rejected by the dedup gate (it'll fail later on
        // resolve/connect, but that proves the gate released cleanly).
        driver.trackers.lock().await.remove(&PartitionId::new(0));
        let retry_err = driver
            .drive_one(PartitionId::new(0), 2, 1)
            .await
            .expect_err("retry must proceed past the dedup gate and fail later");
        let retry_msg = format!("{retry_err}");
        assert!(
            !retry_msg.contains("already in flight"),
            "retry must not be blocked by stale tracker, got: {retry_msg}"
        );

        node.shutdown().await.expect("node shutdown");
    }

    /// Single-node cluster (immediate leader, 1-of-1 quorum) plus a
    /// standalone client-side QUIC endpoint the driver uses to connect
    /// outbound. The "source" node is phantom — address resolution is
    /// done via a test-local [`StubResolver`], not Raft membership.
    async fn new_test_node() -> (crate::ClusterNode, Arc<QuicEndpoint>) {
        use crate::config::ClusterConfig;

        let cfg = ClusterConfig::single_node(1, 4);
        let node = crate::ClusterNode::bootstrap_single(cfg)
            .await
            .expect("bootstrap_single");

        // Separate client-side endpoint for outbound CL-6a/b/c connects.
        let (server_cfg, client_cfg) = dev_quic_configs_insecure();
        let client_ep = Arc::new(
            QuicEndpoint::bind(
                "127.0.0.1:0".parse().expect("parse"),
                server_cfg,
                client_cfg,
            )
            .expect("bind client endpoint"),
        );

        (node, client_ep)
    }

    struct StubResolver(NodeAddress);
    impl NodeResolver for StubResolver {
        fn resolve(&self, node_id: NodeId) -> Option<NodeAddress> {
            if node_id == 2 {
                Some(self.0.clone())
            } else {
                None
            }
        }
    }
}
