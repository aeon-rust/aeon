# Connector Cookbook

Copy-pasteable pipeline manifest fragments for every connector wired
into the Aeon CLI registry, plus a runnable example file under
`docs/examples/` for each one. Intended as the "I want X talking to Y"
shortcut — for deeper reference (lifecycle, error handling, EOS tiers)
see [CONNECTORS.md](CONNECTORS.md), and for the connector-development
contract see [CONNECTOR-DEV-GUIDE.md](CONNECTOR-DEV-GUIDE.md).

> **Prerequisite.** A running engine (`aeon serve` or
> `docker run ghcr.io/aeon-rust/aeon:latest`). Apply any manifest below
> with:
>
> ```bash
> aeon apply -f docs/examples/pipeline-<name>.yaml
> # or
> docker exec -i aeon /usr/local/bin/aeon apply -f - < docs/examples/pipeline-<name>.yaml
> ```

## Section 0 — How the manifest schema works

Every pipeline manifest has the same skeleton:

```yaml
pipelines:
  - name: <pipeline-name>          # globally unique within the cluster
    partitions: 4                  # number of parallel processing slots

    durability:                    # optional; default = mode: none
      mode: ordered_batch          # none | per_event | ordered_batch | unordered_batch
      checkpoint:
        interval: "200ms"          # optional, applies to per_event / ordered_batch

    sources:                       # one or more
      - name: <source-name>
        type: <connector-key>      # see registry table below
        kind: pull | push | poll
        identity: { mode: native | random | compound, ... }
        event_time: { mode: aeon_ingest | broker | header, ... }
        # ── connector-specific keys go HERE at top level ──
        topic: events
        brokers: "redpanda:9092"

    processor:
      name: <processor-name>       # `__identity` for passthrough
      version: "1.0.0"
      tier: native | wasm          # optional; defaults to wasm

    sinks:                         # one or more
      - name: <sink-name>
        type: <connector-key>
        eos_tier: t2_transactional_stream
        # ── connector-specific keys go HERE at top level ──
        topic: events-out
        brokers: "redpanda:9092"
```

**Critical schema rule** — connector-specific keys live at the
*top level* of the source / sink entry. They do **not** go under a
literal `config:` block. The manifest parser uses `#[serde(flatten)]`
on the connector config map; nesting under `config:` produces a single
opaque entry the connector factory can't read. (See
`crates/aeon-types/src/manifest.rs:158-190` for the exact struct.)

**Numeric values must be quoted strings** — the connector-config map
is `BTreeMap<String, String>`, not typed. Use `count: "10000"`, not
`count: 10000`.

**Env-var interpolation** — anywhere in the YAML, `${ENV:NAME}` is
replaced with the engine process's environment variable `NAME` at
manifest-apply time. Use this for credentials, broker addresses, and
anything else that varies across environments.

## Section 1 — Connector registry

| Type key         | Source? | Sink? | Required keys | Example fixture |
|------------------|:--:|:--:|---|---|
| `memory`         | ✅ | — | (none) | [pipeline-t0-baseline.yaml](examples/pipeline-t0-baseline.yaml) |
| `kafka`          | ✅ | ✅ | `topic`, `brokers` | [pipeline-t0-redpanda.yaml](examples/pipeline-t0-redpanda.yaml), [pipeline-g4-kafka-acked.yaml](examples/pipeline-g4-kafka-acked.yaml) |
| `http-webhook`   | ✅ | — | `bind_addr` | [pipeline-http-webhook.yaml](examples/pipeline-http-webhook.yaml) |
| `http-polling`   | ✅ | — | `url` | [pipeline-http-polling.yaml](examples/pipeline-http-polling.yaml) |
| `http`           | — | ✅ | `url` | [pipeline-http-sink.yaml](examples/pipeline-http-sink.yaml) |
| `file`           | ✅ | ✅ | `path` | [pipeline-file-source-sink.yaml](examples/pipeline-file-source-sink.yaml) |
| `websocket`      | ✅ | ✅ | `url` | [pipeline-websocket.yaml](examples/pipeline-websocket.yaml) |
| `redis-streams`  | ✅ | ✅ | source: `url`, `stream_key`, `group`, `consumer`; sink: `url`, `stream_key` | [pipeline-redis-streams.yaml](examples/pipeline-redis-streams.yaml) |
| `nats`           | ✅ | ✅ | source: `url`, `stream`, `subject`; sink: `url`, `subject` | [pipeline-nats.yaml](examples/pipeline-nats.yaml) |
| `postgres-cdc`   | ✅ | — | `connection_string`, `slot_name`, `publication` | [pipeline-postgres-cdc.yaml](examples/pipeline-postgres-cdc.yaml) |
| `mysql-cdc`      | ✅ | — | `url` | [pipeline-mysql-cdc.yaml](examples/pipeline-mysql-cdc.yaml) |
| `mongodb-cdc`    | ✅ | — | `uri`, `database` | [pipeline-mongodb-cdc.yaml](examples/pipeline-mongodb-cdc.yaml) |
| `blackhole`      | — | ✅ | (none) | [pipeline-t0-baseline.yaml](examples/pipeline-t0-baseline.yaml) |
| `stdout`         | — | ✅ | (none) | [pipeline-stdout-sink.yaml](examples/pipeline-stdout-sink.yaml) |

> **Not yet CLI-registered:** MQTT, RabbitMQ, QUIC, WebTransport
> source/sink. Implementations exist under `crates/aeon-connectors`
> but are not wired into `register_defaults` — they will land as part
> of P5.c. See [CONNECTORS.md](CONNECTORS.md) for the per-connector
> status matrix.

## Section 2 — Per-connector reference

### 2.1 `memory` (source)

Synthetic event generator. Used by every benchmark and most tests.
No external dependency.

| Key | Default | Description |
|---|---|---|
| `count` | `1000000` | Number of events to emit. `0` = unbounded (sustained-sweep mode). |
| `payload_size` | `256` | Bytes per event payload. |
| `batch_size` | `1024` | Events per `next_batch()` call. |

```yaml
sources:
  - name: synth
    type: memory
    kind: push
    identity: { mode: random }
    event_time: { mode: aeon_ingest }
    count: "10000"
    payload_size: "256"
    batch_size: "256"
```

### 2.2 `kafka` (source + sink) — for Redpanda too

The same connector talks to Kafka and Redpanda — there's no
`redpanda` connector type. Backed by `rdkafka`. Manual partition
assignment (no consumer groups in the source path) so partition
ownership is deterministic across the cluster.

**Source required:** `topic`, `brokers`.
**Source optional:** `group_id`, `batch_max`, `max_empty_polls`.

**Sink required:** `topic`, `brokers`.
**Sink optional:** `strategy` (`per_event` | `ordered_batch` | `unordered_batch`),
`transactional_id` (supports `${ENV:HOSTNAME}` / `${ENV:POD_NAME}`
placeholders for per-pod uniqueness — Kafka fences on duplicate ids).

```yaml
sources:
  - name: orders-in
    type: kafka
    kind: pull
    identity: { mode: native }
    event_time: { mode: broker }
    topic: orders
    partitions: [0, 1, 2, 3]
    brokers: "redpanda:9092"
    group_id: aeon-orders

sinks:
  - name: orders-out
    type: kafka
    eos_tier: t2_transactional_stream
    topic: orders-processed
    brokers: "redpanda:9092"
    strategy: ordered_batch
    transactional_id: "aeon-orders-${ENV:HOSTNAME}"
```

### 2.3 `http-webhook` (push source)

Axum-based HTTP server. Each `POST` becomes one Event. Use for
inbound webhooks from Stripe, GitHub, Slack, custom apps, etc.

**Required:** `bind_addr` (e.g. `0.0.0.0:8080`).
**Optional:** `path` (default `/webhook`), `source_name`,
`channel_capacity`, `batch_size`, `poll_timeout_ms`. S9 inbound auth
keys (`inbound_auth_mode`, etc.) gate the receiver.

See [pipeline-http-webhook.yaml](examples/pipeline-http-webhook.yaml).

### 2.4 `http-polling` (pull source)

Periodic HTTP `GET` against a URL. Use for upstream APIs that don't
push.

**Required:** `url`.
**Optional:** `interval_ms` (default `10000`), `timeout_ms` (default
`30000`), `source_name`, `header.<name>` (repeatable),
`ssrf_allow_loopback` / `ssrf_allow_private` / `ssrf_allow_link_local`
/ `ssrf_allow_cgnat` (S7 SSRF guard), `ssrf_extra_allow` /
`ssrf_extra_deny` (CIDR lists). S10 outbound auth keys
(`outbound_auth_mode`, etc.) sign the outgoing request.

See [pipeline-http-polling.yaml](examples/pipeline-http-polling.yaml).

### 2.5 `http` (sink)

`POST` each Output payload to a configured URL. Common pattern:
Aeon → AWS Lambda / GCP Cloud Function / generic webhook.

**Required:** `url`.
**Optional:** `timeout_ms`, `header.<name>` (repeatable), the same
SSRF + S10 keys as `http-polling`.

See [pipeline-http-sink.yaml](examples/pipeline-http-sink.yaml).

### 2.6 `file` (source + sink)

Newline-delimited file reader / writer. Source opens lazily on first
`next_batch()`; sink opens lazily on first `write_batch()`. Suitable
for log replay, JSONL transforms, CSV pipelines.

**Source required:** `path`.
**Source optional:** `batch_size` (default `1024`), `source_name`.

**Sink required:** `path`.
**Sink optional:** `append` (`true`/`false`, default `false` —
truncate), `strategy` (`per_event` | `ordered_batch` |
`unordered_batch`, default `ordered_batch`).

See [pipeline-file-source-sink.yaml](examples/pipeline-file-source-sink.yaml).

### 2.7 `websocket` (source + sink)

Persistent ws/wss connection. Source spools incoming messages through
the shared push buffer; sink writes each Output as one binary message.
Both reconnect on disconnect.

**Source / sink required:** `url`.
**Optional:** `source_name`, `channel_capacity`, `poll_timeout_ms`,
S7 SSRF + S10 outbound auth keys.

See [pipeline-websocket.yaml](examples/pipeline-websocket.yaml).

### 2.8 `redis-streams` (source + sink)

XREADGROUP source, XADD sink. The source uses a consumer group so
multiple Aeon nodes can split the workload. Acks happen via XACK
after the Output is delivered.

**Source required:** `url`, `stream_key`, `group`, `consumer`.
**Source optional:** `batch_size` (default `1024`), `block_ms`
(default `1000`), `source_name`.

**Sink required:** `url`, `stream_key`.
**Sink optional:** `strategy`.

See [pipeline-redis-streams.yaml](examples/pipeline-redis-streams.yaml).

### 2.9 `nats` (source + sink)

JetStream durable consumer + publisher. Flip `jetstream: "false"` on
the sink for core (fire-and-forget) NATS.

**Source required:** `url`, `stream`, `subject`.
**Source optional:** `consumer` (default `aeon`), `batch_size`,
`source_name`.

**Sink required:** `url`, `subject`.
**Sink optional:** `jetstream` (default `true`), `strategy`,
`dedup` (sets msg-id deduplication; pairs naturally with
`eos_tier: t5_idempotency_key`).

See [pipeline-nats.yaml](examples/pipeline-nats.yaml).

### 2.10 `postgres-cdc` (source)

Logical-replication-backed CDC. Streams INSERT / UPDATE / DELETE in
commit order. Slot persists progress at the publisher; resume after
restart is automatic.

**Required:** `connection_string`, `slot_name`, `publication`.
**Optional:** `create_slot` (bool, default `true`), `batch_size`,
`source_name`. mTLS via `tokio-postgres-rustls`.

**Postgres requirements:** `wal_level = logical`, `CREATE PUBLICATION`,
REPLICATION privilege.

See [pipeline-postgres-cdc.yaml](examples/pipeline-postgres-cdc.yaml).

### 2.11 `mysql-cdc` (source)

Binlog-polling CDC. ROW format required.

**Required:** `url`.
**Optional:** `server_id` (must be unique across replicas — default
`1000`), `batch_size`, `tables` (comma-separated allow-list,
default = all), `source_name`.

**MySQL requirements:** `binlog_format = ROW`, `binlog_row_image = FULL`,
`log_bin = ON`, `REPLICATION CLIENT` + `REPLICATION SLAVE` privileges.
mTLS supports rustls-tls path with RSA keys only.

See [pipeline-mysql-cdc.yaml](examples/pipeline-mysql-cdc.yaml).

### 2.12 `mongodb-cdc` (source)

Change-streams-backed CDC. Replica-set topology required (change
streams are a no-op on standalone). Resume tokens are persisted to
disk for restart-safety.

**Required:** `uri`, `database`.
**Optional:** `collection` (default = whole database), `batch_size`,
`source_name`, `resume_token_path`. mTLS via process-owned tempfile.

See [pipeline-mongodb-cdc.yaml](examples/pipeline-mongodb-cdc.yaml).

### 2.13 `blackhole` (sink)

Discards every Output. No config. Used to measure the engine's
internal ceiling without downstream I/O cost.

```yaml
sinks:
  - name: discard
    type: blackhole
    eos_tier: t6_fire_and_forget
```

### 2.14 `stdout` (sink)

Prints each Output to stdout. No config. Debug-only — `write(2)`
per event dominates throughput.

```yaml
sinks:
  - name: console
    type: stdout
    eos_tier: t6_fire_and_forget
```

## Section 3 — Authentication

S9 inbound auth (sources that listen — `http-webhook`, future MQTT
broker, etc.) supports IP-allow-list, API-key, HMAC-signature, and
mTLS. S10 outbound auth (sources that fetch — `http-polling`,
`websocket`, broker clients — and every sink) supports Bearer, Basic,
API-key, HMAC-sign, and mTLS.

The exact key names per mode are documented in
[SECURITY.md](SECURITY.md) §S9 / §S10.

## Section 4 — Where to go next

- **Build a custom processor** that consumes the events these
  connectors produce: see [PROCESSOR-GUIDE.md](PROCESSOR-GUIDE.md).
- **Run it in a multi-node cluster** with partition ownership and
  Raft-backed pipeline state: see [CLUSTERING.md](CLUSTERING.md) §3.
- **Wire OpenTelemetry** to ship metrics/traces/logs to your existing
  observability stack: see [OBSERVABILITY.md](OBSERVABILITY.md).
- **Add a new connector** for a system not in the table above: see
  [CONNECTOR-DEV-GUIDE.md](CONNECTOR-DEV-GUIDE.md).
