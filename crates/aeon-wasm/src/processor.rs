//! WasmProcessor — bridges the Aeon `Processor` trait to Wasm guest modules.
//!
//! The guest exports:
//! - `alloc(size: i32) -> i32` — allocate memory in guest
//! - `dealloc(ptr: i32, size: i32)` — free memory in guest
//! - `process(ptr: i32, len: i32) -> i32` — process event, returns ptr to output
//!
//! The host serializes the Event into a simple binary format, writes it into
//! guest memory via `alloc`, calls `process`, reads back the result, and
//! deserializes the outputs.
//!
//! ## Wire Format (host ↔ guest)
//!
//! Event (host → guest):
//! ```text
//! [16 bytes: event id (UUID)]
//! [8 bytes: timestamp i64 LE]
//! [4 bytes: source len][source bytes]
//! [2 bytes: partition u16 LE]
//! [4 bytes: metadata count]
//!   for each: [4 bytes: key len][key bytes][4 bytes: val len][val bytes]
//! [4 bytes: payload len][payload bytes]
//! ```
//!
//! Output list (guest → host):
//! ```text
//! [4 bytes: output count]
//! for each output:
//!   [4 bytes: destination len][destination bytes]
//!   [1 byte: has_key (0 or 1)]
//!     if has_key: [4 bytes: key len][key bytes]
//!   [4 bytes: payload len][payload bytes]
//!   [4 bytes: header count]
//!     for each: [4 bytes: key len][key bytes][4 bytes: val len][val bytes]
//! ```

use std::sync::{Arc, Mutex};

use aeon_types::AeonError;
use aeon_types::event::{Event, Output};
use bytes::Bytes;
use smallvec::SmallVec;

use crate::runtime::{WasmInstance, WasmModule};

/// A Wasm-backed processor implementing the Aeon `Processor` trait.
pub struct WasmProcessor {
    module: Arc<WasmModule>,
    /// Mutex-protected instance for `&self` compatibility with Processor trait.
    instance: Mutex<WasmInstance>,
}

impl WasmProcessor {
    /// Create a new WasmProcessor from a compiled module.
    pub fn new(module: Arc<WasmModule>) -> Result<Self, AeonError> {
        let instance = module.instantiate()?;
        Ok(Self {
            module,
            instance: Mutex::new(instance),
        })
    }

    /// Process a single event through the Wasm guest.
    pub fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
        let mut instance = self
            .instance
            .lock()
            .map_err(|e| AeonError::processor(format!("lock poisoned: {e}")))?;

        // Reset fuel for each call
        if let Some(fuel) = self.module.config().max_fuel {
            instance.reset_fuel(fuel)?;
        }

        // Serialize event to wire format
        let event_bytes = serialize_event(&event);
        let event_len = event_bytes.len() as i32;

        // Allocate guest memory and write event
        let guest_ptr = instance.call_i32_i32("alloc", event_len)?;
        instance.write_memory(guest_ptr as usize, &event_bytes)?;

        // Call process — returns pointer to length-prefixed output
        let result_ptr = instance.call_2i32_i32("process", guest_ptr, event_len)?;

        // Read result length (first 4 bytes at result_ptr)
        let len_bytes = instance.read_memory(result_ptr as usize, 4)?;
        let result_len =
            u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;

        // Read the full result
        let result_bytes = instance.read_memory(result_ptr as usize + 4, result_len)?;

        // Deallocate guest memory
        let _ = instance.call_2i32_i32("dealloc", guest_ptr, event_len);
        let _ = instance.call_2i32_i32("dealloc", result_ptr, (result_len + 4) as i32);

        // Deserialize outputs, propagating event identity
        deserialize_outputs(&result_bytes, &event)
    }

    /// Process a batch of events.
    pub fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        let mut outputs = Vec::with_capacity(events.len());
        for event in events {
            outputs.extend(self.process(event)?);
        }
        Ok(outputs)
    }
}

/// Serialize an Event to wire format bytes.
fn serialize_event(event: &Event) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);

    // UUID (16 bytes)
    buf.extend_from_slice(event.id.as_bytes());

    // Timestamp (8 bytes LE)
    buf.extend_from_slice(&event.timestamp.to_le_bytes());

    // Source string
    let source_bytes = event.source.as_bytes();
    buf.extend_from_slice(&(source_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(source_bytes);

    // Partition (2 bytes LE)
    buf.extend_from_slice(&event.partition.as_u16().to_le_bytes());

    // Metadata count + entries
    buf.extend_from_slice(&(event.metadata.len() as u32).to_le_bytes());
    for (k, v) in &event.metadata {
        let kb = k.as_bytes();
        let vb = v.as_bytes();
        buf.extend_from_slice(&(kb.len() as u32).to_le_bytes());
        buf.extend_from_slice(kb);
        buf.extend_from_slice(&(vb.len() as u32).to_le_bytes());
        buf.extend_from_slice(vb);
    }

    // Payload
    buf.extend_from_slice(&(event.payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&event.payload);

    buf
}

/// Deserialize output list from wire format bytes.
///
/// Propagates event identity (source_event_id, source_partition, source_ts)
/// from the originating event to every output.
fn deserialize_outputs(data: &[u8], source_event: &Event) -> Result<Vec<Output>, AeonError> {
    let source_ts = source_event.source_ts;
    let mut pos = 0;

    let read_u32 = |pos: &mut usize| -> Result<u32, AeonError> {
        if *pos + 4 > data.len() {
            return Err(AeonError::processor("truncated output: expected u32"));
        }
        let val = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
        *pos += 4;
        Ok(val)
    };

    let read_bytes = |pos: &mut usize, len: usize| -> Result<&[u8], AeonError> {
        if *pos + len > data.len() {
            return Err(AeonError::processor("truncated output: expected bytes"));
        }
        let slice = &data[*pos..*pos + len];
        *pos += len;
        Ok(slice)
    };

    let output_count = read_u32(&mut pos)? as usize;
    let mut outputs = Vec::with_capacity(output_count);

    for _ in 0..output_count {
        // Destination
        let dest_len = read_u32(&mut pos)? as usize;
        let dest_bytes = read_bytes(&mut pos, dest_len)?;
        let destination: Arc<str> = Arc::from(String::from_utf8_lossy(dest_bytes).as_ref());

        // Key (optional)
        if pos >= data.len() {
            return Err(AeonError::processor("truncated output: expected has_key"));
        }
        let has_key = data[pos];
        pos += 1;
        let key = if has_key != 0 {
            let key_len = read_u32(&mut pos)? as usize;
            let key_bytes = read_bytes(&mut pos, key_len)?;
            Some(Bytes::copy_from_slice(key_bytes))
        } else {
            None
        };

        // Payload
        let payload_len = read_u32(&mut pos)? as usize;
        let payload_bytes = read_bytes(&mut pos, payload_len)?;
        let payload = Bytes::copy_from_slice(payload_bytes);

        // Headers
        let header_count = read_u32(&mut pos)? as usize;
        let mut headers = SmallVec::new();
        for _ in 0..header_count {
            let hk_len = read_u32(&mut pos)? as usize;
            let hk = read_bytes(&mut pos, hk_len)?;
            let hv_len = read_u32(&mut pos)? as usize;
            let hv = read_bytes(&mut pos, hv_len)?;
            headers.push((
                Arc::from(String::from_utf8_lossy(hk).as_ref()),
                Arc::from(String::from_utf8_lossy(hv).as_ref()),
            ));
        }

        outputs.push(Output {
            destination,
            key,
            payload,
            headers,
            source_ts,
            source_event_id: Some(source_event.id),
            source_partition: Some(source_event.partition),
            source_offset: None,
            l2_seq: source_event.l2_seq,
        });
    }

    Ok(outputs)
}

impl aeon_types::Processor for WasmProcessor {
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
        WasmProcessor::process(self, event)
    }

    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        WasmProcessor::process_batch(self, events)
    }

    /// TR-2: Export the host-side KV state HashMap so it can be carried into
    /// the new instance on a hot-swap. State is host-side (see
    /// `runtime::HostState.state`), so a swap that doesn't transfer it would
    /// lose all guest state — ValueState, MapState, and any raw
    /// `state::put/get` keys would reset to empty.
    fn snapshot_state(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>, AeonError> {
        let instance = self
            .instance
            .lock()
            .map_err(|e| AeonError::processor(format!("wasm snapshot lock poisoned: {e}")))?;
        Ok(instance
            .host_state()
            .state
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    /// TR-2: Replace the host-side KV state HashMap with the snapshot from the
    /// outgoing processor. Clears any state the fresh instance accrued between
    /// instantiation and this call (there shouldn't be any, because restore
    /// runs before the first event).
    fn restore_state(&self, snapshot: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), AeonError> {
        let mut instance = self
            .instance
            .lock()
            .map_err(|e| AeonError::processor(format!("wasm restore lock poisoned: {e}")))?;
        let host = instance.host_state_mut();
        host.state.clear();
        host.state.reserve(snapshot.len());
        for (k, v) in snapshot {
            host.state.insert(k, v);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::partition::PartitionId;

    fn test_event() -> Event {
        Event::new(
            uuid::Uuid::from_bytes([7u8; 16]),
            42,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"test"),
        )
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let event = Event::new(
            uuid::Uuid::nil(),
            1234567890,
            Arc::from("test-source"),
            PartitionId::new(3),
            Bytes::from_static(b"hello world"),
        )
        .with_metadata(Arc::from("key1"), Arc::from("val1"));

        let serialized = serialize_event(&event);

        // Verify we can at least read back the UUID
        assert_eq!(&serialized[0..16], event.id.as_bytes());
        // Verify timestamp
        let ts = i64::from_le_bytes(serialized[16..24].try_into().unwrap());
        assert_eq!(ts, 1234567890);
    }

    #[test]
    fn deserialize_empty_output_list() {
        // 4 bytes: count = 0
        let data = 0u32.to_le_bytes();
        let outputs = deserialize_outputs(&data, &test_event()).unwrap();
        assert!(outputs.is_empty());
    }

    #[test]
    fn deserialize_single_output() {
        let mut data = Vec::new();

        // Count: 1
        data.extend_from_slice(&1u32.to_le_bytes());

        // Destination: "output"
        data.extend_from_slice(&6u32.to_le_bytes());
        data.extend_from_slice(b"output");

        // No key
        data.push(0);

        // Payload: "result"
        data.extend_from_slice(&6u32.to_le_bytes());
        data.extend_from_slice(b"result");

        // Headers: 0
        data.extend_from_slice(&0u32.to_le_bytes());

        let outputs = deserialize_outputs(&data, &test_event()).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(&*outputs[0].destination, "output");
        assert_eq!(outputs[0].payload.as_ref(), b"result");
        assert!(outputs[0].key.is_none());
    }

    #[test]
    fn deserialize_output_with_key_and_headers() {
        let mut data = Vec::new();

        // Count: 1
        data.extend_from_slice(&1u32.to_le_bytes());

        // Destination: "dest"
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(b"dest");

        // Key: "mykey"
        data.push(1);
        data.extend_from_slice(&5u32.to_le_bytes());
        data.extend_from_slice(b"mykey");

        // Payload: "data"
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(b"data");

        // Headers: 1 (k=h1, v=v1)
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(b"h1");
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(b"v1");

        let outputs = deserialize_outputs(&data, &test_event()).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(&*outputs[0].destination, "dest");
        assert_eq!(outputs[0].key.as_ref().unwrap().as_ref(), b"mykey");
        assert_eq!(outputs[0].payload.as_ref(), b"data");
        assert_eq!(outputs[0].headers.len(), 1);
        assert_eq!(&*outputs[0].headers[0].0, "h1");
        assert_eq!(&*outputs[0].headers[0].1, "v1");
    }

    #[test]
    fn deserialize_truncated_data_returns_error() {
        let data = [0u8; 2]; // Too short for even count
        let result = deserialize_outputs(&data, &test_event());
        assert!(result.is_err());
    }

    /// Full integration test: compile a WAT passthrough processor, call process.
    ///
    /// This WAT module:
    /// 1. Implements alloc/dealloc (bump allocator)
    /// 2. On process: reads the event payload, creates a single output
    ///    with destination="output" containing the same payload
    #[test]
    fn wasm_passthrough_processor() {
        // A WAT module that acts as a passthrough processor.
        // It has a bump allocator and a process function that:
        // - Reads the serialized event
        // - Extracts the payload
        // - Creates a serialized output list with one output containing the payload
        let wat = r#"
            (module
                ;; Bump allocator — resets on each alloc (host consumes before next call)
                (global $bump (mut i32) (i32.const 65536))

                (func (export "alloc") (param $size i32) (result i32)
                    (local $ptr i32)
                    (global.set $bump (i32.const 65536))
                    (local.set $ptr (global.get $bump))
                    (global.set $bump (i32.add (global.get $bump) (local.get $size)))
                    (local.get $ptr)
                )

                (func (export "dealloc") (param $ptr i32) (param $size i32)
                    ;; no-op bump allocator
                )

                ;; process(ptr, len) -> ptr
                ;; Reads serialized event, extracts payload, returns serialized output list.
                ;; Output format: [4:result_len][4:count=1][4:dest_len]["output"][1:no_key][4:payload_len][payload][4:0 headers]
                (func (export "process") (param $ptr i32) (param $len i32) (result i32)
                    (local $pos i32)
                    (local $src_len i32)
                    (local $meta_count i32)
                    (local $meta_i i32)
                    (local $skip_len i32)
                    (local $payload_len i32)
                    (local $payload_ptr i32)
                    (local $out_ptr i32)
                    (local $write_pos i32)
                    (local $result_content_len i32)

                    ;; pos = ptr + 16 (skip UUID) + 8 (skip timestamp) = ptr + 24
                    (local.set $pos (i32.add (local.get $ptr) (i32.const 24)))

                    ;; Read source length and skip source
                    (local.set $src_len (i32.load (local.get $pos)))
                    (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $src_len))))

                    ;; Skip partition (2 bytes)
                    (local.set $pos (i32.add (local.get $pos) (i32.const 2)))

                    ;; Read metadata count
                    (local.set $meta_count (i32.load (local.get $pos)))
                    (local.set $pos (i32.add (local.get $pos) (i32.const 4)))

                    ;; Skip all metadata entries
                    (local.set $meta_i (i32.const 0))
                    (block $break_meta
                        (loop $loop_meta
                            (br_if $break_meta (i32.ge_u (local.get $meta_i) (local.get $meta_count)))
                            ;; skip key
                            (local.set $skip_len (i32.load (local.get $pos)))
                            (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $skip_len))))
                            ;; skip value
                            (local.set $skip_len (i32.load (local.get $pos)))
                            (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $skip_len))))
                            (local.set $meta_i (i32.add (local.get $meta_i) (i32.const 1)))
                            (br $loop_meta)
                        )
                    )

                    ;; Read payload length and pointer
                    (local.set $payload_len (i32.load (local.get $pos)))
                    (local.set $payload_ptr (i32.add (local.get $pos) (i32.const 4)))

                    ;; Calculate output size:
                    ;; content = 4(count) + 4(dest_len) + 6("output") + 1(no_key) + 4(payload_len) + payload + 4(header_count)
                    ;; = 23 + payload_len
                    (local.set $result_content_len (i32.add (i32.const 23) (local.get $payload_len)))

                    ;; Allocate output buffer: 4 (length prefix) + content
                    (local.set $out_ptr (call $bump_alloc (i32.add (i32.const 4) (local.get $result_content_len))))
                    (local.set $write_pos (local.get $out_ptr))

                    ;; Write result length prefix
                    (i32.store (local.get $write_pos) (local.get $result_content_len))
                    (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

                    ;; Write output count = 1
                    (i32.store (local.get $write_pos) (i32.const 1))
                    (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

                    ;; Write destination = "output" (len=6)
                    (i32.store (local.get $write_pos) (i32.const 6))
                    (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))
                    ;; Write "output" byte by byte
                    (i32.store8 (local.get $write_pos) (i32.const 111)) ;; 'o'
                    (i32.store8 (i32.add (local.get $write_pos) (i32.const 1)) (i32.const 117)) ;; 'u'
                    (i32.store8 (i32.add (local.get $write_pos) (i32.const 2)) (i32.const 116)) ;; 't'
                    (i32.store8 (i32.add (local.get $write_pos) (i32.const 3)) (i32.const 112)) ;; 'p'
                    (i32.store8 (i32.add (local.get $write_pos) (i32.const 4)) (i32.const 117)) ;; 'u'
                    (i32.store8 (i32.add (local.get $write_pos) (i32.const 5)) (i32.const 116)) ;; 't'
                    (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 6)))

                    ;; No key
                    (i32.store8 (local.get $write_pos) (i32.const 0))
                    (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 1)))

                    ;; Payload length
                    (i32.store (local.get $write_pos) (local.get $payload_len))
                    (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

                    ;; Copy payload bytes
                    (memory.copy
                        (local.get $write_pos)
                        (local.get $payload_ptr)
                        (local.get $payload_len)
                    )
                    (local.set $write_pos (i32.add (local.get $write_pos) (local.get $payload_len)))

                    ;; Header count = 0
                    (i32.store (local.get $write_pos) (i32.const 0))

                    ;; Return pointer to the output buffer
                    (local.get $out_ptr)
                )

                ;; Internal bump allocator (same as exported but callable internally)
                (func $bump_alloc (param $size i32) (result i32)
                    (local $ptr i32)
                    (local.set $ptr (global.get $bump))
                    (global.set $bump (i32.add (global.get $bump) (local.get $size)))
                    (local.get $ptr)
                )

                (memory (export "memory") 2)
            )
        "#;

        let module = WasmModule::from_wat(wat)
            .map_err(|e| format!("compile: {e}"))
            .unwrap();
        let processor = WasmProcessor::new(Arc::new(module)).unwrap();

        let event = Event::new(
            uuid::Uuid::nil(),
            42,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"hello wasm"),
        );

        let outputs = processor.process(event).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(&*outputs[0].destination, "output");
        assert_eq!(outputs[0].payload.as_ref(), b"hello wasm");
        assert!(outputs[0].key.is_none());
        assert!(outputs[0].headers.is_empty());
    }

    #[test]
    fn wasm_processor_batch() {
        let wat = r#"
            (module
                (global $bump (mut i32) (i32.const 65536))

                (func (export "alloc") (param $size i32) (result i32)
                    (local $ptr i32)
                    (global.set $bump (i32.const 65536))
                    (local.set $ptr (global.get $bump))
                    (global.set $bump (i32.add (global.get $bump) (local.get $size)))
                    (local.get $ptr)
                )

                (func (export "dealloc") (param $ptr i32) (param $size i32))

                ;; Returns empty output list (filter/drop processor)
                (func (export "process") (param $ptr i32) (param $len i32) (result i32)
                    (local $out_ptr i32)
                    ;; Allocate 8 bytes: 4 for length prefix + 4 for count=0
                    (local.set $out_ptr (call $bump_alloc (i32.const 8)))
                    ;; result_len = 4 (just the count field)
                    (i32.store (local.get $out_ptr) (i32.const 4))
                    ;; count = 0
                    (i32.store (i32.add (local.get $out_ptr) (i32.const 4)) (i32.const 0))
                    (local.get $out_ptr)
                )

                (func $bump_alloc (param $size i32) (result i32)
                    (local $ptr i32)
                    (local.set $ptr (global.get $bump))
                    (global.set $bump (i32.add (global.get $bump) (local.get $size)))
                    (local.get $ptr)
                )

                (memory (export "memory") 2)
            )
        "#;

        let module = WasmModule::from_wat(wat).unwrap();
        let processor = WasmProcessor::new(Arc::new(module)).unwrap();

        let events: Vec<Event> = (0..3)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i,
                    Arc::from("test"),
                    PartitionId::new(0),
                    Bytes::from(format!("event-{i}")),
                )
            })
            .collect();

        // This filter/drop processor returns 0 outputs per event
        let outputs = processor.process_batch(events).unwrap();
        assert!(outputs.is_empty());
    }

    /// Reproduce C2 bug: 200+ events through passthrough WAT (Kafka-like, no metadata).
    #[test]
    fn wasm_passthrough_many_events_no_metadata() {
        let wat = include_str!("../../aeon-engine/tests/e2e_wasm_passthrough.wat");
        let module = WasmModule::from_wat(wat).unwrap();
        let processor = WasmProcessor::new(Arc::new(module)).unwrap();

        for i in 0..300 {
            let event = Event::new(
                uuid::Uuid::now_v7(),
                i * 1_000_000,
                Arc::from("c2-source"),
                PartitionId::new((i % 4) as u16),
                Bytes::from(format!("test-event-{i}")),
            );

            let outputs = processor
                .process(event)
                .unwrap_or_else(|e| panic!("process failed at event {i}: {e}"));
            assert_eq!(outputs.len(), 1, "event {i}: expected 1 output");
            assert_eq!(
                outputs[0].payload.as_ref(),
                format!("test-event-{i}").as_bytes(),
                "event {i}: payload mismatch"
            );
        }
    }

    /// Stress test: 5000 events to verify bump allocator reset prevents OOM.
    #[test]
    fn wasm_passthrough_stress_5000_events() {
        let wat = include_str!("../../aeon-engine/tests/e2e_wasm_passthrough.wat");
        let module = WasmModule::from_wat(wat).unwrap();
        let processor = WasmProcessor::new(Arc::new(module)).unwrap();

        for i in 0..5000 {
            let event = Event::new(
                uuid::Uuid::now_v7(),
                1712534400000_i64 * 1_000_000,
                Arc::from("stress-source"),
                PartitionId::new(0),
                Bytes::from(format!("stress-payload-{i}")),
            );

            processor
                .process(event)
                .unwrap_or_else(|e| panic!("process failed at event {i}: {e}"));
        }
    }

    /// Reproduce C2 bug variant: events WITH metadata (like Kafka headers).
    #[test]
    fn wasm_passthrough_many_events_with_metadata() {
        let wat = include_str!("../../aeon-engine/tests/e2e_wasm_passthrough.wat");
        let module = WasmModule::from_wat(wat).unwrap();
        let processor = WasmProcessor::new(Arc::new(module)).unwrap();

        for i in 0..300 {
            let mut event = Event::new(
                uuid::Uuid::now_v7(),
                1712534400000_i64 * 1_000_000, // realistic Kafka timestamp (ns)
                Arc::from("c2-source"),
                PartitionId::new(0),
                Bytes::from(format!("test-event-{i}")),
            );
            // Add metadata like Kafka headers would
            event
                .metadata
                .push((Arc::from("kafka-key"), Arc::from("kafka-value")));
            event
                .metadata
                .push((Arc::from("x-trace-id"), Arc::from("abc123def456")));

            let outputs = processor
                .process(event)
                .unwrap_or_else(|e| panic!("process failed at event {i}: {e}"));
            assert_eq!(outputs.len(), 1, "event {i}: expected 1 output");
            assert_eq!(
                outputs[0].payload.as_ref(),
                format!("test-event-{i}").as_bytes(),
                "event {i}: payload mismatch"
            );
        }
    }

    /// TR-2 — snapshot on one instance, restore on another, verify all keys
    /// survive the transfer. Uses the minimal passthrough WAT so we don't
    /// care about processor correctness, only that the host-side HashMap
    /// round-trips through the Processor trait.
    #[test]
    fn tr2_snapshot_restore_roundtrip() {
        use crate::runtime::WasmModule;
        use aeon_types::Processor as ProcessorTrait;

        // A WAT that only needs to instantiate successfully — we never call
        // process(). We just drive snapshot_state / restore_state via the
        // HostState HashMap underneath.
        const MINIMAL_WAT: &str = r#"
            (module
                (import "env" "state_put" (func $state_put (param i32 i32 i32 i32)))
                (import "env" "state_get" (func $state_get (param i32 i32 i32 i32) (result i32)))
                (import "env" "state_delete" (func $state_delete (param i32 i32)))
                (memory (export "memory") 1)
            )
        "#;

        let module = Arc::new(WasmModule::from_wat(MINIMAL_WAT).unwrap());
        let from = WasmProcessor::new(Arc::clone(&module)).unwrap();
        let to = WasmProcessor::new(Arc::clone(&module)).unwrap();

        // Seed state directly on the outgoing instance's HostState.
        {
            let mut inst = from.instance.lock().unwrap();
            let hs = inst.host_state_mut();
            hs.state.insert(b"counter".to_vec(), b"42".to_vec());
            hs.state.insert(b"name".to_vec(), b"alice".to_vec());
            hs.state.insert(b"blob".to_vec(), vec![0xFF; 128]);
        }

        let snap = ProcessorTrait::snapshot_state(&from).unwrap();
        assert_eq!(snap.len(), 3);

        // Fresh instance starts empty.
        {
            let inst = to.instance.lock().unwrap();
            assert!(inst.host_state().state.is_empty());
        }

        ProcessorTrait::restore_state(&to, snap).unwrap();

        // After restore the incoming instance must hold exactly the same KVs.
        let inst = to.instance.lock().unwrap();
        let state = &inst.host_state().state;
        assert_eq!(state.len(), 3);
        assert_eq!(state.get(b"counter".as_ref()).unwrap().as_slice(), b"42");
        assert_eq!(state.get(b"name".as_ref()).unwrap().as_slice(), b"alice");
        assert_eq!(state.get(b"blob".as_ref()).unwrap().as_slice(), &[0xFF; 128][..]);
    }

    /// TR-2 — restore replaces, does not merge. A key present on the target
    /// but absent from the snapshot must be gone after restore.
    #[test]
    fn tr2_restore_replaces_existing_state() {
        use crate::runtime::WasmModule;
        use aeon_types::Processor as ProcessorTrait;

        const MINIMAL_WAT: &str = r#"
            (module
                (import "env" "state_put" (func $state_put (param i32 i32 i32 i32)))
                (import "env" "state_get" (func $state_get (param i32 i32 i32 i32) (result i32)))
                (import "env" "state_delete" (func $state_delete (param i32 i32)))
                (memory (export "memory") 1)
            )
        "#;

        let module = Arc::new(WasmModule::from_wat(MINIMAL_WAT).unwrap());
        let to = WasmProcessor::new(Arc::clone(&module)).unwrap();

        // Pre-populate the target with a stale key.
        {
            let mut inst = to.instance.lock().unwrap();
            inst.host_state_mut()
                .state
                .insert(b"stale".to_vec(), b"old".to_vec());
        }

        let snap = vec![(b"fresh".to_vec(), b"new".to_vec())];
        ProcessorTrait::restore_state(&to, snap).unwrap();

        let inst = to.instance.lock().unwrap();
        let state = &inst.host_state().state;
        assert_eq!(state.len(), 1);
        assert!(!state.contains_key(b"stale".as_ref()));
        assert_eq!(state.get(b"fresh".as_ref()).unwrap().as_slice(), b"new");
    }

    /// TR-2 — default Processor trait impls are stateless. A custom processor
    /// that doesn't override gets an empty snapshot and a no-op restore.
    #[test]
    fn tr2_stateless_processor_defaults() {
        use aeon_types::Processor as ProcessorTrait;

        struct Stateless;
        impl ProcessorTrait for Stateless {
            fn process(&self, _e: Event) -> Result<Vec<Output>, AeonError> {
                Ok(Vec::new())
            }
        }

        let p = Stateless;
        let snap = ProcessorTrait::snapshot_state(&p).unwrap();
        assert!(snap.is_empty());
        // Non-empty snapshot goes in, no-op out — just verifies no panic.
        ProcessorTrait::restore_state(&p, vec![(b"k".to_vec(), b"v".to_vec())]).unwrap();
    }
}
