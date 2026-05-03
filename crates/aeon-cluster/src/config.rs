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

/// Raft consensus timing configuration (FT-5).
///
/// Controls election timing. Widening the `[min, max]` range reduces the probability
/// of split-votes in flaky networks (mitigation for the absence of pre-vote in
/// openraft 0.9.x — see FAULT-TOLERANCE-ANALYSIS.md FT-4).
///
/// Defaults match OpenRaft's recommended ratios (election_min = 3 × heartbeat,
/// election_max = 6 × heartbeat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaftTiming {
    /// Heartbeat interval in milliseconds (leader → follower). Default: 500ms.
    pub heartbeat_ms: u64,
    /// Minimum randomized election timeout in milliseconds. Default: 1500ms.
    pub election_min_ms: u64,
    /// Maximum randomized election timeout in milliseconds. Default: 3000ms.
    pub election_max_ms: u64,
}

impl Default for RaftTiming {
    fn default() -> Self {
        Self {
            heartbeat_ms: 500,
            election_min_ms: 1500,
            election_max_ms: 3000,
        }
    }
}

impl RaftTiming {
    /// Production-recommended timings for a 3-node cluster on a reliable VPC.
    ///
    /// Wider jitter window (2000–6000 ms, W=4000 ms) vs. the default
    /// (1500–3000 ms, W=1500 ms) reduces split-vote probability by ~60%
    /// at the cost of ~3 s additional worst-case failover latency.
    ///
    /// Mitigation for openraft 0.9.x pre-vote absence (FT-4). See
    /// `docs/CLUSTERING.md` §Raft timing for the probability math.
    pub fn prod_recommended() -> Self {
        Self {
            heartbeat_ms: 500,
            election_min_ms: 2000,
            election_max_ms: 6000,
        }
    }

    /// Flaky-network preset — wide jitter window tolerates clock drift and
    /// packet loss at the cost of slower leader failover (~12 s worst case).
    ///
    /// Use when the inter-node network shows occasional multi-second pauses
    /// (congested VPCs, cross-region clusters, sharing a noisy neighbour).
    pub fn flaky_network() -> Self {
        Self {
            heartbeat_ms: 500,
            election_min_ms: 3000,
            election_max_ms: 12000,
        }
    }

    /// G14 — fast-failover preset targeting <5 s leader failover on a
    /// reliable VPC (Gate 2 row).
    ///
    /// Tightens heartbeat to 250 ms and election window to 750–2000 ms.
    /// Combined with `ClusterNode::relinquish_leadership` on graceful
    /// shutdown, the observed failover on a 3-node, no-load cluster should
    /// drop from ~10 s (default timings, no pre-stop hook) to ~1–2 s
    /// (graceful) and ~2–3 s worst-case (ungraceful leader-kill).
    ///
    /// Trade-off vs. `default()` / `prod_recommended()`:
    ///   - 2× heartbeat rate (250 ms vs. 500 ms)
    ///   - Narrower jitter (1250 ms vs. 4000 ms in prod preset) — slightly
    ///     higher split-vote probability, but acceptable on reliable VPCs.
    pub fn fast_failover() -> Self {
        Self {
            heartbeat_ms: 250,
            election_min_ms: 750,
            election_max_ms: 2000,
        }
    }

    /// Validate timing constraints: `heartbeat < election_min < election_max`.
    pub fn validate(&self) -> Result<(), AeonError> {
        if self.heartbeat_ms == 0 {
            return Err(AeonError::Config {
                message: "raft_timing.heartbeat_ms must be > 0".to_string(),
            });
        }
        if self.election_min_ms <= self.heartbeat_ms {
            return Err(AeonError::Config {
                message: format!(
                    "raft_timing.election_min_ms ({}) must be > heartbeat_ms ({})",
                    self.election_min_ms, self.heartbeat_ms
                ),
            });
        }
        if self.election_max_ms <= self.election_min_ms {
            return Err(AeonError::Config {
                message: format!(
                    "raft_timing.election_max_ms ({}) must be > election_min_ms ({})",
                    self.election_max_ms, self.election_min_ms
                ),
            });
        }
        Ok(())
    }
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
    /// When true, use ephemeral self-signed certs (auto-tls feature).
    /// Bypasses the file-based TLS requirement for multi-node clusters.
    #[serde(default)]
    pub auto_tls: bool,
    /// Pre-computed full membership map (node_id → address) for cluster bootstrap.
    /// When set, `bootstrap_multi()` uses this instead of deriving from local_addr + peers.
    #[serde(default)]
    pub initial_members: Vec<(u64, NodeAddress)>,
    /// Externally routable address for this node (used in join requests).
    /// If None, falls back to `bind` address. In K8s, set to the pod DNS name.
    #[serde(default)]
    pub advertise_addr: Option<NodeAddress>,
    /// Raft consensus timing (FT-5). Controls heartbeat and election intervals.
    #[serde(default)]
    pub raft_timing: RaftTiming,
    /// G11.a — PoH chain verification policy on partition transfer
    /// (CL-6b install). One of `"verify"` (default), `"verify_with_key"`,
    /// or `"trust_extend"`. See `aeon_crypto::poh::PohVerifyMode` for
    /// semantics. Settable via the `AEON_CLUSTER_POH_VERIFY_MODE` env
    /// var when constructing `ClusterConfig` from env in `aeon-cli`.
    #[serde(default = "default_poh_verify_mode")]
    pub poh_verify_mode: String,
    /// G9 — port the REST API listens on for *every* pod in this cluster.
    /// Set by `aeon-cli` from the `--addr` flag (or `AEON_API_ADDR` env).
    /// When `Some`, followers can 307-redirect cluster-write requests to
    /// the leader's REST URL derived as `<scheme>://<leader-host>:<port>`.
    /// `None` disables the auto-forward path — writes to followers keep
    /// the pre-G9 behaviour of returning an error.
    #[serde(default)]
    pub rest_api_port: Option<u16>,
    /// G9 — URL scheme for the auto-forward `Location` header.
    /// Typically `"http"` (default when `None`). Set to `"https"` when the
    /// REST API is fronted by TLS.
    #[serde(default)]
    pub rest_scheme: Option<String>,
}

fn default_poh_verify_mode() -> String {
    "verify".to_string()
}

impl ClusterConfig {
    /// Create a minimal single-node config for development/testing.
    pub fn single_node(node_id: u64, num_partitions: u16) -> Self {
        // FT-10: hardcoded socket-address literal is infallible at parse time.
        #[allow(clippy::unwrap_used)]
        let bind = "0.0.0.0:4470".parse().unwrap();
        Self {
            node_id,
            bind,
            num_partitions,
            peers: Vec::new(),
            seed_nodes: Vec::new(),
            tls: None,
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: RaftTiming::default(),
            poh_verify_mode: default_poh_verify_mode(),
            rest_api_port: None,
            rest_scheme: None,
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

        self.raft_timing.validate()?;

        // G11.a — validate poh_verify_mode string maps to a known variant
        // at config-load time, not at first partition-transfer. Keeps the
        // crypto crate's parsing logic as the single source of truth.
        match self.poh_verify_mode.trim().to_ascii_lowercase().as_str() {
            "verify" | "verify_with_key" | "verify-with-key" | "verifywithkey" | "trust_extend"
            | "trust-extend" | "trustextend" => {}
            other => {
                return Err(AeonError::Config {
                    message: format!(
                        "cluster.poh_verify_mode '{other}' is not valid \
                         (expected one of: verify, verify_with_key, trust_extend)"
                    ),
                });
            }
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

        // Multi-node clusters require TLS — plaintext inter-node traffic is not allowed.
        // auto_tls (ephemeral self-signed certs) satisfies this for dev/testing.
        if !self.is_single_node() && self.tls.is_none() && !self.auto_tls {
            return Err(AeonError::Config {
                message: "multi-node clusters require TLS configuration. \
                          Set cluster.tls with cert, key, and ca paths, \
                          or enable auto-tls for development"
                    .to_string(),
            });
        }

        // Advisory: odd-number clusters are preferred for Raft quorum efficiency.
        // Even-number clusters are valid but suboptimal — they waste a node
        // without improving fault tolerance. We log a warning rather than
        // blocking because even-number states occur naturally during scaling
        // (e.g., adding a 2nd node before adding the 3rd, or a node going
        // down temporarily from 3→2).
        let cluster_size = self.initial_cluster_size();
        if cluster_size > 1 && cluster_size % 2 == 0 {
            tracing::warn!(
                cluster_size,
                "even-number cluster size ({cluster_size}) is suboptimal — \
                 add one more node for better fault tolerance"
            );
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
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: RaftTiming::default(),
            poh_verify_mode: "verify".to_string(),
            rest_api_port: None,
            rest_scheme: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("TLS"));
    }

    #[test]
    fn allows_even_cluster_size_with_warning() {
        let cfg = ClusterConfig {
            node_id: 1,
            bind: "0.0.0.0:4470".parse().unwrap(),
            num_partitions: 16,
            peers: vec![NodeAddress::new("10.0.0.2", 4433)],
            seed_nodes: Vec::new(),
            tls: test_tls(),
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: RaftTiming::default(),
            poh_verify_mode: "verify".to_string(),
            rest_api_port: None,
            rest_scheme: None,
        };
        // 1 self + 1 peer = 2 (even) — should succeed (advisory warning only)
        cfg.validate().unwrap();
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
                auto_tls: false,
                initial_members: Vec::new(),
                advertise_addr: None,
                raft_timing: RaftTiming::default(),
                poh_verify_mode: "verify".to_string(),
                rest_api_port: None,
                rest_scheme: None,
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
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: RaftTiming::default(),
            poh_verify_mode: "verify".to_string(),
            rest_api_port: None,
            rest_scheme: None,
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
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: RaftTiming::default(),
            poh_verify_mode: "verify".to_string(),
            rest_api_port: None,
            rest_scheme: None,
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
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: RaftTiming::default(),
            poh_verify_mode: "verify".to_string(),
            rest_api_port: None,
            rest_scheme: None,
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
            auto_tls: false,
            initial_members: Vec::new(),
            advertise_addr: None,
            raft_timing: RaftTiming::default(),
            poh_verify_mode: "verify".to_string(),
            rest_api_port: None,
            rest_scheme: None,
        };

        let bytes = bincode::serialize(&cfg).unwrap();
        let decoded: ClusterConfig = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.node_id, cfg.node_id);
        assert_eq!(decoded.num_partitions, cfg.num_partitions);
        assert_eq!(decoded.peers.len(), 2);
        assert!(decoded.tls.is_some());
    }

    #[test]
    fn raft_timing_defaults() {
        let t = RaftTiming::default();
        assert_eq!(t.heartbeat_ms, 500);
        assert_eq!(t.election_min_ms, 1500);
        assert_eq!(t.election_max_ms, 3000);
        t.validate().unwrap();
    }

    #[test]
    fn raft_timing_prod_recommended_preset() {
        let t = RaftTiming::prod_recommended();
        assert_eq!(t.heartbeat_ms, 500);
        assert_eq!(t.election_min_ms, 2000);
        assert_eq!(t.election_max_ms, 6000);
        t.validate().unwrap();
        // Window must be strictly wider than default to justify the preset.
        let default_window =
            RaftTiming::default().election_max_ms - RaftTiming::default().election_min_ms;
        let prod_window = t.election_max_ms - t.election_min_ms;
        assert!(prod_window > default_window);
    }

    #[test]
    fn raft_timing_flaky_network_preset() {
        let t = RaftTiming::flaky_network();
        assert_eq!(t.heartbeat_ms, 500);
        assert_eq!(t.election_min_ms, 3000);
        assert_eq!(t.election_max_ms, 12000);
        t.validate().unwrap();
        // Flaky preset must be wider than prod-recommended.
        let prod_window = RaftTiming::prod_recommended().election_max_ms
            - RaftTiming::prod_recommended().election_min_ms;
        let flaky_window = t.election_max_ms - t.election_min_ms;
        assert!(flaky_window > prod_window);
    }

    #[test]
    fn raft_timing_fast_failover_preset() {
        let t = RaftTiming::fast_failover();
        assert_eq!(t.heartbeat_ms, 250);
        assert_eq!(t.election_min_ms, 750);
        assert_eq!(t.election_max_ms, 2000);
        t.validate().unwrap();
        // Fast-failover worst-case (2× election_max_ms for a catastrophic
        // split-vote retry) must still come in under the 5 s Gate 2 target.
        assert!(t.election_max_ms * 2 <= 5_000);
        // Must be strictly faster than the default preset's worst case.
        assert!(t.election_max_ms < RaftTiming::default().election_max_ms);
    }

    #[test]
    fn raft_timing_rejects_zero_heartbeat() {
        let t = RaftTiming {
            heartbeat_ms: 0,
            election_min_ms: 1500,
            election_max_ms: 3000,
        };
        let err = t.validate().unwrap_err();
        assert!(err.to_string().contains("heartbeat_ms"));
    }

    #[test]
    fn raft_timing_rejects_min_le_heartbeat() {
        let t = RaftTiming {
            heartbeat_ms: 500,
            election_min_ms: 500,
            election_max_ms: 3000,
        };
        let err = t.validate().unwrap_err();
        assert!(err.to_string().contains("election_min_ms"));
    }

    #[test]
    fn raft_timing_rejects_max_le_min() {
        let t = RaftTiming {
            heartbeat_ms: 500,
            election_min_ms: 3000,
            election_max_ms: 2500,
        };
        let err = t.validate().unwrap_err();
        assert!(err.to_string().contains("election_max_ms"));
    }

    #[test]
    fn raft_timing_accepts_wide_jitter_for_flaky_networks() {
        // Mitigation for FT-4 (pre-vote blocked upstream): wide jitter range
        // reduces split-vote probability on flaky networks.
        let t = RaftTiming {
            heartbeat_ms: 500,
            election_min_ms: 2000,
            election_max_ms: 6000, // 4s jitter window
        };
        t.validate().unwrap();
    }

    #[test]
    fn poh_verify_mode_defaults_to_verify() {
        let cfg = ClusterConfig::single_node(1, 16);
        assert_eq!(cfg.poh_verify_mode, "verify");
        cfg.validate().unwrap();
    }

    #[test]
    fn poh_verify_mode_accepts_known_spellings() {
        for mode in [
            "verify",
            "Verify",
            "verify_with_key",
            "verify-with-key",
            "VerifyWithKey",
            "trust_extend",
            "trust-extend",
            "TrustExtend",
        ] {
            let mut cfg = ClusterConfig::single_node(1, 16);
            cfg.poh_verify_mode = mode.to_string();
            cfg.validate()
                .unwrap_or_else(|e| panic!("mode '{mode}' should validate, got: {e}"));
        }
    }

    #[test]
    fn poh_verify_mode_rejects_garbage() {
        let mut cfg = ClusterConfig::single_node(1, 16);
        cfg.poh_verify_mode = "yolo".to_string();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("poh_verify_mode"));
    }

    #[test]
    fn poh_verify_mode_deserialises_default_from_missing_field() {
        let json = r#"{
            "node_id": 1,
            "bind": "0.0.0.0:4470",
            "num_partitions": 16
        }"#;
        let cfg: ClusterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.poh_verify_mode, "verify");
    }

    #[test]
    fn cluster_config_serde_omits_raft_timing_uses_default() {
        // YAML/JSON without raft_timing field should deserialize with defaults.
        let json = r#"{
            "node_id": 1,
            "bind": "0.0.0.0:4470",
            "num_partitions": 16,
            "peers": [],
            "seed_nodes": [],
            "tls": null,
            "auto_tls": false,
            "initial_members": [],
            "advertise_addr": null
        }"#;
        let cfg: ClusterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.raft_timing.heartbeat_ms, 500);
        assert_eq!(cfg.raft_timing.election_min_ms, 1500);
        assert_eq!(cfg.raft_timing.election_max_ms, 3000);
    }
}
