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
| HTTP Polling | Pull | Controlled interval | — | N/A | No | OK |
| HTTP Webhook | Push | Unbounded | PushBuffer | **HTTP 503** | No | OK — best Phase 3 |
| WebSocket | Push | Unbounded | PushBuffer | **drops msgs** | No | **BUG** — see §4.1 |
| NATS JetStream | Pull | Bounded (consumer grp) | — | Implicit (ack) | Implicit | OK |
| MQTT | Push | Unbounded | PushBuffer | Sleep-poll backoff | No | OK but see §4.2 |
| RabbitMQ | Push | Unbounded | PushBuffer | **nack + requeue** | No | OK — best-in-class |
| Redis Streams | Pull | Bounded (pending) | — | Implicit (XACK) | Yes | OK |
| Postgres CDC | Pull | Bounded (LSN) | — | Implicit | Yes | OK (polling, see §4.5) |
| MySQL CDC | Pull | Bounded (binlog pos) | — | Implicit | Yes | OK (polling, see §4.5) |
| MongoDB CDC | Push | Unbounded | PushBuffer | Blocking send only | No (stub) | See §4.3 |
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
| MQTT | **None differentiated** | Always per-publish await | no-op | **Gap** — see §4.4 |
| RabbitMQ | **None differentiated** | Always per-publish confirm | no-op | **Gap** — see §4.4 |
| Redis Streams | **None differentiated** | Always per-XADD await | no-op | **Gap** — see §4.4 |
| QUIC | None differentiated | New stream per batch | no-op | Functional, see §4.6 |
| WebTransport | None differentiated | New stream per batch | no-op | Functional, see §4.6 |
| WebSocket | None differentiated | Per-output `send` + loop await | `writer.flush()` | Functional |

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

### 4.2 — MQTT source sleeps instead of using protocol backoff

**Severity**: Low — the sleep-poll approach *does* backpressure, but is coarse.
**Location**: `crates/aeon-connectors/src/mqtt/source.rs:132-136`.

**Current**: when overloaded, sleeps the event loop poll for 100ms. The broker
will eventually pile up retained messages or, for QoS 1/2, stop delivering until
we ack.

**Better**: same as WebSocket — remove the `is_overloaded()` branch and let
`tx.send(event).await` block. Not polling the `EventLoop` naturally pauses
delivery because rumqttc's `EventLoop::poll` is where MQTT packet reception
happens.

**Deferred** until the WebSocket fix is proven, because the MQTT fix is the
same mechanism in a different connector and we want to validate the approach
once before repeating it.

### 4.3 — MongoDB CDC has no resume token persistence

**Severity**: Correctness gap — after a crash, the connector restarts the
change stream from the beginning instead of resuming at the last processed
document.
**Location**: `crates/aeon-connectors/src/mongodb_cdc/source.rs` (no
`source_offset` field used; no token persisted).

**Fix**: store `ChangeStream::resume_token()` in `Event.source_offset` (as
bytes or a string reference via metadata). Expose it through the delivery
ledger checkpoint so the connector can seek to it on restart.

**Deferred** — MongoDB CDC is post-Scenario 1 per `CLAUDE.md`.

### 4.4 — MQTT, RabbitMQ, Redis sinks ignore DeliveryStrategy

**Severity**: Missing feature — `DeliveryStrategy::UnorderedBatch` cannot give
its normal throughput/latency benefit because these sinks still await every
publish synchronously.
**Locations**:
- `crates/aeon-connectors/src/mqtt/sink.rs` — always awaits per publish.
- `crates/aeon-connectors/src/rabbitmq/sink.rs` — always awaits publisher
  confirm per publish.
- `crates/aeon-connectors/src/redis_streams/sink.rs` — always awaits each XADD.

**Fix shape** (common pattern, adapted per connector):

- `PerEvent`: current behavior — await per publish.
- `OrderedBatch`: issue all publishes, then await all confirms via `join_all`
  (same pattern as the Kafka sink fix, `kafka/sink.rs:219`).
- `UnorderedBatch`: store publish futures in `pending_acks: Vec<_>`, return
  `BatchResult::all_pending(ids)`. In `flush()`, `join_all` the pending vec
  and report results.

Redis Streams needs a slightly different approach because the redis crate's
`Pipeline` type is the idiomatic way to batch XADDs. For `UnorderedBatch`, use
`redis::pipe().xadd().xadd()...query_async()` in `flush()`.

**Deferred** until the Kafka unordered-batch metric bug (§4.0) is fixed,
because (a) those connectors are post-Scenario 1 and (b) the fix requires the
metric bug to be resolved first so we can validate them.

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
| T3 WebTransport | QUIC, out-of-process | Async `call_batch` via `ProcessorTransport` | Partial — see §5.3 | **Not wired** into `run_buffered` | Host implemented, adapter missing |
| T4 WebSocket | WS, out-of-process | Async `call_batch` via `ProcessorTransport` | Partial — see §5.3 | **Not wired** into `run_buffered` | Host implemented, adapter missing |

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

### 5.3 T3/T4 — Host implemented but not wired to `run_buffered`

`crates/aeon-engine/src/transport/webtransport_host.rs` and
`.../websocket_host.rs` both implement `aeon-types::ProcessorTransport` —
an *async* trait — not the sync `Processor` trait. The engine's
`run_buffered` signature is `P: Processor + Send + Sync + 'static`, so
neither host can be used with the buffered pipeline today.

The T3/T4 hosts track in-flight batches in a `BatchInflight` struct
(`transport/session.rs`) keyed by `batch_id`, with a `DashMap` of pending
`oneshot` receivers. **This structure has no capacity bound.** If a remote
processor stalls, the map grows unboundedly until the 30s per-batch timeout
fires and drops the batch.

**Fixes needed**:

1. **Wire a `ProcessorTransport` → `Processor` adapter** so T3/T4 processor
   hosts can be used with `run_buffered`. The adapter would `block_on` the
   async call, but that defeats async backpressure. Better: add a
   `run_buffered_async` pipeline variant that takes an async processor and
   runs it with `await` inside the processor task. This preserves the yield
   semantics and lets the SPSC fullness propagate backpressure naturally
   through the async call.

2. **Bound `BatchInflight`** with a `tokio::sync::Semaphore`. Configurable
   `max_inflight_batches` on the host config; `call_batch` acquires a permit
   before sending a frame, releases on response/timeout. This gives explicit
   protocol-level backpressure at the transport boundary.

3. **Per-batch timeout + retry policy** — move retry decisions from the
   transport layer into the pipeline via `BatchFailurePolicy`, so a single
   policy controls retries regardless of tier.

All three are **post-Gate 1** because T3/T4 aren't on the Gate 1 critical
path.

---

## 6. Immediate Fix Plan (this pass)

Scope is intentionally narrow — fix what is on the Scenario 1 critical path
and call correctness bugs. Everything else is captured in the matrices
above with file:line references for follow-up phases.

1. **§4.0 — UnorderedBatch metric bug** (pipeline.rs):
   - Track pending count locally in the sink task.
   - On successful `flush()`, credit `outputs_sent` with the drained pending
     count.
   - Update the `gate1_steady_state` bench to use `outputs_sent` again as
     the zero-loss check (remove the `events_received` workaround).

2. **§4.1 — WebSocket source drop** (websocket/source.rs):
   - Remove the `is_overloaded()` drop branches.
   - Add a unit test that produces 2× `channel_capacity + spill_threshold`
     events through the source and asserts zero loss.

Other gaps (§4.2, §4.3, §4.4, §4.5, §4.6, §5.3) are deferred as follow-up
work with clear file:line references for the next phase.

---

## 7. Verdict

**Source side**: Pull-based connectors (Kafka, NATS JetStream, Redis Streams,
CDCs) already propagate natural upstream backpressure via protocol offsets.
Push-based connectors all use the shared three-phase `PushBuffer` and most
of them do the right thing at Phase 3. WebSocket is the one outlier that
drops instead of applying TCP flow control (fixable in this pass).

**Sink side**: File, Kafka, and NATS are fully strategy-aware and
production-grade. MQTT, RabbitMQ, and Redis Streams are functional but
treat every strategy as `PerEvent`. This does not break correctness — it
just means they can't take advantage of `UnorderedBatch` for throughput.
Fix is well-understood (same pattern as Kafka `OrderedBatch` + `join_all`)
and deferred to a dedicated pass.

**Processor side**: T1/T2 are wired correctly and propagate backpressure
end-to-end. T3/T4 hosts are implemented but not yet integrated with the
pipeline runner, and their `BatchInflight` queue is unbounded. Both issues
are post-Gate 1.

**Pipeline orchestrator**: the single correctness bug is the `outputs_sent`
metric not being credited for `UnorderedBatch`. Fixed in this pass.
