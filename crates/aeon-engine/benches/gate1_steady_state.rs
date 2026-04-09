//! Gate 1 Steady-State P99 Latency Measurement.
//!
//! Unlike `gate1_validation.rs` which pre-produces all events and then drains
//! (measuring catch-up latency under saturation), this bench runs a rate-limited
//! producer **concurrently** with the pipeline so latency is measured at a
//! sustainable target rate below saturation.
//!
//! Methodology:
//!   1. Create a fresh timestamped topic (isolation from prior bench runs).
//!   2. Start pipeline with SPSC ring buffers.
//!   3. Start rate-limited producer at AEON_BENCH_TARGET_RATE events/sec.
//!   4. Run for AEON_BENCH_WARMUP_SECS seconds, then reset histogram.
//!   5. Run for AEON_BENCH_MEASURE_SECS seconds recording latency.
//!   6. Stop producer, wait drain grace period, signal shutdown.
//!   7. Report P50/P95/P99 against the <10ms Gate 1 target.
//!
//! Config via env:
//!   AEON_BENCH_TARGET_RATE    Events/sec (default: 20000)
//!   AEON_BENCH_WARMUP_SECS    Warmup before measurement (default: 3)
//!   AEON_BENCH_MEASURE_SECS   Measurement window (default: 10)
//!   AEON_BENCH_PAYLOAD_SIZE   Bytes per event (default: 256)
//!   AEON_BENCH_BROKERS        Redpanda brokers (default: localhost:19092)
//!
//! Requires: Redpanda at localhost:19092.
//! Run: cargo bench -p aeon-engine --bench gate1_steady_state

use aeon_connectors::BlackholeSink;
use aeon_connectors::kafka::{KafkaSinkConfig, KafkaSource, KafkaSourceConfig};
use aeon_engine::{PassthroughProcessor, PipelineConfig, PipelineMetrics, run_buffered};
use aeon_observability::LatencyHistogram;
use aeon_types::{AeonError, DeliveryStrategy, Event, Output, Processor};
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::client::DefaultClientContext;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Processor wrapper that records end-to-end latency (source read → process time).
struct LatencyMeasuringProcessor {
    inner: PassthroughProcessor,
    histogram: Arc<LatencyHistogram>,
}

impl LatencyMeasuringProcessor {
    fn new(inner: PassthroughProcessor, histogram: Arc<LatencyHistogram>) -> Self {
        Self { inner, histogram }
    }
}

impl Processor for LatencyMeasuringProcessor {
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
        if let Some(ts) = event.source_ts {
            let elapsed = Instant::now().duration_since(ts).as_nanos() as u64;
            self.histogram.record_ns(elapsed);
        }
        self.inner.process(event)
    }

    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        let now = Instant::now();
        for event in &events {
            if let Some(ts) = event.source_ts {
                let elapsed = now.duration_since(ts).as_nanos() as u64;
                self.histogram.record_ns(elapsed);
            }
        }
        self.inner.process_batch(events)
    }
}

fn brokers() -> String {
    std::env::var("AEON_BENCH_BROKERS").unwrap_or_else(|_| "localhost:19092".to_string())
}

/// Create topics with 16 partitions. Auto-create would default to 1 partition,
/// leaving the 16-partition source assignment with nothing to read from.
fn create_topics(topics: &[&str], partitions: i32) {
    let admin: AdminClient<DefaultClientContext> = ClientConfig::new()
        .set("bootstrap.servers", brokers())
        .create()
        .expect("admin client");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let new_topics: Vec<NewTopic> = topics
        .iter()
        .map(|name| NewTopic::new(name, partitions, TopicReplication::Fixed(1)))
        .collect();

    rt.block_on(async {
        let opts = AdminOptions::new().request_timeout(Some(Duration::from_secs(10)));
        match admin.create_topics(&new_topics, &opts).await {
            Ok(results) => {
                for r in results {
                    match r {
                        Ok(name) => println!("  Created topic: {name} ({partitions} partitions)"),
                        Err((name, code)) => {
                            println!("  Topic create warning for {name}: {code:?}");
                        }
                    }
                }
            }
            Err(e) => panic!("admin create_topics failed: {e}"),
        }
    });
}

fn make_source(topic: &str, batch_size: usize, drain_ms: u64, partitions: i32) -> KafkaSource {
    // Generous max_empty_polls so occasional producer lulls don't terminate the source.
    // The bench uses explicit shutdown signaling instead.
    //
    // Aggressive fetch tuning for low-latency: broker returns as soon as any byte
    // is available (fetch.min.bytes=1) and waits at most 1ms (fetch.wait.max.ms=1)
    // before flushing a fetch. Default is 500ms which adds unacceptable tail latency.
    let config = KafkaSourceConfig::new(brokers(), topic.to_string())
        .with_partitions((0..partitions).collect())
        .with_batch_max(batch_size)
        .with_poll_timeout(Duration::from_millis(500))
        .with_drain_timeout(Duration::from_millis(drain_ms))
        .with_source_name("bench-steady")
        .with_max_empty_polls(1000)
        .with_config("fetch.wait.max.ms", "1")
        .with_config("fetch.min.bytes", "1")
        .with_config("queued.min.messages", "10000");

    KafkaSource::new(config).expect("source")
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // Check Redpanda availability
    let available: Result<BaseProducer, _> = ClientConfig::new()
        .set("bootstrap.servers", brokers())
        .set("message.timeout.ms", "5000")
        .create();

    if available.is_err() {
        eprintln!("SKIP: Redpanda not available at {}", brokers());
        return;
    }
    drop(available);

    let target_rate: u64 = std::env::var("AEON_BENCH_TARGET_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let warmup_secs: u64 = std::env::var("AEON_BENCH_WARMUP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let measure_secs: u64 = std::env::var("AEON_BENCH_MEASURE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let payload_size: usize = std::env::var("AEON_BENCH_PAYLOAD_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let drain_ms: u64 = std::env::var("AEON_BENCH_DRAIN_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let linger_ms: u64 = std::env::var("AEON_BENCH_LINGER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // When AEON_BENCH_BLACKHOLE_SINK=1, use BlackholeSink instead of KafkaSink.
    // This isolates Aeon pipeline latency from sink-side network/broker overhead.
    let use_blackhole_sink = std::env::var("AEON_BENCH_BLACKHOLE_SINK")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // Partition count on source/sink topics. At low target rates (e.g., 1K/sec)
    // using 16 partitions gives per-partition rates too low for source-side
    // batching (drain_ms) to aggregate events effectively. Match this to the
    // target rate: rule of thumb is partitions ≈ target_rate / 5000.
    let partitions: i32 = std::env::var("AEON_BENCH_PARTITIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);

    // Fresh topics per run so leftover events from prior benches don't skew latency.
    let run_tag = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let source_topic = format!("aeon-bench-steady-src-{run_tag}");
    let sink_topic = format!("aeon-bench-steady-snk-{run_tag}");

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║        GATE 1 STEADY-STATE — P99 LATENCY MEASUREMENT    ║");
    println!("╚══════════════════════════════════════════════════════════╝\n");
    println!("Config:");
    println!("  Target rate:  {target_rate} events/sec");
    println!("  Warmup:       {warmup_secs}s");
    println!("  Measure:      {measure_secs}s");
    println!("  Payload:      {payload_size}B");
    println!("  Drain ms:     {drain_ms}  (source batching delay)");
    println!("  Linger ms:    {linger_ms}  (sink batching delay)");
    println!("  Partitions:   {partitions}");
    println!(
        "  Sink:         {}",
        if use_blackhole_sink {
            "BlackholeSink (Aeon pipeline isolation)"
        } else {
            "KafkaSink → Redpanda"
        }
    );
    println!("  Source topic: {source_topic}");
    println!("  Sink topic:   {sink_topic}");
    println!("  Broker:       {}\n", brokers());

    // Pre-create topics with the requested partition count (auto-create would use 1).
    // Only source topic is needed when using blackhole sink.
    if use_blackhole_sink {
        create_topics(&[&source_topic], partitions);
    } else {
        create_topics(&[&source_topic, &sink_topic], partitions);
    }
    println!();

    let histogram = Arc::new(LatencyHistogram::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let producer_done = Arc::new(AtomicBool::new(false));
    let events_produced = Arc::new(AtomicU64::new(0));

    let total_runtime_secs = warmup_secs + measure_secs;

    // ── Spawn rate-limited producer in a blocking thread ──
    let produced_counter = Arc::clone(&events_produced);
    let prod_done = Arc::clone(&producer_done);
    let prod_topic = source_topic.clone();
    let producer_duration = Arc::new(std::sync::Mutex::new(Duration::ZERO));
    let producer_duration_writer = Arc::clone(&producer_duration);
    let producer_handle = std::thread::spawn(move || {
        let producer: BaseProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers())
            .set("message.timeout.ms", "30000")
            .set("queue.buffering.max.messages", "1000000")
            .set("queue.buffering.max.kbytes", "1048576")
            .set("batch.num.messages", "10000")
            .set("linger.ms", "5")
            .create()
            .expect("producer creation");

        let payload = vec![b'x'; payload_size];
        let num_keys = partitions as usize;
        let keys: Vec<String> = (0..num_keys).map(|i| format!("{i}")).collect();

        let start = Instant::now();
        let mut produced: u64 = 0;

        while start.elapsed().as_secs() < total_runtime_secs {
            let elapsed = start.elapsed().as_secs_f64();
            let expected = (elapsed * target_rate as f64) as u64;

            if produced < expected {
                let batch = (expected - produced).min(256);
                for _ in 0..batch {
                    loop {
                        match producer.send(
                            BaseRecord::to(&prod_topic)
                                .payload(&payload)
                                .key(keys[(produced as usize) % num_keys].as_bytes()),
                        ) {
                            Ok(()) => break,
                            Err((
                                rdkafka::error::KafkaError::MessageProduction(
                                    rdkafka::types::RDKafkaErrorCode::QueueFull,
                                ),
                                _,
                            )) => {
                                producer.poll(Duration::from_millis(1));
                            }
                            Err((e, _)) => panic!("produce failed: {e}"),
                        }
                    }
                    produced += 1;
                }
                produced_counter.store(produced, Ordering::Relaxed);
                producer.poll(Duration::ZERO);
            } else {
                // Ahead of schedule — short sleep to avoid busy-spin
                std::thread::sleep(Duration::from_micros(200));
            }
        }

        producer
            .flush(Duration::from_secs(30))
            .expect("producer flush");
        let prod_elapsed = start.elapsed();
        *producer_duration_writer.lock().unwrap() = prod_elapsed;
        prod_done.store(true, Ordering::Relaxed);
        produced
    });

    // ── Spawn CPU sampler ──
    let cpu_samples: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let cpu_sample_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let pipeline_done = Arc::new(AtomicBool::new(false));
    let cpu_s = Arc::clone(&cpu_samples);
    let cpu_c = Arc::clone(&cpu_sample_count);
    let cpu_done = Arc::clone(&pipeline_done);
    let pid = std::process::id();
    let cpu_thread = std::thread::spawn(move || {
        use sysinfo::{Pid, System};
        let mut sys = System::new();
        let spid = Pid::from_u32(pid);
        while !cpu_done.load(Ordering::Relaxed) {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);
            if let Some(proc_info) = sys.process(spid) {
                let usage_pct = proc_info.cpu_usage();
                cpu_s.fetch_add((usage_pct * 100.0) as u64, Ordering::Relaxed);
                cpu_c.fetch_add(1, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });

    // ── Run pipeline + control tasks ──
    let bench_start = Instant::now();
    let hist = Arc::clone(&histogram);
    let shutdown_pipeline = Arc::clone(&shutdown);
    let prod_done_for_shutdown = Arc::clone(&producer_done);
    let sink_topic_for_pipeline = sink_topic.clone();
    let source_topic_for_pipeline = source_topic.clone();

    let metrics = rt.block_on(async move {
        // Warmup reset task — clears histogram after warmup_secs
        let hist_reset = Arc::clone(&hist);
        let warmup_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(warmup_secs)).await;
            hist_reset.reset();
            println!(
                "  [warmup complete at {warmup_secs}s — histogram reset, measuring for {measure_secs}s]"
            );
        });

        // Shutdown signaler — waits for producer to finish, then grace period, then shuts down
        let shutdown_task = {
            let shutdown = Arc::clone(&shutdown_pipeline);
            let prod_done = Arc::clone(&prod_done_for_shutdown);
            tokio::spawn(async move {
                while !prod_done.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                // Grace period: let pipeline drain in-flight events
                tokio::time::sleep(Duration::from_secs(3)).await;
                shutdown.store(true, Ordering::Relaxed);
            })
        };

        let source = make_source(&source_topic_for_pipeline, 1024, drain_ms, partitions);
        let base_processor = PassthroughProcessor::new(Arc::from(sink_topic_for_pipeline.as_str()));
        let processor = LatencyMeasuringProcessor::new(base_processor, Arc::clone(&hist));

        let config = PipelineConfig {
            source_buffer_capacity: 256,
            sink_buffer_capacity: 256,
            max_batch_size: 1024,
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown_arg = Arc::clone(&shutdown_pipeline);

        if use_blackhole_sink {
            let sink = BlackholeSink::new();
            run_buffered(
                source,
                processor,
                sink,
                config,
                Arc::clone(&metrics),
                shutdown_arg,
                None,
            )
            .await
            .expect("pipeline run");
        } else {
            // UnorderedBatch: sink enqueues fast and flushes in the background
            // rather than awaiting per-batch delivery. This avoids the
            // per-write_batch overhead that kills tiny-batch workloads.
            let sink_config = KafkaSinkConfig::new(brokers(), sink_topic_for_pipeline.clone())
                .with_config("linger.ms", linger_ms.to_string())
                .with_strategy(DeliveryStrategy::UnorderedBatch);
            let sink = aeon_connectors::kafka::KafkaSink::new(sink_config).expect("sink");
            run_buffered(
                source,
                processor,
                sink,
                config,
                Arc::clone(&metrics),
                shutdown_arg,
                None,
            )
            .await
            .expect("pipeline run");
        }

        warmup_task.await.ok();
        shutdown_task.await.ok();

        metrics
    });

    let bench_elapsed = bench_start.elapsed();
    pipeline_done.store(true, Ordering::Relaxed);
    cpu_thread.join().ok();
    let total_produced = producer_handle.join().expect("producer thread");
    let prod_elapsed = *producer_duration.lock().unwrap();

    // ── Results ──
    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    let p50 = histogram.p50_us();
    let p95 = histogram.p95_us();
    let p99 = histogram.p99_us();
    let mean = histogram.mean_us();
    let count = histogram.count();

    let total_cpu = cpu_samples.load(Ordering::Relaxed) as f64 / 100.0;
    let num_samples = cpu_sample_count.load(Ordering::Relaxed).max(1);
    let avg_cpu_total = total_cpu / num_samples as f64;
    let num_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let avg_cpu_pct_of_system = avg_cpu_total / num_cpus as f64;

    let prod_secs = prod_elapsed.as_secs_f64().max(0.001);
    let effective_rate = total_produced as f64 / prod_secs;

    println!("\n--- Results ---");
    println!("  Bench duration:    {bench_elapsed:.1?} (incl. drain)");
    println!("  Producer duration: {prod_elapsed:.1?}");
    println!("  Events produced:   {total_produced}");
    println!("  Events received:   {received}");
    println!("  Outputs sent:      {sent}");
    println!("  Effective rate:    {effective_rate:.0} events/sec (target {target_rate})");

    println!("\n  Latency Histogram ({count} observations in measurement window):");
    println!("    Mean:  {mean:.1}µs ({:.3}ms)", mean / 1000.0);
    println!("    P50:   {p50}µs ({:.3}ms)", p50 as f64 / 1000.0);
    println!("    P95:   {p95}µs ({:.3}ms)", p95 as f64 / 1000.0);
    println!("    P99:   {p99}µs ({:.3}ms)", p99 as f64 / 1000.0);

    println!(
        "\n  CPU Usage ({num_samples} samples): {avg_cpu_pct_of_system:.1}% of system ({avg_cpu_total:.1}% across {num_cpus} cores)"
    );

    // ── Gate 1 Verdict ──
    println!("\n╔══════════════════════════════════════════════════════════╗");
    println!("║               GATE 1 STEADY-STATE VERDICT              ║");
    println!("╠══════════════════════════════════════════════════════════╣");

    let p99_ms = p99 as f64 / 1000.0;
    let p99_pass = p99_ms < 10.0;
    let cpu_pass = avg_cpu_pct_of_system < 50.0;
    // Zero-loss check: every produced event must be both received by the source
    // AND credited as delivered by the sink. `outputs_sent` is now incremented
    // at flush time for UnorderedBatch (via pipeline's credit_pending_on_flush),
    // so we can assert both sides match.
    let loss_pass = received >= total_produced && sent >= total_produced;
    let rate_achieved = effective_rate >= (target_rate as f64 * 0.95);

    println!(
        "║  P99 Latency:    {p99_ms:.3}ms  (target <10ms)  {}    ║",
        if p99_pass { "✅ PASS" } else { "❌ FAIL" }
    );
    println!(
        "║  CPU Usage:      {avg_cpu_pct_of_system:.1}% of system  (target <50%)  {}  ║",
        if cpu_pass { "✅ PASS" } else { "❌ FAIL" }
    );
    println!(
        "║  Zero Loss:      rx {received}/{total_produced} · tx {sent}/{total_produced}  {}  ║",
        if loss_pass { "✅ PASS" } else { "❌ FAIL" }
    );
    println!(
        "║  Rate Sustained: {total_produced}/{target_rate}  {}                    ║",
        if rate_achieved {
            "✅ PASS"
        } else {
            "⚠ PARTIAL"
        }
    );
    println!("╠══════════════════════════════════════════════════════════╣");

    if p99_pass && cpu_pass && loss_pass {
        println!("║         ✅ STEADY-STATE GATE 1 PASSED                  ║");
    } else {
        println!("║         ❌ STEADY-STATE GATE 1 FAILED                  ║");
    }
    println!("╚══════════════════════════════════════════════════════════╝");
}
