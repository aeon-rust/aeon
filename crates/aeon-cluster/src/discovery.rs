//! Node discovery — static peers, Kubernetes DNS, and seed-based join.
//!
//! Three discovery modes:
//! 1. **Static peers**: peer addresses listed in config (bare metal / VM)
//! 2. **Kubernetes DNS**: headless Service → pod DNS names (K8s StatefulSet)
//! 3. **Seed nodes**: contact a seed to join an existing cluster (dynamic)

use crate::config::ClusterConfig;
use crate::types::{NodeAddress, NodeId};

// ── Static peer discovery ───────────────────────────────────────────

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

// ── Kubernetes StatefulSet discovery ─────────────────────────────────

/// Derive a Raft node ID from a Kubernetes StatefulSet pod name.
///
/// Pod names follow the pattern `{statefulset}-{ordinal}` (e.g., `aeon-0`, `aeon-2`).
/// Node IDs are ordinal + 1 (Raft requires node_id > 0).
///
/// Returns None if the pod name doesn't end with a numeric ordinal.
pub fn node_id_from_pod_name(pod_name: &str) -> Option<NodeId> {
    let ordinal_str = pod_name.rsplit('-').next()?;
    let ordinal: u64 = ordinal_str.parse().ok()?;
    Some(ordinal + 1) // Raft node IDs start at 1
}

/// Build peer DNS names for a Kubernetes headless Service StatefulSet.
///
/// Given a StatefulSet with `replicas` pods, the peer DNS names are:
/// `{name}-{ordinal}.{service}.{namespace}.svc.cluster.local:{port}`
///
/// Returns (node_id, address) for all pods EXCEPT `self_ordinal`.
pub fn k8s_peers(
    statefulset_name: &str,
    service_name: &str,
    namespace: &str,
    replicas: u32,
    quic_port: u16,
    self_ordinal: u32,
) -> Vec<(NodeId, NodeAddress)> {
    (0..replicas)
        .filter(|&i| i != self_ordinal)
        .map(|i| {
            let node_id = (i as NodeId) + 1;
            let hostname = format!(
                "{statefulset_name}-{i}.{service_name}.{namespace}.svc.cluster.local"
            );
            (node_id, NodeAddress::new(hostname, quic_port))
        })
        .collect()
}

/// Build the full membership map for a Kubernetes StatefulSet cluster.
///
/// Includes self + all peers. Uses headless Service DNS for addressing.
pub fn k8s_members(
    statefulset_name: &str,
    service_name: &str,
    namespace: &str,
    replicas: u32,
    quic_port: u16,
) -> Vec<(NodeId, NodeAddress)> {
    (0..replicas)
        .map(|i| {
            let node_id = (i as NodeId) + 1;
            let hostname = format!(
                "{statefulset_name}-{i}.{service_name}.{namespace}.svc.cluster.local"
            );
            (node_id, NodeAddress::new(hostname, quic_port))
        })
        .collect()
}

/// Parse Kubernetes environment variables for cluster discovery.
///
/// Expected env vars (set by the StatefulSet template):
/// - `AEON_POD_NAME`: e.g., "aeon-0"
/// - `AEON_NAMESPACE`: e.g., "default"
/// - `AEON_CLUSTER_SERVICE`: e.g., "aeon-headless"
/// - `AEON_CLUSTER_REPLICAS`: e.g., "3"
/// - `AEON_CLUSTER_QUIC_PORT`: e.g., "4470"
/// - `AEON_CLUSTER_PARTITIONS`: e.g., "16"
///
/// Returns None if any required env var is missing or unparseable.
pub fn from_k8s_env() -> Option<K8sDiscovery> {
    let pod_name = std::env::var("AEON_POD_NAME").ok()?;
    let namespace = std::env::var("AEON_NAMESPACE").ok()?;
    let service = std::env::var("AEON_CLUSTER_SERVICE").ok()?;
    let replicas: u32 = std::env::var("AEON_CLUSTER_REPLICAS").ok()?.parse().ok()?;
    let quic_port: u16 = std::env::var("AEON_CLUSTER_QUIC_PORT").ok()?.parse().ok()?;
    let partitions: u16 = std::env::var("AEON_CLUSTER_PARTITIONS").ok()?.parse().ok()?;
    let node_id = node_id_from_pod_name(&pod_name)?;

    // Extract StatefulSet name (pod name without the ordinal suffix)
    let statefulset_name = pod_name.rsplit_once('-')?.0.to_string();
    let ordinal: u32 = pod_name.rsplit('-').next()?.parse().ok()?;

    Some(K8sDiscovery {
        pod_name,
        namespace,
        service,
        statefulset_name,
        replicas,
        quic_port,
        partitions,
        node_id,
        ordinal,
    })
}

/// Parsed Kubernetes discovery configuration.
#[derive(Debug, Clone)]
pub struct K8sDiscovery {
    pub pod_name: String,
    pub namespace: String,
    pub service: String,
    pub statefulset_name: String,
    pub replicas: u32,
    pub quic_port: u16,
    pub partitions: u16,
    pub node_id: NodeId,
    pub ordinal: u32,
}

impl K8sDiscovery {
    /// Get all cluster members (including self) as (node_id, address) pairs.
    pub fn members(&self) -> Vec<(NodeId, NodeAddress)> {
        k8s_members(
            &self.statefulset_name,
            &self.service,
            &self.namespace,
            self.replicas,
            self.quic_port,
        )
    }

    /// Get peer members (excluding self) as (node_id, address) pairs.
    pub fn peers(&self) -> Vec<(NodeId, NodeAddress)> {
        k8s_peers(
            &self.statefulset_name,
            &self.service,
            &self.namespace,
            self.replicas,
            self.quic_port,
            self.ordinal,
        )
    }

    /// Build a ClusterConfig from the discovered K8s environment.
    pub fn to_cluster_config(&self) -> ClusterConfig {
        let peers: Vec<NodeAddress> = self.peers().into_iter().map(|(_, addr)| addr).collect();
        let initial_members = self.members();

        // FT-10: formatting "0.0.0.0:{u16}" always produces a valid SocketAddr.
        #[allow(clippy::unwrap_used)]
        let bind = format!("0.0.0.0:{}", self.quic_port).parse().unwrap();
        ClusterConfig {
            node_id: self.node_id,
            bind,
            num_partitions: self.partitions,
            peers,
            seed_nodes: Vec::new(),
            tls: None, // File-based TLS configured separately via Helm values
            auto_tls: true, // Use ephemeral self-signed certs for dev/testing
            initial_members,
            advertise_addr: None,
            raft_timing: crate::config::RaftTiming::default(),
        }
    }
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
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: crate::config::RaftTiming::default(),
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
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: crate::config::RaftTiming::default(),
        };
        let members = initial_members(&config);
        assert_eq!(members.len(), 3);
        assert_eq!(members[0].0, 1); // self
    }

    // ── Kubernetes discovery tests ──────────────────────────────────

    #[test]
    fn node_id_from_pod_name_valid() {
        assert_eq!(node_id_from_pod_name("aeon-0"), Some(1));
        assert_eq!(node_id_from_pod_name("aeon-1"), Some(2));
        assert_eq!(node_id_from_pod_name("aeon-2"), Some(3));
        assert_eq!(node_id_from_pod_name("my-release-aeon-5"), Some(6));
    }

    #[test]
    fn node_id_from_pod_name_invalid() {
        assert_eq!(node_id_from_pod_name("aeon"), None);
        assert_eq!(node_id_from_pod_name(""), None);
        assert_eq!(node_id_from_pod_name("aeon-abc"), None);
    }

    #[test]
    fn k8s_peers_excludes_self() {
        let peers = k8s_peers("aeon", "aeon-headless", "default", 3, 4470, 0);
        assert_eq!(peers.len(), 2);
        // Self is ordinal 0 (node_id 1), peers are ordinals 1 and 2
        assert_eq!(peers[0].0, 2); // ordinal 1 → node_id 2
        assert_eq!(peers[1].0, 3); // ordinal 2 → node_id 3
        assert!(peers[0].1.to_string().contains("aeon-1.aeon-headless"));
        assert!(peers[1].1.to_string().contains("aeon-2.aeon-headless"));
    }

    #[test]
    fn k8s_members_includes_all() {
        let members = k8s_members("aeon", "aeon-headless", "prod", 3, 4470);
        assert_eq!(members.len(), 3);
        assert_eq!(members[0].0, 1);
        assert_eq!(members[1].0, 2);
        assert_eq!(members[2].0, 3);

        // Verify FQDN format
        assert!(members[0]
            .1
            .to_string()
            .contains("aeon-0.aeon-headless.prod.svc.cluster.local"));
    }

    #[test]
    fn k8s_discovery_to_config() {
        let disc = K8sDiscovery {
            pod_name: "aeon-1".to_string(),
            namespace: "default".to_string(),
            service: "aeon-headless".to_string(),
            statefulset_name: "aeon".to_string(),
            replicas: 3,
            quic_port: 4470,
            partitions: 16,
            node_id: 2,
            ordinal: 1,
        };

        let config = disc.to_cluster_config();
        assert_eq!(config.node_id, 2);
        assert_eq!(config.num_partitions, 16);
        assert_eq!(config.peers.len(), 2); // 3 replicas - 1 self = 2 peers
    }
}
