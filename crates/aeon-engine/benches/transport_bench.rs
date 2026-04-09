//! Phase 12b-8 benchmarks — Transport overhead, codec, batch wire, ED25519.
//!
//! Run: `cargo bench -p aeon-engine --bench transport_bench`

use aeon_engine::{InProcessTransport, PassthroughProcessor};
use aeon_types::PartitionId;
use aeon_types::event::{Event, Output};
use aeon_types::processor_transport::ProcessorTier;
use aeon_types::traits::{Processor, ProcessorTransport};
use aeon_types::transport_codec::TransportCodec;
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::sync::Arc;

fn make_events(count: usize) -> Vec<Event> {
    let source: Arc<str> = Arc::from("bench-source");
    let payload = Bytes::from(r#"{"user":"alice","action":"click","ts":1234567890}"#.as_bytes());
    (0..count)
        .map(|i| {
            Event::new(
                uuid::Uuid::nil(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new(0),
                payload.clone(),
            )
        })
        .collect()
}

// ── InProcessTransport overhead (zero-cost wrapper validation) ───────────

fn bench_in_process_transport(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let processor = PassthroughProcessor::new("bench-out".into());
    let transport = InProcessTransport::new(processor, "bench", "1.0", ProcessorTier::Native);

    let mut group = c.benchmark_group("in_process_transport");

    for &batch_size in &[64usize, 256, 1024] {
        let events = make_events(batch_size);
        group.throughput(Throughput::Elements(batch_size as u64));

        // Direct Processor::process_batch (baseline)
        group.bench_with_input(
            BenchmarkId::new("direct_process_batch", batch_size),
            &events,
            |b, events| {
                let proc = PassthroughProcessor::new("bench-out".into());
                b.iter(|| {
                    let _ = proc.process_batch(events.clone());
                });
            },
        );

        // Via ProcessorTransport::call_batch (should be ~identical)
        group.bench_with_input(
            BenchmarkId::new("transport_call_batch", batch_size),
            &events,
            |b, events| {
                b.iter(|| {
                    let _ = rt.block_on(transport.call_batch(events.clone()));
                });
            },
        );
    }
    group.finish();
}

// ── Transport Codec (MsgPack vs JSON) ───────────────────────────────────

fn bench_transport_codec(c: &mut Criterion) {
    let events = make_events(64);

    let mut group = c.benchmark_group("transport_codec");

    for codec in [TransportCodec::MsgPack, TransportCodec::Json] {
        let label = match codec {
            TransportCodec::MsgPack => "msgpack",
            TransportCodec::Json => "json",
        };

        // Encode batch
        group.throughput(Throughput::Elements(64));
        group.bench_function(BenchmarkId::new("encode_64_events", label), |b| {
            b.iter(|| {
                let _ = codec.encode_events(&events);
            });
        });

        // Decode batch
        let encoded = codec.encode_events(&events).unwrap();
        let encoded_refs: Vec<&[u8]> = encoded.iter().map(|v| v.as_slice()).collect();
        group.bench_function(BenchmarkId::new("decode_64_events", label), |b| {
            b.iter(|| {
                let _ = codec.decode_events(&encoded_refs);
            });
        });
    }
    group.finish();
}

// ── Batch Wire Format ───────────────────────────────────────────────────

fn bench_batch_wire(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_wire");

    for &batch_size in &[64usize, 256, 1024] {
        let events = make_events(batch_size);
        let codec = TransportCodec::MsgPack;

        group.throughput(Throughput::Elements(batch_size as u64));

        // Encode batch request
        group.bench_with_input(
            BenchmarkId::new("encode_request", batch_size),
            &events,
            |b, events| {
                b.iter(|| {
                    let _ = aeon_engine::batch_wire::encode_batch_request(1, events, codec);
                });
            },
        );

        // Decode batch request
        let wire = aeon_engine::batch_wire::encode_batch_request(1, &events, codec).unwrap();
        group.bench_with_input(
            BenchmarkId::new("decode_request", batch_size),
            &wire,
            |b, wire| {
                b.iter(|| {
                    let _ = aeon_engine::batch_wire::decode_batch_request(wire, codec);
                });
            },
        );

        // Encode batch response (with passthrough outputs)
        let processor = PassthroughProcessor::new("bench-out".into());
        let outputs: Vec<Vec<Output>> = events
            .iter()
            .map(|e| processor.process(e.clone()).unwrap_or_default())
            .collect();
        let sig = [0u8; 64];
        group.bench_with_input(
            BenchmarkId::new("encode_response", batch_size),
            &outputs,
            |b, outputs| {
                b.iter(|| {
                    let _ = aeon_engine::batch_wire::encode_batch_response(1, outputs, &sig, codec);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_in_process_transport,
    bench_transport_codec,
    bench_batch_wire,
);
criterion_main!(benches);
