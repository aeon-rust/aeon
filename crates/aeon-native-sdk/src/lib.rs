//! SDK for building native `.so` / `.dll` Aeon processors.
//!
//! # Overview
//!
//! This crate lets Rust developers write processors using the idiomatic
//! `Processor` trait and compile them as shared libraries (`.so` on Linux,
//! `.dll` on Windows, `.dylib` on macOS) that Aeon can load at runtime
//! via `dlopen` / `LoadLibrary`.
//!
//! # Usage
//!
//! ```rust,ignore
//! use aeon_native_sdk::{export_processor, NativeContext};
//! use aeon_types::{Event, Output, AeonError, Processor};
//!
//! struct MyProcessor { /* ... */ }
//!
//! impl Processor for MyProcessor {
//!     fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
//!         // Your processing logic here
//!         Ok(vec![])
//!     }
//! }
//!
//! fn create(config: &[u8]) -> Box<dyn Processor> {
//!     Box::new(MyProcessor { /* ... */ })
//! }
//!
//! export_processor!(create);
//! ```
//!
//! Compile with `cargo build --release` and the resulting `.so` can be
//! registered with `aeon processor register`.

pub use aeon_types::event::{Event, Output};
pub use aeon_types::{AeonError, Processor};

pub mod wire;

/// Re-export common types for processor authors.
pub mod prelude {
    pub use aeon_types::event::{Event, Output};
    pub use aeon_types::partition::PartitionId;
    pub use aeon_types::{AeonError, Processor};
    pub use bytes::Bytes;
    pub use std::sync::Arc;

    pub use crate::export_processor;
}

/// Generate C-ABI export functions for a native `.so` processor.
///
/// Pass a function `fn(&[u8]) -> Box<dyn Processor>` that creates a processor
/// from config bytes. The macro generates the required C-ABI exports:
///
/// - `aeon_processor_create(config_ptr, config_len) -> *mut c_void`
/// - `aeon_processor_destroy(ctx: *mut c_void)`
/// - `aeon_process(ctx, event_ptr, event_len, out_buf, out_cap, out_len) -> i32`
/// - `aeon_process_batch(ctx, events_ptr, events_len, out_buf, out_cap, out_len) -> i32`
/// - `aeon_processor_name() -> *const c_char`
/// - `aeon_processor_version() -> *const c_char`
#[macro_export]
macro_rules! export_processor {
    ($create_fn:expr) => {
        /// Create a processor instance from config bytes.
        #[no_mangle]
        pub extern "C" fn aeon_processor_create(
            config_ptr: *const u8,
            config_len: usize,
        ) -> *mut std::ffi::c_void {
            let config = if config_ptr.is_null() || config_len == 0 {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(config_ptr, config_len) }
            };
            let processor: Box<dyn $crate::Processor> = ($create_fn)(config);
            Box::into_raw(Box::new(processor)) as *mut std::ffi::c_void
        }

        /// Destroy a processor instance.
        #[no_mangle]
        pub extern "C" fn aeon_processor_destroy(ctx: *mut std::ffi::c_void) {
            if !ctx.is_null() {
                // SAFETY: ctx was created by aeon_processor_create via Box::into_raw
                unsafe {
                    let _ = Box::from_raw(ctx as *mut Box<dyn $crate::Processor>);
                }
            }
        }

        /// Process a single event. Returns 0 on success, non-zero on error.
        ///
        /// Wire format: event bytes in, output bytes out (see `aeon_native_sdk::wire`).
        #[no_mangle]
        pub extern "C" fn aeon_process(
            ctx: *mut std::ffi::c_void,
            event_ptr: *const u8,
            event_len: usize,
            out_buf: *mut u8,
            out_capacity: usize,
            out_len: *mut usize,
        ) -> i32 {
            if ctx.is_null() || event_ptr.is_null() || out_buf.is_null() || out_len.is_null() {
                return -1;
            }
            // SAFETY: ctx was created by aeon_processor_create, pointers validated above
            let processor = unsafe { &*(ctx as *const Box<dyn $crate::Processor>) };
            let event_bytes = unsafe { std::slice::from_raw_parts(event_ptr, event_len) };

            let event = match $crate::wire::deserialize_event(event_bytes) {
                Ok(e) => e,
                Err(_) => return -2,
            };

            let outputs = match processor.process(event) {
                Ok(o) => o,
                Err(_) => return -3,
            };

            let serialized = $crate::wire::serialize_outputs(&outputs);
            if serialized.len() > out_capacity {
                return -4; // buffer too small
            }

            unsafe {
                std::ptr::copy_nonoverlapping(serialized.as_ptr(), out_buf, serialized.len());
                *out_len = serialized.len();
            }
            0
        }

        /// Process a batch of events. Returns 0 on success, non-zero on error.
        #[no_mangle]
        pub extern "C" fn aeon_process_batch(
            ctx: *mut std::ffi::c_void,
            events_ptr: *const u8,
            events_len: usize,
            out_buf: *mut u8,
            out_capacity: usize,
            out_len: *mut usize,
        ) -> i32 {
            if ctx.is_null() || events_ptr.is_null() || out_buf.is_null() || out_len.is_null() {
                return -1;
            }
            let processor = unsafe { &*(ctx as *const Box<dyn $crate::Processor>) };
            let events_bytes = unsafe { std::slice::from_raw_parts(events_ptr, events_len) };

            let events = match $crate::wire::deserialize_events(events_bytes) {
                Ok(e) => e,
                Err(_) => return -2,
            };

            let outputs = match processor.process_batch(events) {
                Ok(o) => o,
                Err(_) => return -3,
            };

            let serialized = $crate::wire::serialize_outputs(&outputs);
            if serialized.len() > out_capacity {
                return -4;
            }

            unsafe {
                std::ptr::copy_nonoverlapping(serialized.as_ptr(), out_buf, serialized.len());
                *out_len = serialized.len();
            }
            0
        }

        /// Return the processor name (null-terminated C string).
        #[no_mangle]
        pub extern "C" fn aeon_processor_name() -> *const std::ffi::c_char {
            concat!(env!("CARGO_PKG_NAME"), "\0").as_ptr() as *const std::ffi::c_char
        }

        /// Return the processor version (null-terminated C string).
        #[no_mangle]
        pub extern "C" fn aeon_processor_version() -> *const std::ffi::c_char {
            concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const std::ffi::c_char
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::partition::PartitionId;
    use bytes::Bytes;
    use std::sync::Arc;

    struct PassthroughProcessor;

    impl Processor for PassthroughProcessor {
        fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
            Ok(vec![Output::new(
                Arc::from("output"),
                event.payload.clone(),
            )])
        }
    }

    fn create_passthrough(_config: &[u8]) -> Box<dyn Processor> {
        Box::new(PassthroughProcessor)
    }

    #[test]
    fn wire_roundtrip_single_event() {
        let event = Event::new(
            uuid::Uuid::nil(),
            1234567890,
            Arc::from("test-source"),
            PartitionId::new(0),
            Bytes::from_static(b"hello world"),
        );

        let serialized = wire::serialize_event(&event);
        let deserialized = wire::deserialize_event(&serialized).unwrap();

        assert_eq!(deserialized.id, event.id);
        assert_eq!(deserialized.timestamp, event.timestamp);
        assert_eq!(&*deserialized.source, "test-source");
        assert_eq!(deserialized.payload.as_ref(), b"hello world");
    }

    #[test]
    fn wire_roundtrip_outputs() {
        let outputs = vec![
            Output::new(Arc::from("dest1"), Bytes::from_static(b"payload1")),
            Output::new(Arc::from("dest2"), Bytes::from_static(b"payload2"))
                .with_key(Bytes::from_static(b"key2"))
                .with_header(Arc::from("h1"), Arc::from("v1")),
        ];

        let serialized = wire::serialize_outputs(&outputs);
        let deserialized = wire::deserialize_outputs(&serialized).unwrap();

        assert_eq!(deserialized.len(), 2);
        assert_eq!(&*deserialized[0].destination, "dest1");
        assert_eq!(deserialized[0].payload.as_ref(), b"payload1");
        assert_eq!(&*deserialized[1].destination, "dest2");
        assert_eq!(
            deserialized[1].key.as_ref().map(|k| k.as_ref()),
            Some(b"key2".as_ref())
        );
        assert_eq!(deserialized[1].headers.len(), 1);
        assert_eq!(&*deserialized[1].headers[0].0, "h1");
    }

    #[test]
    fn wire_roundtrip_batch() {
        let events: Vec<Event> = (0..5)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i * 1000,
                    Arc::from("src"),
                    PartitionId::new(0),
                    Bytes::from(format!("payload-{i}")),
                )
            })
            .collect();

        let serialized = wire::serialize_events(&events);
        let deserialized = wire::deserialize_events(&serialized).unwrap();

        assert_eq!(deserialized.len(), 5);
        for (i, e) in deserialized.iter().enumerate() {
            assert_eq!(e.timestamp, i as i64 * 1000);
        }
    }

    #[test]
    fn create_and_process_via_abi_simulation() {
        // Simulate what the engine loader does via C-ABI
        let processor = create_passthrough(b"");
        let event = Event::new(
            uuid::Uuid::nil(),
            999,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"test-payload"),
        );
        let outputs = processor.process(event).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].payload.as_ref(), b"test-payload");
    }

    #[test]
    fn empty_event_batch() {
        let serialized = wire::serialize_events(&[]);
        let deserialized = wire::deserialize_events(&serialized).unwrap();
        assert!(deserialized.is_empty());
    }

    #[test]
    fn empty_outputs() {
        let serialized = wire::serialize_outputs(&[]);
        let deserialized = wire::deserialize_outputs(&serialized).unwrap();
        assert!(deserialized.is_empty());
    }
}
