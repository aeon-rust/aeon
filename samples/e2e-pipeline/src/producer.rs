//! Event Producer — generates JSON events and sends them to Redpanda.
//!
//! Usage:
//!   cargo run --release --bin aeon-producer -- \
//!     --topic aeon-input \
//!     --brokers localhost:19092 \
//!     --count 10000 \
//!     --rate 1000
//!
//! Each event is a JSON payload with a user_id field:
//!   {"user_id":"u-0042","action":"click","value":42}

use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

fn parse_args() -> Args {
    let mut args = Args {
        topic: "aeon-input".to_string(),
        brokers: "localhost:19092".to_string(),
        count: 10_000,
        rate: 0, // 0 = unlimited
        payload_size: 0,
        partition: None,
    };

    let raw: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--topic" | "-t" => {
                i += 1;
                args.topic = raw[i].clone();
            }
            "--brokers" | "-b" => {
                i += 1;
                args.brokers = raw[i].clone();
            }
            "--count" | "-c" => {
                i += 1;
                args.count = raw[i].parse().unwrap_or(10_000);
            }
            "--rate" | "-r" => {
                i += 1;
                args.rate = raw[i].parse().unwrap_or(0);
            }
            "--payload-size" => {
                i += 1;
                args.payload_size = raw[i].parse().unwrap_or(0);
            }
            "--partition" | "-p" => {
                i += 1;
                args.partition = Some(raw[i].parse().unwrap_or(0));
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
        r#"Aeon Event Producer — generates JSON events to Redpanda

USAGE:
  aeon-producer [OPTIONS]

OPTIONS:
  -t, --topic <name>        Target topic [default: aeon-input]
  -b, --brokers <addrs>     Kafka/Redpanda brokers [default: localhost:19092]
  -c, --count <n>           Number of events to produce [default: 10000]
  -r, --rate <n>            Events per second, 0=unlimited [default: 0]
  -p, --partition <n>       Target partition (default: round-robin)
      --payload-size <n>    Pad payload to at least N bytes [default: 0]
  -h, --help                Print this help"#
    );
}

struct Args {
    topic: String,
    brokers: String,
    count: u64,
    rate: u64,
    payload_size: usize,
    partition: Option<i32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Route human-readable logs to stderr so stdout is reserved for the
    // machine-parseable SUMMARY line that load-sweep harnesses capture.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args();

    // Create producer
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &args.brokers)
        .set("message.timeout.ms", "5000")
        .set("queue.buffering.max.messages", "100000")
        .set("batch.num.messages", "1000")
        .set("linger.ms", "5")
        .create()?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let s = Arc::clone(&shutdown);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        s.store(true, Ordering::Relaxed);
    });

    let actions = ["click", "view", "purchase", "scroll", "hover"];
    let sent = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));

    tracing::info!(
        topic = %args.topic,
        brokers = %args.brokers,
        count = args.count,
        rate = args.rate,
        "Starting event production"
    );

    let start = std::time::Instant::now();

    // Rate limiter: if rate > 0, sleep between batches
    let batch_size: u64 = if args.rate > 0 {
        (args.rate / 10).max(1) // send in ~10 bursts per second
    } else {
        1000
    };

    let mut produced: u64 = 0;
    let topic = args.topic.clone();

    while produced < args.count && !shutdown.load(Ordering::Relaxed) {
        let batch_end = (produced + batch_size).min(args.count);
        // Build all payloads and keys first so they outlive the delivery futures
        let batch_count = (batch_end - produced) as usize;
        let mut keys = Vec::with_capacity(batch_count);
        let mut payloads = Vec::with_capacity(batch_count);

        for i in produced..batch_end {
            let action = actions[(i as usize) % actions.len()];
            let user_id = format!("u-{:04}", i % 10_000);
            let mut payload = format!(
                r#"{{"user_id":"{}","action":"{}","value":{}}}"#,
                user_id,
                action,
                i % 1000,
            );

            // Pad if needed
            if args.payload_size > 0 && payload.len() < args.payload_size {
                let pad_len = args.payload_size - payload.len() - 1;
                payload.pop(); // remove }
                payload.push_str(",\"pad\":\"");
                for _ in 0..pad_len.saturating_sub(9) {
                    payload.push('x');
                }
                payload.push_str("\"}");
            }

            keys.push(format!("{}", i % 100));
            payloads.push(payload);
        }

        // Send all records, collecting delivery futures
        let mut delivery_futures = Vec::with_capacity(batch_count);
        for j in 0..batch_count {
            let mut record = FutureRecord::to(&topic).key(&keys[j]).payload(&payloads[j]);

            if let Some(p) = args.partition {
                record = record.partition(p);
            }

            delivery_futures.push(producer.send(record, Duration::from_secs(5)));
        }

        // Await all deliveries in this batch
        for future in delivery_futures {
            match future.await {
                Ok(_) => {
                    sent.fetch_add(1, Ordering::Relaxed);
                }
                Err((e, _)) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    if errors.load(Ordering::Relaxed) <= 5 {
                        tracing::error!(error = %e, "Produce failed");
                    }
                }
            }
        }

        produced = batch_end;

        // Rate limiting
        if args.rate > 0 {
            let expected_time = Duration::from_secs_f64(produced as f64 / args.rate as f64);
            let elapsed = start.elapsed();
            if elapsed < expected_time {
                tokio::time::sleep(expected_time - elapsed).await;
            }
        }
    }

    let elapsed = start.elapsed();
    let total_sent = sent.load(Ordering::Relaxed);
    let total_errors = errors.load(Ordering::Relaxed);
    let rate = total_sent as f64 / elapsed.as_secs_f64();

    tracing::info!(
        sent = total_sent,
        errors = total_errors,
        elapsed_ms = elapsed.as_millis() as u64,
        rate = format!("{:.0} events/sec", rate),
        "Production complete"
    );

    // Machine-parseable summary on stdout. One tab-separated line so shell
    // sweep scripts can `awk` or `cut` per column without matching tracing
    // output. Columns: label, sent, errors, elapsed_ms, events_per_sec,
    // configured_rate, topic, payload_size.
    println!(
        "SUMMARY\tsent={total_sent}\terrors={total_errors}\telapsed_ms={elapsed_ms}\tevents_per_sec={rate:.0}\tconfigured_rate={configured}\ttopic={topic}\tpayload_size={payload}",
        elapsed_ms = elapsed.as_millis() as u64,
        configured = args.rate,
        topic = args.topic,
        payload = args.payload_size,
    );

    Ok(())
}
