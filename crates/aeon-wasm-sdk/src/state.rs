//! Safe wrappers for the host-provided key-value state store.
//!
//! The state store is backed by Aeon's multi-tier state engine (L1 DashMap,
//! L2 mmap, L3 RocksDB). Keys and values are arbitrary byte slices.
//!
//! ```rust,ignore
//! use aeon_wasm_sdk::state;
//!
//! state::put(b"counter", &42u64.to_le_bytes());
//! if let Some(val) = state::get(b"counter") {
//!     // ...
//! }
//! state::delete(b"counter");
//! ```

use alloc::vec::Vec;

/// Maximum value size for a single `get` call (64 KB).
/// Values larger than this are truncated.
const MAX_VALUE_SIZE: usize = 65536;

/// Get a value by key from the host state store.
///
/// Returns `None` if the key does not exist.
pub fn get(key: &[u8]) -> Option<Vec<u8>> {
    let mut buf = alloc::vec![0u8; MAX_VALUE_SIZE];

    // SAFETY: key and buf are valid slices, host function is provided by Aeon runtime
    let result = unsafe {
        crate::host::state_get(
            key.as_ptr() as i32,
            key.len() as i32,
            buf.as_mut_ptr() as i32,
            buf.len() as i32,
        )
    };

    if result < 0 {
        None
    } else {
        buf.truncate(result as usize);
        Some(buf)
    }
}

/// Put a key-value pair into the host state store.
pub fn put(key: &[u8], value: &[u8]) {
    // SAFETY: key and value are valid slices
    unsafe {
        crate::host::state_put(
            key.as_ptr() as i32,
            key.len() as i32,
            value.as_ptr() as i32,
            value.len() as i32,
        );
    }
}

/// Delete a key from the host state store.
pub fn delete(key: &[u8]) {
    // SAFETY: key is a valid slice
    unsafe {
        crate::host::state_delete(key.as_ptr() as i32, key.len() as i32);
    }
}
