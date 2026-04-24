//! WebSocket connector — Source and Sink implementations.
//!
//! `WebSocketSource`: Connects to a WebSocket server and receives messages as events.
//!   Push-source with three-phase backpressure via shared PushBuffer.
//!
//! `WebSocketSink`: Connects to a WebSocket server and sends outputs as messages.

mod mtls;
mod sink;
mod source;

pub use sink::{WebSocketSink, WebSocketSinkConfig};
pub use source::{WebSocketSource, WebSocketSourceConfig};
