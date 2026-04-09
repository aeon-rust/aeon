//! Raw QUIC connector — Source and Sink for external QUIC clients.
//!
//! Listens on port 4472 for external QUIC connections (not inter-node cluster traffic).
//! Each bidirectional stream carries length-prefixed binary messages.
//!
//! `QuicSource`: Accepts incoming connections and reads messages as events.
//!   Push-source with three-phase backpressure.
//!
//! `QuicSink`: Connects to a QUIC server and sends outputs as messages.
//!
//! Protocol framing: `[length: u32 LE][payload]` per message.

mod sink;
mod source;
mod tls;

pub use sink::{QuicSink, QuicSinkConfig};
pub use source::{QuicSource, QuicSourceConfig};
pub use tls::dev_quic_configs;
