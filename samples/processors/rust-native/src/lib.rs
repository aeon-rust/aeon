//! Rust-native JSON enrichment processor.
//!
//! Extracts the "user_id" field from a JSON payload using SIMD-accelerated
//! byte scanning (memchr), enriches the output with a "processed_by" header,
//! and routes to "enriched" destination.
//!
//! This demonstrates the fastest possible processor — zero Wasm overhead,
//! direct trait implementation, zero-copy where possible.

use std::sync::Arc;

use aeon_types::event::{Event, Output};
use aeon_types::{AeonError, Processor, json_field_value};
use bytes::Bytes;

/// A processor that extracts a JSON field and enriches the output.
pub struct JsonEnrichProcessor {
    /// The JSON field to extract.
    field: Arc<str>,
    /// Output destination.
    destination: Arc<str>,
    /// Header value for provenance tracking.
    processor_id: Arc<str>,
}

impl JsonEnrichProcessor {
    pub fn new(field: &str, destination: &str, processor_id: &str) -> Self {
        Self {
            field: Arc::from(field),
            destination: Arc::from(destination),
            processor_id: Arc::from(processor_id),
        }
    }
}

impl Processor for JsonEnrichProcessor {
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
        // Extract the field value using SIMD-accelerated scanner
        let field_value = json_field_value(&self.field, &event.payload);

        // Build enriched output payload
        let enriched = match field_value {
            Some(value) => {
                // Strip surrounding quotes if present
                let stripped =
                    if value.len() >= 2 && value[0] == b'"' && value[value.len() - 1] == b'"' {
                        &value[1..value.len() - 1]
                    } else {
                        value
                    };
                // Found the field — build enriched JSON
                let mut buf = Vec::with_capacity(event.payload.len() + 64);
                buf.extend_from_slice(b"{\"original\":");
                buf.extend_from_slice(&event.payload);
                buf.extend_from_slice(b",\"extracted_");
                buf.extend_from_slice(self.field.as_bytes());
                buf.extend_from_slice(b"\":\"");
                buf.extend_from_slice(stripped);
                buf.extend_from_slice(b"\"}");
                Bytes::from(buf)
            }
            None => {
                // Field not found — pass through original payload
                event.payload.clone()
            }
        };

        Ok(vec![
            Output::new(Arc::clone(&self.destination), enriched)
                .with_header(Arc::from("processed-by"), Arc::clone(&self.processor_id))
                .with_event_identity(&event),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::partition::PartitionId;

    fn make_event(payload: &[u8]) -> Event {
        Event::new(
            uuid::Uuid::nil(),
            1234567890,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from(payload.to_vec()),
        )
    }

    #[test]
    fn extracts_field_and_enriches() {
        let proc = JsonEnrichProcessor::new("user_id", "enriched", "rust-native-v1");
        let event = make_event(b"{\"user_id\":\"u-42\",\"action\":\"click\"}");
        let outputs = proc.process(event).unwrap();

        assert_eq!(outputs.len(), 1);
        assert_eq!(&*outputs[0].destination, "enriched");

        let payload = std::str::from_utf8(&outputs[0].payload).unwrap();
        assert!(payload.contains("\"extracted_user_id\":\"u-42\""));
        assert!(payload.contains("\"original\":"));

        // Check headers — source-event-id is now a structural field, not a header
        assert_eq!(outputs[0].headers.len(), 1);
        assert_eq!(&*outputs[0].headers[0].0, "processed-by");
        assert_eq!(&*outputs[0].headers[0].1, "rust-native-v1");

        // Event identity propagated structurally
        assert_eq!(outputs[0].source_event_id, Some(uuid::Uuid::nil()));
        assert_eq!(outputs[0].source_partition, Some(PartitionId::new(0)));
    }

    #[test]
    fn missing_field_passes_through() {
        let proc = JsonEnrichProcessor::new("user_id", "enriched", "rust-native-v1");
        let event = make_event(b"{\"action\":\"click\"}");
        let outputs = proc.process(event).unwrap();

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].payload.as_ref(), b"{\"action\":\"click\"}");
    }

    #[test]
    fn batch_processing() {
        let proc = JsonEnrichProcessor::new("user_id", "enriched", "rust-native-v1");
        let events: Vec<Event> = (0..100)
            .map(|i| make_event(format!("{{\"user_id\":\"u-{i}\",\"data\":\"x\"}}").as_bytes()))
            .collect();

        let outputs = proc.process_batch(events).unwrap();
        assert_eq!(outputs.len(), 100);
    }
}
