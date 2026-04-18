# Pipeline Design Guide

Decision-level guidance for pipeline authors. How to choose durability
modes, when to fan into one pipeline vs split into several, and how
source kind constrains the shape of your identity and event-time config.

Companion to:
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — engine internals.
- [`EO-2-DURABILITY-DESIGN.md`](EO-2-DURABILITY-DESIGN.md) — durability
  semantics and frozen decisions.
- [`CONNECTORS.md`](CONNECTORS.md) — per-connector config reference.

---

## 1. Pick your source kind first

Every source connector declares one of three kinds. This single choice
drives identity, L2 persistence, event-time config, and backpressure
remedy — getting it wrong is a silent data-loss bug at
`durability != none`.

| Kind | Upstream has durable replay position? | L2 write when `durability != none` | Identity mode | Backpressure remedy |
|------|---------------------------------------|------------------------------------|---------------|---------------------|
| **Pull** | Yes — broker offset / CDC LSN / file byte offset | **Skipped** (broker is buffer of record) | `native` — deterministic UUIDv7 from `(source, partition, offset)` | Stop polling |
| **Push** | No — upstream pushes, cannot pause the wire | **Required** | `random` — UUIDv7 + sub-ms monotonic counter | Phase 3 protocol flow control (503, `basic.cancel`, WS backpressure frame) |
| **Poll** | No — Aeon polls, but no upstream replay | **Required** | `compound` — cursor path + content-hash window | Widen poll interval or skip tick |

Canonical examples:

- **Pull:** Kafka, Redpanda, Redis Streams, NATS JetStream, Postgres/MySQL/MongoDB CDC, file.
- **Push:** WebSocket, WebTransport, webhook, MQTT, RabbitMQ push, raw QUIC.
- **Poll:** HTTP-polling REST endpoints, DNS poll, time-sampled sources.

The engine will **refuse to start** if the source-kind × identity
combination is incompatible (e.g. `Push` with `native` identity) — see
[`aeon_types::manifest::validate_source_shape`].

---

## 2. Pick the durability mode

Durability is a pipeline-level setting. Governs how aggressively event
bodies are persisted to Aeon-owned L2 storage before the upstream is
acked.

| Mode | L2 body write | L3 checkpoint cadence | Push-ack boundary | When to use |
|------|---------------|----------------------|-------------------|-------------|
| `none` | skipped | periodic, best-effort | on ingest | Pull-only pipelines or workloads tolerant of at-most-once on crash |
| `unordered_batch` | per event | per flush interval | after L2 write | Default for push/poll workloads — 20M ev/s achievable on Gen4+ NVMe |
| `ordered_batch` | per event | per batch boundary | after L2 write | Stricter ordering + still high throughput |
| `per_event` | per event + **fsync** | per event | after L2 + fsync | Regulated / financial workloads at ≤10k ev/s per partition — **deliberately outside the throughput budget** |

Rules of thumb:

- **Pull-only pipelines**: any mode is free. L2 is never touched because
  the broker replays. Default `none` is fine.
- **Any push or poll source**: leave `durability` at `none` only if you
  are explicitly OK with at-most-once between ingest and sink ack.
- **Don't select `per_event` because it "sounds safer".** It fsyncs on
  every event. At non-trivial throughput this will not keep up with
  your source. It is for contractual / compliance reasons, not general
  safety.

---

## 3. `event_time`: broker vs aeon_ingest vs header

`event_time` is a per-source setting that controls how `Event.timestamp`
is populated.

- **`broker`** — upstream-provided (Kafka record ts, CDC commit ts, file
  mtime). Zero cost. Requires the connector to advertise
  `Source::supports_broker_event_time() == true`; pipeline fails at
  start otherwise. **No silent fallback.**
- **`aeon_ingest`** — `Instant::now()` at ingestion. ~20 ns. **Loses
  cross-replay determinism** — pull-source UUIDv7 derivation becomes
  non-deterministic.
- **`header:<name>`** — parse a named header as Unix-epoch nanoseconds.
  ~50 ns. Missing or unparsable header → pipeline fails the batch.

Default is `broker`. Push / poll connectors that cannot supply an
upstream timestamp must override — pipeline start validation will force
the operator to choose one of the three options explicitly.

---

## 4. Multi-source: when to fan in

Aeon makes multi-source a first-class manifest shape — use it liberally
when the processor's state requires streams to merge.

**Ordering guarantees (read these carefully):**

- **Within one source partition:** strict.
- **Across partitions within one source:** none — matches Kafka.
- **Across sources:** arrival order at the processor input ring only.
  **No semantic order promise.** If a user needs total order across
  sources, they must merge upstream (e.g. into a single Kafka topic) and
  consume the merged stream.

Good fit for multi-source:
- Join CDC + clickstream in a single enrichment processor.
- Fan several webhook endpoints into one processor.
- Merge primary + sidecar source for backfill scenarios.

Anti-pattern:
- Using multi-source to emulate total ordering across heterogeneous
  streams. Merge upstream instead.

---

## 5. Multi-sink: one pipeline or several?

Multi-sink is supported but has honest tradeoffs.

**What actually happens with N sinks:**

- L2 GC cursor = `min(per_sink_ack_seq)`. **The slowest sink stalls the
  pipeline.** If sink B is 10 s behind, L2 holds ≥10 s of events for
  every sink.
- **Upgrade granularity is pipeline-wide.** Blue-green cuts over all
  sinks together.
- **One processor runs once per event** and emits outputs routed to
  each sink by the processor's routing table.

### Use multi-sink when

The sinks are **semantically coupled**:

- **Primary + audit** — every event lands in both by design, audit
  failures should stall primary.
- **Hot / warm / cold tiered storage** — same event, three retention
  tiers. Tier divergence would be a bug.
- **Multi-region same-event replication** — regions are peers, one
  region dropping means the event didn't ship correctly.

### Use separate pipelines when

The sinks are **independent consumers with independent failure
profiles**:

- Two partner webhooks with different SLAs — one partner's outage
  should not backpressure the other.
- Real-time alert stream vs batch analytics warehouse — wildly
  different latency envelopes.
- Experimental consumer you plan to tear down next quarter — don't
  couple its lifecycle to production.

**Rule of thumb:** if you would accept "sink B is down for 10 minutes,
but sink A keeps flowing", you want **separate pipelines**. If that
scenario is a data-quality bug, you want **one pipeline with two sinks**.

---

## 6. `eos_tier`: declare what your sink actually is

Each sink in the manifest declares its EOS tier. The engine validates
the declaration against the connector's runtime `SinkEosTier` at
pipeline start and refuses to run on mismatch (no silent downgrade of
EOS guarantees).

| Tier | Commit primitive | Examples |
|------|------------------|----------|
| `t1_transactional_db` | SQL `COMMIT` + `ON CONFLICT DO NOTHING` | Postgres, MySQL, SQLite, MongoDB |
| `t2_transactional_stream` | Producer `commit_transaction` + `send_offsets_to_transaction` | Kafka, Redpanda |
| `t3_atomic_rename_file` | Write to `*.tmp`, `rename()` into place | S3 (multi-part), filesystem |
| `t4_dedup_keyed` | Pipeline EXEC (Redis) / ack-all + msg-id (JetStream) | Redis Streams, NATS JetStream |
| `t5_idempotency_key` | HTTP request with `Idempotency-Key: <event_id>` | webhook, WebSocket, WebTransport sinks |
| `t6_fire_and_forget` | none — EOS not meaningful | stdout, blackhole |

Picking the wrong tier for a connector won't compile-fail — it will
fail at pipeline start with an explicit diagnostic. Always write the
tier that matches the **connector**, not the tier you wish it had.

---

## 7. Capacity planning quick-reference

Throughput ceilings come from §10.3 of `EO-2-DURABILITY-DESIGN.md`.
Summary for planning:

- **Pull-only, any mode:** 20M ev/s aggregate. Multi-node scaling, broker
  replay for recovery. L2 bandwidth untouched.
- **Push/poll at `unordered_batch` or `ordered_batch`:** 20M ev/s given
  **Gen4+ NVMe** and **≥8 partitions** per node. Consumer Gen3 NVMe
  is insufficient at this scale.
- **Push/poll at `per_event`:** tens of thousands of events/sec **per
  partition**, scales linearly with partition count. Designed for
  regulated workloads, not throughput.
- **HDD, any class:** unsupported for any durability mode except
  `none`. Random fsyncs on HDD are fatal to latency.

Set these at manifest time, not after the load test fails.

---

## 8. Partition count

**Immutable after creation.** Pick it once based on:

- Target throughput. Modern NVMe needs ≥8 concurrent writers to reach
  peak. Single-partition push/poll pipelines leave bandwidth on the
  floor.
- Consumer parallelism. Partitions cap the degree of processor
  parallelism for a given pipeline.
- Cluster size. Partition ownership is single-writer per node via Raft
  (Pillar 3); partition count should be ≥ node count and ideally a
  small multiple.

Starting point: **8 partitions** for single-node; **8 × node-count** for
clustered deployments. Revisit only if you are moving between throughput
classes.
