//! HTTP connectors — Webhook source, Polling source, and HTTP sink.
//!
//! `HttpWebhookSource`: Runs an axum HTTP server that accepts POST requests.
//!   Each request body becomes an Event. Push-source with three-phase backpressure.
//!
//! `HttpPollingSource`: Periodically polls an HTTP endpoint via GET.
//!   Each response body becomes an Event. Pull-source (no backpressure needed).
//!
//! `HttpSink`: POSTs output payloads to an external HTTP endpoint.
//!   Enables Aeon → serverless fan-out (Lambda, Cloud Functions, webhooks).

mod polling_source;
mod sink;
mod webhook_source;

pub use polling_source::{HttpPollingSource, HttpPollingSourceConfig};
pub use sink::{HttpSink, HttpSinkConfig};
pub use webhook_source::{HttpWebhookSource, HttpWebhookSourceConfig};
