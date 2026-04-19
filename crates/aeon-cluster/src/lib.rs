//! Raft consensus, Proof of History, and QUIC inter-node transport.

// FT-10: no-panic policy. Production code in this crate must not use
// `.unwrap()` or `.expect()` except for explicitly-documented startup-time
// invariants, which must carry an `#[allow(...)]` attribute with rationale.
// Test modules and benches are exempt (`cfg(not(test))`).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub mod discovery;
pub mod partition_manager;
pub mod rebalance;
pub mod snapshot;
pub mod transfer;
pub mod types;

#[cfg(feature = "cluster")]
pub mod node;
#[cfg(feature = "cluster")]
pub mod partition_driver;
#[cfg(feature = "cluster")]
pub mod raft_config;
#[cfg(feature = "cluster")]
pub mod log_store;
#[cfg(feature = "cluster")]
pub mod store;
#[cfg(feature = "cluster")]
pub mod transport;

pub use config::{ClusterConfig, TlsConfig};
pub use rebalance::{plan_drain, plan_rebalance};
pub use snapshot::ClusterSnapshot;
pub use types::{
    ClusterRequest, ClusterResponse, NodeAddress, NodeId, PartitionCutoverRequest,
    PartitionCutoverResponse, PartitionOwnership, PartitionTable, PartitionTransferEnd,
    PartitionTransferRequest, PohChainTransferRequest, PohChainTransferResponse, TransferStatus,
};

#[cfg(feature = "cluster")]
pub use node::ClusterNode;
#[cfg(feature = "cluster")]
pub use partition_driver::{
    NodeResolver, PartitionTransferDriver, PohChainInstaller, RaftNodeResolver,
    SegmentInstaller,
};
#[cfg(feature = "cluster")]
pub use store::SharedClusterState;
