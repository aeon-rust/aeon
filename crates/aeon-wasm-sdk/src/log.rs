//! Safe wrappers for host-provided logging.
//!
//! Log messages are forwarded to the Aeon engine's tracing infrastructure.
//!
//! ```rust,ignore
//! use aeon_wasm_sdk::log;
//!
//! log::info("processing event");
//! log::warn("payload too large, truncating");
//! log::error("failed to parse JSON");
//! log::debug("raw bytes received");
//! ```

/// Log at info level.
pub fn info(msg: &str) {
    // SAFETY: msg is a valid str slice
    unsafe {
        crate::host::log_info(msg.as_ptr() as i32, msg.len() as i32);
    }
}

/// Log at warn level.
pub fn warn(msg: &str) {
    // SAFETY: msg is a valid str slice
    unsafe {
        crate::host::log_warn(msg.as_ptr() as i32, msg.len() as i32);
    }
}

/// Log at error level.
pub fn error(msg: &str) {
    // SAFETY: msg is a valid str slice
    unsafe {
        crate::host::log_error(msg.as_ptr() as i32, msg.len() as i32);
    }
}
