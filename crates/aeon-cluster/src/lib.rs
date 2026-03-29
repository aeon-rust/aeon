//! Raft consensus, Proof of History, and QUIC inter-node transport.

pub mod config;
pub mod discovery;
pub mod partition_manager;
pub mod snapshot;
pub mod transfer;
pub mod types;

#[cfg(feature = "cluster")]
pub mod node;
#[cfg(feature = "cluster")]
pub mod raft_config;
#[cfg(feature = "cluster")]
pub mod store;
#[cfg(feature = "cluster")]
pub mod transport;

pub use config::{ClusterConfig, TlsConfig};
pub use snapshot::ClusterSnapshot;
pub use types::{
    ClusterRequest, ClusterResponse, NodeAddress, NodeId, PartitionOwnership, PartitionTable,
};

#[cfg(feature = "cluster")]
pub use node::ClusterNode;
