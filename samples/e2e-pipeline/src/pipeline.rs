//! Aeon Load Test Pipeline — configurable source, processor, and sink.
//!
//! Modes:
//!   # Blackhole ceiling (memory source → native passthrough → blackhole sink)
//!   aeon-pipeline --source memory --processor native --sink blackhole --count 1000000
//!
//!   # Redpanda + native processor
//!   aeon-pipeline --source redpanda --processor native --sink redpanda
//!
//!   # Redpanda + Wasm processor
//!   aeon-pipeline --source redpanda --processor wasm --wasm path/to/proc.wasm --sink redpanda
//!
//!   # Redpanda source → blackhole sink (isolate source throughput)
//!   aeon-pipeline --source redpanda --processor native --sink blackhole

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aeon_connectors::BlackholeSink;
use aeon_connectors::MemorySource;
use aeon_connectors::kafka::{
    KafkaSink, KafkaSource, redpanda_sink_config, redpanda_source_config,
};
use aeon_engine::{
    PassthroughProcessor, PipelineConfig, PipelineMetrics, ShutdownCoordinator, run_buffered,
};
use aeon_types::{Event, PartitionId};
use aeon_wasm::{WasmConfig, WasmModule, WasmProcessor};
use bytes::Bytes;

#[derive(Clone, Debug)]
struct Args {
    source_mode: String,    // "redpanda" | "memory"
    processor_mode: String, // "native" | "wasm"
    sink_mode: String,      // "redpanda" | "blackhole"
    wasm_path: Option<String>,
    source_topic: String,
    sink_topic: String,
    brokers: String,
    partitions: Vec<i32>,
    batch_size: usize,
    buffer_capacity: usize,
    namespace: String,
    fuel: Option<u64>,
    count: u64,          // for memory source
    payload_size: usize, // for memory source
    duration_secs: u64,  // 0 = run until shutdown or source exhausted
}

fn parse_args() -> Args {
    let mut args = Args {
        source_mode: "redpanda".to_string(),
        processor_mode: "native".to_string(),
        sink_mode: "redpanda".to_string(),
        wasm_path: None,
        source_topic: "aeon-bench-source".to_string(),
        sink_topic: "aeon-bench-sink".to_string(),
        brokers: "localhost:19092".to_string(),
        partitions: vec![0],
        batch_size: 1024,
        buffer_capacity: 8192,
        namespace: "default".to_string(),
        fuel: Some(1_000_000),
        count: 1_000_000,
        payload_size: 128,
        duration_secs: 0,
    };

    let raw: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--source" => {
                i += 1;
                args.source_mode = raw[i].clone();
            }
            "--processor" => {
                i += 1;
                args.processor_mode = raw[i].clone();
            }
            "--sink" => {
                i += 1;
                args.sink_mode = raw[i].clone();
            }
            "--wasm" | "-w" => {
                i += 1;
                args.wasm_path = Some(raw[i].clone());
            }
            "--source-topic" | "-s" => {
                i += 1;
                args.source_topic = raw[i].clone();
            }
            "--sink-topic" | "-o" => {
                i += 1;
                args.sink_topic = raw[i].clone();
            }
            "--brokers" | "-b" => {
                i += 1;
                args.brokers = raw[i].clone();
            }
            "--partitions" | "-p" => {
                i += 1;
                args.partitions = raw[i]
                    .split(',')
                    .filter_map(|s| s.trim().parse::<i32>().ok())
                    .collect();
            }
            "--batch-size" => {
                i += 1;
                args.batch_size = raw[i].parse().unwrap_or(1024);
            }
            "--buffer-capacity" => {
                i += 1;
                args.buffer_capacity = raw[i].parse().unwrap_or(8192);
            }
            "--namespace" | "-n" => {
                i += 1;
                args.namespace = raw[i].clone();
            }
            "--fuel" => {
                i += 1;
                let v: u64 = raw[i].parse().unwrap_or(1_000_000);
                args.fuel = if v == 0 { None } else { Some(v) };
            }
            "--count" | "-c" => {
                i += 1;
                args.count = raw[i].parse().unwrap_or(1_000_000);
            }
            "--payload-size" => {
                i += 1;
                args.payload_size = raw[i].parse().unwrap_or(128);
            }
            "--duration" | "-d" => {
                i += 1;
                args.duration_secs = raw[i].parse().unwrap_or(0);
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("Unknown argument: {other}");
                print_usage();
                std::process::exit(1);
            }
        }
        i += 1;
    }
    args
}

fn print_usage() {
    eprintln!(
        r#"Aeon Load Test Pipeline

USAGE:
  aeon-pipeline [OPTIONS]

SOURCE/PROCESSOR/SINK:
      --source <mode>           "redpanda" or "memory" [default: redpanda]
      --processor <mode>        "native" or "wasm" [default: native]
      --sink <mode>             "redpanda" or "blackhole" [default: redpanda]

WASM OPTIONS:
  -w, --wasm <path>             Path to .wasm processor file (required if --processor wasm)
  -n, --namespace <name>        Wasm state namespace [default: default]
      --fuel <n>                Wasm fuel per call, 0=unlimited [default: 1000000]

REDPANDA OPTIONS:
  -b, --brokers <addrs>         Kafka/Redpanda brokers [default: localhost:19092]
  -s, --source-topic <name>     Source topic [default: aeon-bench-source]
  -o, --sink-topic <name>       Sink topic [default: aeon-bench-sink]
  -p, --partitions <list>       Comma-separated partition IDs [default: 0]

MEMORY SOURCE OPTIONS:
  -c, --count <n>               Events to generate [default: 1000000]
      --payload-size <n>        Payload bytes per event [default: 128]

PIPELINE OPTIONS:
      --batch-size <n>          Max batch size [default: 1024]
      --buffer-capacity <n>     SPSC ring buffer capacity [default: 8192]
  -d, --duration <secs>         Max duration, 0=unlimited [default: 0]

EXAMPLES:
  # Blackhole ceiling test
  aeon-pipeline --source memory --processor native --sink blackhole -c 5000000

  # Redpanda end-to-end with native processor
  aeon-pipeline --source redpanda --processor native --sink redpanda

  # Redpanda with Wasm processor
  aeon-pipeline --source redpanda --processor wasm --wasm proc.wasm --sink redpanda"#
    );
}

/// Generate synthetic events for MemorySource.
fn generate_events(count: u64, payload_size: usize) -> Vec<Event> {
    let source_name: Arc<str> = Arc::from("memory-gen");
    (0..count)
        .map(|i| {
            let payload = if payload_size > 0 {
                let json = format!(
                    r#"{{"user_id":"u-{:04}","action":"click","value":{},"seq":{}}}"#,
                    i % 10_000,
                    i % 1000,
                    i,
                );
                if json.len() >= payload_size {
                    Bytes::from(json)
                } else {
                    let mut padded = json.into_bytes();
                    padded.resize(payload_size, b'x');
                    Bytes::from(padded)
                }
            } else {
                Bytes::from_static(b"{}")
            };

            Event::new(
                uuid::Uuid::now_v7(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as i64,
                Arc::clone(&source_name),
                PartitionId::new((i % 16) as u16),
                payload,
            )
        })
        .collect()
}

/// Dispatch macro — run_buffered is generic, so we need to monomorphize at the call site.
/// This avoids trait objects on the hot path (static dispatch per CLAUDE.md rule 9).
macro_rules! run_pipeline {
    ($source:expr, $processor:expr, $sink:expr, $config:expr, $metrics:expr, $shutdown:expr) => {
        run_buffered(
            $source, $processor, $sink, $config, $metrics, $shutdown, None,
        )
        .await
    };
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args();

    // Validate
    if args.processor_mode == "wasm" && args.wasm_path.is_none() {
        eprintln!("Error: --wasm <path> is required when --processor wasm");
        std::process::exit(1);
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(PipelineMetrics::new());

    let pipeline_config = PipelineConfig {
        source_buffer_capacity: args.buffer_capacity,
        sink_buffer_capacity: args.buffer_capacity,
        max_batch_size: args.batch_size,
        ..Default::default()
    };

    // Signal handler
    let coordinator = ShutdownCoordinator::new(Arc::clone(&shutdown));
    tokio::spawn(coordinator.watch_signals());

    // Duration timer
    if args.duration_secs > 0 {
        let s = Arc::clone(&shutdown);
        let dur = args.duration_secs;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(dur)).await;
            tracing::info!(duration_secs = dur, "Duration limit reached, shutting down");
            s.store(true, Ordering::Relaxed);
        });
    }

    // Metrics reporter
    let m = Arc::clone(&metrics);
    let s = Arc::clone(&shutdown);
    let start_time = std::time::Instant::now();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut prev_processed: u64 = 0;
        loop {
            interval.tick().await;
            if s.load(Ordering::Relaxed) {
                break;
            }
            let received = m.events_received.load(Ordering::Relaxed);
            let processed = m.events_processed.load(Ordering::Relaxed);
            let sent = m.outputs_sent.load(Ordering::Relaxed);
            let elapsed = start_time.elapsed().as_secs_f64();
            let delta = processed - prev_processed;
            let rate = if elapsed > 0.0 {
                processed as f64 / elapsed
            } else {
                0.0
            };
            let instant_rate = delta as f64 / 2.0; // 2-second interval
            prev_processed = processed;
            tracing::info!(
                received,
                processed,
                sent,
                avg_rate = format!("{:.0}/s", rate),
                instant_rate = format!("{:.0}/s", instant_rate),
                "Pipeline metrics"
            );
        }
    });

    tracing::info!(
        source = %args.source_mode,
        processor = %args.processor_mode,
        sink = %args.sink_mode,
        "Starting pipeline — press Ctrl+C to stop"
    );

    let result = match (
        args.source_mode.as_str(),
        args.processor_mode.as_str(),
        args.sink_mode.as_str(),
    ) {
        // ─── Memory + Native + Blackhole (pure CPU ceiling) ─────────
        ("memory", "native", "blackhole") => {
            tracing::info!(
                count = args.count,
                payload_size = args.payload_size,
                "Generating events"
            );
            let source = MemorySource::new(
                generate_events(args.count, args.payload_size),
                args.batch_size,
            );
            let processor = PassthroughProcessor::new("bench".into());
            let sink = BlackholeSink::new();
            run_pipeline!(
                source,
                processor,
                sink,
                pipeline_config,
                Arc::clone(&metrics),
                Arc::clone(&shutdown)
            )
        }

        // ─── Memory + Wasm + Blackhole (Wasm ceiling) ───────────────
        ("memory", "wasm", "blackhole") => {
            let wasm_path = args.wasm_path.as_ref().unwrap();
            tracing::info!(path = %wasm_path, count = args.count, "Loading Wasm processor");
            let wasm_bytes = std::fs::read(wasm_path)?;
            let wasm_config = WasmConfig {
                max_fuel: args.fuel,
                namespace: args.namespace,
                ..WasmConfig::default()
            };
            let module = Arc::new(WasmModule::from_bytes(&wasm_bytes, wasm_config)?);
            let processor = WasmProcessor::new(Arc::clone(&module))?;
            let source = MemorySource::new(
                generate_events(args.count, args.payload_size),
                args.batch_size,
            );
            let sink = BlackholeSink::new();
            run_pipeline!(
                source,
                processor,
                sink,
                pipeline_config,
                Arc::clone(&metrics),
                Arc::clone(&shutdown)
            )
        }

        // ─── Redpanda + Native + Redpanda ───────────────────────────
        ("redpanda", "native", "redpanda") => {
            let source_config = redpanda_source_config(&args.brokers, &args.source_topic)
                .with_partitions(args.partitions.clone())
                .with_batch_max(args.batch_size)
                .with_source_name("redpanda");
            let source = KafkaSource::new(source_config)?;
            let processor = PassthroughProcessor::new("bench".into());
            let sink_config = redpanda_sink_config(&args.brokers, &args.sink_topic);
            let sink = KafkaSink::new(sink_config)?;
            tracing::info!(brokers = %args.brokers, src = %args.source_topic, dst = %args.sink_topic, "Redpanda connected");
            run_pipeline!(
                source,
                processor,
                sink,
                pipeline_config,
                Arc::clone(&metrics),
                Arc::clone(&shutdown)
            )
        }

        // ─── Redpanda + Native + Blackhole ──────────────────────────
        ("redpanda", "native", "blackhole") => {
            let source_config = redpanda_source_config(&args.brokers, &args.source_topic)
                .with_partitions(args.partitions.clone())
                .with_batch_max(args.batch_size)
                .with_source_name("redpanda");
            let source = KafkaSource::new(source_config)?;
            let processor = PassthroughProcessor::new("bench".into());
            let sink = BlackholeSink::new();
            tracing::info!(brokers = %args.brokers, src = %args.source_topic, "Redpanda → Blackhole");
            run_pipeline!(
                source,
                processor,
                sink,
                pipeline_config,
                Arc::clone(&metrics),
                Arc::clone(&shutdown)
            )
        }

        // ─── Redpanda + Wasm + Redpanda ─────────────────────────────
        ("redpanda", "wasm", "redpanda") => {
            let wasm_path = args.wasm_path.as_ref().unwrap();
            tracing::info!(path = %wasm_path, "Loading Wasm processor");
            let wasm_bytes = std::fs::read(wasm_path)?;
            let wasm_config = WasmConfig {
                max_fuel: args.fuel,
                namespace: args.namespace,
                ..WasmConfig::default()
            };
            let module = Arc::new(WasmModule::from_bytes(&wasm_bytes, wasm_config)?);
            let processor = WasmProcessor::new(Arc::clone(&module))?;
            let source_config = redpanda_source_config(&args.brokers, &args.source_topic)
                .with_partitions(args.partitions.clone())
                .with_batch_max(args.batch_size)
                .with_source_name("redpanda");
            let source = KafkaSource::new(source_config)?;
            let sink_config = redpanda_sink_config(&args.brokers, &args.sink_topic);
            let sink = KafkaSink::new(sink_config)?;
            tracing::info!(brokers = %args.brokers, src = %args.source_topic, dst = %args.sink_topic, "Redpanda + Wasm");
            run_pipeline!(
                source,
                processor,
                sink,
                pipeline_config,
                Arc::clone(&metrics),
                Arc::clone(&shutdown)
            )
        }

        // ─── Redpanda + Wasm + Blackhole ────────────────────────────
        ("redpanda", "wasm", "blackhole") => {
            let wasm_path = args.wasm_path.as_ref().unwrap();
            tracing::info!(path = %wasm_path, "Loading Wasm processor");
            let wasm_bytes = std::fs::read(wasm_path)?;
            let wasm_config = WasmConfig {
                max_fuel: args.fuel,
                namespace: args.namespace,
                ..WasmConfig::default()
            };
            let module = Arc::new(WasmModule::from_bytes(&wasm_bytes, wasm_config)?);
            let processor = WasmProcessor::new(Arc::clone(&module))?;
            let source_config = redpanda_source_config(&args.brokers, &args.source_topic)
                .with_partitions(args.partitions.clone())
                .with_batch_max(args.batch_size)
                .with_source_name("redpanda");
            let source = KafkaSource::new(source_config)?;
            let sink = BlackholeSink::new();
            tracing::info!(brokers = %args.brokers, src = %args.source_topic, "Redpanda + Wasm → Blackhole");
            run_pipeline!(
                source,
                processor,
                sink,
                pipeline_config,
                Arc::clone(&metrics),
                Arc::clone(&shutdown)
            )
        }

        (src, proc_, snk) => {
            eprintln!("Unsupported combination: --source {src} --processor {proc_} --sink {snk}");
            eprintln!(
                "Supported: source=redpanda|memory, processor=native|wasm, sink=redpanda|blackhole"
            );
            std::process::exit(1);
        }
    };

    // Final stats
    let elapsed = start_time.elapsed();
    let received = metrics.events_received.load(Ordering::Relaxed);
    let processed = metrics.events_processed.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);
    let rate = if elapsed.as_secs_f64() > 0.0 {
        processed as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    tracing::info!(
        received,
        processed,
        sent,
        elapsed_ms = elapsed.as_millis() as u64,
        throughput = format!("{:.0} events/sec", rate),
        "Pipeline complete"
    );

    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::error!(error = %e, "Pipeline error");
            Err(e.into())
        }
    }
}
