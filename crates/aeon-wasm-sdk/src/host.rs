//! Raw FFI declarations for Aeon host imports.
//!
//! These are the `extern "C"` functions provided by the Aeon Wasm runtime.
//! Users should prefer the safe wrappers in [`state`], [`log`], [`metrics`],
//! and [`clock`] modules instead of calling these directly.
//!
//! Only linked when targeting `wasm32` — on native targets, stubs are provided
//! so the crate compiles for testing.

#[cfg(target_arch = "wasm32")]
unsafe extern "C" {
    // State
    pub fn state_get(key_ptr: i32, key_len: i32, out_ptr: i32, out_max_len: i32) -> i32;
    pub fn state_put(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32);
    pub fn state_delete(key_ptr: i32, key_len: i32);

    // Logging
    pub fn log_info(ptr: i32, len: i32);
    pub fn log_warn(ptr: i32, len: i32);
    pub fn log_error(ptr: i32, len: i32);

    // Clock
    pub fn clock_now_ms() -> i64;

    // Metrics
    pub fn metrics_counter_inc(name_ptr: i32, name_len: i32, delta: i64);
    pub fn metrics_gauge_set(name_ptr: i32, name_len: i32, value: f64);
}

// Native stubs for testing — these are never called in production,
// they just allow the crate to compile on the host for `cargo test`.
//
// # Safety
//
// These are no-op stubs used only for host-side compilation. They do not
// dereference pointers or perform any unsafe operations.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn state_get(_: i32, _: i32, _: i32, _: i32) -> i32 {
    -1
}
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn state_put(_: i32, _: i32, _: i32, _: i32) {}
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn state_delete(_: i32, _: i32) {}
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn log_info(_: i32, _: i32) {}
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn log_warn(_: i32, _: i32) {}
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn log_error(_: i32, _: i32) {}
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn clock_now_ms() -> i64 {
    0
}
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn metrics_counter_inc(_: i32, _: i32, _: i64) {}
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn metrics_gauge_set(_: i32, _: i32, _: f64) {}
