//! Connector factories for the binary's `ConnectorRegistry`.
//!
//! Each `*Factory` reads its config from the matching `SourceConfig` /
//! `SinkConfig` (a free-form `BTreeMap<String, String>` plus the typed
//! `topic` and `partitions` fields) and produces a boxed `DynSource` /
//! `DynSink`. The supervisor stores these in a uniform `HashMap` and
//! hands them to the generic pipeline runners through `BoxedSourceAdapter`
//! / `BoxedSinkAdapter`.
//!
//! The keys registered here drive the `type:` strings accepted in
//! pipeline manifests:
//!
//! ```yaml
//! sources:
//!   - type: memory          # synthetic event generator
//!   - type: kafka           # rdkafka StreamConsumer
//!   - type: http-webhook    # axum HTTP POST receiver (push)
//!   - type: http-polling    # periodic HTTP GET (pull)
//!   - type: file            # newline-delimited file reader
//! sinks:
//!   - type: blackhole       # benchmark ceiling sink
//!   - type: stdout          # debug print sink
//!   - type: kafka           # rdkafka FutureProducer
//!   - type: http            # POST outputs to an HTTP endpoint
//!   - type: file            # newline-delimited file writer
//! ```
//!
//! Memory source synthesises its events on construction (count + payload size
//! controlled via the config map) — it is **not** the pre-loaded
//! `MemorySource::new(events, …)` used by unit tests, because the supervisor
//! has no `Vec<Event>` to hand it. Tests that need pre-loaded events should
//! continue to construct `MemorySource` directly.

use std::sync::Arc;

use aeon_connectors::{
    BlackholeSink, StdoutSink, StreamingMemorySource,
    file::{FileSink, FileSinkConfig, FileSource, FileSourceConfig},
    http::{
        HttpPollingSource, HttpPollingSourceConfig, HttpSink, HttpSinkConfig, HttpWebhookSource,
        HttpWebhookSourceConfig,
    },
    kafka::{KafkaSink, KafkaSinkConfig, KafkaSource, KafkaSourceConfig},
    push_buffer::PushBufferConfig,
};
use aeon_engine::{ConnectorRegistry, DynSink, DynSource, SinkFactory, SourceFactory};
use aeon_types::{
    AeonError, DeliveryStrategy,
    registry::{SinkConfig, SourceConfig},
};

// ─── Memory source ─────────────────────────────────────────────────────────

/// Generates synthetic events of `payload_size` bytes each, served in batches
/// of `batch_size`. All three are pulled from `SourceConfig::config` with
/// sensible defaults — the supervisor only needs to know the key (`memory`),
/// the rest is config-driven.
///
/// `count = 0` runs unbounded (sustained-sweep mode for Session A load tests);
/// any positive `count` bounds the run to exactly that many events. Events are
/// synthesized lazily in `next_batch`, so a 10 M run does not pre-allocate
/// 2.5 GiB of `Vec<Event>` up front — that OOM is what blocked the 3-minute
/// sustained sweep in Session 0 (see `docs/GATE2-ACCEPTANCE-PLAN.md § 11.5`).
///
/// Used for the T0 isolation matrix: drives the pipeline at a deterministic
/// rate without any external broker.
pub struct MemorySourceFactory;

impl SourceFactory for MemorySourceFactory {
    fn build(&self, cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
        let count = parse_usize(cfg.config.get("count"), 1_000_000)?;
        let payload_size = parse_usize(cfg.config.get("payload_size"), 256)?;
        let batch_size = parse_usize(cfg.config.get("batch_size"), 1024)?;

        Ok(Box::new(StreamingMemorySource::new(
            count,
            payload_size,
            batch_size,
        )))
    }
}

// ─── Blackhole sink ────────────────────────────────────────────────────────

/// Discards every output. Used to measure Aeon's internal ceiling without
/// downstream I/O cost.
pub struct BlackholeSinkFactory;

impl SinkFactory for BlackholeSinkFactory {
    fn build(&self, _cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
        Ok(Box::new(BlackholeSink::new()))
    }
}

// ─── Stdout sink ───────────────────────────────────────────────────────────

/// Prints each output to stdout. Debug only — not for hot-path benchmarks.
pub struct StdoutSinkFactory;

impl SinkFactory for StdoutSinkFactory {
    fn build(&self, _cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
        Ok(Box::new(StdoutSink::new()))
    }
}

// ─── Kafka source ──────────────────────────────────────────────────────────

/// Builds a `KafkaSource` from a `SourceConfig`. Required keys:
/// - `topic` (the typed field on `SourceConfig`)
/// - `brokers` (config map; e.g. `redpanda:19092`)
///
/// Optional keys (config map): `group_id`, `batch_max`, `max_empty_polls`.
/// Partitions come from `SourceConfig::partitions` (cast u16 → i32).
pub struct KafkaSourceFactory;

impl SourceFactory for KafkaSourceFactory {
    fn build(&self, cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
        let topic = cfg
            .topic
            .as_deref()
            .ok_or_else(|| AeonError::config("kafka source requires 'topic'"))?;
        let brokers = cfg
            .config
            .get("brokers")
            .ok_or_else(|| AeonError::config("kafka source requires config.brokers"))?;

        let partitions: Vec<i32> = if cfg.partitions.is_empty() {
            // G1 fallback. Reached only when the supervisor couldn't
            // resolve cluster ownership (no resolver installed, or the
            // partition table is still empty at pipeline-start time) —
            // in a healthy cluster the supervisor fills partitions
            // before we see the cfg, so this is a loud signal that
            // something upstream didn't get wired.
            tracing::warn!(
                topic,
                "kafka source: no partitions specified and no cluster ownership \
                 resolved — falling back to [0]. This will silently under-read a \
                 multi-partition topic. Ensure `AEON_CLUSTER_ENABLED=true` or \
                 supply an explicit `partitions:` list."
            );
            vec![0]
        } else {
            cfg.partitions.iter().map(|p| *p as i32).collect()
        };

        let mut kcfg = KafkaSourceConfig::new(brokers, topic).with_partitions(partitions);

        if let Some(g) = cfg.config.get("group_id") {
            kcfg = kcfg.with_group_id(g);
        }
        if let Some(b) = cfg.config.get("batch_max") {
            kcfg = kcfg.with_batch_max(parse_usize(Some(b), 1024)?);
        }
        if let Some(m) = cfg.config.get("max_empty_polls") {
            kcfg = kcfg.with_max_empty_polls(parse_u32(Some(m), 10)?);
        }

        Ok(Box::new(KafkaSource::new(kcfg)?))
    }
}

// ─── Kafka sink ────────────────────────────────────────────────────────────

/// Builds a `KafkaSink` from a `SinkConfig`. Required keys:
/// - `topic` (typed field) — default destination topic
/// - `brokers` (config map)
///
/// Optional config keys: `strategy` (`per_event` | `ordered_batch` |
/// `unordered_batch`), `transactional_id` (turns on EO-2 T2 path).
pub struct KafkaSinkFactory;

impl SinkFactory for KafkaSinkFactory {
    fn build(&self, cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
        let topic = cfg
            .topic
            .as_deref()
            .ok_or_else(|| AeonError::config("kafka sink requires 'topic'"))?;
        let brokers = cfg
            .config
            .get("brokers")
            .ok_or_else(|| AeonError::config("kafka sink requires config.brokers"))?;

        let mut kcfg = KafkaSinkConfig::new(brokers, topic);

        if let Some(s) = cfg.config.get("strategy") {
            kcfg = kcfg.with_strategy(parse_strategy(s)?);
        }
        if let Some(tid) = cfg.config.get("transactional_id") {
            // G3: Kafka fences any second producer that opens a
            // transaction with a `transactional_id` already in use by a
            // live producer — so two pods in a ReplicaSet that share a
            // manifest must NOT resolve to the same id. We support
            // `${HOSTNAME}` and `${POD_NAME}` placeholders so a single
            // manifest works across every pod without hand-editing.
            let resolved = substitute_env_placeholders(tid)?;
            kcfg = kcfg.with_transactional_id(resolved);
        }

        Ok(Box::new(KafkaSink::new(kcfg)?))
    }
}

// ─── HTTP webhook source (push) ────────────────────────────────────────────

/// Builds an `HttpWebhookSource` — axum-based HTTP server that accepts POST
/// requests as events. First push source exposed through the YAML manifest
/// layer (V4, 2026-04-20). Others (websocket / mqtt / rabbitmq / quic /
/// webtransport / mongodb-cdc) follow the same pattern — tracked as P5.c in
/// `docs/ROADMAP.md` §Phase 5.
///
/// Required keys (config map):
/// - `bind_addr` (e.g. `0.0.0.0:8080`)
///
/// Optional keys (config map):
/// - `path` (default `/webhook`)
/// - `source_name` (default `http-webhook`; goes into `Event.source`)
/// - `channel_capacity` (push-buffer Phase 1 bounded channel, default 8192)
/// - `batch_size` (default 1024; events per `next_batch()` drain)
/// - `poll_timeout_ms` (default 1000; first-event wait before returning empty)
///
/// The shared `push_buffer.rs` three-phase contract applies: Phase 1 bounded
/// mpsc → Phase 2 await on full → Phase 3 returns HTTP 503 once
/// `spill_threshold` (default 4096) is crossed. See
/// `docs/CONNECTOR-AUDIT.md` §2 for the full matrix.
pub struct HttpWebhookSourceFactory;

impl SourceFactory for HttpWebhookSourceFactory {
    fn build(&self, cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
        let bind_addr_str = cfg.config.get("bind_addr").ok_or_else(|| {
            AeonError::config("http-webhook source requires config.bind_addr (e.g. 0.0.0.0:8080)")
        })?;
        let bind_addr: std::net::SocketAddr = bind_addr_str.parse().map_err(|e| {
            AeonError::config(format!(
                "http-webhook source: invalid bind_addr '{bind_addr_str}': {e}"
            ))
        })?;

        let mut buffer_config = PushBufferConfig::default();
        if let Some(cap) = cfg.config.get("channel_capacity") {
            buffer_config.channel_capacity = parse_usize(Some(cap), 8192)?;
        }
        if let Some(bs) = cfg.config.get("batch_size") {
            buffer_config.batch_size = parse_usize(Some(bs), 1024)?;
        }

        let poll_timeout_ms = cfg
            .config
            .get("poll_timeout_ms")
            .map(|v| parse_u64(Some(v), 1000))
            .transpose()?
            .unwrap_or(1000);

        let mut hcfg = HttpWebhookSourceConfig::new(bind_addr)
            .with_channel_capacity(buffer_config.channel_capacity)
            .with_poll_timeout(std::time::Duration::from_millis(poll_timeout_ms));

        if let Some(path) = cfg.config.get("path") {
            hcfg = hcfg.with_path(path.clone());
        }
        if let Some(name) = cfg.config.get("source_name") {
            hcfg = hcfg.with_source_name(Arc::<str>::from(name.as_str()));
        }

        // HttpWebhookSource::new is async (binds the TCP listener). The
        // supervisor builds factories from a blocking context, so we enter
        // the tokio runtime via Handle::current(). If no runtime is
        // installed this returns a clear Config error rather than
        // panicking — factories are also exercised from unit tests that
        // construct a runtime explicitly.
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            AeonError::config("http-webhook source must be built from within a tokio runtime")
        })?;
        let src = tokio::task::block_in_place(|| handle.block_on(HttpWebhookSource::new(hcfg)))?;
        Ok(Box::new(src))
    }
}

// ─── HTTP polling source (pull) ────────────────────────────────────────────

/// Builds an `HttpPollingSource` — periodic HTTP GET. Pull-source analog to
/// `HttpWebhookSource`; no push buffer needed because the poll interval is
/// natural flow control.
///
/// Required keys (config map):
/// - `url`
///
/// Optional keys (config map):
/// - `interval_ms` (default 10000)
/// - `timeout_ms` (default 30000)
/// - `source_name` (default `http-poll`; goes into `Event.source`)
/// - `header.<name>` (repeatable; each `header.X-Foo: bar` adds one request header)
pub struct HttpPollingSourceFactory;

impl SourceFactory for HttpPollingSourceFactory {
    fn build(&self, cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
        let url = cfg
            .config
            .get("url")
            .ok_or_else(|| AeonError::config("http-polling source requires config.url"))?;

        let mut pcfg = HttpPollingSourceConfig::new(url);

        if let Some(v) = cfg.config.get("interval_ms") {
            pcfg = pcfg.with_interval(std::time::Duration::from_millis(parse_u64(Some(v), 10_000)?));
        }
        if let Some(v) = cfg.config.get("timeout_ms") {
            pcfg = pcfg.with_timeout(std::time::Duration::from_millis(parse_u64(Some(v), 30_000)?));
        }
        if let Some(name) = cfg.config.get("source_name") {
            pcfg = pcfg.with_source_name(Arc::<str>::from(name.as_str()));
        }
        for (k, v) in &cfg.config {
            if let Some(header_name) = k.strip_prefix("header.") {
                pcfg = pcfg.with_header(header_name.to_string(), v.clone());
            }
        }

        Ok(Box::new(HttpPollingSource::new(pcfg)?))
    }
}

// ─── HTTP sink ─────────────────────────────────────────────────────────────

/// Builds an `HttpSink` — POSTs each output payload to a configured URL. Used
/// for Aeon → serverless fan-out (Lambda, Cloud Functions, webhooks).
///
/// Required keys (config map):
/// - `url`
///
/// Optional keys (config map):
/// - `timeout_ms` (default 30000)
/// - `header.<name>` (repeatable; each `header.X-Foo: bar` adds one request header)
pub struct HttpSinkFactory;

impl SinkFactory for HttpSinkFactory {
    fn build(&self, cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
        let url = cfg
            .config
            .get("url")
            .ok_or_else(|| AeonError::config("http sink requires config.url"))?;

        let mut scfg = HttpSinkConfig::new(url);

        if let Some(v) = cfg.config.get("timeout_ms") {
            scfg = scfg.with_timeout(std::time::Duration::from_millis(parse_u64(Some(v), 30_000)?));
        }
        for (k, v) in &cfg.config {
            if let Some(header_name) = k.strip_prefix("header.") {
                scfg = scfg.with_header(header_name.to_string(), v.clone());
            }
        }

        Ok(Box::new(HttpSink::new(scfg)?))
    }
}

// ─── File source ───────────────────────────────────────────────────────────

/// Builds a `FileSource` — reads newline-delimited records from a local file.
/// Opens lazily on first `next_batch()`; returns empty batches once the file
/// is exhausted. Suitable for log replay, JSONL, CSV, etc.
///
/// Required keys (config map):
/// - `path`
///
/// Optional keys (config map):
/// - `batch_size` (default 1024)
/// - `source_name` (default `file`; goes into `Event.source`)
pub struct FileSourceFactory;

impl SourceFactory for FileSourceFactory {
    fn build(&self, cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
        let path = cfg
            .config
            .get("path")
            .ok_or_else(|| AeonError::config("file source requires config.path"))?;

        let mut fcfg = FileSourceConfig::new(path);
        if let Some(v) = cfg.config.get("batch_size") {
            fcfg = fcfg.with_batch_size(parse_usize(Some(v), 1024)?);
        }
        if let Some(name) = cfg.config.get("source_name") {
            fcfg = fcfg.with_source_name(Arc::<str>::from(name.as_str()));
        }
        // Partition comes from the supervisor's resolver via `cfg.partitions`.
        // File is single-partition; honour the first entry if present.
        if let Some(p) = cfg.partitions.first() {
            fcfg = fcfg.with_partition(aeon_types::PartitionId::new(*p));
        }

        Ok(Box::new(FileSource::new(fcfg)))
    }
}

// ─── File sink ─────────────────────────────────────────────────────────────

/// Builds a `FileSink` — writes each output payload as one newline-delimited
/// line to a local file. Opens lazily on first `write_batch()`; flush
/// behaviour follows the configured `DeliveryStrategy`.
///
/// Required keys (config map):
/// - `path`
///
/// Optional keys (config map):
/// - `append` (`true`/`false`, default `false` — truncate)
/// - `strategy` (`per_event` | `ordered_batch` | `unordered_batch`,
///   default `ordered_batch`)
pub struct FileSinkFactory;

impl SinkFactory for FileSinkFactory {
    fn build(&self, cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
        let path = cfg
            .config
            .get("path")
            .ok_or_else(|| AeonError::config("file sink requires config.path"))?;

        let mut fcfg = FileSinkConfig::new(path);
        if let Some(v) = cfg.config.get("append") {
            fcfg = fcfg.with_append(parse_bool(Some(v))?);
        }
        if let Some(s) = cfg.config.get("strategy") {
            fcfg = fcfg.with_strategy(parse_strategy(s)?);
        }

        Ok(Box::new(FileSink::new(fcfg)))
    }
}

// ─── Registration ──────────────────────────────────────────────────────────

/// Register every connector compiled into the binary onto the registry.
/// Called once at startup from `cmd_serve` after constructing
/// `ConnectorRegistry::new()`.
pub fn register_defaults(reg: &mut ConnectorRegistry) {
    reg.register_source("memory", Arc::new(MemorySourceFactory));
    reg.register_source("kafka", Arc::new(KafkaSourceFactory));
    reg.register_source("http-webhook", Arc::new(HttpWebhookSourceFactory));
    reg.register_source("http-polling", Arc::new(HttpPollingSourceFactory));
    reg.register_source("file", Arc::new(FileSourceFactory));

    reg.register_sink("blackhole", Arc::new(BlackholeSinkFactory));
    reg.register_sink("stdout", Arc::new(StdoutSinkFactory));
    reg.register_sink("kafka", Arc::new(KafkaSinkFactory));
    reg.register_sink("http", Arc::new(HttpSinkFactory));
    reg.register_sink("file", Arc::new(FileSinkFactory));
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn parse_usize(s: Option<&String>, default: usize) -> Result<usize, AeonError> {
    match s {
        Some(v) => v
            .parse::<usize>()
            .map_err(|e| AeonError::config(format!("invalid usize '{v}': {e}"))),
        None => Ok(default),
    }
}

fn parse_u32(s: Option<&String>, default: u32) -> Result<u32, AeonError> {
    match s {
        Some(v) => v
            .parse::<u32>()
            .map_err(|e| AeonError::config(format!("invalid u32 '{v}': {e}"))),
        None => Ok(default),
    }
}

fn parse_u64(s: Option<&String>, default: u64) -> Result<u64, AeonError> {
    match s {
        Some(v) => v
            .parse::<u64>()
            .map_err(|e| AeonError::config(format!("invalid u64 '{v}': {e}"))),
        None => Ok(default),
    }
}

fn parse_bool(s: Option<&String>) -> Result<bool, AeonError> {
    match s.map(|v| v.as_str()) {
        Some("true" | "True" | "1") => Ok(true),
        Some("false" | "False" | "0") => Ok(false),
        Some(other) => Err(AeonError::config(format!(
            "invalid bool '{other}' — expected true | false | 1 | 0"
        ))),
        None => Ok(false),
    }
}

/// Substitute `${VAR}` placeholders in `input` with the values of the
/// corresponding process environment variables. Only `HOSTNAME` and
/// `POD_NAME` are recognised — deliberately narrow, because the only
/// caller today is Kafka `transactional_id` resolution (G3), and
/// accepting arbitrary env vars here would turn config parsing into an
/// implicit exfil vector.
///
/// Returns `AeonError::Config` if a referenced variable is unset. We
/// fail-loud rather than substituting empty because an empty string
/// silently collides across pods — exactly the fencing scenario this
/// fix exists to prevent.
///
/// Supports multiple placeholders in one string (e.g.
/// `"aeon-${POD_NAME}-${HOSTNAME}"`). Unknown placeholder names
/// (`${FOO}`) are also an error, not a passthrough — typos surface at
/// startup rather than causing runtime fencing in prod.
fn substitute_env_placeholders(input: &str) -> Result<String, AeonError> {
    // Fast path: nothing to substitute.
    if !input.contains("${") {
        return Ok(input.to_string());
    }

    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| {
            AeonError::config(format!(
                "transactional_id template: unterminated '${{' in '{input}'"
            ))
        })?;
        let name = &after[..end];
        let value = match name {
            "HOSTNAME" | "POD_NAME" => std::env::var(name).map_err(|_| {
                AeonError::config(format!(
                    "transactional_id template references ${{{name}}} but \
                     environment variable is unset — set it via the K8s \
                     downward API or `env:` spec"
                ))
            })?,
            other => {
                return Err(AeonError::config(format!(
                    "transactional_id template: unknown placeholder \
                     '${{{other}}}' — only ${{HOSTNAME}} and ${{POD_NAME}} \
                     are supported"
                )));
            }
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn parse_strategy(s: &str) -> Result<DeliveryStrategy, AeonError> {
    match s {
        "per_event" | "PerEvent" => Ok(DeliveryStrategy::PerEvent),
        "ordered_batch" | "OrderedBatch" => Ok(DeliveryStrategy::OrderedBatch),
        "unordered_batch" | "UnorderedBatch" => Ok(DeliveryStrategy::UnorderedBatch),
        other => Err(AeonError::config(format!(
            "unknown delivery strategy '{other}' — expected per_event | ordered_batch | unordered_batch"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::collections::BTreeMap;

    #[test]
    fn defaults_registers_expected_keys() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        for k in ["memory", "kafka", "http-webhook", "http-polling", "file"] {
            assert!(reg.has_source(k), "missing source: {k}");
        }
        for k in ["blackhole", "stdout", "kafka", "http", "file"] {
            assert!(reg.has_sink(k), "missing sink: {k}");
        }
    }

    #[tokio::test]
    async fn memory_source_factory_yields_synthetic_events() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        let mut config = BTreeMap::new();
        config.insert("count".into(), "10".into());
        config.insert("payload_size".into(), "32".into());
        config.insert("batch_size".into(), "4".into());

        let cfg = SourceConfig {
            source_type: "memory".into(),
            topic: None,
            partitions: vec![],
            config,
        };
        let mut src = reg.build_source(&cfg).expect("build memory source");

        use aeon_types::Source;
        let mut total = 0;
        loop {
            let batch = src.next_batch().await.expect("next_batch");
            if batch.is_empty() {
                break;
            }
            assert!(batch.iter().all(|e| e.payload.len() == 32));
            total += batch.len();
        }
        assert_eq!(total, 10);
    }

    #[tokio::test]
    async fn blackhole_sink_factory_swallows_outputs() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        let cfg = SinkConfig {
            sink_type: "blackhole".into(),
            topic: None,
            config: BTreeMap::new(),
        };
        let mut sink = reg.build_sink(&cfg).expect("build blackhole");

        use aeon_types::{Output, Sink};
        // BlackholeSink only tracks outputs that carry a source_event_id
        // (the production path always does). Stamp one explicitly here so
        // the assertion isn't a no-op.
        let mut out = Output::new(Arc::from("x"), Bytes::from_static(b"a"));
        out.source_event_id = Some(uuid::Uuid::nil());
        let result = sink.write_batch(vec![out]).await.expect("write_batch");
        assert_eq!(result.delivered.len(), 1);
    }

    #[tokio::test]
    async fn kafka_source_requires_topic_and_brokers() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        let cfg = SourceConfig {
            source_type: "kafka".into(),
            topic: None,
            partitions: vec![],
            config: BTreeMap::new(),
        };
        match reg.build_source(&cfg) {
            Err(AeonError::Config { .. }) => {}
            Err(other) => panic!("expected Config error for missing topic, got {other:?}"),
            Ok(_) => panic!("expected error for missing topic, got Ok"),
        }

        let mut topic_only = SourceConfig {
            source_type: "kafka".into(),
            topic: Some("t".into()),
            partitions: vec![],
            config: BTreeMap::new(),
        };
        match reg.build_source(&topic_only) {
            Err(AeonError::Config { .. }) => {}
            Err(other) => panic!("expected Config error for missing brokers, got {other:?}"),
            Ok(_) => panic!("expected error for missing brokers, got Ok"),
        }

        topic_only
            .config
            .insert("brokers".into(), "localhost:9092".into());
        // We don't actually connect — KafkaSource::new may still fail at
        // rdkafka client creation if librdkafka isn't reachable, but the
        // config-validation arm above is what we're asserting. Build is
        // best-effort here; the absence of a Config error is sufficient.
        let _ = reg.build_source(&topic_only);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_webhook_source_requires_bind_addr() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        // Missing bind_addr must surface as Config, not panic.
        let cfg = SourceConfig {
            source_type: "http-webhook".into(),
            topic: None,
            partitions: vec![],
            config: BTreeMap::new(),
        };
        match reg.build_source(&cfg) {
            Err(AeonError::Config { .. }) => {}
            Err(other) => {
                panic!("expected Config error for missing bind_addr, got {other:?}")
            }
            Ok(_) => panic!("expected error for missing bind_addr, got Ok"),
        }

        // Malformed bind_addr must also surface as Config.
        let mut bad = BTreeMap::new();
        bad.insert("bind_addr".into(), "not-a-socket-addr".into());
        let cfg = SourceConfig {
            source_type: "http-webhook".into(),
            topic: None,
            partitions: vec![],
            config: bad,
        };
        match reg.build_source(&cfg) {
            Err(AeonError::Config { .. }) => {}
            Err(other) => panic!("expected Config for malformed bind_addr, got {other:?}"),
            Ok(_) => panic!("expected error for malformed bind_addr, got Ok"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_webhook_source_binds_ephemeral_port() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        // 127.0.0.1:0 — kernel picks a free port, confirms the factory
        // actually drives HttpWebhookSource::new's async bind through
        // block_in_place without hanging.
        let mut config = BTreeMap::new();
        config.insert("bind_addr".into(), "127.0.0.1:0".into());
        config.insert("path".into(), "/ingest".into());
        config.insert("source_name".into(), "v4-smoke".into());
        config.insert("channel_capacity".into(), "256".into());
        config.insert("poll_timeout_ms".into(), "50".into());

        let cfg = SourceConfig {
            source_type: "http-webhook".into(),
            topic: None,
            partitions: vec![],
            config,
        };
        let mut src = reg.build_source(&cfg).expect("build http-webhook");

        // No events have been posted; next_batch returns empty within the
        // configured poll timeout rather than blocking indefinitely.
        use aeon_types::Source;
        let batch = src.next_batch().await.expect("next_batch");
        assert!(batch.is_empty(), "idle source should return empty batch");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_polling_source_requires_url() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        let cfg = SourceConfig {
            source_type: "http-polling".into(),
            topic: None,
            partitions: vec![],
            config: BTreeMap::new(),
        };
        match reg.build_source(&cfg) {
            Err(AeonError::Config { .. }) => {}
            Err(other) => panic!("expected Config error for missing url, got {other:?}"),
            Ok(_) => panic!("expected error for missing url, got Ok"),
        }

        // Valid url: factory returns Ok; we don't actually poll.
        let mut valid = BTreeMap::new();
        valid.insert("url".into(), "http://127.0.0.1:1/".into());
        valid.insert("interval_ms".into(), "5000".into());
        valid.insert("timeout_ms".into(), "1000".into());
        valid.insert("source_name".into(), "poll-smoke".into());
        valid.insert("header.X-From".into(), "aeon".into());
        let ok_cfg = SourceConfig {
            source_type: "http-polling".into(),
            topic: None,
            partitions: vec![],
            config: valid,
        };
        let _ = reg.build_source(&ok_cfg).expect("build http-polling");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_sink_factory_requires_url_and_posts() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        // Missing url must surface as Config.
        let missing = SinkConfig {
            sink_type: "http".into(),
            topic: None,
            config: BTreeMap::new(),
        };
        match reg.build_sink(&missing) {
            Err(AeonError::Config { .. }) => {}
            Err(other) => panic!("expected Config for missing url, got {other:?}"),
            Ok(_) => panic!("expected error for missing url, got Ok"),
        }

        // Spin up a tiny receiver and verify the factory builds a working sink.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let received_clone = Arc::clone(&received);
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let rx = Arc::clone(&received_clone);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    rx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                });
            }
        });

        let mut config = BTreeMap::new();
        config.insert("url".into(), format!("http://{addr}/"));
        config.insert("timeout_ms".into(), "2000".into());
        config.insert("header.X-Aeon".into(), "test".into());
        let cfg = SinkConfig {
            sink_type: "http".into(),
            topic: None,
            config,
        };
        let mut sink = reg.build_sink(&cfg).expect("build http sink");

        use aeon_types::{Output, Sink};
        let mut out = Output::new(Arc::from("x"), Bytes::from_static(b"hello"));
        out.source_event_id = Some(uuid::Uuid::nil());
        let result = sink.write_batch(vec![out]).await.expect("write_batch");
        assert_eq!(result.delivered.len(), 1);

        // Best-effort wait for the accept side to count the request.
        for _ in 0..20 {
            if received.load(std::sync::atomic::Ordering::Relaxed) >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(received.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn file_source_factory_reads_lines() {
        use std::io::Write;
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        // Write a tiny file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.log");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "alpha").unwrap();
            writeln!(f, "beta").unwrap();
            writeln!(f, "gamma").unwrap();
        }

        let mut config = BTreeMap::new();
        config.insert("path".into(), path.to_string_lossy().into_owned());
        config.insert("batch_size".into(), "8".into());
        config.insert("source_name".into(), "file-smoke".into());
        let cfg = SourceConfig {
            source_type: "file".into(),
            topic: None,
            partitions: vec![0],
            config,
        };
        let mut src = reg.build_source(&cfg).expect("build file source");

        use aeon_types::Source;
        let mut total = 0;
        loop {
            let batch = src.next_batch().await.expect("next_batch");
            if batch.is_empty() {
                break;
            }
            for ev in &batch {
                assert!(!ev.payload.is_empty());
            }
            total += batch.len();
        }
        assert_eq!(total, 3);
    }

    #[tokio::test]
    async fn file_source_factory_requires_path() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        let cfg = SourceConfig {
            source_type: "file".into(),
            topic: None,
            partitions: vec![],
            config: BTreeMap::new(),
        };
        match reg.build_source(&cfg) {
            Err(AeonError::Config { .. }) => {}
            Err(other) => panic!("expected Config error for missing path, got {other:?}"),
            Ok(_) => panic!("expected error for missing path, got Ok"),
        }
    }

    #[tokio::test]
    async fn file_sink_factory_writes_lines() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");

        let mut config = BTreeMap::new();
        config.insert("path".into(), path.to_string_lossy().into_owned());
        config.insert("strategy".into(), "ordered_batch".into());
        let cfg = SinkConfig {
            sink_type: "file".into(),
            topic: None,
            config,
        };
        let mut sink = reg.build_sink(&cfg).expect("build file sink");

        use aeon_types::{Output, Sink};
        let mut out = Output::new(Arc::from("x"), Bytes::from_static(b"hello"));
        out.source_event_id = Some(uuid::Uuid::nil());
        let result = sink.write_batch(vec![out]).await.expect("write_batch");
        assert_eq!(result.delivered.len(), 1);
        sink.flush().await.expect("flush");

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("hello"));
    }

    #[tokio::test]
    async fn file_sink_factory_requires_path() {
        let mut reg = ConnectorRegistry::new();
        register_defaults(&mut reg);

        let cfg = SinkConfig {
            sink_type: "file".into(),
            topic: None,
            config: BTreeMap::new(),
        };
        match reg.build_sink(&cfg) {
            Err(AeonError::Config { .. }) => {}
            Err(other) => panic!("expected Config error for missing path, got {other:?}"),
            Ok(_) => panic!("expected error for missing path, got Ok"),
        }
    }

    #[test]
    fn parse_bool_accepts_common_forms() {
        assert!(parse_bool(Some(&"true".to_string())).unwrap());
        assert!(!parse_bool(Some(&"false".to_string())).unwrap());
        assert!(parse_bool(Some(&"1".to_string())).unwrap());
        assert!(!parse_bool(Some(&"0".to_string())).unwrap());
        assert!(!parse_bool(None).unwrap());
        assert!(parse_bool(Some(&"maybe".to_string())).is_err());
    }

    #[test]
    fn parse_strategy_accepts_snake_and_camel() {
        assert_eq!(
            parse_strategy("per_event").unwrap(),
            DeliveryStrategy::PerEvent
        );
        assert_eq!(
            parse_strategy("OrderedBatch").unwrap(),
            DeliveryStrategy::OrderedBatch
        );
        assert!(parse_strategy("nope").is_err());
    }

    // ─── G3: transactional_id template substitution ───

    /// Env mutation is process-global, so all substitution tests share
    /// a mutex to avoid racing each other's `set_var`/`remove_var` calls
    /// when `cargo test` runs the module with multiple threads.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn env_substitution_fast_path_without_placeholders() {
        let _g = env_lock();
        let out = substitute_env_placeholders("aeon-static-tx-id").unwrap();
        assert_eq!(out, "aeon-static-tx-id");
    }

    #[test]
    fn env_substitution_resolves_hostname() {
        let _g = env_lock();
        // SAFETY: test-only env mutation, guarded by `env_lock()` so
        // no concurrent test observes the half-applied state.
        unsafe {
            std::env::set_var("HOSTNAME", "pod-7");
        }
        let out = substitute_env_placeholders("aeon-${HOSTNAME}-tx").unwrap();
        assert_eq!(out, "aeon-pod-7-tx");
    }

    #[test]
    fn env_substitution_resolves_pod_name() {
        let _g = env_lock();
        unsafe {
            std::env::set_var("POD_NAME", "aeon-3");
        }
        let out = substitute_env_placeholders("${POD_NAME}").unwrap();
        assert_eq!(out, "aeon-3");
    }

    #[test]
    fn env_substitution_handles_multiple_placeholders_in_one_string() {
        let _g = env_lock();
        unsafe {
            std::env::set_var("HOSTNAME", "h");
            std::env::set_var("POD_NAME", "p");
        }
        let out = substitute_env_placeholders("aeon-${POD_NAME}-on-${HOSTNAME}-tx").unwrap();
        assert_eq!(out, "aeon-p-on-h-tx");
    }

    #[test]
    fn env_substitution_rejects_unset_variable() {
        let _g = env_lock();
        unsafe {
            std::env::remove_var("HOSTNAME");
        }
        let err = substitute_env_placeholders("aeon-${HOSTNAME}").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("HOSTNAME") && msg.contains("unset"),
            "error must name the missing var, got: {msg}"
        );
    }

    #[test]
    fn env_substitution_rejects_unknown_placeholder() {
        let _g = env_lock();
        let err = substitute_env_placeholders("aeon-${USER}-tx").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("USER") && msg.contains("HOSTNAME"),
            "error must name the offender and list allowed vars, got: {msg}"
        );
    }

    #[test]
    fn env_substitution_rejects_unterminated_placeholder() {
        let _g = env_lock();
        let err = substitute_env_placeholders("aeon-${HOSTNAME-tx").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unterminated"),
            "error must explain the syntax problem, got: {msg}"
        );
    }
}
