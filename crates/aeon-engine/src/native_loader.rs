//! Dynamic loader for native `.so` / `.dll` processors.
//!
//! Loads a shared library via `dlopen` / `LoadLibrary`, resolves C-ABI symbols
//! (`aeon_process`, `aeon_process_batch`, `aeon_processor_create`, etc.),
//! and wraps them in a `Processor` trait implementation.
//!
//! # Safety
//!
//! Native processors run in-process with full memory access. Only load
//! trusted `.so` files — a buggy processor can crash the Aeon process.
//!
//! # Integrity Verification (OWASP A08)
//!
//! When `expected_hash` is provided to `load()` / `load_with_buffer()`, the loader
//! computes the SHA-256 hash of the shared library file and verifies it matches
//! before loading. This prevents tampered or corrupted artifacts from executing.
//! The expected hash can be stored in the processor registry or YAML manifest.

use std::path::Path;
use std::sync::Mutex;

use aeon_types::event::{Event, Output};
use aeon_types::{AeonError, Processor};

use aeon_native_sdk::wire;

/// Type aliases for C-ABI function pointers.
type CreateFn = unsafe extern "C" fn(*const u8, usize) -> *mut std::ffi::c_void;
type DestroyFn = unsafe extern "C" fn(*mut std::ffi::c_void);
type ProcessFn = unsafe extern "C" fn(
    *mut std::ffi::c_void,
    *const u8,
    usize,
    *mut u8,
    usize,
    *mut usize,
) -> i32;
type ProcessBatchFn = unsafe extern "C" fn(
    *mut std::ffi::c_void,
    *const u8,
    usize,
    *mut u8,
    usize,
    *mut usize,
) -> i32;

/// Default output buffer size (1 MB). Grown if needed.
const DEFAULT_OUT_BUF_SIZE: usize = 1024 * 1024;

/// A processor loaded from a native shared library.
///
/// The library must export the C-ABI symbols defined in `aeon-native-sdk`.
/// The processor context and output buffer are protected by a Mutex
/// (same pattern as WasmProcessor — single-threaded execution per instance,
/// multiple instances per pipeline for parallelism).
pub struct NativeProcessor {
    /// Loaded shared library (kept alive to prevent dlclose).
    _lib: libloading::Library,
    /// Processor context returned by `aeon_processor_create`.
    ctx: *mut std::ffi::c_void,
    /// Destroy function pointer.
    destroy_fn: DestroyFn,
    /// Process single event function pointer.
    process_fn: ProcessFn,
    /// Process batch function pointer. Retained for future wire format identity propagation
    /// (Layer 7). Currently unused: process_batch() calls process() per-event for identity stamping.
    #[allow(dead_code)]
    process_batch_fn: ProcessBatchFn,
    /// Reusable output buffer (avoids repeated allocation).
    out_buf: Mutex<Vec<u8>>,
}

// SAFETY: The processor context is accessed only under Mutex.
// The library and function pointers are immutable after construction.
unsafe impl Send for NativeProcessor {}
unsafe impl Sync for NativeProcessor {}

impl NativeProcessor {
    /// Load a native processor from a shared library path.
    ///
    /// The library must export: `aeon_processor_create`, `aeon_processor_destroy`,
    /// `aeon_process`, `aeon_process_batch`.
    ///
    /// # Safety
    ///
    /// The shared library runs in-process. Only load trusted code.
    /// Load a native processor with default output buffer size (1 MB).
    pub fn load(path: impl AsRef<Path>, config: &[u8]) -> Result<Self, AeonError> {
        Self::load_with_buffer(path, config, DEFAULT_OUT_BUF_SIZE)
    }

    /// Load a native processor, verifying its SHA-256 hash before loading.
    ///
    /// `expected_hash` is a hex-encoded SHA-256 digest (64 characters).
    /// Returns an error if the file hash does not match.
    pub fn load_verified(
        path: impl AsRef<Path>,
        config: &[u8],
        expected_hash: &str,
    ) -> Result<Self, AeonError> {
        let path = path.as_ref();
        Self::verify_integrity(path, expected_hash)?;
        Self::load_with_buffer(path, config, DEFAULT_OUT_BUF_SIZE)
    }

    /// Compute the SHA-256 hash of a file and verify it matches the expected value.
    pub fn verify_integrity(path: &Path, expected_hash: &str) -> Result<(), AeonError> {
        use sha2::{Digest, Sha256};

        let file_bytes = std::fs::read(path).map_err(|e| AeonError::Config {
            message: format!(
                "failed to read '{}' for integrity check: {e}",
                path.display()
            ),
        })?;

        let mut hasher = Sha256::new();
        hasher.update(&file_bytes);
        let actual_hash = hex::encode(hasher.finalize());

        if actual_hash != expected_hash {
            return Err(AeonError::Config {
                message: format!(
                    "integrity check failed for '{}': expected SHA-256 {}, got {}",
                    path.display(),
                    expected_hash,
                    actual_hash
                ),
            });
        }

        tracing::info!(
            path = %path.display(),
            sha256 = %actual_hash,
            "native processor integrity verified"
        );
        Ok(())
    }

    /// Compute the SHA-256 hash of a native processor artifact.
    pub fn compute_hash(path: impl AsRef<Path>) -> Result<String, AeonError> {
        use sha2::{Digest, Sha256};

        let path = path.as_ref();
        let file_bytes = std::fs::read(path).map_err(|e| AeonError::Config {
            message: format!("failed to read '{}': {e}", path.display()),
        })?;

        let mut hasher = Sha256::new();
        hasher.update(&file_bytes);
        Ok(hex::encode(hasher.finalize()))
    }

    /// Load a native processor with a custom initial output buffer size.
    ///
    /// The buffer auto-grows if needed. Use larger values for processors
    /// that produce large outputs to avoid re-allocation.
    pub fn load_with_buffer(
        path: impl AsRef<Path>,
        config: &[u8],
        initial_buffer_size: usize,
    ) -> Result<Self, AeonError> {
        let path = path.as_ref();

        // SAFETY: Loading a shared library is inherently unsafe — the library
        // runs in our process space. We trust the caller to only load verified artifacts.
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| AeonError::Config {
            message: format!("failed to load native processor '{}': {e}", path.display()),
        })?;

        // Resolve required symbols
        let create_fn: CreateFn = unsafe {
            *lib.get::<CreateFn>(b"aeon_processor_create\0")
                .map_err(|e| AeonError::Config {
                    message: format!("missing symbol 'aeon_processor_create': {e}"),
                })?
        };
        let destroy_fn: DestroyFn = unsafe {
            *lib.get::<DestroyFn>(b"aeon_processor_destroy\0")
                .map_err(|e| AeonError::Config {
                    message: format!("missing symbol 'aeon_processor_destroy': {e}"),
                })?
        };
        let process_fn: ProcessFn = unsafe {
            *lib.get::<ProcessFn>(b"aeon_process\0")
                .map_err(|e| AeonError::Config {
                    message: format!("missing symbol 'aeon_process': {e}"),
                })?
        };
        let process_batch_fn: ProcessBatchFn = unsafe {
            *lib.get::<ProcessBatchFn>(b"aeon_process_batch\0")
                .map_err(|e| AeonError::Config {
                    message: format!("missing symbol 'aeon_process_batch': {e}"),
                })?
        };

        // Create processor context
        let ctx = unsafe { create_fn(config.as_ptr(), config.len()) };
        if ctx.is_null() {
            return Err(AeonError::Processor {
                message: "aeon_processor_create returned null".into(),
                source: None,
            });
        }

        Ok(Self {
            _lib: lib,
            ctx,
            destroy_fn,
            process_fn,
            process_batch_fn,
            out_buf: Mutex::new(vec![0u8; initial_buffer_size]),
        })
    }

    /// Validate that a shared library exports all required C-ABI symbols.
    ///
    /// Does NOT create a processor instance — just checks symbol existence.
    pub fn validate(path: impl AsRef<Path>) -> Result<Vec<String>, AeonError> {
        let path = path.as_ref();
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| AeonError::Config {
            message: format!("failed to load library '{}': {e}", path.display()),
        })?;

        let required = &[
            "aeon_processor_create",
            "aeon_processor_destroy",
            "aeon_process",
            "aeon_process_batch",
        ];
        let optional = &["aeon_processor_name", "aeon_processor_version"];

        let mut found = Vec::new();
        let mut missing = Vec::new();

        for sym in required {
            let sym_bytes = format!("{sym}\0");
            if unsafe { lib.get::<*const ()>(sym_bytes.as_bytes()) }.is_ok() {
                found.push(sym.to_string());
            } else {
                missing.push(sym.to_string());
            }
        }

        if !missing.is_empty() {
            return Err(AeonError::Config {
                message: format!(
                    "native processor '{}' missing required symbols: {}",
                    path.display(),
                    missing.join(", ")
                ),
            });
        }

        for sym in optional {
            let sym_bytes = format!("{sym}\0");
            if unsafe { lib.get::<*const ()>(sym_bytes.as_bytes()) }.is_ok() {
                found.push(sym.to_string());
            }
        }

        Ok(found)
    }
}

impl Processor for NativeProcessor {
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
        let event_bytes = wire::serialize_event(&event);
        let mut out_buf = self.out_buf.lock().map_err(|_| AeonError::Processor {
            message: "native processor mutex poisoned".into(),
            source: None,
        })?;

        let mut out_len: usize = 0;

        // SAFETY: ctx is valid (created in load()), function pointers are valid,
        // event_bytes and out_buf are valid slices.
        let rc = unsafe {
            (self.process_fn)(
                self.ctx,
                event_bytes.as_ptr(),
                event_bytes.len(),
                out_buf.as_mut_ptr(),
                out_buf.len(),
                &mut out_len,
            )
        };

        if rc == -4 {
            // Buffer too small — grow and retry
            let new_size = out_buf.len() * 2;
            out_buf.resize(new_size, 0);
            let rc2 = unsafe {
                (self.process_fn)(
                    self.ctx,
                    event_bytes.as_ptr(),
                    event_bytes.len(),
                    out_buf.as_mut_ptr(),
                    out_buf.len(),
                    &mut out_len,
                )
            };
            if rc2 != 0 {
                return Err(AeonError::Processor {
                    message: format!("aeon_process returned error code {rc2}"),
                    source: None,
                });
            }
        } else if rc != 0 {
            return Err(AeonError::Processor {
                message: format!("aeon_process returned error code {rc}"),
                source: None,
            });
        }

        // Host-side stamp: native wire format doesn't carry event identity,
        // so we stamp source_event_id/partition/offset on all deserialized outputs.
        let mut outputs = wire::deserialize_outputs(&out_buf[..out_len])?;
        for output in &mut outputs {
            output.source_event_id = Some(event.id);
            output.source_partition = Some(event.partition);
            output.source_offset = event.source_offset;
            if output.source_ts.is_none() {
                output.source_ts = event.source_ts;
            }
        }
        Ok(outputs)
    }

    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        // Process per-event to ensure host-side event identity stamping.
        // The bulk wire call (process_batch_fn) returns a flat output list without
        // event-to-output mapping, making identity propagation impossible.
        let mut outputs = Vec::with_capacity(events.len());
        for event in events {
            outputs.extend(self.process(event)?);
        }
        Ok(outputs)
    }
}

impl Drop for NativeProcessor {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY: ctx was created by aeon_processor_create, destroy_fn is valid
            unsafe {
                (self.destroy_fn)(self.ctx);
            }
            self.ctx = std::ptr::null_mut();
        }
    }
}

impl std::fmt::Debug for NativeProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeProcessor")
            .field("ctx", &format_args!("{:?}", self.ctx))
            .finish()
    }
}
