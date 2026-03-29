//! QUIC transport layer for inter-node Raft RPCs and partition transfer.

pub mod framing;
pub mod tls;
pub mod endpoint;
pub mod network;
pub mod server;

pub use endpoint::QuicEndpoint;
pub use framing::{MessageType, read_frame, write_frame};
pub use network::{QuicNetworkConnection, QuicNetworkFactory};
pub use tls::{build_client_config, build_server_config};
