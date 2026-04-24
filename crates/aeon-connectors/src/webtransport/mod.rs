//! WebTransport connector — Streams (reliable) and Datagrams (lossy).
//!
//! `WebTransportSource`: Accepts WebTransport sessions, reads from
//!   bidirectional streams (reliable, ordered) as events.
//!
//! `WebTransportSink`: Opens WebTransport session to server,
//!   sends outputs over bidirectional streams.
//!
//! `WebTransportDatagramSource`: Reads unreliable datagrams from
//!   WebTransport sessions. Requires explicit `accept_loss: true` config.
//!   Datagrams may be dropped, reordered, or duplicated.

mod datagram_source;
mod mtls;
mod sink;
mod source;

pub use datagram_source::{WebTransportDatagramSource, WebTransportDatagramSourceConfig};
pub use mtls::mtls_client_config_from_signer;
pub use sink::{WebTransportSink, WebTransportSinkConfig};
pub use source::{WebTransportSource, WebTransportSourceConfig};
