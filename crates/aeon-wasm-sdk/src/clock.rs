//! Safe wrapper for host-provided clock.
//!
//! ```rust,ignore
//! use aeon_wasm_sdk::clock;
//!
//! let now = clock::now_ms();
//! ```

/// Get current time in milliseconds since Unix epoch.
pub fn now_ms() -> i64 {
    // SAFETY: host function is provided by Aeon runtime
    unsafe { crate::host::clock_now_ms() }
}
