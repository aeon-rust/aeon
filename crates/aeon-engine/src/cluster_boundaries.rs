//! G9.d.b — Per-partition sequence-bounded transition primitive.
//!
//! Bridges the `PartitionBoundaries` Raft-log payload (G9.d.a) onto the
//! engine-side per-partition primitives ([`crate::write_gate::WriteGate`]
//! and [`crate::partition_install::LivePohChainRegistry`]). After a
//! lifecycle command commits in Raft, every node calls
//! [`drain_partitions_at_seq`] to wait until each partition's PoH chain
//! has crossed its committed boundary, then freeze the per-partition
//! write gate so the in-stream cut-over applies at the same logical
//! point on every node.
//!
//! Two complementary surfaces:
//!
//! - [`compute_partition_boundaries`] — leader-side, pre-propose. Reads
//!   `LivePohChainRegistry::sequence()` for each partition the pipeline
//!   owns and returns a `PartitionBoundaries` map naming the seq at
//!   which the transition will apply.
//! - [`drain_partitions_at_seq`] — every-node, post-Raft-commit.
//!   Iterates the boundary map, awaits per-partition `chain.sequence()
//!   >= target_seq` (bounded by `wait_timeout`), then drives the
//!   existing `WriteGate::request_freeze_and_drain` for that partition.
//!
//! Empty boundary maps are a no-op on both surfaces — preserves G9.c
//! immediate-on-apply semantics for v0.1 log entries that don't carry
//! the new field.
//!
//! Feature-gated behind `cluster` + `processor-auth`, matching the
//! gates on `partition_install` (which owns `LivePohChainRegistry`) and
//! `engine_cutover` (which owns `WriteGateRegistry` indirectly through
//! the pipeline supervisor).

use std::sync::Arc;
use std::time::Duration;

use aeon_types::AeonError;
use aeon_types::partition::PartitionId;
use aeon_types::registry::PartitionBoundaries;

use crate::partition_install::LivePohChainRegistry;
use crate::write_gate::WriteGateRegistry;

/// Default deadline for the per-partition `chain.sequence() >= target`
/// poll loop. Conservative — production batches commit in tens of
/// milliseconds, but a stuck source loop shouldn't wedge a Raft applier
/// indefinitely. Surfaces as `AeonError::Timeout` so the caller can
/// log + continue rather than crash the cluster_applier task.
pub const DEFAULT_DRAIN_WAIT: Duration = Duration::from_secs(5);

/// Default poll cadence between `chain.sequence()` reads inside the
/// wait loop. Coarse enough that the lock-grab cost stays well off the
/// per-event hot path, fine enough that a typical commit latency
/// (single-digit ms) doesn't add observable handover delay.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Leader-side: compute the per-partition boundary map for a lifecycle
/// transition by sampling each partition's current PoH head sequence.
///
/// Called inside the REST handler before `propose_registry` so the
/// Raft entry carries the boundaries to every node in the same commit
/// — there's no race where one node reads a different sequence from
/// another. Partitions without a registered live chain (not running
/// here, transferred away, never started) are silently omitted; the
/// applier on the missing-chain pod will see no entry for that
/// partition and apply the transition immediately.
///
/// Returns an empty map iff `partitions` is empty.
pub async fn compute_partition_boundaries(
    pipeline: &str,
    partitions: &[PartitionId],
    poh: &LivePohChainRegistry,
) -> PartitionBoundaries {
    let mut out = PartitionBoundaries::new();
    for &partition in partitions {
        if let Some(chain_arc) = poh.get(pipeline, partition) {
            let seq = chain_arc.lock().await.sequence();
            out.insert(partition, seq);
        }
    }
    out
}

/// Every-node: drain each partition until its live PoH chain has
/// crossed the committed `target_seq`, then freeze the per-partition
/// `WriteGate`. Returns when all entries have either drained + frozen,
/// timed out, or had no chain registered (skipped — see below).
///
/// **Per-partition skip semantics:**
///
/// - If `LivePohChainRegistry` has no entry for `(pipeline, partition)`
///   the partition is **skipped** with a `tracing::debug!` — the
///   pipeline isn't running here for that partition, so there's
///   nothing in this engine to drain.
/// - If `WriteGateRegistry` has no entry, the gate is **skipped** —
///   the source task already exited (or never started); freezing a
///   non-existent gate would not change observable behaviour.
/// - If the chain's sequence is already `>= target_seq` at first
///   read, the wait loop returns immediately (zero poll iterations).
///
/// **Timeout behaviour:** the per-partition wait is bounded by
/// `wait_timeout`. A timeout returns `AeonError::Timeout` and **does
/// not** attempt to freeze that partition's gate — leaving it open is
/// safer than closing a gate when we couldn't establish the boundary.
/// The caller (cluster_applier) logs + continues to the next variant.
///
/// Empty `boundaries` is a no-op (returns `Ok(())` immediately) —
/// preserves G9.c immediate-on-apply semantics for v0.1 log entries.
pub async fn drain_partitions_at_seq(
    pipeline: &str,
    boundaries: &PartitionBoundaries,
    poh: &LivePohChainRegistry,
    gates: Arc<WriteGateRegistry>,
    wait_timeout: Duration,
) -> Result<(), AeonError> {
    if boundaries.is_empty() {
        return Ok(());
    }
    for (&partition, &target_seq) in boundaries.iter() {
        let waited =
            wait_for_partition_seq(pipeline, partition, target_seq, poh, wait_timeout).await?;
        if !waited {
            // No live chain on this node — partition isn't running
            // here, so don't freeze a gate that doesn't correspond to
            // a real Raft-committed boundary on this engine.
            continue;
        }
        if let Some(gate) = gates.get(pipeline, partition) {
            gate.request_freeze_and_drain(wait_timeout).await.map_err(|e| {
                AeonError::state(format!(
                    "drain_partitions_at_seq: freeze failed for pipeline '{pipeline}' partition {}: {e}",
                    partition.as_u16()
                ))
            })?;
        } else {
            tracing::debug!(
                pipeline = %pipeline,
                partition = partition.as_u16(),
                "drain_partitions_at_seq: no write-gate (pipeline not writing here); skipping freeze"
            );
        }
    }
    Ok(())
}

/// Internal helper: poll until `chain.sequence() >= target_seq` or
/// `wait_timeout` elapses. Returns:
///
/// - `Ok(true)` — chain is registered locally and reached the target.
/// - `Ok(false)` — chain isn't registered locally (silent skip per
///   module docs); caller should also skip the per-partition freeze.
/// - `Err(AeonError::timeout(...))` — chain is registered but didn't
///   reach the target within `wait_timeout`.
async fn wait_for_partition_seq(
    pipeline: &str,
    partition: PartitionId,
    target_seq: u64,
    poh: &LivePohChainRegistry,
    wait_timeout: Duration,
) -> Result<bool, AeonError> {
    let Some(chain_arc) = poh.get(pipeline, partition) else {
        tracing::debug!(
            pipeline = %pipeline,
            partition = partition.as_u16(),
            target_seq,
            "drain_partitions_at_seq: no live PoH chain (pipeline not running here); skipping wait"
        );
        return Ok(false);
    };
    let deadline = tokio::time::Instant::now() + wait_timeout;
    loop {
        let observed = chain_arc.lock().await.sequence();
        if observed >= target_seq {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(AeonError::timeout(format!(
                "drain_partitions_at_seq: pipeline '{pipeline}' partition {} did not reach \
                 seq {target_seq} within {wait_timeout:?} (observed {observed})",
                partition.as_u16()
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_crypto::poh::PohChain;
    use tokio::sync::Mutex as TokioMutex;

    /// Test helper: build a fresh PoH chain for a partition.
    fn fresh_chain(partition: u16) -> Arc<TokioMutex<PohChain>> {
        Arc::new(TokioMutex::new(PohChain::new(
            PartitionId::new(partition),
            16,
        )))
    }

    /// Test helper: advance a chain by `n` batches so its sequence
    /// counter reaches exactly `n`. Each batch is a single payload so
    /// the recent_entries cap never matters here.
    async fn advance_chain(chain: &Arc<TokioMutex<PohChain>>, n: u64) {
        let mut c = chain.lock().await;
        for i in 0..n {
            let payloads: &[&[u8]] = &[b"event"];
            c.append_batch(payloads, i as i64, None)
                .expect("append_batch must accept non-empty payloads");
        }
        assert_eq!(c.sequence(), n);
    }

    /// Empty boundary map is a no-op on the drain side — preserves G9.c
    /// immediate-on-apply for v0.1 log entries.
    #[tokio::test]
    async fn empty_boundaries_drain_returns_immediately() {
        let poh = LivePohChainRegistry::new();
        let gates = Arc::new(WriteGateRegistry::new());
        let res = drain_partitions_at_seq(
            "p",
            &PartitionBoundaries::new(),
            &poh,
            gates,
            Duration::from_millis(50),
        )
        .await;
        assert!(res.is_ok());
    }

    /// Boundary that's already satisfied at first read returns without
    /// waiting; gate ends up `Frozen` afterwards.
    #[tokio::test]
    async fn already_at_target_seq_freezes_gate_immediately() {
        let poh = LivePohChainRegistry::new();
        let chain = fresh_chain(0);
        advance_chain(&chain, 3).await;
        poh.register("p", PartitionId::new(0), chain.clone())
            .expect("register");
        let gates = Arc::new(WriteGateRegistry::new());
        let _ = gates.get_or_create("p", PartitionId::new(0));

        let mut bounds = PartitionBoundaries::new();
        bounds.insert(PartitionId::new(0), 2);
        let res =
            drain_partitions_at_seq("p", &bounds, &poh, gates.clone(), Duration::from_secs(1))
                .await;
        assert!(res.is_ok(), "drain should succeed: {res:?}");
        let gate = gates.get("p", PartitionId::new(0)).expect("gate present");
        assert_eq!(gate.state(), crate::write_gate::GateState::Frozen);
    }

    /// Wait loop blocks until the chain catches up, then freezes.
    /// Multi-thread runtime so the appender task can advance the chain
    /// while the drain future polls it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waits_for_chain_to_reach_target_then_freezes() {
        let poh = LivePohChainRegistry::new();
        let chain = fresh_chain(0);
        poh.register("p", PartitionId::new(0), chain.clone())
            .expect("register");
        let gates = Arc::new(WriteGateRegistry::new());
        let _ = gates.get_or_create("p", PartitionId::new(0));

        // Appender advances the chain after a short delay so the drain's
        // wait loop has to do at least one poll round.
        let chain_writer = chain.clone();
        let appender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            advance_chain(&chain_writer, 5).await;
        });

        let mut bounds = PartitionBoundaries::new();
        bounds.insert(PartitionId::new(0), 5);
        let res =
            drain_partitions_at_seq("p", &bounds, &poh, gates.clone(), Duration::from_secs(2))
                .await;
        assert!(res.is_ok(), "drain should succeed: {res:?}");
        appender.await.expect("appender join");

        let gate = gates.get("p", PartitionId::new(0)).expect("gate");
        assert_eq!(gate.state(), crate::write_gate::GateState::Frozen);
    }

    /// Chain that never reaches the target surfaces a timeout error
    /// and does NOT freeze the gate (open is safer than wrong-state
    /// closed when we couldn't establish the boundary).
    #[tokio::test]
    async fn target_unreachable_within_budget_returns_timeout_and_leaves_gate_open() {
        let poh = LivePohChainRegistry::new();
        let chain = fresh_chain(0);
        poh.register("p", PartitionId::new(0), chain)
            .expect("register");
        let gates = Arc::new(WriteGateRegistry::new());
        let gate = gates.get_or_create("p", PartitionId::new(0));

        let mut bounds = PartitionBoundaries::new();
        bounds.insert(PartitionId::new(0), 99);
        let err =
            drain_partitions_at_seq("p", &bounds, &poh, gates.clone(), Duration::from_millis(40))
                .await
                .expect_err("must time out");
        let msg = format!("{err}");
        assert!(msg.contains("did not reach seq 99"), "got: {msg}");
        // Gate must still be Open — partial cutover state would be worse
        // than no cutover.
        assert_eq!(gate.state(), crate::write_gate::GateState::Open);
    }

    /// Partition with no registered live chain on this node is silently
    /// skipped (the pipeline isn't running here for that partition);
    /// drain returns Ok and the gate (if any) is left untouched.
    #[tokio::test]
    async fn missing_chain_skips_partition_silently() {
        let poh = LivePohChainRegistry::new();
        let gates = Arc::new(WriteGateRegistry::new());
        let gate = gates.get_or_create("p", PartitionId::new(0));

        let mut bounds = PartitionBoundaries::new();
        bounds.insert(PartitionId::new(0), 100);
        let res =
            drain_partitions_at_seq("p", &bounds, &poh, gates.clone(), Duration::from_secs(1))
                .await;
        assert!(res.is_ok(), "missing chain skips, doesn't error: {res:?}");
        assert_eq!(gate.state(), crate::write_gate::GateState::Open);
    }

    /// Multi-partition: every entry in the boundary map is honoured;
    /// the drain returns when every partition has either frozen or
    /// been skipped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_partition_drain_freezes_all_present_gates() {
        let poh = LivePohChainRegistry::new();
        let gates = Arc::new(WriteGateRegistry::new());
        for p in [0_u16, 1, 2] {
            let chain = fresh_chain(p);
            advance_chain(&chain, p as u64 + 1).await;
            poh.register("p", PartitionId::new(p), chain).expect("reg");
            gates.get_or_create("p", PartitionId::new(p));
        }
        let mut bounds = PartitionBoundaries::new();
        bounds.insert(PartitionId::new(0), 1);
        bounds.insert(PartitionId::new(1), 2);
        bounds.insert(PartitionId::new(2), 3);

        let res =
            drain_partitions_at_seq("p", &bounds, &poh, gates.clone(), Duration::from_secs(2))
                .await;
        assert!(res.is_ok(), "multi-partition drain ok: {res:?}");
        for p in [0_u16, 1, 2] {
            let g = gates.get("p", PartitionId::new(p)).expect("gate");
            assert_eq!(g.state(), crate::write_gate::GateState::Frozen, "p={p}");
        }
    }

    /// Leader-side compute reads each partition's current PoH head into
    /// the boundary map; partitions without a chain registered are
    /// silently omitted.
    #[tokio::test]
    async fn compute_partition_boundaries_samples_present_chains_only() {
        let poh = LivePohChainRegistry::new();
        let chain0 = fresh_chain(0);
        let chain2 = fresh_chain(2);
        advance_chain(&chain0, 2).await;
        advance_chain(&chain2, 1).await;
        poh.register("p", PartitionId::new(0), chain0).expect("r0");
        poh.register("p", PartitionId::new(2), chain2).expect("r2");

        let bounds = compute_partition_boundaries(
            "p",
            &[
                PartitionId::new(0),
                PartitionId::new(1),
                PartitionId::new(2),
            ],
            &poh,
        )
        .await;
        // Partition 1 has no chain — silently omitted.
        assert_eq!(bounds.len(), 2);
        assert_eq!(bounds.get(&PartitionId::new(0)), Some(&2));
        assert_eq!(bounds.get(&PartitionId::new(2)), Some(&1));
        assert!(bounds.get(&PartitionId::new(1)).is_none());
    }

    /// Empty partition slice → empty boundary map.
    #[tokio::test]
    async fn compute_partition_boundaries_empty_partitions_yields_empty_map() {
        let poh = LivePohChainRegistry::new();
        let bounds = compute_partition_boundaries("p", &[], &poh).await;
        assert!(bounds.is_empty());
    }
}
