//! Engine-side [`CutoverCoordinator`] implementation.
//!
//! Bridges the cluster-side partition handover protocol (CL-6c) onto the
//! engine-side [`WriteGateRegistry`] primitive. When the leader drives a
//! partition transfer and a cutover request lands on the source node's
//! QUIC endpoint, the cluster's `serve_partition_cutover_stream`
//! dispatcher invokes [`EngineCutoverCoordinator::drain_and_freeze`],
//! which:
//!
//! 1. Looks up the per-(pipeline, partition) `WriteGate` in the registry.
//!    A missing entry surfaces as `AeonError::State` so the target aborts
//!    cleanly (the partition is not hosted here — misrouted request).
//! 2. Calls `gate.request_freeze_and_drain(timeout)` which flips the
//!    gate to `FreezeRequested`, awaits in-flight `next_batch` fetches to
//!    complete (drain), then commits the `Frozen` transition atomically.
//! 3. Reads the final source-anchor offset and PoH sequence via the
//!    optional [`CutoverWatermarkReader`] hook. Deployments that do not
//!    install a reader (or that have no Pull source on this partition)
//!    get the `-1 / 0` sentinels the wire protocol expects.
//!
//! This module is feature-gated behind `cluster`, matching the optional
//! `aeon-cluster` dependency — single-node builds do not pay for it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aeon_cluster::transport::cutover::{CutoverCoordinator, CutoverOffsets};
use aeon_cluster::types::PartitionCutoverRequest;
use aeon_types::AeonError;
use aeon_types::partition::PartitionId;

use crate::write_gate::WriteGateRegistry;

/// Pluggable read-side for the source-offset + PoH-sequence watermarks
/// the cutover response must carry.
///
/// The write-gate primitive (P1.1a) knows *when* a partition is frozen
/// but not *which offsets* the source was at. Implementations live close
/// to the pipeline runtime — typically backed by per-partition offset
/// trackers that the source task updates after each successful batch —
/// and are injected into [`EngineCutoverCoordinator`] by the top-level
/// binary at wiring time.
///
/// Deployments without a reader (pure push/poll sources, early test
/// harnesses, single-node setups) fall back to the `-1 / 0` sentinels
/// documented on [`CutoverOffsets`].
pub trait CutoverWatermarkReader: Send + Sync {
    /// Return the final watermarks for `(pipeline, partition)` — called
    /// after the gate has committed `Frozen`, so there is no racing
    /// source fetch in flight.
    fn read_watermarks(&self, pipeline: &str, partition: PartitionId) -> CutoverOffsets;
}

/// [`CutoverCoordinator`] impl backed by a [`WriteGateRegistry`] plus an
/// optional [`CutoverWatermarkReader`].
pub struct EngineCutoverCoordinator {
    gates: Arc<WriteGateRegistry>,
    watermarks: Option<Arc<dyn CutoverWatermarkReader>>,
    drain_timeout: Duration,
}

impl EngineCutoverCoordinator {
    /// Construct with a shared gate registry. `drain_timeout` bounds how
    /// long `drain_and_freeze` will wait for in-flight fetches to drain;
    /// 2 s is conservative for the ~ms batch latencies Aeon targets but
    /// short enough that a wedged source surfaces as a cluster error
    /// rather than silently stalling the transfer.
    pub fn new(gates: Arc<WriteGateRegistry>) -> Self {
        Self {
            gates,
            watermarks: None,
            drain_timeout: Duration::from_secs(2),
        }
    }

    /// Install a watermark reader. Without one, cutover responses carry
    /// the `-1 / 0` sentinels (correct for push/poll partitions; pull
    /// deployments must set a reader for exactly-once handover).
    pub fn with_watermarks(mut self, reader: Arc<dyn CutoverWatermarkReader>) -> Self {
        self.watermarks = Some(reader);
        self
    }

    /// Override the drain timeout. Primarily a test seam — production
    /// should rely on the 2 s default.
    pub fn with_drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }
}

impl CutoverCoordinator for EngineCutoverCoordinator {
    fn drain_and_freeze<'a>(
        &'a self,
        req: &'a PartitionCutoverRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CutoverOffsets, AeonError>> + Send + 'a>> {
        Box::pin(async move {
            // No write-gate registered for (pipeline, partition) means
            // the pipeline isn't currently running on this node — either
            // it exited cleanly, was never started here, or stopped
            // between the partition transfer's BulkSync phase and the
            // Cutover handshake. There's nothing to drain (no live
            // source loop is writing) and no real offset to communicate,
            // so return the no-offset sentinel `final_source_offset =
            // -1` and continue the transfer rather than aborting it.
            //
            // Before [2026-04-25] this returned `AeonError::State`,
            // which surfaced as a remote cutover failure that aborted
            // the partition transfer. T3 drain testing surfaced the
            // race: a fast pipeline (T0 baseline at 1M events / ~1 s)
            // exits before the operator gets to type `aeon cluster
            // drain`, the supervisor cleans up the WriteGate, and the
            // first transfer attempt finds the gate missing and dies.
            // Mirrors the empty-response sentinel applied in
            // `aeon_engine::engine_providers::PohChainExportProvider`.
            let Some(gate) = self.gates.get(&req.pipeline, req.partition) else {
                tracing::debug!(
                    pipeline = %req.pipeline,
                    partition = req.partition.as_u16(),
                    "cutover: no write-gate (pipeline not running here); \
                     returning no-offset sentinel and continuing"
                );
                return Ok(CutoverOffsets {
                    final_source_offset: -1,
                    final_poh_sequence: 0,
                });
            };

            gate.request_freeze_and_drain(self.drain_timeout)
                .await
                .map_err(|e| {
                    AeonError::state(format!(
                        "cutover: drain failed for pipeline '{}' partition {}: {}",
                        req.pipeline,
                        req.partition.as_u16(),
                        e
                    ))
                })?;

            let offsets = match self.watermarks.as_ref() {
                Some(r) => r.read_watermarks(&req.pipeline, req.partition),
                None => CutoverOffsets {
                    final_source_offset: -1,
                    final_poh_sequence: 0,
                },
            };
            Ok(offsets)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Test reader that returns fixed watermarks for a single
    /// (pipeline, partition) key — captures request shape so tests can
    /// assert the coordinator passes the right key through.
    struct FixedReader {
        expected: (String, PartitionId),
        out: CutoverOffsets,
        seen: Mutex<Option<(String, PartitionId)>>,
    }
    impl CutoverWatermarkReader for FixedReader {
        fn read_watermarks(&self, pipeline: &str, partition: PartitionId) -> CutoverOffsets {
            *self.seen.lock().expect("lock") = Some((pipeline.to_string(), partition));
            assert_eq!(
                (pipeline, partition),
                (self.expected.0.as_str(), self.expected.1),
                "watermark reader called with unexpected key"
            );
            self.out
        }
    }

    fn req(pipeline: &str, partition: u16) -> PartitionCutoverRequest {
        PartitionCutoverRequest {
            pipeline: pipeline.to_string(),
            partition: PartitionId::new(partition),
        }
    }

    #[tokio::test]
    async fn drain_and_freeze_flips_gate_and_returns_sentinels_without_reader() {
        let gates = Arc::new(WriteGateRegistry::new());
        let _ = gates.get_or_create("pl", PartitionId::new(0));
        let coord = EngineCutoverCoordinator::new(Arc::clone(&gates));

        let offsets = coord
            .drain_and_freeze(&req("pl", 0))
            .await
            .expect("drain ok");
        assert_eq!(offsets.final_source_offset, -1);
        assert_eq!(offsets.final_poh_sequence, 0);
        // Gate must be Frozen after a successful cutover.
        let gate = gates.get("pl", PartitionId::new(0)).expect("present");
        assert_eq!(gate.state(), crate::write_gate::GateState::Frozen);
    }

    #[tokio::test]
    async fn drain_and_freeze_uses_watermark_reader_when_present() {
        let gates = Arc::new(WriteGateRegistry::new());
        let _ = gates.get_or_create("pl", PartitionId::new(3));
        let reader = Arc::new(FixedReader {
            expected: ("pl".to_string(), PartitionId::new(3)),
            out: CutoverOffsets {
                final_source_offset: 12_345,
                final_poh_sequence: 678,
            },
            seen: Mutex::new(None),
        });
        let coord =
            EngineCutoverCoordinator::new(Arc::clone(&gates)).with_watermarks(reader.clone());

        let offsets = coord
            .drain_and_freeze(&req("pl", 3))
            .await
            .expect("drain ok");
        assert_eq!(offsets.final_source_offset, 12_345);
        assert_eq!(offsets.final_poh_sequence, 678);
        assert_eq!(
            *reader.seen.lock().expect("lock"),
            Some(("pl".to_string(), PartitionId::new(3)))
        );
    }

    #[tokio::test]
    async fn missing_gate_returns_no_offset_sentinel() {
        // 2026-04-25: flipped from Err("no write-gate") to Ok(sentinel
        // offsets) so partition transfer doesn't abort when the
        // pipeline has exited / never started on this node — there's
        // nothing to drain and nothing to communicate. Mirrors the
        // empty-bytes sentinel in PohChainExportProvider.
        let gates = Arc::new(WriteGateRegistry::new());
        let coord = EngineCutoverCoordinator::new(gates);

        let offsets = coord
            .drain_and_freeze(&req("unknown", 0))
            .await
            .expect("missing gate must yield sentinel, not error");
        assert_eq!(
            offsets.final_source_offset, -1,
            "final_source_offset must be the no-offset sentinel"
        );
        assert_eq!(offsets.final_poh_sequence, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_timeout_surfaces_as_state_error() {
        let gates = Arc::new(WriteGateRegistry::new());
        let gate = gates.get_or_create("pl", PartitionId::new(0));
        // Hold a guard for the whole test — drain will never reach 0.
        let _guard = gate.try_enter().expect("open");

        let coord = EngineCutoverCoordinator::new(Arc::clone(&gates))
            .with_drain_timeout(Duration::from_millis(25));

        let err = coord
            .drain_and_freeze(&req("pl", 0))
            .await
            .expect_err("must time out");
        let msg = format!("{err}");
        assert!(msg.contains("drain failed"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sibling_partition_freeze_does_not_affect_others() {
        let gates = Arc::new(WriteGateRegistry::new());
        let gate0 = gates.get_or_create("pl", PartitionId::new(0));
        let _gate1 = gates.get_or_create("pl", PartitionId::new(1));
        let coord = EngineCutoverCoordinator::new(Arc::clone(&gates));

        coord
            .drain_and_freeze(&req("pl", 0))
            .await
            .expect("drain ok");
        assert_eq!(gate0.state(), crate::write_gate::GateState::Frozen);
        // Sibling still Open — unaffected by the cross-partition freeze.
        let g1 = gates.get("pl", PartitionId::new(1)).expect("present");
        assert_eq!(g1.state(), crate::write_gate::GateState::Open);
    }
}
