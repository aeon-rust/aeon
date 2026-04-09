//! OpenRaft type configuration for Aeon cluster.

#[cfg(feature = "cluster")]
pub use inner::*;

#[cfg(feature = "cluster")]
mod inner {
    use std::io::Cursor;

    use crate::types::{ClusterRequest, ClusterResponse, NodeAddress};

    // Use the declare_raft_types! macro to define our type config
    openraft::declare_raft_types!(
        /// Aeon's Raft type configuration.
        pub AeonRaftConfig:
            D            = ClusterRequest,
            R            = ClusterResponse,
            NodeId       = u64,
            Node         = NodeAddress,
            Entry        = openraft::Entry<AeonRaftConfig>,
            SnapshotData = Cursor<Vec<u8>>,
            Responder    = openraft::impls::OneshotResponder<AeonRaftConfig>,
            AsyncRuntime = openraft::TokioRuntime,
    );
}
