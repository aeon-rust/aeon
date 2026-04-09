//! Node discovery — static peer resolution and seed-based dynamic join.

use crate::config::ClusterConfig;
use crate::types::{NodeAddress, NodeId};

/// Resolve static peers from the cluster config.
/// Returns (node_id, address) pairs. Node IDs are assigned sequentially starting from 2.
pub fn resolve_peers(config: &ClusterConfig) -> Vec<(NodeId, NodeAddress)> {
    config
        .peers
        .iter()
        .enumerate()
        .map(|(i, addr)| {
            let node_id = (i as NodeId) + 2; // self is 1, peers start at 2
            (node_id, addr.clone())
        })
        .collect()
}

/// Build the initial membership map (self + peers).
pub fn initial_members(config: &ClusterConfig) -> Vec<(NodeId, NodeAddress)> {
    let mut members = vec![(
        config.node_id,
        NodeAddress::new(config.bind.ip().to_string(), config.bind.port()),
    )];

    for (id, addr) in resolve_peers(config) {
        members.push((id, addr));
    }

    members
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_peers_empty() {
        let config = ClusterConfig::single_node(1, 16);
        let peers = resolve_peers(&config);
        assert!(peers.is_empty());
    }

    #[test]
    fn resolve_peers_assigns_sequential_ids() {
        let config = ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4433".parse().unwrap(),
            num_partitions: 16,
            peers: vec![
                NodeAddress::new("10.0.0.2", 4433),
                NodeAddress::new("10.0.0.3", 4433),
            ],
            seed_nodes: Vec::new(),
            tls: None,
        };
        let peers = resolve_peers(&config);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].0, 2);
        assert_eq!(peers[1].0, 3);
    }

    #[test]
    fn initial_members_includes_self() {
        let config = ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4433".parse().unwrap(),
            num_partitions: 16,
            peers: vec![
                NodeAddress::new("10.0.0.2", 4433),
                NodeAddress::new("10.0.0.3", 4433),
            ],
            seed_nodes: Vec::new(),
            tls: None,
        };
        let members = initial_members(&config);
        assert_eq!(members.len(), 3);
        assert_eq!(members[0].0, 1); // self
    }
}
