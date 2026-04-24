# Connector Backpressure Audit

**Date**: 2026-04-09
**Scope**: All source and sink connectors in `crates/aeon-connectors/`, plus processor
integration tiers (T1 native, T2 Wasm, T3 WebTransport, T4 WebSocket).

**Goal**: Verify every connector's backpressure strategy matches the nature of its
upstream/downstream, and that the pipeline engine (`run_buffered`) propagates flow
control end-to-end without unbounded intermediate queues.

---

## 1. Pipeline Backpressure Model (Reference)

`crates/aeon-engine/src/pipeline.rs::run_buffered` runs three concurrent tasks
connected by two SPSC ring buffers (`rtrb`):

```
Source task ──[source SPSC]──▶ Processor task ──[sink SPSC]──▶ Sink task
```

- **Source task** (`pipeline.rs:392-423`): calls `source.next_batch()`, pushes the
  batch into the source SPSC. If the ring is full, it yields with
  `tokio::task::yield_now().await` and retries.
- **Processor task** (`pipeline.rs:431-470`): pops from source SPSC, invokes
  `processor.process_batch()` synchronously, pushes outputs to the sink SPSC. Same
  yield-on-full pattern.
- **Sink task** (`pipeline.rs:505-699`): pops outputs, calls `sink.write_batch()`,
  applies `DeliveryStrategy` and `BatchFailurePolicy`. For non-blocking strategies
  it accumulates `pending_count` and calls `sink.flush()` at `flush_interval` or
  `max_pending`.

**Consequence**: backpressure propagates backward through SPSC fullness. When the
sink stalls, sink SPSC fills → processor yields → source SPSC fills → source
yields → source's `next_batch` back-offs naturally. No unbounded queue sits between
stages **as long as each stage only pushes when there is room**. Any connector that
violates this by buffering internally without bound is a gap.

`DeliveryStrategy::is_blocking()` (`aeon-types/src/delivery.rs:61`) controls whether
the sink task treats `write_batch` as synchronous (`PerEvent`, `OrderedBatch`) or
decoupled (`UnorderedBatch`, flushed at interval).

---

## 2. Source Connector Matrix

| Connector | Model | Upstream bound? | Buffer | Phase 3 flow control | Offset tracking | Status |
|---|---|---|---|---|---|---|
| Memory | Pull | Bounded (Vec) | — | N/A | No | Test-only, OK |
| Kafka | Pull | Bounded (offsets) | rdkafka prefetch | Implicit (offsets) | **Yes** | Production-grade |
| HTTP Polling | Poll | Controlled interval | — | N/A | No | OK |
| HTTP Webhook | Push | Unbounded | PushBuffer | **HTTP 503** | No | OK — best Phase 3 |
| WebSocket | Push | Unbounded | PushBuffer | TCP window (blocking send) | No | OK (fixed §4.1) |
| NATS JetStream | Pull | Bounded (consumer grp) | — | Implicit (ack) | Implicit | OK |
| MQTT | Push | Unbounded | PushBuffer | TCP window (blocking send) | No | OK (fixed §4.2) |
| RabbitMQ | Push | Unbounded | PushBuffer | **nack + requeue** | No | OK — best-in-class |
| Redis Streams | Pull | Bounded (pending) | — | Implicit (XACK) | Yes | OK |
| Postgres CDC | Pull | Bounded (LSN) | — | Implicit | Yes | OK (polling, see §4.5) |
| MySQL CDC | Pull | Bounded (binlog pos) | — | Implicit | Yes | OK (polling, see §4.5) |
| MongoDB CDC | Pull | Bounded (resume token) | PushBuffer (internal shaping) | Blocking send only | File-based (at-least-once) | OK (fixed §4.3, reclassified §8.1) |
| QUIC | Push | Unbounded | PushBuffer | Zero-length frame | No | OK |
| WebTransport (streams) | Push | Unbounded | PushBuffer | Zero-length frame | No | OK |
| WebTransport (datagrams) | Push | Lossy | PushBuffer | **intentional drop** | No | OK (opt-in `accept_loss`) |

**PushBuffer** (`crates/aeon-connectors/src/push_buffer.rs`) is the shared
three-phase backpressure primitive used by every push source:

- Phase 1: bounded `tokio::mpsc` channel (default 8192).
- Phase 2: when channel is full, `PushBufferTx::send` falls back to `.await`
  on the channel (line 47-51) — this is blocking, not dropping.
- Phase 3: when spill count exceeds `spill_threshold` (default 4096),
  `is_overloaded()` flag is set; connectors read it to apply protocol-level
  backoff (HTTP 503, AMQP nack, MQTT sleep, QUIC zero-length frame, etc.).

---

## 3. Sink Connector Matrix

| Connector | Delivery strategies honored | write_batch cost | flush() | Status |
|---|---|---|---|---|
| Memory | None (always all_delivered) | O(1) push | no-op | Test-only, OK |
| Blackhole | None (always all_delivered) | O(1) | no-op | Bench-only, OK |
| Stdout | None (always all_delivered) | Blocking stdout write | no-op | Debug-only, OK |
| File | **All three honored** | Per-event fsync / batch fsync / buffered | Drains BufWriter | Production-grade |
| Kafka | **All three honored** | Per-future await / `join_all` / enqueue only | `producer.flush()` | Production-grade |
| NATS JetStream | **All three honored** | Per-ack / batch-ack / deferred-ack | Awaits `pending_acks` | Production-grade |
| MQTT | **N/A** — rumqttc API is already fire-and-enqueue | Always per-publish enqueue | no-op | No differentiation possible — see §4.4 |
| RabbitMQ | **All three honored** | Per-confirm await / `join_all` / stash for flush | Awaits `pending_confirms` | Production-grade |
| Redis Streams | **All three honored** | Per-XADD / pipelined XADD / spawned pipeline | Awaits `pending_tasks` | Production-grade |
| QUIC | None differentiated | New stream per batch | no-op | Functional, see §4.6 |
| WebTransport | None differentiated | New stream per batch | no-op | Functional, see §4.6 |
| WebSocket | None differentiated | Per-output `send` + loop await | `writer.flush()` | Functional |

---

## 3.5 CLI Registry Wiring Status (2026-04-20)

Separate from "is the library code correct?" is "can it be declared in a YAML
manifest?" Today, `aeon-cli/src/connectors.rs::register_defaults` only registers
**two sources** and **three sinks**. Everything else in this audit exists as
library code but is unreachable from a pipeline manifest. Surfaced during V4
(`docs/ROADMAP.md` §Phase 3.5) on 2026-04-20.

| Connector | Library exists? | Audited OK? | Wired into CLI? |
|---|---|---|---|
| memory (source + sink) | ✅ | ✅ | ✅ source only |
| blackhole sink | ✅ | ✅ | ✅ |
| stdout sink | ✅ | ✅ | ✅ |
| kafka (source + sink) | ✅ | ✅ | ✅ |
| http-webhook source (push) | ✅ | ✅ | ⏳ V4 (this session) |
| http-polling source (poll) | ✅ | ✅ | ⏳ P5.c (post-V6) |
| http sink | ✅ | — | ⏳ P5.c |
| file (source + sink) | ✅ | ✅ | ⏳ P5.c |
| websocket (source + sink) | ✅ | ✅ | ⏳ P5.c |
| mqtt (source + sink) | ✅ | ✅ | ⏳ P5.c |
| rabbitmq (source + sink) | ✅ | ✅ | ⏳ P5.c |
| nats (source + sink) | ✅ | ✅ | ⏳ P5.c |
| redis-streams (source + sink) | ✅ | ✅ | ⏳ P5.c |
| quic (source + sink) | ✅ | ✅ | ⏳ P5.c |
| webtransport (streams + datagrams + sink) | ✅ | ✅ | ⏳ P5.c |
| mongodb-cdc source | ✅ | ✅ | ⏳ P5.c |
| postgres-cdc source | ✅ | ✅ | ⏳ P5.c |
| mysql-cdc source | ✅ | ✅ | ⏳ P5.c |

**P5.c** (`docs/ROADMAP.md` §Phase 5) is the follow-up that wires the remaining
11 sources and 9 sinks using the pattern established by V4. Pattern-repetition
work (~0.5–1 day): feature-gate in `aeon-cli/Cargo.toml`, add `*Factory` stubs
following the `KafkaSourceFactory` shape, extend `register_defaults`, extend
the unit tests in `connectors.rs`, and document YAML schema in
`docs/CONNECTORS.md`. Not a Session B blocker — runs after V6.

---

**Pipeline-level metric bug** (see §4.0): the sink task only credits `outputs_sent`
from `batch_result.delivered.len()`. For `UnorderedBatch`, sinks return
`BatchResult::all_pending(ids)` — pending count is accumulated, but when `flush()`
completes successfully, no mechanism propagates the flushed count back to
`outputs_sent`. The counter stays at 0 for the entire run, defeating the zero-loss
metric. **This is a Scenario 1 (Redpanda) correctness bug.**

---

## 4. Identified Gaps and Fixes

### 4.0 — PipelineMetrics.outputs_sent not incremented for UnorderedBatch

**Severity**: Correctness bug, affects Kafka/Redpanda on the Gate 1 path.
**Location**: `crates/aeon-engine/src/pipeline.rs:587-594`, `:639` (flush call).

**Current behavior**:
```rust
let delivered_count = batch_result.delivered.len() as u64;
metrics_sink.outputs_sent.fetch_add(delivered_count, Ordering::Relaxed);
```
For `UnorderedBatch`, `delivered` is always empty (sinks return `all_pending`).
The pipeline then calls `sink.flush().await?` which resolves the pending futures,
but the flush result is `Result<(), AeonError>` — no count returned, no metric
updated.

**Fix** (smallest workable): track pending count locally; on successful flush,
credit `outputs_sent` with the pending count that was drained. This matches the
sink's internal state without changing the `Sink` trait.

**Alternative** (cleaner, bigger): change `Sink::flush` to return
`Result<FlushResult, AeonError>` where `FlushResult` has a `delivered: Vec<Uuid>`
or at least a `delivered_count: usize`. That lets the ledger track individual
acks on flush instead of credited-in-bulk.

**Decision**: start with the smallest workable fix (credit on success). Ledger
integration is a follow-up because it requires changing every sink connector.

### 4.1 — WebSocket source drops messages on overload

**Severity**: Correctness bug — any "unbounded upstream" producer behind the
push_buffer should apply protocol-level backpressure, not silently drop.
**Location**: `crates/aeon-connectors/src/websocket/source.rs:87-91, 104-107`.

**Current behavior**:
```rust
if tx.is_overloaded() {
    tracing::warn!("websocket source overloaded, dropping message");
    continue;
}
```
The drop branch runs *before* `tx.send(event).await`. Since `PushBufferTx::send`
is already blocking (falls back to `.await` on channel-full, `push_buffer.rs:47`),
the caller would naturally apply TCP-level backpressure through tungstenite's
read loop — we don't need to drop. The `is_overloaded()` check was intended for
protocol-level backoff like HTTP 503, but WebSocket has no equivalent response
code; the correct WebSocket behavior is to *stop reading*, which blocking send
achieves automatically.

**Fix**: remove the `is_overloaded()` drop branches. Let `tx.send(event).await`
block and let TCP flow control handle the rest. The reader task stops polling
`read.next()` while awaiting `tx.send`, so the socket naturally stops reading
and backpressure reaches the peer via TCP window.

**Test**: unit test that produces 2x channel capacity + 2x spill threshold events
and asserts zero loss.

### 4.2 — MQTT source sleep-then-continue — FIXED

**Severity** was: Low — the sleep-poll approach *did* backpressure, but
coarsely (100ms fixed sleep, no signal coupling to actual drain rate).
**Location**: `crates/aeon-connectors/src/mqtt/source.rs::mqtt_reader`.

**Previous behavior**: when `tx.is_overloaded()`, sleep 100ms and continue.
This added 100ms latency spikes under pressure and was a fixed constant
regardless of actual drain rate.

**Fix (landed this pass)**: removed the `is_overloaded()` sleep branch
entirely. `tx.send(event).await` already blocks when the push buffer is
full, which suspends the reader task, which stops `EventLoop::poll()`,
which stops draining rumqttc's internal receiver, which stops reading
from the TCP socket. The OS receive window fills and the broker slows its
delivery (QoS 0) or stops delivering pending messages until we catch up
and ack (QoS 1/2). Same TCP-flow-control mechanism as the WebSocket source
fix in §4.1.

**Regression test**: the existing
`push_buffer::tests::test_push_buffer_zero_loss_under_backpressure` guards
the invariant for all push sources that use `PushBufferTx::send` — MQTT,
WebSocket, QUIC, WebTransport streams, HTTP Webhook, MongoDB CDC, and
RabbitMQ all share the same blocking-send contract.

### 4.3 — MongoDB CDC resume token persistence

**FIXED in this pass.**

**Before**: the connector opened a change stream with no `resume_after`
and discarded the per-event `ResumeToken`. After a crash or clean restart
the stream resumed at "now", silently skipping every change that occurred
while the process was down.

**After**: `crates/aeon-connectors/src/mongodb_cdc/source.rs`

- `MongoDbCdcSourceConfig` gains two fields:
  - `resume_token_path: Option<PathBuf>` — file path for the persisted
    token. If `None`, behavior is unchanged (stream starts from "now").
  - `resume_token_flush_every_n: usize` — flush cadence (default 100).
- On `MongoDbCdcSource::new`, if the file exists and parses, the token is
  passed to `ChangeStreamOptions::resume_after` before calling `watch()`.
  Unparseable / missing files log a warning and fall through to "start
  from now" (never fail startup).
- The reader task captures `change_event.id.clone()` as the latest token,
  bumps an unflushed counter after every `tx.send`, and atomically writes
  the latest token to disk (write-to-temp + rename) every N events.
- On reader shutdown (stream close or buffer closed), a final flush
  ensures a clean stop never loses the latest observed token.
- Persistence format: JSON via serde (`ResumeToken` derives
  `Serialize`/`Deserialize`). A token is ~100 bytes on disk.

**Delivery semantics**: at-least-once. The token is flushed after the
event is handed to the push buffer, not after sinks ack it, so a crash
between "buffered" and "delivered" will re-process the affected window.
Exactly-once would require threading the token through the pipeline
delivery-ledger checkpoint; since `CheckpointRecord.source_offsets` is
`HashMap<u16, i64>`, that means either a WAL format bump (v1 → v2 with
a parallel `source_tokens: HashMap<u16, Vec<u8>>`) or a sidecar store.
Deferred — at-least-once with file-based resume is a safe, standard
starting point for CDC workloads and closes the correctness gap.

**Regression tests** (`mongodb_cdc::source::tests`, 6 tests):
- `load_missing_file_returns_none` — nonexistent path is not an error
- `load_empty_file_returns_none` — zero-byte file is not an error
- `save_then_load_round_trips_token` — write + read + equality
- `save_overwrites_existing_token_atomically` — overwrite leaves no
  stale `.token.tmp` sibling
- `save_creates_missing_parent_dirs` — nested paths auto-create dirs
- `load_corrupt_file_returns_err` — invalid JSON surfaces `AeonError`

### 4.4 — MQTT, RabbitMQ, Redis sink strategies

**RabbitMQ** — **FIXED in this pass**.
`crates/aeon-connectors/src/rabbitmq/sink.rs` now honors all three delivery
strategies, mirroring the Kafka sink contract:
- `PerEvent`: `basic_publish().await` followed by `confirm.await` per output.
- `OrderedBatch` (default): issue all publishes in order, then `join_all` the
  `PublisherConfirm` futures at the batch boundary. AMQP preserves per-channel
  publish order, so ordering is maintained even when confirms are awaited
  concurrently.
- `UnorderedBatch`: stash the `PublisherConfirm` futures in
  `pending_confirms: Vec<PublisherConfirm>`; `write_batch()` returns
  `BatchResult::all_pending(ids)`. `flush()` drains and `join_all`s the vec,
  then credits `self.delivered`. Nacks are converted to `AeonError` via
  `check_confirmation()`.

**Redis Streams** — **FIXED in this pass**.
`crates/aeon-connectors/src/redis_streams/sink.rs` now honors all three:
- `PerEvent`: XADD each output with a per-call `query_async`.
- `OrderedBatch` (default): build a single `redis::pipe()` containing all XADDs
  and execute it in one round-trip. Redis is single-threaded and pipelined
  commands execute in order, so ordering is preserved.
- `UnorderedBatch`: spawn the pipeline execution on a tokio task (the
  `MultiplexedConnection` is clonable and internally multiplexes concurrent
  commands onto one TCP connection). Store the `JoinHandle` in
  `pending_tasks`. `flush()` awaits all handles and credits them. Useful when
  the pipeline sink task wants to decouple write_batch latency from the
  broker round-trip.

**MQTT** — **strategy differentiation is infeasible at the current rumqttc API
level.** This is a finding, not a deferral.
`rumqttc::AsyncClient::publish()` (`~/.cargo/registry/src/index.crates.io-*/
rumqttc-0.24.0/src/client.rs:69-89`) signature is
`async fn publish(...) -> Result<(), ClientError>`. The implementation
constructs a `Request::Publish(...)` and sends it over an internal flume
channel to the `EventLoop` task; it returns as soon as the request is
enqueued on that channel. **There is no per-publish future or handle
returned** — the PUBACK (QoS 1) / PUBCOMP (QoS 2) for that specific message
is consumed inside the EventLoop and never surfaced back to the caller. The
only failure mode `publish()` reports is "failed to enqueue the Request"
(client dropped or channel closed).

This means the `AsyncClient::publish().await` path is already semantically
identical to `UnorderedBatch`: publishes are batched inside rumqttc's own
request queue, processed asynchronously by the EventLoop, and confirmed by
the broker in the background. There is nothing for the Aeon sink to stash.
The three Aeon-level strategies would all compile down to the same for-loop
of `client.publish(...).await`.

**What would be required to add real strategy differentiation for MQTT**:
1. A client that exposes PUBACK/PUBCOMP events via a side channel (rumqttc's
   `EventLoop::poll()` does emit these as `Incoming::PubAck`/`Incoming::PubComp`,
   but the sink currently doesn't see them — they are consumed by the
   background poll loop spawned in `MqttSink::new`, `sink.rs:81-91`).
2. A packet-id → oneshot map maintained inside the sink, populated from the
   poll loop, so the sink can await per-publish confirmation.
3. This is a non-trivial redesign of the MQTT sink and is **not justified for
   Scenario 1** (MQTT is post-Gate 2 per `CLAUDE.md`). Captured here so a
   future pass can pick it up; the MQTT sink currently behaves correctly for
   every strategy by virtue of rumqttc's architecture.

**No code change** to `mqtt/sink.rs` in this pass; the per-publish await is
semantically correct given the API surface. The module-level docs could be
amended to state this explicitly so future readers don't mistake it for a
bug, but that is a cosmetic change.

### 4.5 — Postgres/MySQL CDC use polling instead of streaming replication

**Severity**: Performance — acceptable for low-rate CDC, won't scale to high
event rates.
**Locations**: `postgres_cdc/source.rs:150` uses `pg_logical_slot_get_changes`,
`mysql_cdc/source.rs:150` uses `SHOW BINLOG EVENTS`.

**Fix**: use `START_REPLICATION` (Postgres streaming replication protocol) and
MySQL binlog client library respectively. This is significant work —
Debezium-scale rework.

**Deferred** — post-Gate 2.

### 4.6 — QUIC/WebTransport sinks open a new stream per batch

**Severity**: Performance — each batch pays stream-setup overhead.
**Locations**: `quic/sink.rs:82`, `webtransport/sink.rs:65`.

**Fix**: keep a long-lived bidi stream open, write length-prefixed frames
into it, await ack frames from the peer. The `WebTransportProcessorHost`
already does this pattern (`webtransport_host.rs`); the sinks can mirror it.

**Deferred** — post-Gate 2; these are T3/T4 processor *transport* concerns
already handled better by the processor host implementations.

---

## 5. Processor Integration Tier Audit

| Tier | Transport | Engine invocation | Backpressure reaches source? | Pipeline integration | Status |
|---|---|---|---|---|---|
| T1 Native | dylib (libloading) | Sync call in processor task | Yes, via SPSC | `run_buffered()` via `Processor` trait | Complete |
| T2 Wasm | Wasmtime in-process | Sync call in processor task | Yes, via SPSC | `run_buffered()` via `Processor` trait | Complete |
| T3 WebTransport | QUIC, out-of-process | Async `call_batch` via `ProcessorTransport` | OK (fixed §5.3) | `run_buffered_transport` + bounded `BatchInflight` | Production-grade |
| T4 WebSocket | WS, out-of-process | Async `call_batch` via `ProcessorTransport` | OK (fixed §5.3) | `run_buffered_transport` + bounded `BatchInflight` | Production-grade |

### 5.1 T1 Native — OK

`crates/aeon-engine/src/native_loader.rs` — `NativeProcessor` implements
`Processor` (`native_loader.rs:266-339`). The sync `process_batch` call
inside the engine's processor task blocks the task while the guest runs,
which naturally propagates SPSC backpressure upstream. No gap.

### 5.2 T2 Wasm — OK

`crates/aeon-wasm/src/processor.rs` — `WasmProcessor` also implements the
sync `Processor` trait. Fuel metering (`runtime.rs:24`) bounds per-call CPU
but returns a hard error on exhaustion, which terminates the pipeline. That
is a *feature* for now (forces operators to tune fuel limits); future work
could add cooperative yield.

### 5.3 T3/T4 — FIXED: `run_buffered_transport` + bounded `BatchInflight`

`crates/aeon-engine/src/transport/webtransport_host.rs` and
`.../websocket_host.rs` both implement `aeon-types::ProcessorTransport` —
an *async* trait — not the sync `Processor` trait. Before this pass the
engine's `run_buffered` signature (`P: Processor + Send + Sync + 'static`)
could not accept them, and `BatchInflight` was an unbounded `DashMap` of
pending oneshots that could grow without limit if a remote processor
stalled.

**What landed**:

1. **`run_buffered_transport<S, T, K>`** (`crates/aeon-engine/src/pipeline.rs`)
   — new public entry point accepting
   `T: ProcessorTransport + Send + Sync + 'static + ?Sized` (so an
   `Arc<dyn ProcessorTransport>` works). Structurally identical to
   `run_buffered`: same source/processor/sink SPSC layout, same delivery
   pipeline, same failure policy, same checkpoint behavior. The processor
   task body is the single difference — it calls
   `transport.call_batch(events).await` instead of the synchronous
   `processor.process_batch(events)`. Sink task body is shared with
   `run_buffered` via the extracted `run_sink_task` helper.

2. **Bounded `BatchInflight`** (`crates/aeon-engine/src/transport/session.rs`)
   — the pending DashMap is now paired with a `tokio::sync::Semaphore`
   holding `max_inflight_batches` permits (default 1024). `start_batch` is
   now `async`: it awaits `Semaphore::acquire_owned().await` before
   allocating a `batch_id`, stores the `OwnedSemaphorePermit` alongside the
   oneshot sender in the DashMap, and releases the permit automatically
   when the entry is removed by `complete_batch` / `cancel_all`. A slow
   remote processor can no longer grow the pending map without bound —
   instead it exerts backpressure on the pipeline's processor task, which
   suspends, stops draining the source ring, and cascades the slowdown
   upstream to the broker the same way any other blocking sink would.

3. **`max_inflight_batches` config knob** threaded through
   `HandshakeConfig`, `WebTransportHostConfig`, and `WebSocketHostConfig`.
   Defaults to `transport::session::DEFAULT_MAX_INFLIGHT_BATCHES = 1024`.

Backpressure path end-to-end: transport saturation → `Semaphore::acquire`
suspends inside `call_batch` → processor task suspends → source→processor
ring fills → source task yields → push-source `PushBufferTx::send` blocks
→ TCP receive window closes → broker stops delivering. Zero loss.

Regression coverage:
- `transport::session::tests::batch_inflight_bounded_by_capacity` — proves
  a third `start_batch` blocks when capacity is 2 and unblocks exactly when
  `complete_batch` drops the first permit.
- `transport::session::tests::batch_inflight_permit_released_on_cancel_all`
  — proves `cancel_all` releases all permits.
- `pipeline::tests::buffered_transport_pipeline_passthrough` — 1k events
  through `run_buffered_transport` with `InProcessTransport` wrapping a
  sync `Processor`, verifying it is behaviorally equivalent to
  `run_buffered` (same metrics, same output count).
- `pipeline::tests::buffered_transport_pipeline_large_volume` — 50k events
  through a small ring buffer (256 capacity) to exercise the backpressure
  path.
- `pipeline::tests::buffered_transport_pipeline_propagates_transport_error`
  — inline-defined `FailingTransport` verifies that transport errors are
  surfaced cleanly instead of hanging the pipeline.

Item #3 (per-batch timeout + retry policy migration from transport to
pipeline `BatchFailurePolicy`) remains deferred — the current per-batch
`batch_timeout` on the host configs is still honored by the hosts
themselves, and `run_buffered_transport` already applies the pipeline's
existing `BatchFailurePolicy` via the shared sink task.

---

## 6. Fixes Landed This Pass

Scope is intentionally narrow — fix what is on the Scenario 1 critical path,
call correctness bugs, and extend strategy coverage to the sinks where the
client library allows it. Everything else is captured in the matrices above
with file:line references for follow-up phases.

1. **§4.0 — UnorderedBatch metric bug** (pipeline.rs) — **DONE**.
   - Sink task now accumulates `pending_ids: Vec<Uuid>` and credits them to
     `outputs_sent` + delivery ledger at every flush site via the shared
     `credit_pending_on_flush` helper.
   - Regression test:
     `pipeline::tests::buffered_pipeline_unordered_credits_metric_at_flush`.
   - `gate1_steady_state` bench now validates both `events_received` and
     `outputs_sent` against `total_produced`.

2. **§4.1 — WebSocket source drop** (websocket/source.rs) — **DONE**.
   - Removed the `is_overloaded()` drop branches. The blocking
     `PushBufferTx::send(event).await` provides the backpressure the drop
     path was trying to achieve, by not draining the tungstenite `read` loop.
   - Regression test: `push_buffer::tests::test_push_buffer_zero_loss_under_backpressure`.

3. **§4.2 — MQTT source sleep-poll backoff** (mqtt/source.rs) — **DONE**.
   - Removed the `is_overloaded()` + 100ms sleep branch from `mqtt_reader`.
     Same mechanism as §4.1: blocking `tx.send(event).await` suspends
     `EventLoop::poll()`, which stops draining the TCP socket, which tells
     the broker to slow down (or stop, for QoS 1/2) via the OS receive
     window.
   - Regression coverage is the shared
     `test_push_buffer_zero_loss_under_backpressure` — MQTT, WebSocket,
     and every other push source rely on the same `PushBufferTx::send`
     contract.

4. **§4.4 — RabbitMQ, Redis Streams sink strategies** — **DONE**.
   - `rabbitmq/sink.rs`: all three strategies implemented via
     `pending_confirms: Vec<PublisherConfirm>` stashing and `join_all` at
     the batch boundary / flush boundary.
   - `redis_streams/sink.rs`: `PerEvent` loops per-XADD, `OrderedBatch` uses
     a single `redis::pipe()` round-trip, `UnorderedBatch` spawns the
     pipeline execution on a tokio task and stashes the `JoinHandle` for
     `flush()` to await.
   - MQTT is documented as infeasible at the current rumqttc API level
     (see §4.4 body).

5. **§5.3 — T3/T4 pipeline wiring + bounded `BatchInflight`** — **DONE**.
   - New `run_buffered_transport<S, T, K>` in `pipeline.rs` accepts
     `T: ProcessorTransport + ?Sized`, sharing the source and sink task
     bodies with `run_buffered` via the extracted `run_sink_task` helper.
     The processor task calls `transport.call_batch(events).await` so
     transport backpressure suspends the pipeline naturally.
   - `BatchInflight` is now bounded by a `tokio::sync::Semaphore` with
     default capacity 1024, configurable via `max_inflight_batches` on
     `HandshakeConfig` / `WebTransportHostConfig` / `WebSocketHostConfig`.
     `start_batch` is now `async` and awaits a permit; `complete_batch`
     and `cancel_all` release permits by dropping them with the DashMap
     entry.
   - Regression tests:
     `transport::session::tests::batch_inflight_bounded_by_capacity`,
     `transport::session::tests::batch_inflight_permit_released_on_cancel_all`,
     `pipeline::tests::buffered_transport_pipeline_passthrough`,
     `pipeline::tests::buffered_transport_pipeline_large_volume`,
     `pipeline::tests::buffered_transport_pipeline_propagates_transport_error`.

6. **§4.3 — MongoDB CDC resume token persistence** — **DONE**.
   - `MongoDbCdcSourceConfig` gains `resume_token_path` + cadence knob.
   - On startup the source loads the token (if present) and passes it
     to `ChangeStreamOptions::resume_after` so the stream resumes from
     the last observed event instead of silently skipping to "now".
   - During streaming the reader atomically flushes the latest token
     every N events via write-to-temp + rename, with a final flush on
     reader shutdown.
   - Delivery semantics are at-least-once (token flushes after buffer
     send, not after sink ack). Exactly-once would require a WAL
     format bump (see §4.3 body).
   - Regression tests: 6 unit tests in `mongodb_cdc::source::tests`
     covering load/save round-trip, missing/empty/corrupt files, atomic
     overwrite, and parent-dir auto-creation.

Other gaps (§4.5 Postgres/MySQL CDC streaming replication, §4.6
QUIC/WebTransport sink stream reuse) remain deferred as follow-up work
with clear file:line references for the next phase.

---

## 7. Verdict

**Audit pass status: CLOSED (2026-04-09).**

Six fixes landed this pass; two gaps remain captured-but-deferred with
clear file:line references and post-Gate-2 rationale. Gate 1 was
re-validated after the largest structural change (§5.3) with zero
regression at the 10K evt/s steady-state baseline (see
`docs/GATE1-VALIDATION.md` "Re-validation run" entry).

### Fixes landed

1. **§4.0 — `outputs_sent` metric credited on flush** (`pipeline.rs`)
   The single correctness bug in the Scenario-1 hot path. The sink task
   now drains a `pending_ids: Vec<Uuid>` into the `outputs_sent` counter
   and delivery ledger at every flush site via the shared
   `credit_pending_on_flush` helper. `gate1_steady_state` now asserts
   `events_received == outputs_sent == total_produced`.

2. **§4.1 — WebSocket source no longer drops on overload**
   (`websocket/source.rs`). The `is_overloaded()` drop branch is gone;
   blocking `PushBufferTx::send(event).await` stops the tungstenite
   read loop, which closes the TCP receive window on the peer.

3. **§4.2 — MQTT source no longer sleep-polls** (`mqtt/source.rs`).
   Same mechanism as §4.1: blocking send suspends `EventLoop::poll()`,
   which stops draining the rumqttc receiver, which closes the broker
   socket's window.

4. **§4.3 — MongoDB CDC resume token persistence**
   (`mongodb_cdc/source.rs`). File-backed `resume_after` at
   configurable flush cadence, atomic write-to-temp + rename, graceful
   fallthrough on missing/corrupt files, 6 unit tests. Delivery
   semantics: at-least-once (exactly-once requires a WAL format bump —
   documented in §4.3 body).

5. **§4.4 — RabbitMQ and Redis Streams sink strategies**
   (`rabbitmq/sink.rs`, `redis_streams/sink.rs`). All three delivery
   strategies now implemented. MQTT sink is documented as infeasible
   at the current rumqttc API level — captured as a finding, not a
   deferral (the per-publish await is semantically already what
   `UnorderedBatch` would be, and real differentiation would require
   a sink redesign around PUBACK/PUBCOMP demultiplexing).

6. **§5.3 — T3/T4 `run_buffered_transport` + bounded `BatchInflight`**
   (`pipeline.rs`, `transport/session.rs`). New pipeline entry point
   accepts `T: ProcessorTransport + ?Sized`, sharing the sink task body
   with `run_buffered` via the extracted `run_sink_task` helper.
   `BatchInflight` is now paired with a `tokio::sync::Semaphore`
   (default capacity 1024, configurable via `max_inflight_batches`) so
   a slow remote processor suspends the processor task instead of
   growing the pending DashMap without limit. Gate 1 re-validation
   after this change: steady-state P99 = 2.500ms at 10K evt/s, CPU
   21.8%, zero loss — identical to the pre-§5.3 baseline.

### Deferred (post-Gate 2)

- **§4.5 — Postgres/MySQL CDC streaming replication.**
  `postgres_cdc/source.rs:150` still uses `pg_logical_slot_get_changes`
  polling; `mysql_cdc/source.rs:150` still uses `SHOW BINLOG EVENTS`.
  Correct and OK for low-rate CDC, but won't scale. The upgrade path is
  a Debezium-class rework (Postgres `START_REPLICATION` protocol or a
  MySQL binlog client library). Deferred — CDCs are post-Gate 2 and the
  current polling implementation is not a correctness gap, only a
  throughput ceiling.

- **§4.6 — QUIC/WebTransport sinks open a new stream per batch.**
  `quic/sink.rs:82` and `webtransport/sink.rs:65` pay stream-setup
  overhead on every write. The fix is a long-lived bidi stream with
  length-prefixed framing and ack frames — the pattern already used by
  `WebTransportProcessorHost`. Deferred — these are T3/T4 *sink*
  concerns, and the T3/T4 processor *transport* path (which is what
  Scenario 1 actually needs) is already production-grade after §5.3.

### Summary by layer

- **Source side.** Pull-based connectors (Kafka, NATS JetStream, Redis
  Streams, CDCs) propagate natural upstream backpressure via protocol
  offsets. Push-based connectors all share the three-phase `PushBuffer`
  primitive; after §4.1 / §4.2 / §4.3 they converge on one correct
  pattern — blocking `PushBufferTx::send(event).await` cascades into
  protocol-native flow control (TCP window for
  WebSocket/MQTT/QUIC/WebTransport, HTTP 503 for Webhook, AMQP
  nack+requeue for RabbitMQ, change-stream backpressure for MongoDB
  CDC). No push source silently drops or sleep-polls any more, and
  MongoDB CDC now persists resume tokens for at-least-once recovery
  across restarts.

- **Sink side.** File, Kafka, NATS, RabbitMQ, and Redis Streams are
  fully strategy-aware and production-grade. MQTT is correct by
  construction (see §4.4). QUIC/WebTransport sinks are functional
  with the stream-reuse optimization deferred as §4.6.

- **Processor side.** T1 native and T2 Wasm run via `run_buffered`
  with the sync `Processor` trait. T3 WebTransport and T4 WebSocket
  run via `run_buffered_transport` with the async `ProcessorTransport`
  trait; their per-session `BatchInflight` is bounded by a Semaphore so
  a slow remote processor exerts real backpressure on the pipeline
  instead of growing the pending map without limit. All four tiers
  propagate backpressure end-to-end.

- **Pipeline orchestrator.** The `outputs_sent` metric bug is fixed
  (§4.0). The Scenario-1 hot path has no known correctness gaps. Gate
  1 re-validation confirms zero regression from the §5.3 refactor.

---

## 8. Pre-ECR-Bake Revisit (2026-04-21)

Re-audit in preparation for the AWS ECR pre-bake (`#6 P4.iii`). Surfaced
findings the 2026-04-09 pass did not cover, plus corrections to two
source-method labels that were set from the *implementation shape*
rather than the *source semantics*.

### 8.1 Source-method taxonomy (clarified)

The audit's pull/push/poll labels conflated "how does the impl handle
data" with "how does the upstream protocol behave". The cleaner frame:

| Mode | Who holds position? | Who initiates reads? | Backpressure shape |
|------|---------------------|----------------------|--------------------|
| **Pull** | Connector | Connector (continuous long-lived read) | Natural — connector paces |
| **Poll** | None persistent | Connector (timer-driven) | Timer interval + backoff on errors |
| **Push** | N/A — upstream has no position for us | External producer writes to our endpoint | Three-phase `PushBuffer` → protocol-level 503 / TCP-window / AMQP-cancel |

Under this frame:

- **MongoDB CDC is Pull, not Push** (corrected from §2). Change streams
  are a tailable cursor over the oplog; the resume token is the
  connector-owned position marker; `stream.next()` is a pull driven by
  the connector. The current impl wrapping the tail in a `PushBuffer`
  is an internal rate-shaping detail and does not make it a push
  source. The tell: push sources have nothing to persist on shutdown.
  MongoDB CDC persists a resume token.
- **Postgres/MySQL CDC are Pull, not Poll** (clarified against §2). The
  intent is streaming replication (`START_REPLICATION` /
  `COM_BINLOG_DUMP`) with connector-held LSN / binlog position. The
  current impls use `pg_logical_slot_get_changes` and
  `SHOW BINLOG EVENTS` on a timer — poll *behaviour* but pull *intent*
  (§4.5 already captured the streaming-replication upgrade as
  deferred). Taxonomy stays Pull; §4.5 remains the debt.

Labels updated in §2's matrix as part of this revisit.

### 8.2 `Uuid::nil()` on Event.id across 12 sources — correctness gap

**Status (2026-04-21)**: ✅ closed by ROADMAP B1. All 14 sources stamp
UUIDv7 via `CoreLocalUuidGenerator`; regression tests in
`delivery_ledger.rs` (`nil_uuids_collapse_tracking_slots`,
`distinct_uuids_get_distinct_slots`) lock the invariant.

**Severity**: correctness hazard under EO-2 at-least-once.
**Scope**: every source except **Kafka** (uses
`CoreLocalUuidGenerator`, `kafka/source.rs:126,176,208`) and the
**HTTP webhook** source (uses `Uuid::now_v7()`,
`http/webhook_source.rs:138`) currently stamps `Event.id =
uuid::Uuid::nil()` — verified by repo-wide grep
(`memory/file/websocket/mqtt/rabbitmq/nats/mqtt/redis_streams/quic/
webtransport[streams+datagrams]/mongodb_cdc/postgres_cdc/mysql_cdc/
http_polling`).

**Why it matters.** `crates/aeon-engine/src/delivery_ledger.rs` keys
its PerEvent / OrderedBatch / UnorderedBatch ack tracking off
`source_event_id`. If every event in a batch carries the zero UUID,
`mark_acked(id)` and `mark_batch_acked(&ids)` collapse distinct
events into one ledger slot — acking one silently acks all, defeating
at-least-once recovery. `IdempotentSink::has_seen(event_id)` is
likewise useless with a shared zero id.

**Fix (two-line per source)**. The `PushBuffer`-fed sources can either
share an `Arc<Mutex<CoreLocalUuidGenerator>>` through the config, or
call `uuid::Uuid::now_v7()` at the event-construction site — whichever
matches the surrounding hot-path budget. Pull sources (CDC, Redis/NATS
streams) have the simplest path: stamp `Uuid::now_v7()` at event
construction in the source loop (same shape as the HTTP webhook).

**Test**. One shared unit test asserting batch-distinct event ids
across every non-Kafka source, plus the delivery-ledger invariant
"N sent events → N ledger slots → N acks required to advance the
checkpoint".

Captured as ROADMAP `B1 — push/poll source UUIDv7 stamping`.

### 8.3 `source_offset` stamping gaps for pull sources

**Status (2026-04-21)**: ✅ closed by ROADMAP B2. Redis Streams /
NATS JetStream / Postgres / MySQL CDC all stamp `source_offset` at
event construction via documented packed-i64 helpers (with unit
tests). MongoDB resume-token sidecar unchanged pending P13 WAL bump.

**Severity**: blocks EO-2 at-least-once on every pull source other
than Kafka.
**Scope**: `source_offset` is currently populated only by the Kafka
source (`kafka/source.rs:219`). Redis Streams (message id), NATS
JetStream (sequence), MongoDB CDC (resume token — partially, via
separate file), Postgres CDC (LSN), MySQL CDC (binlog coords) all
have an upstream-native position but do not stamp it into `Event.
source_offset`.

**Why it matters.** The EO-2 checkpoint replicator advances recovery
offsets from `Event.source_offset`; a pull source that does not
stamp it gives the ledger no recovery point, which means replay
after crash restarts from "now" (silent data loss) or from start
(duplicate work), depending on source defaults.

**Shape of the fix**. For each pull source, stamp
`event.source_offset = Some(upstream_position)` at event
construction. The MongoDB resume-token sidecar file (§4.3) stays —
it is a token, not an `i64`, and EO-2 P13 tracks the WAL format bump
that would unify them. The simpler `i64`-positioned pull sources
(Redis/NATS/PG/MySQL) can land today.

Captured as ROADMAP `B2 — pull-source offset stamping`.

### 8.4 Consumer-group mode for compatible pull sources (optional, opt-in)

**Status (2026-04-21): ✅ closed by ROADMAP B4.** See §8.5 row for the
shipped shape; the design below is preserved for historical context.

**Scope — strictly connector-local.** Core pipeline (SPSC, Raft-
coordinated `partition_table`, EO-2 checkpoint) stays unchanged. The
feature is a config flag per source that selects between today's
manual-assign mode and a broker-coordinated consumer-group mode.

**Compatible upstreams**:

| Source | Library | Mode surface |
|--------|---------|--------------|
| Kafka / Redpanda | `rdkafka` | `consumer.assign` vs `consumer.subscribe` + rebalance callbacks |
| Redis Streams | `redis` | `XREAD` vs `XREADGROUP` (already uses `XREADGROUP`; exposes the group/consumer as config) |
| Valkey Streams | same as Redis protocol | identical to Redis |

**Not compatible** — out of scope: RabbitMQ super-streams (SAC is
not a consumer-group model; rebuilding the consumer on every
rebalance is incompatible with `reassign_partitions`). Audit
finding confirmed in the 2026-04-21 revisit, no implementation.

**Config shape (per source)**:

```yaml
consumer_mode:
  kind: single        # default; manual-assign, ordering preserved, Aeon owns offsets
  # OR
  kind: group         # broker-coordinated, broker auto-commit still disabled
  group_id: aeon-ingest
  broker_commit: false  # Aeon's EO-2 ledger remains the truth
```

**Hard guardrail.** `kind: group` is **mutually exclusive** with
cluster-coordinated partition ownership. When a source is in group
mode the broker owns partition movement; Raft-driven
`reassign_partitions` must stay out. The pipeline start path
validates this — a source advertising group-mode on a clustered
deployment fails configuration.

**Design rationale** (against a separate `ConsumerGroupSource`
trait): the user-facing surface is a configuration decision, not a
type-system decision. Keeping it in config preserves YAML symmetry
across sources and means the pipeline supervisor never has to branch
on trait variants. The trait-level safety (no accidental
`reassign_partitions` in group mode) is enforced by a single runtime
assertion at source startup.

Captured as ROADMAP `B4 — consumer-group config mode for pull
sources (Kafka / Redpanda / Redis / Valkey)`.

### 8.5 Pre-bake correctness blockers and polish

| Item | Severity | Captured as | Status (2026-04-21) |
|------|----------|-------------|---------------------|
| 8.2 push/poll UUIDv7 stamping | Bake blocker (EO-2 correctness) | ROADMAP B1 | ✅ shipped |
| 8.3 pull-source `source_offset` stamping | Bake blocker (EO-2 correctness) | ROADMAP B2 | ✅ shipped |
| Pod disruption policy in Helm chart | Polish (bake-safe without, but one eviction ⇒ downtime) | ROADMAP B3 | ✅ shipped |
| `docs/DEPLOYMENT.md` blue-green + canary walkthroughs | Docs polish | ROADMAP B3 | ✅ shipped |
| 8.4 consumer-group config mode | Post-bake design + impl | ROADMAP B4 | ✅ shipped |

All B1 + B2 + B3 + B4 items landed in the same session on 2026-04-21.
Pre-bake bar is clear; the only open ECR-bake-path item is P4.iii
(task #6, pre-bake image to ECR `us-east-1`). B4 shipped the
`ConsumerMode::{Single,Group}` enum for Kafka/Redpanda + Redis Streams
sources with a pipeline-start guardrail rejecting
`ConsumerMode::Group` + Raft-driven `partition_reassign` together —
the broker rebalance protocol and Aeon's `partition_table` are
mutually exclusive by construction.

Helm terminology note: the Kubernetes object `kind: PodDisruptionBudget`
is a fixed API-server type; we inherit it. Everywhere Aeon-owned
documents reference it we use **"pod disruption policy"** or **"pod
disruption cap"** to stay consistent with the project-wide "Capacity
not Budget" convention for our own types.

### 8.6 Zero-downtime deployment — already largely in place

For completeness, the 2026-04-21 revisit confirmed via grep + REST-
API review that the pre-bake work order does **not** need to ship
blue-green or canary — both already exist:

- Drain-swap processor hot-swap (<1ms): `pipeline.rs`
  `PipelineControl::drain_and_swap` family.
- Blue-green: `POST /api/v1/pipelines/{name}/upgrade/blue-green`,
  `/cutover`, `/rollback` → `start_blue_green` /
  `cutover_blue_green` / `rollback_upgrade`.
- Canary: `POST /api/v1/pipelines/{name}/upgrade/canary`
  (configurable traffic steps, default 10/50/100%) → `start_canary`
  / `set_canary_pct` / `complete_canary`.
- Raft leadership relinquish + Helm `preStop` + `WriteGate` drain
  cover graceful pod termination.
- Cluster membership blue-green via CL-6 partition transfer (primitive
  complete; orchestration tooling — `aeon cluster drain` — tracked as
  Pillar 3 G5).

Since 2026-04-21 the pod disruption policy (B3) and the operator-
facing walkthrough (`docs/DEPLOYMENT.md`) have shipped, closing the
polish gap. Canary auto-rollback on metric threshold stays manual for
now — acceptable for v0.1.

## 9. Full-Repo Audit Critical Fixes — S8 (2026-04-21)

A cross-cutting audit (roadmap/stubs/OWASP/12-factor/Flink diff)
surfaced two critical correctness items on connectors that this
matrix had not previously flagged, plus one config-surface hygiene
fix. All three landed together as workstream **S8**.

### 9.1 Push-source `source_kind()` overrides — **Closed 2026-04-21**

`Source::source_kind()` defaults to `SourceKind::Pull` in
`aeon-types/src/traits.rs`. Push sources that forget to override it
silently skip L2 body persistence under `durability != none` — a
data-loss bug invisible until recovery. Fixed by adding explicit
overrides on every push source connector:

| Connector                                   | New override            |
| ------------------------------------------- | ----------------------- |
| `mqtt/source.rs`                            | `SourceKind::Push`      |
| `rabbitmq/source.rs`                        | `SourceKind::Push`      |
| `quic/source.rs`                            | `SourceKind::Push`      |
| `websocket/source.rs`                       | `SourceKind::Push`      |
| `webtransport/source.rs`                    | `SourceKind::Push`      |
| `webtransport/datagram_source.rs`           | `SourceKind::Push`      |
| `http/polling_source.rs`                    | `SourceKind::Poll`      |

`http/polling_source.rs` is not a push source — it's HTTP-style
poll (no durable replay). Marking it `Poll` keeps EO-2 from
attempting L2 body writes it can't replay, and matches the
SourceKind taxonomy defined in §8.1.

### 9.2 Sink `on_ack_callback()` overrides — **Closed 2026-04-21**

`Sink::on_ack_callback()` has a default no-op. Sinks that don't
override it never drive `outputs_acked_total`, so operators see
`outputs_sent_total` tick but no broker-confirmed delivery count —
the batch-async vs sync-ack gap disappears from metrics. Fixed by
wiring the callback on every non-pass-through sink:

| Connector                   | Fire point                                          |
| --------------------------- | --------------------------------------------------- |
| `nats/sink.rs`              | PerEvent/OrderedBatch batch end; UnorderedBatch on `flush` drain |
| `redis_streams/sink.rs`     | PerEvent after loop; OrderedBatch after pipeline await; UnorderedBatch on `flush` drain |
| `http/sink.rs`              | Per-success counter; partial on error-exit          |
| `mqtt/sink.rs`              | On enqueue (rumqttc has no packet-id observability — documented) |
| `rabbitmq/sink.rs`          | PerEvent/OrderedBatch batch end; UnorderedBatch on `flush` drain |

Kafka was already wired. MQTT's "fire on enqueue" is the honest
reflection of what the MQTT API lets us observe — §4.4 notes this
would require PUBACK/PUBCOMP interception, deferred post-Gate 2.

### 9.3 L3 RocksDB compile-time gate — **Closed 2026-04-21**

`aeon_types::L3Backend::RocksDb` was visible in the enum but the
adapter is a placeholder (FT-7). A YAML config setting
`l3.backend: rocksdb` deserialized fine and then failed at runtime
with `AeonError::state(...)` — a silent config footgun. Fixed by
gating the variant behind a new `rocksdb` cargo feature on
`aeon-types` (re-exported from `aeon-state`). Without
`--features rocksdb`:

- The variant is absent from the enum
- Serde rejects `rocksdb` at config-parse time
- The tiered-store match arm no longer needs to cover it

With the feature, behaviour is unchanged (still returns the
"not yet implemented" error) — the test
`tiered_l3_backend_rocksdb_not_implemented` is now gated to match.

### 9.4 S2 payload-never-in-logs sweep — **Closed 2026-04-21**

Audit finding C4 (Kafka SASL URL logged in
`AeonError::connection(format!(...))`) generalised to every
URL-shaped field on every connector. Fixed by shipping a
shared redaction helper in `aeon-types` and a build-time
debug-logging gate on `aeon-engine`.

**`aeon_types::redact::redact_uri` helper.** Zero-alloc
`Cow<'_, str>` wrapper that strips `user[:password]@` from any
URI-shaped string while preserving scheme / host / port / path /
query / fragment. Lives in `aeon-types` (not
`aeon-observability`) so every crate — connectors, engine,
processor-client, CLI — can reach it without a new dependency.
10 unit tests cover: empty input, no-scheme, no-userinfo,
userinfo-with-password, userinfo-without-password, password
containing literal `@`, nested scheme in query string
(`?callback=https://...` must not confuse the authority parser),
fragment, missing path component, and custom schemes
(`quic+wt://...`).

**Log-site sweep.** Every connector + processor-client site that
previously emitted a raw connection URL now passes it through
`redact_uri()`:

| Connector / client                             | Sites patched                                          |
| ---------------------------------------------- | ------------------------------------------------------ |
| `http/sink.rs`                                 | `HttpSink created` info + POST-fail / non-success errors |
| `http/polling_source.rs`                       | `created` info + send-fail / non-success / body-read warns |
| `websocket/sink.rs`                            | connect-error + `connected` info                       |
| `websocket/source.rs`                          | connect-error + `connected` info + reconnect warns (pre-redacted once, reused in reader task) |
| `webtransport/sink.rs`                         | connect-error + `connected` info                       |
| `aeon-processor-client/src/websocket.rs`       | `Connected to Aeon` info                               |
| `aeon-processor-client/src/webtransport.rs`    | `Connected to Aeon via WebTransport` info              |

Both `tracing::*!` log sites **and** the `AeonError::connection(format!(...))`
error chains are redacted — errors bubble up through logs the same
way, so redacting one without the other leaves the leak in place.

### 9.5 S2 build-time `debug-payload-logging` gate — **Closed 2026-04-21**

Added a `debug-payload-logging` cargo feature on `aeon-engine` as
an opt-in dev diagnostic. A `compile_error!` guard refuses to
compile it in release builds:

```rust
#[cfg(all(feature = "debug-payload-logging", not(debug_assertions)))]
compile_error!("feature `debug-payload-logging` is a dev-only diagnostic …");
```

Invariant: shipping payload-logging to production requires
deleting the gate in source, not forgetting a CLI flag. Verified
both directions — `cargo check --release --features
debug-payload-logging` fails with the compile_error; a debug
build with the same feature succeeds.

### 9.6 S7 SSRF guard — `aeon_types::ssrf` module — **Closed 2026-04-22**

Audit finding C3 (SSRF / external URL hardening) lands as a new
`aeon_types::ssrf` module so every crate can reach it without new
deps. `SsrfPolicy` carries four category toggles
(`allow_loopback` / `allow_private` / `allow_link_local` /
`allow_cgnat`) plus `extra_deny` / `extra_allow` as
`Vec<ipnet::IpNet>`. Three presets:

- `production()` — VPC-safe default. Allows RFC1918 (so sidecars
  and in-VPC HTTP APIs still work), denies loopback, link-local,
  CGNAT 100.64.0.0/10, ULA fc00::/7, and the always-denied host set.
- `strict()` — also denies private. For edge deployments that
  should never dial into their own VPC.
- `permissive_for_tests()` — allows everything EXCEPT the
  `ALWAYS_DENIED_HOSTS` set. Layered over every preset so a test
  config cannot accidentally dial cloud metadata even with
  `allow_link_local = true`:
  - AWS IMDS — `169.254.169.254`
  - Alibaba ECS metadata — `100.100.100.200`
  - GCP metadata — `169.254.169.253`
  - Oracle Cloud metadata — `192.0.0.192`

API:

```rust
policy.check_addr(ip)?;                       // pre-resolved IP
policy.check_host("api.example.com", 443)?;   // DNS resolve + filter
policy.check_url("https://foo.example/v1")?;  // scheme-aware parse
```

`check_host` / `check_url` run the full `ToSocketAddrs` resolve and
return every address that passed so the caller can dial them
directly — no rebinding gap. `From<SsrfError> for AeonError` so
call-sites just use `?` (AddressDenied → `AeonError::Config`,
ResolutionFailed / NoAddresses → `AeonError::Connection { retryable:
true }`). Twenty unit tests cover the preset matrix, IMDS layering
on every preset, loopback/link-local/CGNAT/ULA on both v4 and v6,
unspecified/multicast/broadcast, `extra_deny` + `extra_allow`
precedence, the invariant that `extra_allow` **cannot** override
`ALWAYS_DENIED_HOSTS`, host/port parsing edge cases
(userinfo-with-`@`-in-password, IPv6-literal brackets, default-port
inference per scheme), and JSON serde roundtrip defaults.

### 9.7 S7 connector wiring — **Closed 2026-04-22**

Every URL-based connector's `::new()` now runs the SSRF guard before
any socket opens:

- HTTP sink (`crates/aeon-connectors/src/http/sink.rs`)
- HTTP polling source (`crates/aeon-connectors/src/http/polling_source.rs`)
- WebSocket sink (`crates/aeon-connectors/src/websocket/sink.rs`)
- WebSocket source (`crates/aeon-connectors/src/websocket/source.rs`)
- WebTransport sink (`crates/aeon-connectors/src/webtransport/sink.rs`)

Each gains a `ssrf_policy: SsrfPolicy` field (defaults to
`SsrfPolicy::production()`) plus a `with_ssrf_policy()` builder.
`::new()` calls `config.ssrf_policy.check_url(&config.url)?` — fails
fast with `AeonError::Config` before any DNS query in the connector's
main loop.

**Anti-rebinding**: WebSocket source re-runs the guard inside its
reconnect loop, so a DNS record that flipped to a private IP between
the initial connect and a later reconnect fails closed. (HTTP
clients typically rebuild their connection pool per request, so the
`::new()`-time check plus the underlying resolver cache TTL is
sufficient; long-lived WebSocket sessions re-check on every
reconnect.)

Broker connectors (Kafka, NATS, MQTT, Redis, RabbitMQ, QUIC transport)
deliberately do **not** go through this guard — they're designed for
in-VPC use and their client libraries do their own connection pooling
and endpoint validation. The audit finding C3 is specifically about
attacker-controlled URLs reaching the HTTP / WebSocket / WebTransport
families.

Engine test fixtures in `crates/aeon-engine/tests/e2e_tier_e.rs`
(3 sites: HttpPollingSource, HttpSink, WebTransportSink) and
`e2e_tier_f.rs` (2 sites: WebSocketSource, WebSocketSink) migrated to
`aeon_types::SsrfPolicy::permissive_for_tests()` so loopback 127.0.0.1
still works but IMDS stays blocked even in tests. Three new
connector-level SSRF rejection tests (HTTP sink IMDS, HTTP polling
IMDS, WebSocket sink loopback-denied) written with
`let Err(err) = ... else { panic!(...) }` because these builders don't
impl `Debug` on the success variant.

### 9.8 S7 CLI YAML surface — **Closed 2026-04-22**

`aeon-cli/src/connectors.rs` gains `parse_ssrf_policy()` and
`parse_cidr_list()` helpers that translate six YAML keys into an
`SsrfPolicy`:

| Key                       | Type        | Default (production)                       |
| ------------------------- | ----------- | ------------------------------------------ |
| `ssrf_allow_loopback`     | bool        | `false`                                    |
| `ssrf_allow_private`      | bool        | `true` (in-VPC)                            |
| `ssrf_allow_link_local`   | bool        | `false`                                    |
| `ssrf_allow_cgnat`        | bool        | `false`                                    |
| `ssrf_extra_deny`         | CIDR list   | empty                                      |
| `ssrf_extra_allow`        | CIDR list   | empty                                      |

CIDR lists are comma-separated, e.g.
`"10.0.0.0/8,192.168.0.0/16,2001:db8::/32"`. Invalid CIDRs fail
deserialisation — misconfiguration cannot silently widen the guard.
Wired into `HttpPollingSourceFactory` and `HttpSinkFactory` with
updated docstrings listing the new keys. Three parser tests cover
defaults, explicit toggle round-trip, and CIDR-list parsing.

### 9.9 S3–S6, S10 workstreams — status

S8 closed the **critical** audit items (silent data-loss + silent
config failure); S2 closed **payload-never-in-logs**; S7 closed
**SSRF / external URL hardening**; S1 closed **secret provider + KEK
envelope encryption + rotation** (see §9.10–§9.12); S9 closed
**inbound connector auth** (see §9.13–§9.15); **S10 closed 2026-04-23**
— primitives + HTTP surface + WebSocket handshake wiring + Kafka +
Redis + NATS + Postgres-CDC + MySQL-CDC + Mongo-CDC broker-native +
WebTransport sink HTTP/3 CONNECT header auth (2026-04-22) plus WS/WT
mTLS TLS-layer integration (task #34, 2026-04-23): WS sink+source
route mTLS through `Connector::Rustls(Arc<ClientConfig>)`; WT sink
exposes `webtransport::mtls_client_config_from_signer` to bake the
identity into `wtransport::ClientConfig`; WT source extracts peer-cert
CN + SAN post-handshake via
`aeon_crypto::tls::CertificateStore::parse_cert_subjects` for S9
subject matching. **S3, S4.2, S5, S6, S1.4, S2.5 all closed
2026-04-22/23** — see ROADMAP §"Security & Compliance Index" for the
landing map. Remaining security/compliance work:

- **S10 remaining** — SDK-level mTLS follow-ups on legacy connector
  stacks (Redis `tls-rustls` feature, NATS in-memory PEM, Postgres
  `tokio-postgres-rustls`, MySQL `native-tls-tls`/`rustls-tls`
  feature, MongoDB tempfile-or-patch for
  `TlsOptions::cert_key_file_path`); aeon-cli factory registration
  for WebSocket / Redis / NATS / PG-CDC / MySQL-CDC / Mongo-CDC
  source/sink (P5.c).
- **S4.3 / S4.4** — Compliance CLI YAML surface + COMPLIANCE.md
  expansion (S4.1 primitives + S4.2 validator closed).
- **S1.4 driver impl** — HSM / PKCS#11 trait landed; concrete
  backend deferred (task #33).
- **S2.5 call-site wiring** — audit channel shipped; remaining
  emissions (auth rejections, KEK rotation, erasure requests,
  compliance refusals) tracked as in-code TODOs.

All S1–S10 config is **env-var-first** — every YAML value resolves
via `${ENV:VAR}`, `${VAULT:path/key}`, `${AWS_SM:name}`,
`${AWS_KMS:key}`, `${DOTENV:VAR}` through the S1 secret-provider
trait. Helm / CI-CD tooling injects env vars; secrets live in Vault
(self-hosted or managed) as primary, `.env` outside the deployed
artifact as last resort. Literal values warn at load.

Sequence approved 2026-04-22: S8 → S2 → S7 → S1 → S9 → **S10** →
S4 → S3 → S5 → S6 — all closed by 2026-04-23. S10 landed in two
waves: primitives + HTTP + WebSocket + Kafka + Redis + NATS +
Postgres-CDC + MySQL-CDC + Mongo-CDC broker-native + WebTransport
sink HTTP/3 header auth (2026-04-22), then WS/WT mTLS TLS-layer via
task #34 (2026-04-23). SDK-level mTLS follow-ups on legacy stacks
(Redis / NATS / PG-CDC / MySQL-CDC / Mongo-CDC) and aeon-cli
factory wiring for the unregistered connectors remain at P5.c.

### 9.10 S1.1 Secret provider — `aeon_types::secrets` module — **Closed 2026-04-22**

Zero-heavy-dep module in `aeon-types` so every crate can reach it.
`SecretScheme` enum (`Env` / `DotEnv` / `Vault` / `AwsSm` / `AwsKms`
/ `Literal`), `SecretRef::parse(&str)` returning `Ok(None)` for
plain strings and `Err` for malformed `${SCHEME:path}` tokens.
`SecretBytes([u8; _])` — `zeroize` on drop, no `Debug`, no `Clone`;
exposes `expose_bytes()` / `expose_str()`. `SecretProvider` trait
with three local implementations: `EnvProvider` (`std::env::var`),
`DotEnvProvider` (reads `.env` at `AEON_DOTENV_PATH`, **no
auto-discovery**), `LiteralProvider` (warn-once per run so operators
see they've inlined a secret). `SecretRegistry::default_local()`
registers Env + DotEnv + Literal; Vault / AWS SM / AWS KMS providers
are deliberately **not** in `aeon-types` — they'll land in a future
`aeon-secrets` crate and register via `SecretRegistry::register()`.
`interpolate_str<'a>(&self, s: &'a str) -> Result<Cow<'a, str>, _>`
returns `Cow::Borrowed` on no-`$` (zero-alloc fast path), handles
`${...}` escape, errors on unknown scheme / unregistered provider /
malformed token. `From<SecretError> for AeonError` routes to
`AeonError::Config` (non-retryable). 34 unit tests.

### 9.11 S1.2 Dual-domain KEK + envelope encryption — `aeon_crypto::kek` module — **Closed 2026-04-22**

`KekDomain::{LogContext, DataContext}` — strictly non-fungible
(log keys and payload keys never cross, even under misconfiguration).
`WrappedDek { kek_domain, kek_id, nonce, ciphertext }` — serde-
serializable, wire-stable. `DekBytes([u8; 32])` — zeroize-on-drop,
no `Debug`, no `Clone`. `KekHandle` carries `domain`, `active_id +
active_ref`, optional `previous_id + previous_ref`, and `Arc<SecretRegistry>`.
`wrap_new_dek()` generates + wraps; `unwrap_dek()` dispatches on
`wrapped.kek_id` (active vs previous) for **hot-key rotation**,
rejects domain mismatch and unknown kek_id. AES-256-GCM (12-byte
random nonce, 128-bit tag) via `aes-gcm` 0.10 — same primitive
planned for S3 at-rest encryption. 10 unit tests cover
active-roundtrip, rotation (previous-key roundtrip), domain-
mismatch rejection, unknown-kek_id rejection, tamper detection via
GCM tag, wrong-length KEK rejection, debug redaction, DEK non-zero,
`KekDomain` string-stability, `WrappedDek` JSON serde roundtrip.

### 9.12 S1.3 CLI pre-parse YAML interpolation — `aeon-cli/src/main.rs` — **Closed 2026-04-22**

`read_and_interpolate_manifest(&Path) -> Result<String>` helper
reads the manifest, enforces existing `MAX_MANIFEST_SIZE`, and runs
`SecretRegistry::default_local().interpolate_str(&raw)` before
`serde_yaml::from_str`. Wired into `cmd_apply` and `cmd_diff`.
`cmd_check` deliberately left on raw-read — it's a syntax-only
validator that should run offline without env vars set. Pre-parse
interpolation means **no per-field plumbing** is needed: one edit
covers every connector and config field. Documented limitation:
interpolated values containing YAML-reserved characters may break
parsing (base64-encode multi-line secrets). 5 new tests:
env-ref replacement, plain-YAML passthrough, missing-env-var error,
unregistered-scheme error, oversize-file rejection. Also fixed 2
pre-existing SSRF test gaps in `connectors.rs` that were using
`127.0.0.1` against `SsrfPolicy::production()` defaults
(`http_polling_source_requires_url`,
`http_sink_factory_requires_url_and_posts`) — added
`ssrf_allow_loopback: true` to both configs. Full CLI suite back
to 34/34 green.

### 9.13 S9.1 Inbound auth primitives — `aeon_types::auth` module — **Closed 2026-04-22**

Protocol-agnostic verifier so HTTP / WebTransport / QUIC all funnel
through one type. `auth::hmac_sig` exposes `sign_request` /
`verify_request` over the canonical
`method || "\n" || path || "\n" || ts || "\n" || body` preimage;
`HmacAlgorithm::{HmacSha256, HmacSha512}` with serde kebab-case;
constant-time tag compare via `hmac::Mac::verify_slice`; candidate
list for `[active, previous]` rotation. `auth::inbound` defines
`InboundAuthMode` (4 modes, `serde(snake_case)`), per-block config
structs (`IpAllowlistConfig` / `ApiKeyConfig` / `HmacConfig` /
`MtlsConfig`), and `InboundAuthConfig` as the top-level document.
`InboundAuthVerifier::build()` compiles the config, moves secrets
into `SecretBytes`, and validates non-empty mode blocks up-front.
`verify(&AuthContext)` runs modes in declaration order —
cheap-reject-first ordering is the operator's responsibility. Custom
redacted `Debug` surfaces mode list + count-per-block but never
key bytes. `AuthRejection::reason_tag()` returns bounded-cardinality
labels for metrics; `redacted_peer_ip()` applies the S2 rule
(v4 last octet / v6 lower 32 bits). 15 hmac_sig tests + 33 inbound
tests = 48 new aeon-types tests (276 total crate-wide, clippy clean).

### 9.14 S9.2 Connector wiring — **Closed 2026-04-22**

`HttpWebhookSource` receives the full four-mode surface:
`ConnectInfo<SocketAddr>` extractor provides the peer IP,
`HeaderMap` is materialised into a flat `&[(&str, &[u8])]` slice
(aeon-types stays protocol-agnostic), auth runs **before** the
push-buffer overload check, `status_for(rejection)` maps to HTTP
401 (API-key / HMAC) vs 403 (IP / mTLS), and rejection emits a
`tracing::warn` with `reason_tag` + redacted peer IP. Raw
push-endpoint sources — `QuicSource`, `WebTransportSource`,
`WebTransportDatagramSource` — get an IP allow-list hook at
pre-handshake acceptance: `incoming.remote_address().ip()` is
resolved without paying the TLS cost and the session is refused
(`quinn::Incoming::refuse()` / `wtransport::IncomingSession::refuse()`)
on rejection. API-key / HMAC / mTLS for QUIC+WT require header/
cert-subject plumbing that's deferred to a follow-up. 8 new webhook
auth tests (baseline + all 4 modes with real reqwest round-trips),
existing QUIC/WT tests regression-checked — 40/40 connector tests
green under `http,quic,webtransport` features.

### 9.15 S9.3 CLI YAML surface — `aeon-cli/src/connectors.rs` — **Closed 2026-04-22**

`parse_inbound_auth_config(&BTreeMap<String, String>)` reads a
flat-key convention that fits the existing factory config shape:
`auth_modes` (comma-separated), `auth_ip_cidrs`, `auth_api_keys`,
`auth_hmac_secrets`, `auth_hmac_algorithm`, `auth_hmac_skew_seconds`,
`auth_mtls_subjects`, plus optional header-name overrides. Absent
`auth_modes` returns `Ok(None)` — no verifier is installed, source
stays open. Secret-valued fields (`auth_api_keys`, `auth_hmac_secrets`)
carry plaintext by the time the factory runs — S1.3 pre-parse
interpolation has already resolved `${VAULT:...}` / `${ENV:...}`
tokens. `build_inbound_auth_verifier` wraps in `Arc` and plugs into
`HttpWebhookSourceFactory::build()`. Five parser tests (absent-key,
single-mode, all-four-modes incl. sha512 + custom skew, unknown
mode rejected, unknown algorithm rejected) — full CLI suite 29/29
green under `rest-api`. Wiring for QUIC + WT YAML factories is
separate work under the P5.c connector-catalog track.

### 9.16 S10.1 Outbound auth primitives in `aeon_types::auth::outbound` — **Closed 2026-04-22**

Matches the S9 shape but inverts direction: Aeon is the client, the
remote is the server. `OutboundAuthMode` enum exposes seven modes
(`None`, `Bearer`, `Basic`, `ApiKey`, `HmacSign`, `Mtls`,
`BrokerNative`) with a `tag()` helper for bounded-cardinality
metric labels. `OutboundAuthConfig` carries the mode + per-mode
blocks (`BearerConfig`, `BasicConfig`, `OutboundApiKeyConfig`,
`HmacSignConfig`, `OutboundMtlsConfig`, `BrokerNativeConfig`).

**Single-mode discipline.** Unlike S9 (which *stacks* IP allow-list
/ API-key / HMAC / mTLS as defence-in-depth), S10 allows exactly
one `auth.mode` per connector. The design note in the module
header explains why: outbound is a single handshake, and composing
modes (Bearer + HMAC? which credential caused the 401? which do we
rotate?) breaks retry semantics. Stackable auth is the inbound
world's job.

`OutboundAuthSigner::build()` moves every secret into `SecretBytes`
(zeroize-on-drop); it rejects mode/block mismatches (`Bearer`
selected but `bearer:` absent → `ModeConfigMissing`) and empty
credential values (`BearerEmpty`, `BasicUsernameEmpty`, `ApiKeyEmpty`,
`HmacSecretEmpty`, `MtlsEmpty`) up-front so misconfiguration fails
loud at pipeline start. Internal state is a typed `CompiledSigner`
enum variant per mode — every accessor pattern-matches rather than
unwrapping options, so there are no `.unwrap()` / `.expect()` on
the hot path (FT-10 compliant without any `#[allow(...)]` escapes).

`http_headers(&OutboundSignContext)` returns per-request headers
for HTTP-applicable modes: `Authorization: Bearer …` for Bearer,
`Authorization: Basic <b64(user:pass)>` for Basic, configurable
header for ApiKey, and `(X-Aeon-Timestamp, X-Aeon-Signature)` for
HmacSign (signed over the canonical `method\npath\nts\nbody`
preimage — same shape as S9 verifier). Returns empty `Vec` for
`None` / `Mtls` / `BrokerNative`; `mtls_cert_pem()` /
`mtls_key_pem()` / `broker_native()` surface the non-HTTP
material for connectors that install it on the transport. Custom
`Debug` impl redacts secret bytes — surfaces mode + `has_bearer:
true` / `api_key_header: …` / `broker_native_keys: […]` but never
the key material itself.

20 unit tests cover: every mode emits the right header set or
surfaces the right material; empty-credential variants all return
the right build error; HMAC timestamp varies between calls (so the
signature varies), base64 matches the exact expected string, Debug
redacts, mode tags are bounded-cardinality, serde is snake_case,
and build errors convert cleanly to `AeonError`. 296 aeon-types
tests total; clippy clean with `-D warnings`.

### 9.17 S10.2 HTTP connector wiring — **Closed 2026-04-22**

`HttpSinkConfig::with_auth(Arc<OutboundAuthSigner>)` and the
matching builder on `HttpPollingSourceConfig`. Per-request header
merge is unconditional — `signer.http_headers(OutboundSignContext)`
handles the `None` / `Mtls` / `BrokerNative` no-ops internally so
the call site stays uniform. Context is built per-request
(`method`, `path` pre-parsed from the URL at construction time,
`body` = output payload for sink / empty for GET source, `now_unix`
from `SystemTime`).

`Mtls` mode is handled specially: PEM cert + key are concatenated
and installed on the `reqwest::Client` via `Identity::from_pem()`
at construction time (rustls-tls feature). `BrokerNative` is
logged-and-ignored for HTTP with a `tracing::warn!` so operators
see the misconfiguration without killing the pipeline.

Tests exercise Bearer, HmacSign, ApiKey, and None modes end-to-end
against an axum test server — verifying `Authorization`,
`X-Aeon-Signature` (hex-only), `X-Aeon-Timestamp` (parseable i64),
and custom-header injection on both sink and polling source. All
8/8 HTTP sink tests and 5/5 HTTP polling tests pass; full
`cargo test -p aeon-connectors --features http http::` suite 23/23
green. Clippy clean with `-D warnings`.

### 9.18 S10.3 CLI YAML surface — **Closed 2026-04-22**

`parse_outbound_auth_config(&BTreeMap<String, String>)` in
`aeon-cli::connectors` reads a flat-key convention parallel to the
S9 inbound parser:

- `auth_mode` — required to trigger parsing; absent ⇒ `Ok(None)`.
  Values: `none` | `bearer` | `basic` | `api_key` | `hmac_sign` |
  `mtls` | `broker_native`.
- `auth_bearer_token` — for `bearer`.
- `auth_basic_username` / `auth_basic_password` — for `basic`.
- `auth_api_key_header` (default `X-Aeon-Api-Key`) /
  `auth_api_key` — for `api_key`.
- `auth_hmac_sign_signature_header` (default `X-Aeon-Signature`) /
  `auth_hmac_sign_timestamp_header` (default `X-Aeon-Timestamp`) /
  `auth_hmac_sign_secret` / `auth_hmac_sign_algorithm`
  (`hmac-sha256` | `hmac-sha512`) — for `hmac_sign`.
- `auth_mtls_cert_pem` / `auth_mtls_key_pem` — for `mtls`.
- `auth_broker_native.<key>` — each such pair lands in the
  `BrokerNativeConfig.values` map verbatim, so SDK-specific keys
  (`sasl_mechanism`, `username`, `password`, `oauth_token`,
  `creds_path`, …) pass through unchanged.

`build_outbound_auth_signer` wraps in `Arc` and plugs into
`HttpSinkFactory::build()` + `HttpPollingSourceFactory::build()`.
Secret-valued fields are plaintext by the time the factory runs —
S1.3 pre-parse interpolation has already resolved `${VAULT:...}` /
`${ENV:...}` tokens. Unknown mode / unknown algorithm rejections
match the S9 shape so YAML misconfiguration fails loud at pipeline
start.

13 parser tests (absent-key, every mode, custom headers, sha512
algorithm, broker-native map round-trip, unknown-mode rejection,
unknown-algorithm rejection, empty-credential build rejection) —
full `cargo test -p aeon-cli --features rest-api connectors::`
suite 42/42 green.

### 9.19 S10.4 WebSocket source/sink wiring — **Closed 2026-04-22**

Both `WebSocketSourceConfig` and `WebSocketSinkConfig` gained an
`auth: Option<Arc<OutboundAuthSigner>>` field and a
`with_auth(signer)` builder. Auth headers are injected on the
WebSocket **upgrade handshake** — the only point where the
tungstenite stack accepts custom headers — by building an
`http::Request<()>` via
`tokio_tungstenite::tungstenite::client::IntoClientRequest` and
inserting the headers returned from
`signer.http_headers(OutboundSignContext{ method: "GET", path,
body: b"", now_unix })` before calling
`tokio_tungstenite::connect_async(request)`.

Per-mode behaviour:

- **Bearer / Basic / ApiKey / HmacSign** — headers flow onto the
  handshake exactly as for HTTP. HMAC canonicalizes over the
  handshake request line with an empty body, so server-side
  verification can reuse the HTTP HMAC verifier with the same
  rules.
- **None** — unmodified handshake, no Authorization header
  appears.
- **Mtls** — logged-and-ignored at this layer. Tungstenite's
  built-in TLS path doesn't accept a client identity; a custom
  rustls connector is required. Tracked as follow-up S10 work.
- **BrokerNative** — warned-and-ignored (not applicable to
  WebSocket).

The source-side reconnect loop re-builds the request on every
reconnect so HMAC timestamps stay fresh (no stale `X-Aeon-Ts`
replays past the server skew window), and continues to re-run
the S7 SSRF guard before each dial — a sign-step failure is
warned and retried on the next backoff tick rather than
terminating the reader task. The sink side is a single
handshake (no internal reconnect loop).

Path canonicalization is a 10-line pure helper (`extract_path`)
that strips scheme + authority and returns `/` for malformed
URLs — the downstream `connect_async` is the actual URL
validator, so the helper never panics on user input.

10 unit tests in `src/websocket/{source,sink}.rs`:
`build_ws_request` for Bearer / Basic / ApiKey / HmacSign header
injection, `None` absence check, `extract_path` common cases.
No live-server dependency; factories are not yet registered in
`aeon-cli` (tracked under P5.c), so YAML parser wiring for
WebSocket is deferred.

### 9.20 S10.5 Kafka broker-native wiring — **Closed 2026-04-22**

`KafkaSourceConfig` and `KafkaSinkConfig` gained
`auth: Option<Arc<OutboundAuthSigner>>` + `with_auth(signer)`
builder. A new `aeon-connectors/src/kafka/auth.rs` module
provides `apply_outbound_auth(&mut ClientConfig, Option<&Arc<...>>)`
which is called from both `KafkaSource::new` and `KafkaSink::new`
**after** the base rdkafka defaults but **before** the user
`config_overrides` loop — the existing debug escape hatch still
wins.

Per-mode translation onto rdkafka's `ClientConfig`:

- **`BrokerNative`** — iterate `signer.broker_native().values`
  (a `BTreeMap<String, String>`) and call `client_config.set(k, v)`
  for each pair. The operator supplies rdkafka keys directly —
  typical surface is `security.protocol`, `sasl.mechanism`
  (`PLAIN` | `SCRAM-SHA-256` | `SCRAM-SHA-512` | `OAUTHBEARER`),
  `sasl.username`, `sasl.password`, `sasl.oauthbearer.config`,
  `ssl.ca.location`. Aeon deliberately does NOT enforce a
  whitelist — the rdkafka knob namespace is the source of truth,
  and refusing unknown keys would break forward-compat with new
  librdkafka versions.
- **`Mtls`** — set `security.protocol=ssl` plus the inline-PEM
  knobs `ssl.certificate.pem` and `ssl.key.pem` (librdkafka ≥ 1.5).
  PEM bytes are validated as UTF-8 before setting; invalid bytes
  silently skip (rdkafka will produce a clear error on connect).
  Operators needing SASL + TLS combine modes via the `config_overrides`
  escape hatch: `security.protocol: sasl_ssl` + `sasl.*` keys, with
  `auth.mode: broker_native` carrying both SASL and TLS settings.
- **`None`** — explicit no-op (network-layer auth on the broker).
- **`Bearer` | `Basic` | `ApiKey` | `HmacSign`** — warned and
  skipped. These are HTTP-layer modes and mean nothing to the
  Kafka wire protocol; logging rather than rejecting follows the
  S9 "loud but non-fatal" pattern so a shared outbound-auth YAML
  block across mixed connectors doesn't hard-fail a Kafka sink.

Wired into `KafkaSourceFactory::build()` and
`KafkaSinkFactory::build()` via the same
`build_outbound_auth_signer` helper used for HTTP — no new parser
surface, no new tests required on the aeon-cli side. 5 new unit
tests in `kafka/auth.rs` cover every mode plus the absent-signer
case; full `cargo test -p aeon-connectors --features kafka
--lib kafka::` suite passes 11/11 green.

### 9.21 S10.6 Redis broker-native wiring — **Closed 2026-04-22**

`RedisSourceConfig` and `RedisSinkConfig` gained
`auth: Option<Arc<OutboundAuthSigner>>` + `with_auth(signer)`
builder. A new `aeon-connectors/src/redis_streams/auth.rs` module
provides `resolve_connection_info(url, signer)` which returns the
`redis::ConnectionInfo` handed to `redis::Client::open` — called
by both `RedisSource::new` and `RedisSink::new` in place of the
previous raw-URL open path.

Per-mode translation onto `redis::ConnectionInfo`:

- **`BrokerNative`** — parse the URL via `IntoConnectionInfo`
  first, then overlay `signer.broker_native().values` keys onto
  `info.redis`: `username` → `info.redis.username`, `password` →
  `info.redis.password`, `db` → `info.redis.db` (parsed as i64;
  non-integer values return a config error, never panic). If the
  URL already carried `user:pass@host` userinfo and the signer
  also supplied credentials, the signer wins and a warn line is
  emitted so the conflict is visible in logs. This matches the
  `AUTH` / Redis 6 ACL (user+pass) surface — no dedicated flat
  key in the URL, so the override path is the `ConnectionInfo`
  struct, not URL mutation.
- **`Mtls`** — warn-and-skip. The workspace pins
  `redis = "0.27"` without the `tls-rustls` feature flag, so
  `redis::TlsCertificates` cannot be wired and client certs can't
  be attached. Operators needing server-auth TLS can still use
  `rediss://` URLs via `BrokerNative`; client-auth mTLS is a
  follow-up that needs a Cargo feature toggle on the redis dep.
- **`None`** — pass-through (the parsed `ConnectionInfo` is
  returned unmodified).
- **`Bearer` | `Basic` | `ApiKey` | `HmacSign`** — warned and
  skipped. These are HTTP-layer modes and mean nothing to RESP;
  logging rather than rejecting follows the same "loud but
  non-fatal" pattern as the Kafka translator.

7 unit tests in `redis_streams/auth.rs`: no-signer pass-through,
`BrokerNative` overrides URL userinfo (username + password + db),
password-only signer, non-integer-`db` rejection, `None` mode
pass-through, `Bearer` warn-and-skip, bad-URL rejection. Full
`cargo test -p aeon-connectors --features redis-streams --lib
redis_streams::` suite passes 16/16 green (7 new + 9 existing).
aeon-cli YAML factory wiring for the Redis source/sink is
deferred to P5.c (same "not-yet-registered connectors" bucket as
WebSocket).

### 9.22 S10.7 NATS broker-native wiring — **Closed 2026-04-22**

`NatsSourceConfig` and `NatsSinkConfig` gained
`auth: Option<Arc<OutboundAuthSigner>>` + `with_auth(signer)`
builder. A new `aeon-connectors/src/nats/auth.rs` module exposes
`connect_with_auth(url, signer)` which builds an
`async_nats::ConnectOptions` then calls `.connect(url)` — replaces
the previous `async_nats::connect(&url)` call in both
`NatsSource::new` and `NatsSink::new`.

Per-mode translation onto `ConnectOptions` (async-nats 0.38):

- **`BrokerNative`** — the NATS credential surface has four
  distinct shapes that the translator picks in this order of
  precedence:
    1. `credentials` — inline contents of a `.creds` file
       (JWT + NKey pair, the canonical NATS decentralized-auth
       format produced by `nsc`). Routed to
       `ConnectOptions::credentials(&str)`; malformed input surfaces
       as `AeonError::config`, never panics.
    2. `nkey_seed` — raw NKey seed string. Routed to
       `ConnectOptions::nkey(String)`. Parser runs lazily during
       connect, so any non-empty string passes translation.
    3. `token` — bearer-style NATS token. Routed to
       `ConnectOptions::token(String)`.
  Independently of the first three, `username` + `password` together
  are routed to `ConnectOptions::user_and_password`. Providing one
  without the other is a config error — misconfiguration is loud,
  never silently under-authenticated.
- **`Mtls`** — warn-and-skip. async-nats 0.38's
  `ConnectOptions::add_client_certificate` takes `PathBuf` not inline
  PEM. Aeon's `OutboundAuthSigner` holds cert/key bytes in
  `SecretBytes` (zeroize-on-drop) and writing them to disk — even
  under `/tmp` — would violate the S1 "no secrets on disk outside
  the deploy dir" posture. Tracked as follow-up: either patch
  async-nats for in-memory PEM, use a ramfs-backed path, or
  pre-provision cert files out-of-band with paths supplied via
  `BrokerNative` free-form keys.
- **`None`** — returns a default `ConnectOptions` (no auth).
- **`Bearer` | `Basic` | `ApiKey` | `HmacSign`** — warned and
  skipped. Same "loud but non-fatal" pattern as Kafka / Redis so
  shared outbound-auth YAML blocks across mixed connectors don't
  hard-fail a NATS client.

10 unit tests in `nats/auth.rs` cover every mode branch plus
the username-without-password / password-without-username config
errors and the malformed-credentials error path. Tests drive
`build_connect_options` (the translator) directly so no live NATS
server is required. Full `cargo test -p aeon-connectors --features
nats --lib nats::` suite passes 12/12 green (10 new auth + 2
existing sink dedup). aeon-cli YAML factory wiring for the NATS
source/sink is deferred to P5.c (same "not-yet-registered
connectors" bucket as WebSocket / Redis).

### 9.23 S10.8 CDC broker-native wiring (Postgres / MySQL / MongoDB) — **Closed 2026-04-22**

The three CDC source connectors closed broker-native in one sweep.
All three follow the same pattern: a new `auth.rs` sibling module,
`auth: Option<Arc<OutboundAuthSigner>>` + `with_auth(signer)` on
the `*SourceConfig`, and the client-construction call inside
`*Source::new` replaced with a call through the translator.

**Postgres CDC (`postgres_cdc/auth.rs`)** — translator
`resolve_config(conn_string, signer)` parses the Postgres keyword /
URL connection string into `tokio_postgres::Config` via
`Config::from_str`, then for `BrokerNative` overlays `user` /
`password` / `dbname` / `application_name` from
`broker_native().values` using `Config::user()` / `.password()` /
`.dbname()` / `.application_name()`. When the connection string
already embedded `user`/`password` and the signer also supplies
credentials, a warn line records the conflict (signer wins). `Mtls`
is warn-and-skip — the workspace does not depend on
`tokio-postgres-rustls`; wiring requires adding that crate and
plumbing `rustls::ClientConfig` through `Config::connect(tls_ctor)`.
HTTP modes warn-and-skip. 8 unit tests cover no-signer,
override-user/password, application_name, password-only (preserves
URL user), `None` mode, mTLS warn-skip, Bearer warn-skip, and
malformed-connection-string error.

**MySQL CDC (`mysql_cdc/auth.rs`)** — translator `resolve_opts(url,
signer)` parses the URL into `mysql_async::Opts` via
`Opts::from_url`, then for `BrokerNative` rebuilds via
`OptsBuilder::from_opts(opts)` and applies `user` / `password` /
`db_name` through the builder's `Some`-wrapped setters. The `Opts`
→ `OptsBuilder` → `Opts` round trip is the only way to mutate
parsed Opts (no direct setters). Conflict-warn on URL userinfo +
signer creds; signer wins. `Mtls` is warn-and-skip — mysql_async's
`SslOpts` requires the `native-tls-tls` or `rustls-tls` cargo
feature on the workspace dep, neither is enabled. HTTP modes
warn-and-skip. 7 unit tests cover no-signer, full override,
password-only (preserves URL user), `None` mode, mTLS warn-skip,
Bearer warn-skip, and malformed-URL rejection.

**MongoDB CDC (`mongodb_cdc/auth.rs`)** — translator
`resolve_options(uri, signer)` is **async** because
`mongodb::options::ClientOptions::parse` is async (it resolves
`mongodb+srv://` DNS records on the `dns-resolver` feature). For
`BrokerNative` the translator takes
`options.credential.unwrap_or_default()`, overlays
`Credential.username` / `.password` / `.source` from
`broker_native().values.{username, password, auth_source}`, and
writes it back into `options.credential`. A companion
`resolve_client(uri, signer)` wraps the options helper and calls
`mongodb::Client::with_options`. `Mtls` is warn-and-skip — MongoDB's
`TlsOptions::cert_key_file_path` takes a `PathBuf`, and inline PEM
from `SecretBytes` can't be written to `/tmp` without violating
S1's "no secrets outside deploy dir" posture (same follow-up
shape as NATS). HTTP modes warn-and-skip. 8 tests (all
`#[tokio::test]`) cover no-signer, URI-userinfo override,
credentials-on-URI-without-userinfo, password-only preserves URI
username, `None` mode, mTLS warn-skip, Bearer warn-skip, and
malformed-URI rejection.

Cross-cutting properties of the sweep:

- **All three** carry `Arc<OutboundAuthSigner>` (not a raw pointer)
  so the zeroize-on-drop `SecretBytes` inside the signer clones
  cheaply into pipelines and tests alike.
- **All three** apply the established "signer wins over URI /
  connection-string userinfo, warn on conflict" rule — operators
  trust the signer path to be the authoritative credential source.
- **All three** warn-and-skip on HTTP-style modes (Bearer / Basic /
  ApiKey / HmacSign) rather than hard-failing, so a shared
  outbound-auth YAML block across a mixed-protocol pipeline never
  takes down a CDC source.
- **Three distinct mTLS follow-ups** are queued because the three
  SDKs expose radically different TLS surfaces. None is blocking
  for the Gate 2 auth story — broker-native covers the common
  "username/password over TLS" deployment; operator-managed mTLS
  is the edge case.

Tests: `cargo test -p aeon-connectors --features postgres-cdc --lib
postgres_cdc::` 11/11 green (8 new auth + 3 existing); `--features
mysql-cdc --lib mysql_cdc::` 7/7 green (all new); `--features
mongodb-cdc --lib mongodb_cdc::` 14/14 green (8 new auth + 6
existing resume-token). aeon-cli factory wiring for the three CDC
sources is deferred to P5.c (same "not-yet-registered connectors"
bucket).

### 9.24 S10.9 WebTransport sink HTTP/3 CONNECT header auth — **Closed 2026-04-22**

`WebTransportSinkConfig` gained `auth: Option<Arc<OutboundAuthSigner>>`
+ `with_auth(signer)`. The sink's `new()` now builds a
`wtransport::endpoint::ConnectOptions` via a new
`build_connect_options(url, signer)` helper and hands it to
`endpoint.connect(options)` instead of the previous raw `&url`
call.

Per-mode translation onto the HTTP/3 CONNECT request:

- **`Bearer` / `Basic` / `ApiKey` / `HmacSign`** — the signer's
  `http_headers(&OutboundSignContext { method: "CONNECT", path,
  body: b"", now_unix })` produces the header list, each injected
  via `ConnectRequestBuilder::add_header(k, v)`. The CONNECT
  request carries them to the server as application-layer auth on
  the HTTP/3 session-initiation handshake, before any WebTransport
  stream is opened. HMAC canonicalizes over `"CONNECT" | path | ts
  | ""` — path is extracted from the `https://` URL by the same
  `extract_path` shape used in the WebSocket sink so operators can
  generate verifying HMACs with a single algorithm regardless of
  transport.
- **`Mtls`** — logged-and-ignored at the sink layer. The client
  certificate must be baked into the `wtransport::ClientConfig`
  that the operator already supplies to `WebTransportSinkConfig::new`,
  via `ClientConfigBuilder::with_custom_tls(rustls::ClientConfig)`
  at build time. The sink can't overlay client auth onto the config
  after the fact because `ClientConfig` is opaque post-build —
  rebuilding it from the signer's PEM material requires a parallel
  builder helper. Tracked as follow-up: expose a
  `WebTransportSinkConfig::new_with_mtls(url, signer)` helper that
  constructs the `rustls::ClientConfig` with `with_client_auth_cert`
  from the signer's `mtls_cert_pem()`/`mtls_key_pem()` directly,
  then chains `.with_custom_tls()`.
- **`BrokerNative`** — warn-and-skip. There is no "broker-native"
  concept for WebTransport — HTTP/3 sessions are the wire format.
- **`None`** — no headers added; builder call chain equivalent to
  the previous direct `endpoint.connect(&url)` path.

9 unit tests on `build_connect_options` cover every mode branch
plus a no-signer baseline, a path-extractor test for common URL
shapes, and assertions that the HTTP modes populate the
`additional_headers` HashMap while the mTLS / BrokerNative / None
modes leave it empty. Tests drive the helper directly so no live
WebTransport server is required. Full `cargo test -p aeon-connectors
--features webtransport --lib webtransport::` suite passes 10/10
green (9 new auth + 1 pre-existing datagram-source test).

This closes the **HTTP-style** half of S10 WebTransport. The mTLS
half is queued as a follow-up. aeon-cli YAML factory wiring for
the WT sink already exists at `aeon-cli/src/connectors.rs` —
adding the auth keys is a drop-in that pairs with the follow-up
builder helper for mTLS.
