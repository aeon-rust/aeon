//! Safe wrappers for host-provided metrics.
//!
//! Metrics are forwarded to the Aeon engine's Prometheus exporter.
//!
//! ```rust,ignore
//! use aeon_wasm_sdk::metrics;
//!
//! metrics::counter_inc("events_processed", 1);
//! metrics::gauge_set("queue_depth", 42.0);
//! ```

/// Increment a counter metric by `delta`.
pub fn counter_inc(name: &str, delta: u64) {
    // SAFETY: name is a valid str slice
    unsafe {
        crate::host::metrics_counter_inc(name.as_ptr() as i32, name.len() as i32, delta as i64);
    }
}

/// Set a gauge metric to `value`.
pub fn gauge_set(name: &str, value: f64) {
    // SAFETY: name is a valid str slice
    unsafe {
        crate::host::metrics_gauge_set(name.as_ptr() as i32, name.len() as i32, value);
    }
}
