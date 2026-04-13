//! Built-in processors.

use aeon_types::{AeonError, Event, Output, Processor};
use std::sync::Arc;

/// Identity processor — passes events through unchanged.
///
/// Converts each `Event` into an `Output` with the same payload.
/// Used for benchmarking the pipeline overhead (source→sink) without
/// any processing logic.
pub struct PassthroughProcessor {
    destination: Arc<str>,
}

impl PassthroughProcessor {
    pub fn new(destination: Arc<str>) -> Self {
        Self { destination }
    }
}

impl Processor for PassthroughProcessor {
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
        // clone: Arc<str> refcount bump — destination is shared across all
        //        outputs this processor emits.
        // clone: Bytes refcount bump (zero-copy) — payload is shared with the
        //        source event; event.payload is still borrowed by
        //        with_event_identity immediately after.
        Ok(vec![
            Output::new(Arc::clone(&self.destination), event.payload.clone())
                .with_event_identity(&event),
        ])
    }

    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        let mut outputs = Vec::with_capacity(events.len());
        for event in events {
            // clone × 2: both refcount bumps — see process() above for rationale.
            outputs.push(
                Output::new(Arc::clone(&self.destination), event.payload.clone())
                    .with_event_identity(&event),
            );
        }
        Ok(outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::PartitionId;
    use bytes::Bytes;

    #[test]
    fn passthrough_preserves_payload() {
        let proc = PassthroughProcessor::new(Arc::from("output"));
        let event = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("src"),
            PartitionId::new(0),
            Bytes::from_static(b"hello"),
        );
        let outputs = proc.process(event).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].payload.as_ref(), b"hello");
        assert_eq!(&*outputs[0].destination, "output");
    }

    #[test]
    fn passthrough_batch() {
        let proc = PassthroughProcessor::new(Arc::from("out"));
        let events: Vec<Event> = (0..100)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i,
                    Arc::from("src"),
                    PartitionId::new(0),
                    Bytes::from(format!("e{i}")),
                )
            })
            .collect();
        let outputs = proc.process_batch(events).unwrap();
        assert_eq!(outputs.len(), 100);
        assert_eq!(outputs[99].payload.as_ref(), b"e99");
    }
}
