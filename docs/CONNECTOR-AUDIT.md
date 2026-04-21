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
| 8.4 consumer-group config mode | Post-bake design + impl | ROADMAP B4 | ⏳ pending (deliberate — post-bake) |

All B1 + B2 + B3 items landed in the same session on 2026-04-21.
Pre-bake bar is clear; the only open ECR-bake-path item is P4.iii
(task #6, pre-bake image to ECR `us-east-1`).

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
