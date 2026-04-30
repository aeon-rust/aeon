# EO-2 — Aeon-Native Event Durability Design

> Status: **Design frozen (2026-04-15).** No code yet. This doc captures the
> agreed shape from the design discussion; implementation phases at the end.
> Supersedes the sink-participating framing in `FAULT-TOLERANCE-ANALYSIS.md §7 Pillar 5`.

---

## 1. Goals & Non-Goals

### Goals

1. **Zero event loss** across crashes, restarts, and partition migrations,
   without requiring cooperation from the source upstream or sink downstream.
2. **Aeon owns the durable sequence.** Not Kafka, not any specific broker.
   Every supported source/sink combination gets the same correctness story.
3. **No hot-path penalty** at the 20M events/sec target. Durability cost
   amortises at checkpoint cadence.
4. **Per-pipeline configurability** so operators pay only for the durability
   they actually need (the ETL framing: noisy pipelines don't starve quiet
   ones; single-big-pipeline deployments need zero extra config).

### Non-Goals

- **Cross-sink atomicity.** If sink A commits and sink B fails, sink A is
  *not* rolled back. Per-sink EOS only. Cross-sink atomicity would require
  two-phase commit across heterogeneous systems — out of scope.
- **Cross-source total order.** Timestamps from different upstreams are
  incomparable. Aeon preserves per-partition order within each source; it
  does not merge-sort across sources.
- **Transactional pipelines over multiple hosts.** Partition is the unit of
  ownership; each partition is single-owner at any moment.

---

## 2. Architecture Overview

```
 ┌──────────┐      ┌─────────────┐     ┌──────────┐     ┌─────────────┐    ┌────────┐
 │ Source   │─────▶│ SPSC ring   │────▶│Processor │────▶│ SPSC ring   │───▶│ Sinks  │
 │ (N kinds)│      └─────────────┘     │ (1 job)  │     └─────────────┘    │(M of   │
 └──────────┘             │            └──────────┘            │           │ kinds) │
       │                  ▼                                    ▼           └────────┘
       │            ┌──────────────────────────────────────────────┐            │
       │            │           L2 event-body store                │            │
       │            │  (append-only mmap, per-partition, push+poll │            │
       │            │   only; pull sources use broker as buffer)   │            │
       │            └──────────────────────────────────────────────┘            │
       │                              │                                         │
       │                              ▼                                         │
       │            ┌──────────────────────────────────────────────┐            │
       └───────────▶│      L3 checkpoint store (redb/rocksdb)      │◀───────────┘
                    │   per-partition cursors: source offsets +    │
                    │   per-sink ACK seq + pending_event_ids       │
                    │   WAL fallback if L3 unavailable             │
                    └──────────────────────────────────────────────┘
```

Three persistent tiers of concern:

- **L2**: append-only mmap log, holds event bodies for push + poll sources
  when durability is on. Pull sources skip L2 (broker replay via `Seekable`).
- **L3**: transactional KV (redb default, RocksDB alternative via
  pluggable `L3Store` trait), holds checkpoint records — tiny, metadata only.
- **WAL** (`CheckpointWriter` on `checkpoint.rs`): automatic fallback of the
  L3 checkpoint store when L3 is unavailable or write-errors. **Not a peer
  option** — removing the symmetry that existed before this design.

---

## 3. Source Model — Three Kinds

| Kind | Examples | Characteristic | L2 used? | Identity strategy |
|------|----------|----------------|----------|-------------------|
| **Pull** | Kafka, Redis Streams, Postgres/MySQL/Mongo CDC, file, Redpanda | Aeon polls; upstream has a durable replay position (offset/LSN/resume-token/byte-offset) | **No** — broker is the buffer of record | Deterministic UUIDv7 from `(source, partition, offset)` |
| **Push** | WebSocket, WebTransport, webhook/HTTP ingress, MQTT, RabbitMQ, QUIC | Upstream pushes; Aeon cannot tell it to pause at the wire; no durable replay position | **Yes** when `durability != none` | Random UUIDv7 with sub-ms monotonic counter |
| **Poll** | HTTP polling against arbitrary REST endpoints, DNS poll, any time-sampled source | Aeon polls; **no durable upstream position** | **Yes** when `durability != none` | Content-hash or cursor-based; see §5.3 |

Exposed on the `Source` trait as:

```rust
pub trait Source: Send + Sync {
    fn source_kind() -> SourceKind;
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError>;
    fn event_identity(ev: &RawEvent) -> Bytes;   // for deterministic UUIDv7
    // existing: pause(), resume(), Seekable for pull
}

pub enum SourceKind { Pull, Push, Poll }
```

Poll is **new**; previously collapsed into pull. The distinction matters because
poll has no durable upstream position, so it needs Aeon-side L2 persistence
even though it's pull-shaped at the wire.

---

## 4. UUIDv7 Identity

Event identity is minted by the source connector at ingest. The `Event.id`
field is a standard 128-bit UUIDv7. Derivation differs by source kind:

### 4.1 Pull sources — deterministic

```
UUIDv7 layout:  [ unix_ts_ms : 48 ][ ver:4 ][ rand_a:12 ][ var:2 ][ rand_b:62 ]

Pull derivation:
  unix_ts_ms  ← source event timestamp (Kafka ts, LSN→ts, binlog ts, oplog ts)
  rand_a (12) ← (broker_offset & 0xFFF)      — within-partition monotonic
  rand_b (62) ← SHA-256(source_name ‖ source.event_identity(ev)).take(62 bits)
```

Properties:

- **Deterministic on replay.** Same `(source, partition, offset)` always
  produces the same UUID, across crashes, restarts, and partition
  reassignments. Sink idempotency-key dedup works without persisting a mapping.
- **Time-ordered** by source event time — the correct reference frame.
- **Within-partition monotonic** via offset in `rand_a`. Up to 4096 events
  per ms per partition before rand_a collisions — comfortably above 20M/s
  distributed across partitions.
- **Zero storage cost.** Pure function of source-native identity.

Cost: one SHA-256 per event (~30 ns with hardware acceleration).

### 4.2 Push sources — random + monotonic

Standard RFC 9562 UUIDv7 generation with current microtime for the 48-bit
prefix and sub-ms counter in rand_a for within-ms monotonicity. Existing
implementation.

### 4.3 Poll sources — identity config

Poll sources have no upstream offset, so identity is config-driven:

```yaml
identity:
  mode: compound           # cursor | etag | content_hash | compound
  cursor_path: $.next      # primary when present
  content_hash:
    algorithm: xxhash3_64
    window: 5m             # L2-backed bounded seen-set TTL
```

- `cursor` / `etag` — upstream-authoritative; zero Aeon-side dedup cost.
- `content_hash` — Aeon-owned, bounded seen-set in L2. xxhash3_64 of
  normalised payload; ~30 ns/event + one L2 write.
- `compound` (default) — cursor primary, content_hash safety net. Degrades
  gracefully for uncooperative upstreams.

### 4.4 `event_time` configuration — no silent fallbacks

Timestamp source is explicit per connector config, no exceptions:

```yaml
event_time: broker                  # broker ts (default for connectors that expose it)
event_time: aeon_ingest             # Aeon microtime (loses cross-replay determinism)
event_time: header:X-Event-Time     # pull from named metadata header
```

Pipeline **fails at start** if `broker` is configured on a connector that
cannot produce it. No runtime fallback.

Cost:
- `broker`: free (already read).
- `aeon_ingest`: one `Instant::now()` ≈ 20 ns.
- `header:<name>`: SmallVec lookup + parse ≈ 50 ns.

---

## 5. L2 — Event Body Store

Reuses the existing `aeon-state/src/l2.rs` (646 lines, 10 tests — append-only
mmap with index + recovery + compaction). Extended for event-body workload:

- **Segment-rolled**: new segment every 256 MB (config: `l2.segment_bytes`).
  Oversized single events roll to a dedicated segment so large events don't
  force all segments up.
- **Per-partition per-pipeline**: `$STATE/l2/<pipeline>/<partition>/<seq>.seg`.
  Directory layout maps 1:1 to partition-single-owner model.
- **Append-only from pipeline runner**: writer is the source task; readers
  are sink tasks + (future) `ReplayOrchestrator`.
- **GC cursor**: segment `S` is deleted when
  `min(per_sink_ack_seq) > max_event_seq_in(S)`. Slowest sink holds the log
  — by design, falls out of the multi-sink correctness story.
- **fsync cadence** is durability-mode-driven (§7): `per_event` syncs each
  append; `ordered_batch` syncs at batch boundary; `unordered_batch` syncs
  at checkpoint cadence; `none` does not write L2 at all.
- **Format versioning**: 8-byte magic `AEON-L2B` + 2-byte version. Matches
  the discipline in the existing KV l2.rs format.

### 5.1 Recovery flow

On pipeline start (or partition migration):

1. Read L3 checkpoint for this partition. Get `source_offsets` + per-sink
   `ack_seq` values + `pending_event_ids`.
2. For **pull source partitions**: `Seekable::seek(source_offsets)`, drop
   any `pending_event_ids` that didn't make the last commit (sink dedups
   them on replay via EO-1/EO-3).
3. For **push/poll source partitions**: scan L2 from
   `min(per_sink_ack_seq) + 1` forward. Re-deliver to each sink that hadn't
   ack'd past that seq. Sink dedups.
4. Resume normal operation once all sinks are caught up to the head of L2.

### 5.2 Large-event tolerance

Events larger than `l2.segment_bytes` get their own dedicated segment; the
current segment is sealed early. Prevents pathological "one huge event
wedges segment allocation."

---

## 6. L3 — Checkpoint Store

Reuses existing `aeon-engine/src/checkpoint.rs` — `CheckpointRecord` +
`L3CheckpointStore` (backed by `L3Store` trait, redb default, RocksDB alt).

### 6.1 Record shape (extended from current)

```rust
pub struct CheckpointRecord {
    pub checkpoint_id: u64,
    pub timestamp_nanos: i64,
    pub source_offsets: HashMap<u16, i64>,        // per-partition broker offsets
    pub per_sink_ack_seq: HashMap<String, u64>,   // NEW — per-sink ack cursors
    pub pending_event_ids: Vec<[u8; 16]>,
    pub delivered_count: u64,
    pub failed_count: u64,
}
```

Addition over today: `per_sink_ack_seq` — required for the GC cursor
computation in §5 and for recovery in §5.1.

### 6.2 WAL as automatic fallback

Current code has `CheckpointBackend::{Wal, StateStore, Kafka, None}` as
peers (`pipeline.rs:834-873`). Design change:

- **Remove `Kafka`** variant entirely. Today it's already a no-op
  (`pipeline.rs:873` matches it the same as `None`). Removal is cosmetic.
- **`StateStore` becomes primary**. On L3 open/write error: log degradation,
  emit metric `aeon_checkpoint_fallback_wal{pipeline="..."}`, transparently
  begin writing to a WAL file in the state dir. Next successful L3 probe
  reverses the fallback.
- **`Wal`** remains as an explicit "I don't want L3" choice (small
  single-node deployments with no redb/rocksdb dependency).
- **`None`** remains as the durability-off escape hatch.

### 6.3 WAL is disk-backed (not in-memory)

Explicit: the WAL fallback writes to disk, same storage class as L3. An
in-memory WAL would defeat the purpose — a fallback that loses everything
on crash is not a fallback. The existing `CheckpointWriter`
(`aeon-engine/src/checkpoint.rs`) is already a file-backed append-only log
(magic `AEON-CKP` + version + CRC32 per record); this design reuses it
unchanged for the fallback path.

Why WAL + L3 when both are on disk:

| Aspect | L3 (redb/rocksdb) | WAL (append-only file) |
|--------|-------------------|------------------------|
| Write pattern | Random-access KV with transactions | Pure sequential append |
| Read pattern | Point lookup + scan by key prefix | Sequential scan from head |
| Dependencies | redb/rocksdb crate surface | `std::fs` + our format only |
| Failure modes | Corrupted page, full txn log, lock contention, crate bug | Disk full, fsync fail |
| Recovery | Open txn store, resume | Scan file, validate CRCs, skip trailing corruption |

So WAL fallback means "when the sophisticated store breaks, write to a
simpler format we control end-to-end so we don't lose checkpoints while
degraded." Not "same thing, different drive."

### 6.4 Fallback activation, replay, and recovery

Operational semantics:

1. On L3 open or write error, emit `aeon_checkpoint_fallback_wal{pipeline}`
   counter, log `WARN` once per transition.
2. Open a WAL file at `$STATE/checkpoints/<pipeline>.wal` (same directory
   as L3 database).
3. Subsequent checkpoint records append to WAL.
4. Background task re-probes L3 periodically. On probe success: replay WAL
   records into L3 (idempotent by `checkpoint_id`), delete WAL, return to
   L3-primary mode.
5. On pipeline restart with both files present: prefer L3 if healthy; if
   WAL has records beyond L3's last `checkpoint_id`, merge them into L3
   before resuming.

Throughput cost of fallback: zero on the event hot path (checkpoints are
1–10 Hz, ~100–200 B each). Documented in §10.3.

---

## 7. Durability Modes

Reuses existing `DeliveryStrategy` enum (`aeon-types/src/delivery.rs:60`).
Semantics extended from "ack timing" to also cover "event-body persistence".

| Mode | L2 write | L2 fsync | Sink ack | Use case |
|------|----------|----------|----------|----------|
| `none` | skipped | n/a | immediate | Best throughput, fire-and-forget, lossy on crash (documented, best-effort) |
| `unordered_batch` | append | at checkpoint (1–10 Hz) | batch-boundary | Max throughput with durability; within-batch order not guaranteed |
| `ordered_batch` (**default for EOS**) | append | at batch boundary + checkpoint | batch-boundary | Per-partition ordered delivery, EOS via sink dedup |
| `per_event` | append + fsync per event | every event | per-event | Regulated/financial workloads; budget deliberately blown |

### 7.1 Push-source ack boundary

When `durability != none`, acknowledgement to upstream (webhook 2xx /
WebSocket pong / WT stream continue / MQTT PUBACK / AMQP ack) happens
**after** L2 append completes. Honest: upstream only learns we have the
event when we actually do.

When `durability == none`, ack immediately — the RAM loss is accepted.

### 7.2 Backwards compatibility

Existing pipelines **default to `durability: none`** to preserve today's
behavior exactly. Opt-in only — no silent semantic change.

---

## 8. Sink EOS Tiers

Each sink declares its tier via `TransactionalSink::eos_tier()`. Engine
matches declaration against connector capability at pipeline start;
mismatch = fail fast.

| Tier | Examples | Native primitive |
|------|----------|------------------|
| **T1** Transactional DBs | Postgres, MySQL, SQLite, Mongo | tx COMMIT + `ON CONFLICT DO NOTHING` |
| **T2** Transactional streams | Kafka, Redpanda | `commit_transaction` + `send_offsets_to_transaction` |
| **T3** Atomic-rename files | File sink | write `.tmp` → rename |
| **T4** Dedup-keyed | Redis Streams, NATS JetStream | `SETNX + EX` / `Nats-Msg-Id` within `duplicate_window` |
| **T5** Idempotency-Key receivers | Webhook, WS, WT | `Idempotency-Key: <source_event_id>` — correctness **depends on receiver** |
| **T6** Fire-and-forget | Stdout, Blackhole | N/A — EOS not meaningful |

T5 caveat explicit in docs: EOS is receiver-dependent. If the webhook
target ignores the `Idempotency-Key` header, the honest ceiling is
at-least-once. Aeon doesn't silently claim more.

---

## 9. Multi-Source and Multi-Sink

### 9.1 Multi-source

Strongly justified. Use liberally when the processor's state requires
streams to merge. Competitive analogs: Vector.dev, Benthos/Bento, Flink,
Logstash.

**Ordering guarantees:**
- Within one source partition: strict.
- Across partitions within one source: none (matches Kafka).
- **Across sources: arrival order at processor input ring only. No semantic
  order promise.** If a user needs total order across sources, they must
  merge upstream (e.g. into a Kafka topic) and consume the merged stream.

Documented in the manifest schema + `docs/CONNECTORS.md`.

### 9.2 Multi-sink

Justified but with honest tradeoffs:

- **Slowest sink stalls the pipeline** (L2 GC cursor = `min(per_sink_ack)`).
- **Upgrade granularity is pipeline-wide.** Blue-green cuts over all sinks
  together.

**When to use multi-sink in one pipeline:** semantically coupled sinks —
primary+audit, hot+warm+cold tiered storage, multi-region same-event
replication.

**When to use separate pipelines instead:** independent consumers with
independent failure modes and upgrade cadences — e.g., two partner
webhooks with different reliability profiles.

This guidance goes in `docs/PIPELINE-DESIGN-GUIDE.md` (to be written).

---

## 10. Budget Hierarchy

Three levels; single-pipeline deployments set nothing.

| Level | Purpose | Config | Default |
|-------|---------|--------|---------|
| Node-global cap | Disk-size safety net | `state.l2.node_max_bytes` (node config) | 80% of state dir volume |
| Per-pipeline cap | Fair-share ceiling, SLO envelope | `pipeline.durability.l2_max_bytes` (manifest) | unset → inherits node-global |
| Per-partition soft target | Early-warning threshold | derived = `pipeline_cap / partitions` | auto |

### 10.1 Backpressure escalation

1. Partition exceeds soft target → slow that partition's source only.
   - Push: Phase 3 `PushBuffer` protocol-level flow control (reject 503, `basic.cancel`, etc.).
   - Pull: stop polling, broker queues up.
   - Poll: increase poll interval / skip poll.
2. Pipeline approaches its cap → throttle all partitions in that pipeline.
3. Node approaches global cap → throttle all pipelines, emit
   `aeon_l2_pressure{level="node"}` alert.

### 10.2 Node vs pipeline config surfaces

Explicit split — two different audiences:
- **Node config** (`/etc/aeon/config.yaml` or env): L3 backend choice,
  node-global L2 cap, state dir path. Owned by ops.
- **Pipeline manifest** (YAML applied via `aeon apply`): per-pipeline
  durability mode, L2 cap, sources/sinks/processor. Owned by pipeline
  author.

### 10.3 Hardware assumptions and per-mode throughput ceilings

The 20M ev/s target is realistic under specific, documented conditions.
This section makes the conditions explicit so operators can right-size
hardware and users don't mistake one mode's ceiling for another's.

#### Per-source-kind ceiling

| Scenario | L2 writes? | 20M/s ceiling holds? |
|----------|------------|----------------------|
| Pull-only pipeline (Kafka, CDC) at **any** durability mode | No — broker is buffer | **Yes, unconditionally**. L2 never touched; L3 writes are 1–10 Hz, ~100–200 B. |
| Push/poll at typical rates (100k–1M/s) | Yes | Yes — NVMe idle. |
| Push/poll at extreme rates (approaching 20M/s single node) | Yes | **NVMe-bound** — see SSD class table below. |
| Any source with `per_event` fsync mode | Yes | **No — deliberately outside budget**, regulated workloads only. |

#### SSD class requirements for 20M ev/s push/poll on a single node

At 20M events × 256 B average payload = **5 GB/s** raw throughput:

| NVMe class | Seq write ceiling | Headroom at 5 GB/s | Supported? |
|------------|-------------------|--------------------|------------|
| Consumer Gen3 (~3.5 GB/s) | 3.5 GB/s | 0.7× — **insufficient** | No |
| Consumer Gen4 (~7 GB/s) | 7 GB/s | 1.4× | Yes |
| Enterprise Gen4 (~10 GB/s) | 10 GB/s | 2.0× | Yes, comfortable |
| Enterprise Gen5 (13+ GB/s) | 13+ GB/s | 2.6×+ | Yes |
| HDD, any class | — | — | **Unsupported for any mode except `none`** |

Per-event append cost on mmap (separate from bandwidth): 20–50 ns
memcpy-to-mapped-page. Fits inside the 400 ns/core budget.

#### fsync cost by durability mode

| Mode | fsync cadence | Cost at 20M ev/s |
|------|---------------|-------------------|
| `none` | never | 0 ns/event |
| `unordered_batch` | at checkpoint (1–10 Hz) | ≤20k fsyncs/s × ~1 μs each = <0.1% wall time → **~0 ns/event amortised** |
| `ordered_batch` | at batch boundary (~1k events) + checkpoint | Same magnitude as `unordered_batch` → **~0 ns/event amortised** |
| `per_event` | every event | 20M fsyncs/s — **not achievable on any disk**; mode is for compliance workloads at 1k–10k ev/s scale |

#### Partition parallelism (what makes the ceiling reachable)

Modern NVMe reaches peak throughput only at deep queue depth (≥32
concurrent writes). Single-partition pipelines underutilise the drive;
8–16 partitions bring queue depth into the saturation zone.

| Partitions | Writer streams | NVMe utilisation |
|------------|----------------|-------------------|
| 1 | 1 sequential writer | Bandwidth-limited by a single queue; far below drive ceiling |
| 8 | 8 independent writers | Near peak on Gen4+ |
| 16+ | 16+ independent writers | Saturation on Gen4+ |

This is why the per-partition single-owner model (Raft ownership + disk
throughput) was locked earlier. Each partition owns its own L2 segment
file + writer task + CPU core; no cross-partition coordination, no
shared lock.

#### WAL fallback throughput impact

Zero on the event hot path. WAL writes only checkpoint records (~100–200
B at 1–10 Hz), not event bodies. Whether the pipeline is on L3 or on
WAL fallback, the 5 GB/s event-body bandwidth goes to L2 either way.

#### Documented risks (operators must see these)

1. **HDD is unsupported for any durability mode except `none`.** Random
   fsyncs on HDD are fatal to latency. Aeon will run but miss every SLA.
2. **`per_event` mode is explicitly non-throughput-optimal.** Document
   the cost at knob-flip time — regulated workloads only.
3. **L2 GC cursor stalls on slow-sink scenarios.** If the slowest sink
   goes offline, L2 fills until cap is hit → Phase 3 protocol-level flow
   control → upstream sees 503s (webhook) / paused reads (WS/WT) / etc.
   Correct by design; operators must know "slow sink ⇒ upstream feels
   it" is the intended behaviour.
4. **Push/poll at 20M/s on single node requires Gen4+ NVMe and ≥8
   partitions.** Benchmarks run on lower classes should not be compared
   to the 20M/s claim.

#### Summary claim (to be used in docs and marketing)

- **Pull pipelines: 20M ev/s aggregate at any durability mode** (multi-node
  scaling, broker replay for recovery).
- **Push/poll pipelines at `ordered_batch` or `unordered_batch`: 20M ev/s
  aggregate** given Gen4+ NVMe and ≥8 partitions per node.
- **Push/poll pipelines at `per_event`: ceiling is tens of thousands of
  events/sec per partition**, scales linearly with partition count.
  Intended for regulated/financial workloads where per-event durability
  is contractually required.

---

## 11. Pipeline Manifest Shape

See the full annotated example committed in §12 below. Summary of new
fields relative to today's manifest:

- `durability` block (mode, l2_max_bytes, checkpoint backend, flush).
- `sources` is now a **list** (was single).
- Each source: `kind: pull|push|poll`, `identity`, `event_time`.
- `sinks` is now a **list** (was single).
- Each sink: `eos_tier` declaration.
- `CheckpointBackend::Kafka` removed from schema.

---

## 12. Annotated Example

```yaml
pipelines:
  - name: orders-enrichment
    # ─── Durability (pipeline-level, inherits to sources/sinks) ─────────
    durability:
      mode: ordered_batch        # none | unordered_batch | ordered_batch | per_event
      l2_max_bytes: 8Gi          # per-pipeline cap; per-partition soft target derived
      checkpoint:
        backend: state_store     # state_store (primary) | wal | none
                                 # state_store auto-falls-back to wal on L3 error
        interval: 1s
        retention: 24h
      flush:
        max_pending: 50000
        adaptive: true

    partitions: 8                # immutable after creation

    sources:
      # Pull source — deterministic UUIDv7 from (source, partition, offset)
      - name: orders-kafka
        type: kafka
        kind: pull
        topics: [orders]
        partitions: [0, 1, 2, 3, 4, 5, 6, 7]
        identity: { mode: native }
        event_time: broker
        # Connector-specific keys are flattened onto the source (or sink)
        # itself — do NOT nest under a literal `config:` block. See the
        # `source_config_keys_must_be_flat_not_nested` test in
        # `aeon-types::manifest` for the authoritative shape.
        bootstrap.servers: "redpanda:9092"

      # Push source — random UUIDv7, ack-after-L2
      - name: returns-webhook
        type: webhook
        kind: push
        listen: "0.0.0.0:8080"
        path: /returns
        identity: { mode: random }
        event_time: header:X-Event-Time
        backpressure:
          phase1_buffer: 1024
          phase3_threshold: 0.8

      # Poll source — compound identity, L2-backed dedup window
      - name: weather-api
        type: http_polling
        kind: poll
        url: "https://api.weather.example/current"
        interval: 30s
        jitter: 10%
        timeout: 5s
        identity:
          mode: compound
          cursor_path: $.next_cursor
          content_hash:
            algorithm: xxhash3_64
            window: 5m
        event_time: aeon_ingest

    processor:
      name: orders-enricher
      version: "1.4.2"
      tier: t2_wasm
      runtime:
        fuel: 10_000_000
        memory_pages: 256

    sinks:
      - name: enriched-kafka
        type: kafka
        eos_tier: t2_transactional_stream
        topic: orders-enriched
        # Same flatten rule on SinkManifest — connector keys at the top.
        bootstrap.servers: "redpanda:9092"
        transactional.id: "aeon-orders-enrichment-enriched"

      - name: cache-redis
        type: redis_streams
        eos_tier: t4_dedup_keyed
        stream: orders:enriched
        dedup:
          key_prefix: "aeon:dedup:"
          ttl: 1h

      - name: partner-webhook
        type: webhook_sink
        eos_tier: t5_idempotency_key
        url: "https://partner.example/events"
        headers:
          Idempotency-Key: "{event.id}"
        retry:
          max_attempts: 5
          backoff: { initial: 100ms, max: 30s, jitter: 20% }

    upgrade_strategy: blue_green
```

---

## 13. Coupling to Other Work

- **CL-6 partition transfer** (Pillar 3): L2 segments are per-partition.
  When a partition migrates, its L2 segments must stream to the target
  node as part of CL-6a (bulk sync). EO-2 pipeline wiring should land
  before or alongside CL-6a, or CL-6a needs to be L2-aware from the start.
- **`BatchInflight` / `ReplayOrchestrator`**: stays RAM-only across all
  durability modes. Labeled in code + docs as "best-effort convenience for
  transient processor disconnects," not a durability guarantee. When
  `durability: none`, it's the *only* retention mechanism — loss on crash
  is accepted.
- **EO-1 Kafka `IdempotentSink`**: already shipped. Stays the dedup path
  for T2 sinks on replay.
- **EO-3 Redis/NATS `IdempotentSink`**: already shipped. Stays the dedup
  path for T4 sinks on replay.
- **FT-3 L3 checkpoint store**: already shipped. Extended with
  `per_sink_ack_seq` field (§6.1).
- **TR-1 `ReplayOrchestrator`**: stays as today; future refactor to read
  from L2 when durability is on is a nice-to-have, not blocking.

---

## 14. Deprecations

- `CheckpointBackend::Kafka` — removed from the enum. Already a silent
  no-op at `pipeline.rs:873`; removal is rename + test/bench cleanup.

---

## 15. Observability

New metrics (all per-pipeline, per-partition where relevant):

| Metric | Type | Labels | Purpose |
|--------|------|--------|---------|
| `aeon_l2_bytes` | gauge | `pipeline`, `partition` | Current L2 size per partition |
| `aeon_l2_segments` | gauge | `pipeline`, `partition` | Active segment count |
| `aeon_l2_gc_lag_seq` | gauge | `pipeline`, `partition` | `max_seq − min(per_sink_ack)` — how far behind the slowest sink is |
| `aeon_l2_pressure` | gauge | `level=partition|pipeline|node` | 0–1 fill ratio; ≥1 triggers backpressure |
| `aeon_sink_ack_seq` | gauge | `pipeline`, `partition`, `sink` | Per-sink ack cursor |
| `aeon_checkpoint_fallback_wal` | counter | `pipeline` | L3 → WAL fallback events |
| `aeon_uuid_identity_collisions` | counter | `pipeline`, `source` | Hash collision detection (should stay 0) |

---

## 16. Testing Plan

- **Unit**: L2 segment rollover, GC cursor computation, UUIDv7 determinism
  (same input → same UUID, replay roundtrip), `event_time` validation
  failures, `CheckpointBackend::Kafka` absent from enum.
- **Integration**: crash-recovery test (kill during batch, restart, verify
  no loss + no duplicates), slow-sink backpressure test (multi-sink with
  one stalled; verify L2 GC holds; fast sinks don't race), multi-source
  interleaving test (pull + push + poll into one processor), L3-to-WAL
  failover test (corrupt L3, verify transparent WAL fallback + metric
  emission).
- **Benchmarks**: per-event overhead with each durability mode, L2 write
  throughput at 20M ev/s, fsync amortisation at various checkpoint
  cadences, content-hash dedup cost per poll source.

---

## 17. Implementation Phases

| Phase | Scope | Depends on | Notes |
|-------|-------|------------|------|
| **P1** | Schema & config: extend `DeliveryStrategy` semantics, add `SourceKind` enum, extend `CheckpointRecord` with `per_sink_ack_seq`, remove `CheckpointBackend::Kafka` | — | Non-functional prep. |
| **P2** | UUIDv7 derivation — per-source-kind; `event_identity()` trait method; `event_time` config validation | P1 | Unit-testable standalone. |
| **P3** | L2 event-body store — segment rollover, per-partition paths, append API, GC cursor | P1 | Builds on `aeon-state/src/l2.rs`. |
| **P4** | Pipeline runner wiring — L2 write on source batch (push+poll), L2 read on sink task, GC sweep, fsync cadence by mode | P2, P3 | Ties into `pipeline.rs` `run_buffered_*` variants. |
| **P5** | L3 → WAL fallback orchestration; `per_sink_ack_seq` persistence; recovery flow | P1 | Extends `checkpoint.rs` `L3CheckpointStore`. |
| **P6** | Multi-source + multi-sink manifest shape (arrays); validation (EOS tier declaration matches connector capability) | P1 | Manifest parser + `aeon apply` changes. |
| **P7** | Backpressure hierarchy (partition → pipeline → node); escalation paths per source kind | P3, P4 | Wires `PushBuffer` Phase 3 + pull stop-polling + poll interval scaling. |
| **P8** | Observability — all metrics listed in §15 | P3, P4, P5 | Hooks into existing `PipelineObservability`. |
| **P9** | Integration tests: crash-recovery, slow-sink, multi-source, L3→WAL failover | P4, P5 | Pre-merge gate. |
| **P10** | Benchmarks — validate per-event cost, fsync amortisation, 20M/s target still holds | P4 | Gate on performance regression. |
| **P11** | Docs: `PIPELINE-DESIGN-GUIDE.md` (multi-sink vs multi-pipeline), `CONNECTORS.md` source_kind matrix, migration guide for existing pipelines | — | Written alongside implementation. |
| **P12** | CL-6a coordination — ensure partition transfer moves L2 segments | Pillar 3 CL-6a | Shared design effort with cluster team. |

**Recommended landing order**: P1 → P2 → P3 → P5 → P4 → P6 → P7 → P8 → P9
→ P10 → P11 → P12. Each phase is independently testable; no phase lands
without its tests.

---

## 18. Open Items

Nothing blocking. All design questions from the 2026-04-15 discussion are
resolved. Items that will surface during implementation:

- Exact L2 segment size default (256 MB proposed; may tune after P10 benchmarks).
- Whether `ReplayOrchestrator` refactors to read from L2 — deferred
  post-implementation; cleanup, not correctness.
- Prior L3-ACK-TTL code review — deferred per user direction; revisit if
  anything in the shipped code conflicts with this design during P1.

---

## 19. Frozen Decisions Checklist

| # | Decision | Locked |
|---|----------|--------|
| 1 | Source owns identity; sink owns EOS tier primitive | ✅ |
| 2 | Three source kinds: Pull / Push / Poll | ✅ |
| 3 | UUIDv7 deterministic for Pull; random for Push; config for Poll | ✅ |
| 4 | `event_time` config: broker \| aeon_ingest \| header:<name>, no silent fallback | ✅ |
| 5 | L2 = event bodies (push+poll only); L3 = checkpoints | ✅ |
| 6 | WAL = automatic fallback of L3, not a peer | ✅ |
| 7 | `CheckpointBackend::Kafka` removed | ✅ |
| 8 | Durability modes reuse `DeliveryStrategy`: none / unordered_batch / ordered_batch / per_event | ✅ |
| 9 | Existing pipelines default to `durability: none` (backwards compat) | ✅ |
| 10 | Push ack boundary: after L2 when durability on; immediate when off | ✅ |
| 11 | Multi-source first-class; per-partition order only; no cross-source merge | ✅ |
| 12 | Multi-sink first-class; slowest sink paces pipeline; separate-pipeline guidance for independent sinks | ✅ |
| 13 | Budget hierarchy: node-global → per-pipeline → per-partition | ✅ |
| 14 | Node vs pipeline config surfaces split explicitly | ✅ |
| 15 | No cross-sink atomicity (per-sink EOS only) | ✅ |
| 16 | `ReplayOrchestrator` stays RAM-only across all modes, documented best-effort | ✅ |
| 17 | `partitions` immutable after pipeline creation; no cross-partition merge | ✅ |
| 18 | Deterministic UUIDv7 identity via centralized SHA-256 hashing over per-connector `event_identity()` bytes | ✅ |
| 19 | WAL is disk-backed (not RAM); automatic transient fallback of L3 with replay-back on L3 recovery — see §6.3/§6.4 | ✅ |
| 20 | Per-mode throughput ceilings documented with SSD class requirements; HDD unsupported for any durability mode ≠ `none` — see §10.3 | ✅ |
