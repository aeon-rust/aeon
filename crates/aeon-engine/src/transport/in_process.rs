//! In-process transport adapter (T1 Native, T2 Wasm).
//!
//! Wraps a sync `Processor` into the async `ProcessorTransport` trait with
//! zero overhead — `call_batch()` resolves immediately (no real await point).

use aeon_types::error::AeonError;
use aeon_types::event::{Event, Output};
use aeon_types::processor_transport::{ProcessorHealth, ProcessorInfo, ProcessorTier};
use aeon_types::traits::{Processor, ProcessorTransport};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

/// Adapter that wraps a sync `Processor` into the async `ProcessorTransport` trait.
///
/// Used for T1 (Native) and T2 (Wasm) processors that run in-process.
/// `call_batch()` delegates to `Processor::process_batch()` and resolves
/// immediately — no network round-trip, no real await point.
pub struct InProcessTransport<P: Processor> {
    processor: Arc<P>,
    info: ProcessorInfo,
    created_at: Instant,
}

impl<P: Processor> InProcessTransport<P> {
    /// Create a new in-process transport wrapping the given processor.
    pub fn new(
        processor: P,
        name: impl Into<String>,
        version: impl Into<String>,
        tier: ProcessorTier,
    ) -> Self {
        Self {
            processor: Arc::new(processor),
            info: ProcessorInfo {
                name: name.into(),
                version: version.into(),
                tier,
                capabilities: vec!["batch".into()],
            },
            created_at: Instant::now(),
        }
    }
}

impl<P: Processor + 'static> ProcessorTransport for InProcessTransport<P> {
    fn call_batch(
        &self,
        events: Vec<Event>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Output>, AeonError>> + Send + '_>> {
        Box::pin(async move { self.processor.process_batch(events) })
    }

    fn health(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessorHealth, AeonError>> + Send + '_>> {
        let uptime = self.created_at.elapsed().as_secs();
        Box::pin(async move {
            Ok(ProcessorHealth {
                healthy: true,
                latency_us: None,
                pending_batches: None,
                uptime_secs: Some(uptime),
            })
        })
    }

    fn drain(&self) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + '_>> {
        // In-process processors have no in-flight network work to drain.
        Box::pin(async { Ok(()) })
    }

    fn info(&self) -> ProcessorInfo {
        self.info.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::event::Event;
    use aeon_types::partition::PartitionId;
    use bytes::Bytes;

    struct TestProcessor;

    impl Processor for TestProcessor {
        fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
            Ok(vec![Output::new(Arc::from("out"), event.payload.clone())])
        }
    }

    struct FailProcessor;

    impl Processor for FailProcessor {
        fn process(&self, _event: Event) -> Result<Vec<Output>, AeonError> {
            Err(AeonError::processor("fail"))
        }
    }

    fn make_event(payload: &[u8]) -> Event {
        Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::copy_from_slice(payload),
        )
    }

    #[tokio::test]
    async fn call_batch_passthrough() {
        let transport =
            InProcessTransport::new(TestProcessor, "test-proc", "1.0.0", ProcessorTier::Native);
        let events = vec![make_event(b"hello"), make_event(b"world")];
        let outputs = transport.call_batch(events).await.unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].payload.as_ref(), b"hello");
        assert_eq!(outputs[1].payload.as_ref(), b"world");
    }

    #[tokio::test]
    async fn call_batch_empty() {
        let transport =
            InProcessTransport::new(TestProcessor, "test-proc", "1.0.0", ProcessorTier::Wasm);
        let outputs = transport.call_batch(vec![]).await.unwrap();
        assert!(outputs.is_empty());
    }

    #[tokio::test]
    async fn call_batch_error_propagation() {
        let transport =
            InProcessTransport::new(FailProcessor, "fail-proc", "1.0.0", ProcessorTier::Native);
        let result = transport.call_batch(vec![make_event(b"x")]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn health_always_healthy() {
        let transport =
            InProcessTransport::new(TestProcessor, "test-proc", "1.0.0", ProcessorTier::Native);
        let health = transport.health().await.unwrap();
        assert!(health.healthy);
        assert!(health.uptime_secs.is_some());
    }

    #[tokio::test]
    async fn drain_is_noop() {
        let transport =
            InProcessTransport::new(TestProcessor, "test-proc", "1.0.0", ProcessorTier::Native);
        transport.drain().await.unwrap();
    }

    #[tokio::test]
    async fn info_returns_metadata() {
        let transport =
            InProcessTransport::new(TestProcessor, "my-proc", "2.5.0", ProcessorTier::Wasm);
        let info = transport.info();
        assert_eq!(info.name, "my-proc");
        assert_eq!(info.version, "2.5.0");
        assert_eq!(info.tier, ProcessorTier::Wasm);
        assert!(info.capabilities.contains(&"batch".to_string()));
    }

    /// Compile-time proof that InProcessTransport can be used as &dyn ProcessorTransport.
    #[tokio::test]
    async fn dyn_dispatch() {
        let transport =
            InProcessTransport::new(TestProcessor, "dyn-test", "1.0.0", ProcessorTier::Native);
        let dyn_ref: &dyn ProcessorTransport = &transport;
        let outputs = dyn_ref.call_batch(vec![make_event(b"dyn")]).await.unwrap();
        assert_eq!(outputs.len(), 1);
    }
}
