//! QUIC transport layer for inter-node Raft RPCs and partition transfer.

pub mod cutover;
pub mod endpoint;
pub mod framing;
pub mod health;
pub mod network;
pub mod partition_transfer;
pub mod poh_transfer;
pub mod server;
pub mod throttle;
pub mod tls;
pub mod transfer_metrics;

pub use cutover::{
    CutoverCoordinator, CutoverOffsets, drive_partition_cutover, request_partition_cutover,
    serve_partition_cutover_stream, serve_partition_cutover_with_request,
};
pub use endpoint::QuicEndpoint;
pub use framing::{MessageType, read_frame, write_frame};
pub use health::{
    HealthPing, HealthPingerConfig, HealthPong, HealthStats, SharedHealthState, new_health_state,
    send_health_ping, spawn_health_pinger,
};
pub use network::{QuicNetworkConnection, QuicNetworkFactory};
pub use partition_transfer::{
    ChunkIter, PartitionTransferProvider, drive_partition_transfer, request_partition_transfer,
    serve_partition_transfer_stream, serve_partition_transfer_with_request,
};
pub use poh_transfer::{
    PohChainProvider, drive_poh_chain_transfer, request_poh_chain_transfer,
    serve_poh_chain_transfer_stream, serve_poh_chain_transfer_with_request,
};
pub use server::SourceProviderSlots;
pub use throttle::TransferThrottle;
pub use tls::{build_client_config, build_server_config, quic_configs_for_cluster};
pub use transfer_metrics::{PartitionTransferMetrics, TransferRole};
