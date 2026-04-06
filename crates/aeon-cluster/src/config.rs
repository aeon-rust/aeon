//! Cluster configuration and validation.

use std::net::SocketAddr;
use std::path::PathBuf;

use aeon_types::AeonError;
use serde::{Deserialize, Serialize};

use crate::types::NodeAddress;

/// TLS certificate paths for mTLS between cluster nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Path to the node's certificate (PEM).
    pub cert: PathBuf,
    /// Path to the node's private key (PEM).
    pub key: PathBuf,
    /// Path to the CA certificate for verifying peers (PEM).
    pub ca: PathBuf,
}

/// Complete cluster configuration, matching the YAML manifest schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// Unique node ID within the cluster.
    pub node_id: u64,
    /// Address to bind the QUIC listener.
    pub bind: SocketAddr,
    /// Total number of partitions (immutable after cluster creation).
    pub num_partitions: u16,
    /// Static peer addresses for initial cluster bootstrap.
    #[serde(default)]
    pub peers: Vec<NodeAddress>,
    /// Seed node addresses for dynamic cluster join.
    #[serde(default)]
    pub seed_nodes: Vec<NodeAddress>,
    /// TLS configuration (None = insecure dev mode).
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

impl ClusterConfig {
    /// Create a minimal single-node config for development/testing.
    pub fn single_node(node_id: u64, num_partitions: u16) -> Self {
        Self {
            node_id,
            bind: "0.0.0.0:4470".parse().unwrap(),
            num_partitions,
            peers: Vec::new(),
            seed_nodes: Vec::new(),
            tls: None,
        }
    }

    /// True if this node has no peers or seed nodes (standalone).
    pub fn is_single_node(&self) -> bool {
        self.peers.is_empty() && self.seed_nodes.is_empty()
    }

    /// Expected initial cluster size (self + peers).
    pub fn initial_cluster_size(&self) -> usize {
        1 + self.peers.len()
    }

    /// Validate the configuration. Returns Err on invalid settings.
    pub fn validate(&self) -> Result<(), AeonError> {
        if self.num_partitions == 0 {
            return Err(AeonError::Config {
                message: "num_partitions must be > 0".to_string(),
            });
        }

        if self.node_id == 0 {
            return Err(AeonError::Config {
                message: "node_id must be > 0".to_string(),
            });
        }

        // Check for duplicate peers
        for (i, a) in self.peers.iter().enumerate() {
            for b in &self.peers[i + 1..] {
                if a == b {
                    return Err(AeonError::Config {
                        message: format!("duplicate peer address: {a}"),
                    });
                }
            }
        }

        // Multi-node clusters require TLS — plaintext inter-node traffic is not allowed
        if !self.is_single_node() && self.tls.is_none() {
            return Err(AeonError::Config {
                message: "multi-node clusters require TLS configuration. \
                          Set cluster.tls with cert, key, and ca paths"
                    .to_string(),
            });
        }

        // Odd-number enforcement for multi-node clusters
        let cluster_size = self.initial_cluster_size();
        if cluster_size > 1 && cluster_size % 2 == 0 {
            return Err(AeonError::Config {
                message: format!(
                    "cluster size must be odd (got {cluster_size}). \
                     Even-number clusters waste a node without improving fault tolerance"
                ),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_node_config_valid() {
        let cfg = ClusterConfig::single_node(1, 16);
        assert!(cfg.is_single_node());
        assert_eq!(cfg.initial_cluster_size(), 1);
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_zero_partitions() {
        let cfg = ClusterConfig::single_node(1, 0);
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("num_partitions"));
    }

    #[test]
    fn rejects_zero_node_id() {
        let cfg = ClusterConfig::single_node(0, 16);
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("node_id"));
    }

    fn test_tls() -> Option<TlsConfig> {
        Some(TlsConfig {
            cert: PathBuf::from("/etc/aeon/tls/node.pem"),
            key: PathBuf::from("/etc/aeon/tls/node.key"),
            ca: PathBuf::from("/etc/aeon/tls/ca.pem"),
        })
    }

    #[test]
    fn rejects_multi_node_without_tls() {
        let cfg = ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4470".parse().unwrap(),
            num_partitions: 16,
            peers: vec![
                NodeAddress::new("10.0.0.2", 4433),
                NodeAddress::new("10.0.0.3", 4433),
            ],
            seed_nodes: Vec::new(),
            tls: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("TLS"));
    }

    #[test]
    fn rejects_even_cluster_size() {
        let cfg = ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4470".parse().unwrap(),
            num_partitions: 16,
            peers: vec![NodeAddress::new("10.0.0.2", 4433)],
            seed_nodes: Vec::new(),
            tls: test_tls(),
        };
        // 1 self + 1 peer = 2 (even)
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("odd"));
    }

    #[test]
    fn accepts_odd_cluster_sizes() {
        for peer_count in [2, 4, 6] {
            let peers: Vec<NodeAddress> = (0..peer_count)
                .map(|i| NodeAddress::new(format!("10.0.0.{}", i + 2), 4433))
                .collect();
            let cfg = ClusterConfig {
                node_id: 1,
                bind: "0.0.0.0:4470".parse().unwrap(),
                num_partitions: 16,
                peers,
                seed_nodes: Vec::new(),
                tls: test_tls(),
            };
            // 1 + 2=3, 1+4=5, 1+6=7 — all odd
            cfg.validate().unwrap();
        }
    }

    #[test]
    fn rejects_duplicate_peers() {
        let cfg = ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4470".parse().unwrap(),
            num_partitions: 16,
            peers: vec![
                NodeAddress::new("10.0.0.2", 4433),
                NodeAddress::new("10.0.0.2", 4433), // duplicate
            ],
            seed_nodes: Vec::new(),
            tls: test_tls(),
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn seed_nodes_require_tls() {
        // seed_nodes implies joining a cluster, so TLS is required
        let cfg = ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4470".parse().unwrap(),
            num_partitions: 16,
            peers: Vec::new(),
            seed_nodes: vec![NodeAddress::new("10.0.0.1", 4433)],
            tls: None,
        };
        assert!(!cfg.is_single_node());
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("TLS"));
    }

    #[test]
    fn seed_nodes_with_tls_valid() {
        let cfg = ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4470".parse().unwrap(),
            num_partitions: 16,
            peers: Vec::new(),
            seed_nodes: vec![NodeAddress::new("10.0.0.1", 4433)],
            tls: test_tls(),
        };
        assert!(!cfg.is_single_node());
        // cluster size = 1 (self only), odd-number check passes
        cfg.validate().unwrap();
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4470".parse().unwrap(),
            num_partitions: 16,
            peers: vec![
                NodeAddress::new("10.0.0.2", 4433),
                NodeAddress::new("10.0.0.3", 4433),
            ],
            seed_nodes: Vec::new(),
            tls: Some(TlsConfig {
                cert: PathBuf::from("/etc/aeon/tls/node.pem"),
                key: PathBuf::from("/etc/aeon/tls/node.key"),
                ca: PathBuf::from("/etc/aeon/tls/ca.pem"),
            }),
        };

        let bytes = bincode::serialize(&cfg).unwrap();
        let decoded: ClusterConfig = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.node_id, cfg.node_id);
        assert_eq!(decoded.num_partitions, cfg.num_partitions);
        assert_eq!(decoded.peers.len(), 2);
        assert!(decoded.tls.is_some());
    }
}
