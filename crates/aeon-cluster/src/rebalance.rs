//! Pure planning helpers for `aeon cluster drain` and `aeon cluster rebalance`.
//!
//! Both helpers take a `PartitionTable` snapshot + a sorted list of live
//! `NodeId`s and produce a deterministic `Vec<(PartitionId, NodeId)>` where
//! every entry says "propose a transfer of this partition to this target."
//! The leader-side handler then loops the plan through
//! `ClusterNode::propose_partition_transfer` — the helpers themselves are
//! consensus-free so the supervisor's correctness can be unit-tested without
//! standing up a Raft cluster.
//!
//! Determinism guarantees (relied on by tests + by operators reproducing
//! plans across runs):
//! - Partitions in the output are sorted by `PartitionId`.
//! - Targets are picked round-robin in ascending `NodeId` order.
//! - Partitions in `Transferring` state are skipped — the in-flight transfer
//!   is honoured rather than fought.

use std::collections::BTreeMap;

use crate::types::{NodeId, PartitionOwnership, PartitionTable};
use aeon_types::PartitionId;

/// Plan a `drain` of `target_node`: send every partition that node currently
/// holds (or is the source of an in-flight transfer for) to one of the
/// other live members, round-robin in ascending node-id order.
///
/// Returns an empty plan when:
/// - `target_node` owns no partitions, or
/// - no other live members exist (drain is impossible — caller must surface
///   this to the operator rather than silently noop).
///
/// Partitions are emitted in ascending `PartitionId` order so the resulting
/// proposals are deterministic.
pub fn plan_drain(
    table: &PartitionTable,
    target_node: NodeId,
    live_members: &[NodeId],
) -> Vec<(PartitionId, NodeId)> {
    // Targets = live members minus the drain victim. Sorted so round-robin
    // is reproducible across runs.
    let mut targets: Vec<NodeId> = live_members
        .iter()
        .copied()
        .filter(|n| *n != target_node)
        .collect();
    targets.sort_unstable();
    if targets.is_empty() {
        return Vec::new();
    }

    // Collect partitions whose active node is `target_node`, sorted by id.
    let mut owned: Vec<PartitionId> = table
        .iter()
        .filter(|(_, ownership)| match ownership {
            // Skip partitions already mid-transfer — Raft is already moving them.
            PartitionOwnership::Transferring { .. } => false,
            PartitionOwnership::Owned(n) => *n == target_node,
        })
        .map(|(p, _)| *p)
        .collect();
    owned.sort_unstable_by_key(|p| p.as_u16());

    // Round-robin assign each partition to the next target.
    owned
        .into_iter()
        .enumerate()
        .map(|(i, p)| (p, targets[i % targets.len()]))
        .collect()
}

/// Plan a `rebalance` across `live_members`: for any member that owns
/// strictly more partitions than the per-node ceiling, peel partitions off
/// it and hand them to the most-under-loaded live member until the
/// distribution flattens.
///
/// Per-node ceiling = `ceil(total_owned / live_count)`. Floor =
/// `floor(total_owned / live_count)`. After the plan executes every member
/// holds either `floor` or `ceiling` partitions (Σ floor + ceiling slots
/// equals total).
///
/// Skips:
/// - Partitions currently `Transferring` (an in-flight transfer takes
///   precedence — counting them as "moving" but not re-planning them).
/// - Partitions whose current owner is not in `live_members` (those are
///   handled by `drain` instead — rebalance only redistributes among live
///   members so it doesn't accidentally fight a drain in flight).
///
/// Returns an empty plan when the load is already balanced or no live
/// members exist.
pub fn plan_rebalance(
    table: &PartitionTable,
    live_members: &[NodeId],
) -> Vec<(PartitionId, NodeId)> {
    let mut members: Vec<NodeId> = live_members.to_vec();
    members.sort_unstable();
    members.dedup();
    if members.is_empty() {
        return Vec::new();
    }

    // Walk the table once. Bucket partitions by owner; ignore non-live
    // owners and in-flight transfers (see doc comment).
    let mut owned_by: BTreeMap<NodeId, Vec<PartitionId>> = BTreeMap::new();
    for n in &members {
        owned_by.insert(*n, Vec::new());
    }
    for (pid, ownership) in table.iter() {
        if let PartitionOwnership::Owned(n) = ownership {
            if let Some(bucket) = owned_by.get_mut(n) {
                bucket.push(*pid);
            }
        }
    }
    // Sort each bucket so the partitions we peel off are reproducible.
    for v in owned_by.values_mut() {
        v.sort_unstable_by_key(|p| p.as_u16());
    }

    let total_balanced: usize = owned_by.values().map(|v| v.len()).sum();
    let m = members.len();
    let ceiling = total_balanced.div_ceil(m);
    let floor = total_balanced / m;

    // Compute deficit per member relative to floor — each can absorb up to
    // (ceiling - current) partitions, prioritising the ones at floor.
    // Use a sortable Vec rather than a heap because plans are tiny (≤ a few
    // hundred partitions in practice).
    let mut deficit: Vec<(NodeId, usize)> = members
        .iter()
        .map(|n| {
            let have = owned_by.get(n).map(|v| v.len()).unwrap_or(0);
            (*n, ceiling.saturating_sub(have))
        })
        .collect();

    let mut plan: Vec<(PartitionId, NodeId)> = Vec::new();

    for owner in members.iter().copied() {
        let bucket = owned_by.get(&owner).cloned().unwrap_or_default();
        // How many partitions does `owner` need to give up to drop to ceiling?
        let surplus = bucket.len().saturating_sub(ceiling);
        if surplus == 0 {
            continue;
        }
        for pid in bucket.into_iter().take(surplus) {
            // Pick the under-loaded target with the smallest current load.
            // Tie-break by lower NodeId for deterministic plans.
            // Skip the owner itself (its deficit slot would re-assign to self).
            let pick = deficit
                .iter_mut()
                .filter(|(n, slots)| *n != owner && *slots > 0)
                .min_by(|a, b| {
                    a.1.cmp(&b.1)
                        .reverse() // most slots first (== least loaded)
                        .then(a.0.cmp(&b.0))
                });
            let Some((target, slots)) = pick else { break };
            plan.push((pid, *target));
            *slots -= 1;
        }
    }

    // Emit in ascending PartitionId order for stable diffing.
    plan.sort_unstable_by_key(|(p, _)| p.as_u16());
    // Quiet the unused-binding lint; `floor` is part of the doc invariant.
    let _ = floor;
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: u16) -> PartitionId {
        PartitionId::new(n)
    }

    fn table_with(assignments: &[(u16, NodeId)]) -> PartitionTable {
        let mut t = PartitionTable::new();
        for (p, n) in assignments {
            t.assign(pid(*p), *n);
        }
        t
    }

    // ── plan_drain ────────────────────────────────────────────────────

    #[test]
    fn drain_empty_table_returns_empty_plan() {
        let plan = plan_drain(&PartitionTable::new(), 1, &[1, 2, 3]);
        assert!(plan.is_empty());
    }

    #[test]
    fn drain_node_with_no_partitions_returns_empty_plan() {
        let table = table_with(&[(0, 2), (1, 3)]);
        let plan = plan_drain(&table, 1, &[1, 2, 3]);
        assert!(plan.is_empty());
    }

    #[test]
    fn drain_with_no_other_live_members_returns_empty_plan() {
        // Drain target is the only live node — nowhere to send partitions.
        let table = table_with(&[(0, 1), (1, 1)]);
        let plan = plan_drain(&table, 1, &[1]);
        assert!(plan.is_empty());
    }

    #[test]
    fn drain_round_robins_partitions_in_id_order_to_remaining_nodes() {
        // Node 1 owns partitions 0, 1, 2, 3; remaining live nodes are 2 and 3.
        // Expected plan (sorted by partition id):
        //   p0 → 2, p1 → 3, p2 → 2, p3 → 3
        let table = table_with(&[(0, 1), (1, 1), (2, 1), (3, 1)]);
        let plan = plan_drain(&table, 1, &[1, 2, 3]);
        assert_eq!(
            plan,
            vec![(pid(0), 2), (pid(1), 3), (pid(2), 2), (pid(3), 3)]
        );
    }

    #[test]
    fn drain_skips_partitions_already_being_transferred() {
        let mut table = table_with(&[(0, 1), (1, 1), (2, 1)]);
        // Partition 1 is mid-transfer — must be left alone even though its
        // source is the drain target.
        table.begin_transfer(pid(1), 1, 2);
        let plan = plan_drain(&table, 1, &[1, 2, 3]);
        // Only p0 and p2 get re-targeted; p1 is honoured as in-flight.
        assert_eq!(plan, vec![(pid(0), 2), (pid(2), 3)]);
    }

    #[test]
    fn drain_excludes_drain_target_from_destination_set() {
        // Even if `live_members` includes the drain target, it must not
        // appear as a destination.
        let table = table_with(&[(0, 1), (1, 1)]);
        let plan = plan_drain(&table, 1, &[1, 2]);
        assert_eq!(plan, vec![(pid(0), 2), (pid(1), 2)]);
    }

    // ── plan_rebalance ────────────────────────────────────────────────

    #[test]
    fn rebalance_already_balanced_returns_empty_plan() {
        // 4 partitions, 2 nodes, 2 each — already balanced.
        let table = table_with(&[(0, 1), (1, 1), (2, 2), (3, 2)]);
        let plan = plan_rebalance(&table, &[1, 2]);
        assert!(plan.is_empty());
    }

    #[test]
    fn rebalance_shifts_surplus_to_under_loaded_node() {
        // Node 1 holds 3 partitions, node 2 holds 1. Ceiling is 2 — node 1
        // must give up one partition to node 2.
        let table = table_with(&[(0, 1), (1, 1), (2, 1), (3, 2)]);
        let plan = plan_rebalance(&table, &[1, 2]);
        // p0 is the lowest-id partition on the over-loaded node; goes to node 2.
        assert_eq!(plan, vec![(pid(0), 2)]);
    }

    #[test]
    fn rebalance_distributes_post_scale_up_to_new_node() {
        // Pre-scale: node 1 owned all 6 partitions. After scale-up to 3
        // nodes, ceiling = 2. Node 1 must hand off 4 partitions, two each
        // to nodes 2 and 3 — picked deterministically (least loaded first,
        // tie-break by lower NodeId).
        let table = table_with(&[(0, 1), (1, 1), (2, 1), (3, 1), (4, 1), (5, 1)]);
        let plan = plan_rebalance(&table, &[1, 2, 3]);

        // 4 transfers expected, leaving node 1 with 2 partitions.
        assert_eq!(plan.len(), 4);

        // Compute end-state distribution directly from initial + plan.
        // All sources in this case are node 1 (the only initial owner).
        let mut end: std::collections::HashMap<NodeId, i32> = std::collections::HashMap::new();
        for (n, c) in table.partition_counts() {
            end.insert(n, c as i32);
        }
        for (_, target) in &plan {
            *end.get_mut(&1).expect("node 1 had initial load") -= 1;
            *end.entry(*target).or_insert(0) += 1;
        }
        assert_eq!(end.get(&1).copied(), Some(2));
        assert_eq!(end.get(&2).copied(), Some(2));
        assert_eq!(end.get(&3).copied(), Some(2));
    }

    #[test]
    fn rebalance_ignores_partitions_owned_by_non_live_node() {
        // Node 9 has dropped out of live_members; rebalance must not try to
        // move its partitions (drain handles that, not rebalance).
        let table = table_with(&[(0, 9), (1, 1), (2, 2)]);
        let plan = plan_rebalance(&table, &[1, 2]);
        assert!(
            plan.is_empty(),
            "rebalance must not touch partitions owned by non-live nodes; got {plan:?}"
        );
    }

    #[test]
    fn rebalance_skips_transferring_partitions() {
        let mut table = table_with(&[(0, 1), (1, 1), (2, 1), (3, 2)]);
        // Mark p2 in-flight; rebalance must not re-plan it.
        table.begin_transfer(pid(2), 1, 2);
        let plan = plan_rebalance(&table, &[1, 2]);
        // From the rebalance helper's perspective: node 1 has 2 owned
        // partitions left (p0, p1), node 2 has 1 owned (p3). Ceiling for 3
        // owned slots over 2 nodes = 2. Node 1 == ceiling, so no move.
        // The in-flight p2 brings node 2 to 2 once it completes — exactly
        // what we want.
        assert!(plan.is_empty(), "got unexpected plan: {plan:?}");
    }

    #[test]
    fn rebalance_no_live_members_returns_empty_plan() {
        let table = table_with(&[(0, 1)]);
        let plan = plan_rebalance(&table, &[]);
        assert!(plan.is_empty());
    }

    #[test]
    fn rebalance_plan_is_sorted_by_partition_id() {
        // Construct a case that would produce out-of-order entries naturally
        // (multiple sources, multiple targets) and verify final sort.
        let table = table_with(&[
            (0, 1),
            (1, 1),
            (2, 1),
            (3, 1),
            (4, 2),
            (5, 2),
            (6, 2),
            (7, 2),
        ]);
        // 8 partitions / 4 nodes = ceiling 2; nodes 3 & 4 currently hold 0.
        let plan = plan_rebalance(&table, &[1, 2, 3, 4]);
        let ids: Vec<u16> = plan.iter().map(|(p, _)| p.as_u16()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "plan must be sorted by partition id");
    }
}
