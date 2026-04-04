//! HTTP connector — Webhook source and Polling source.
//!
//! `HttpWebhookSource`: Runs an axum HTTP server that accepts POST requests.
//!   Each request body becomes an Event. Push-source with three-phase backpressure.
//!
//! `HttpPollingSource`: Periodically polls an HTTP endpoint via GET.
//!   Each response body becomes an Event. Pull-source (no backpressure needed).

mod polling_source;
mod webhook_source;

pub use polling_source::{HttpPollingSource, HttpPollingSourceConfig};
pub use webhook_source::{HttpWebhookSource, HttpWebhookSourceConfig};
