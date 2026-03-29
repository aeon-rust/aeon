//! Partition assignment, rebalancing, and ownership queries.

use std::collections::HashMap;

use aeon_types::PartitionId;

use crate::types::{NodeId, PartitionTable};

/// Compute an initial round-robin partition assignment.
pub fn initial_assignment(num_partitions: u16, nodes: &[NodeId]) -> Vec<(PartitionId, NodeId)> {
    assert!(!nodes.is_empty(), "at least one node required");
    (0..num_partitions)
        .map(|i| {
            let node = nodes[i as usize % nodes.len()];
            (PartitionId::new(i), node)
        })
        .collect()
}

/// Get all partitions actively owned by (or served by) a given node.
pub fn my_partitions(table: &PartitionTable, node_id: NodeId) -> Vec<PartitionId> {
    table.partitions_for_node(node_id)
}

/// Compute the minimum set of partition moves to rebalance across `nodes`.
///
/// Returns `(partition, from_node, to_node)` for each move.
/// Algorithm: greedy — move partitions from overloaded to underloaded nodes.
pub fn compute_rebalance(
    table: &PartitionTable,
    nodes: &[NodeId],
) -> Vec<(PartitionId, NodeId, NodeId)> {
    if nodes.is_empty() || table.num_partitions() == 0 {
        return Vec::new();
    }

    let total = table.num_partitions();
    let n = nodes.len();
    let base = total / n;
    let extra = total % n;

    // Target: first `extra` nodes get `base+1`, rest get `base`
    let mut targets: HashMap<NodeId, usize> = HashMap::new();
    for (i, &node) in nodes.iter().enumerate() {
        targets.insert(node, if i < extra { base + 1 } else { base });
    }

    // Current counts (only for nodes in the target set)
    let current_counts = table.partition_counts();

    // Identify over-assigned and under-assigned nodes
    let mut overloaded: Vec<(NodeId, usize)> = Vec::new(); // (node, excess)
    let mut underloaded: Vec<(NodeId, usize)> = Vec::new(); // (node, deficit)

    for &node in nodes {
        let current = current_counts.get(&node).copied().unwrap_or(0);
        let target = targets[&node];
        if current > target {
            overloaded.push((node, current - target));
        } else if current < target {
            underloaded.push((node, target - current));
        }
    }

    // Also, partitions on nodes NOT in the target set need to be moved
    let mut orphaned_partitions: Vec<(PartitionId, NodeId)> = Vec::new();
    for (partition, ownership) in table.iter() {
        let active_node = ownership.active_node();
        if !targets.contains_key(&active_node) {
            orphaned_partitions.push((*partition, active_node));
        }
    }

    let mut moves = Vec::new();

    // First: move orphaned partitions to underloaded nodes
    for (partition, from) in orphaned_partitions {
        if let Some(pos) = underloaded.iter().position(|(_, deficit)| *deficit > 0) {
            let (to, deficit) = &mut underloaded[pos];
            moves.push((partition, from, *to));
            *deficit -= 1;
        }
    }

    // Second: move partitions from overloaded to underloaded
    for (over_node, mut excess) in overloaded {
        if excess == 0 {
            continue;
        }

        // Find partitions on this node to move
        let partitions_on_node = table.partitions_for_node(over_node);
        for partition in partitions_on_node {
            if excess == 0 {
                break;
            }
            if let Some(pos) = underloaded.iter().position(|(_, deficit)| *deficit > 0) {
                let (to, deficit) = &mut underloaded[pos];
                moves.push((partition, over_node, *to));
                *deficit -= 1;
                excess -= 1;
            }
        }
    }

    moves
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_assignment_single_node() {
        let assignments = initial_assignment(4, &[1]);
        assert_eq!(assignments.len(), 4);
        for (_, node) in &assignments {
            assert_eq!(*node, 1);
        }
    }

    #[test]
    fn initial_assignment_round_robin() {
        let assignments = initial_assignment(6, &[1, 2, 3]);
        let mut counts = HashMap::new();
        for (_, node) in &assignments {
            *counts.entry(*node).or_insert(0usize) += 1;
        }
        assert_eq!(counts[&1], 2);
        assert_eq!(counts[&2], 2);
        assert_eq!(counts[&3], 2);
    }

    #[test]
    fn initial_assignment_uneven() {
        // 16 partitions across 3 nodes = [6, 5, 5]
        let assignments = initial_assignment(16, &[1, 2, 3]);
        let mut counts = HashMap::new();
        for (_, node) in &assignments {
            *counts.entry(*node).or_insert(0usize) += 1;
        }
        assert_eq!(counts[&1], 6);
        assert_eq!(counts[&2], 5);
        assert_eq!(counts[&3], 5);
    }

    #[test]
    fn my_partitions_returns_correct_set() {
        let table = PartitionTable::single_node(8, 1);
        let mine = my_partitions(&table, 1);
        assert_eq!(mine.len(), 8);
        assert_eq!(my_partitions(&table, 2).len(), 0);
    }

    #[test]
    fn rebalance_already_balanced() {
        let mut table = PartitionTable::new();
        table.assign(PartitionId::new(0), 1);
        table.assign(PartitionId::new(1), 2);
        table.assign(PartitionId::new(2), 3);

        let moves = compute_rebalance(&table, &[1, 2, 3]);
        assert!(moves.is_empty(), "already balanced, no moves needed");
    }

    #[test]
    fn rebalance_add_node() {
        // 4 partitions on 2 nodes → add a 3rd node
        let mut table = PartitionTable::new();
        table.assign(PartitionId::new(0), 1);
        table.assign(PartitionId::new(1), 1);
        table.assign(PartitionId::new(2), 2);
        table.assign(PartitionId::new(3), 2);

        let moves = compute_rebalance(&table, &[1, 2, 3]);

        // After rebalance: should be [2, 1, 1] or [1, 2, 1] or [1, 1, 2] — 1 move
        assert!(!moves.is_empty());
        assert!(moves.len() <= 2, "should minimize moves");

        // Verify target gets at least 1 partition
        let move_targets: Vec<NodeId> = moves.iter().map(|(_, _, to)| *to).collect();
        assert!(
            move_targets.contains(&3),
            "new node should receive partitions"
        );
    }

    #[test]
    fn rebalance_remove_node() {
        // 6 partitions on 3 nodes → remove node 3
        let mut table = PartitionTable::new();
        for i in 0..6 {
            table.assign(PartitionId::new(i), (i as u64 % 3) + 1);
        }

        let moves = compute_rebalance(&table, &[1, 2]); // node 3 removed

        // Node 3's partitions should be moved to nodes 1 and 2
        for (_, from, _) in &moves {
            assert_eq!(*from, 3, "only node 3 should lose partitions");
        }
        assert_eq!(moves.len(), 2, "node 3 has 2 partitions to redistribute");
    }

    #[test]
    fn rebalance_minimize_moves() {
        // 16 partitions on 3 nodes, add 4th
        let mut table = PartitionTable::new();
        for i in 0..16 {
            table.assign(PartitionId::new(i), (i as u64 % 3) + 1);
        }

        let moves = compute_rebalance(&table, &[1, 2, 3, 4]);
        // Target: [4, 4, 4, 4] — node 4 needs 4 partitions
        // Current: [6, 5, 5, 0]
        // Optimal: move 2 from node 1, 1 from node 2, 1 from node 3 → 4 moves
        assert_eq!(moves.len(), 4, "should move exactly 4 partitions to node 4");

        let to_node4 = moves.iter().filter(|(_, _, to)| *to == 4).count();
        assert_eq!(to_node4, 4, "all moves should go to node 4");
    }

    #[test]
    fn rebalance_empty_table() {
        let table = PartitionTable::new();
        let moves = compute_rebalance(&table, &[1, 2, 3]);
        assert!(moves.is_empty());
    }

    #[test]
    fn rebalance_no_nodes() {
        let table = PartitionTable::single_node(4, 1);
        let moves = compute_rebalance(&table, &[]);
        assert!(moves.is_empty());
    }
}
