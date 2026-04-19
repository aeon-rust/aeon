# Aeon Windowing + Watermarks — Design Sketch (Deferred)

> **Captured 2026-04-19 from discussion; not a commitment.** This is a
> thinking document — a precise record of where Aeon stands relative to
> Flink's windowing + event-time story, and what the minimum addition
> would look like if we chose to close the gap. **Parked until Gate 2
> closes and Session B produces a ceiling number.** Re-open then.
>
> Source of the idea: `docs/POSITIONING.md` § 6 lists "Windowing / CEP"
> and "SQL authoring" as the two largest Flink-parity gaps. The
> question this doc answers is: *how much of that lift is genuinely
> new work, vs. plumbing Aeon already has?*

---

## 1. Scope

Two features, tightly coupled:

- **F1 — Windowing primitives**: tumbling / sliding / session windows,
  with per-key aggregation state.
- **F2 — Watermarks + event-time**: the mechanism that lets a window
  decide when to close and emit, including handling of late-arriving
  events.

F1 without F2 gives you processing-time windows only — fine for rate
counters and ops dashboards, wrong for any workload where event
semantics are tied to when the event *actually happened* (finance,
IoT, surveillance, audit trails). F2 without F1 is infrastructure
with no user-visible surface. They ship together.

Out of scope for this sketch: CEP / pattern matching (Flink's
`PatternSelectFunction`), streaming SQL DDL. Both need F1+F2 as
prerequisites.

---

## 2. Current Aeon Primitives That Already Solve Pieces

| Primitive | What it gives you | What it doesn't |
|---|---|---|
| `Event.timestamp` (i64 nanos) | Already the **event-time** when upstream populated it correctly (Kafka broker ts, sensor clock) | Not all sources populate it meaningfully |
| UUIDv7 per-core generator | Monotonic **ingest-time** per generator; cheap "current time at source" signal | Ingest-time, not event-time — it's when Aeon stamped the id |
| `source_ts: Option<Instant>` | Wall-clock moment the source observed the event | Single-point signal; not a watermark |
| Source offset (pull kind) | Monotonic position per partition | Position only; no time info |
| `per_sink_ack_seq` + `AckSeqTracker` | Delivery watermark for L2 GC and crash recovery | Orthogonal to windowing — "did the sink persist?" ≠ "can I close the window?" |
| `DurabilityMode::{OrderedBatch, UnorderedBatch, PerEvent, None}` | Delivery semantics + batching | Not about *when to emit* an aggregation |
| L1/L2/L3 tiered state store | Keyed state backend with TTL-shaped semantics | No window-keyed index yet |

Takeaway: the **per-partition monotonic signal**, **min-across-inputs
reducer**, and **tiered state** patterns are all already in the
codebase. A watermark tracker is *structurally identical* to the
existing `AckSeqTracker`.

---

## 3. The Real Gap — What Watermarks Actually Are

A watermark is a **monotonic promise that flows through the DAG
separately from the data plane**:

> *No event with `timestamp < T` will arrive after this.*

Three pieces Aeon doesn't have:

1. **Generation strategy** — someone has to emit `Watermark(T)`.
   Four standard approaches:
   - **Broker-provided** — Kafka read-committed transaction markers
     carry this for some setups. Not universal.
   - **Periodic ascending** — assume `timestamp` is monotonic per
     partition. Simplest. Breaks on out-of-order events.
   - **Bounded out-of-orderness** — `Watermark(max(timestamp) − N ms)`.
     Most widely used in production. Configurable tolerance.
   - **Punctuated** — specific events carry watermark info in headers.
     Rare; purpose-built feeds.
2. **Propagation channel** — events do NOT carry watermarks. Watermarks
   are a sibling signal flowing through SPSC rings, processor
   boundaries, cluster transfer, and fan-in points.
3. **Fan-in `min`** — at a multi-partition or multi-source join,
   `Watermark = min(upstream_watermarks)`, because the slowest source
   dictates safety.

Then, on top of those three, windowing adds: **window assigner**,
**window state** (keyed by `(window_id, partition_key)`, backed by
L1/L2/L3), **trigger** (watermark-based / count-based / time-based),
**window function** (reduce / aggregate), **allowed lateness**, and a
**side output for late events past lateness**.

---

## 4. Reuse Opportunities

Two concrete shortcuts fall out of existing Aeon plumbing:

### 4.1 Processing-time watermarks for free

UUIDv7's embedded ms-precision timestamp + per-core monotonicity gives
you a processing-time watermark at zero additional cost. Good enough
for any workload where the user is fine with "events were seen by Aeon
in this minute" rather than "events happened in this minute."

That covers a surprising fraction of real workloads: ops dashboards,
rate limiting, simple bucket counts, alerting. Flink exposes this as
`TimeCharacteristic.ProcessingTime`.

### 4.2 Event-time watermarks from pull sources via bounded OOO

`(source_ts, offset)` + a configurable `max_out_of_orderness: Duration`
→ emit `Watermark(max(source_ts) − N)` every K events or M ms. That's
exactly Flink's `BoundedOutOfOrderness` strategy, and it slots
alongside the existing `source_ts` plumbing without restructuring.

### 4.3 The structural analogue

The EO-2 `AckSeqTracker` (per-sink → min across sinks → L2 GC
frontier) is *shape-identical* to what a watermark tracker needs
(per-partition → min across partitions → window close frontier). The
cleanest implementation is literally another tracker instance keyed
by partition instead of sink, reusing the same min-reducer and
propagation discipline.

---

## 5. Minimum New Work

Given what's already in the codebase:

1. **`Watermark` signal type** — sibling of `Event`, carries
   `{ partition_id, timestamp_ns, source_kind }`. 64-byte aligned.
2. **`WatermarkStrategy` trait on source connectors** —
   `on_event(&Event) → Option<Watermark>` + periodic tick.
   Two built-ins: `PeriodicAscending`, `BoundedOutOfOrderness(Duration)`.
   User-plug-in via Wasm.
3. **SPSC ring propagation** — interleaved enum
   `RingMsg::{ Event(Event), Watermark(Watermark) }`, or a side
   channel. Decision depends on whether the ring-buffer hot-path cost
   of the `match` outweighs the complexity of a parallel channel;
   lean toward interleaved for simplicity, benchmark it.
4. **Fan-in merger** — `WatermarkTracker { per_partition_wm:
   DashMap<PartitionId, i64> }` with `min_across_partitions() → i64`.
   Direct clone of `AckSeqTracker`'s shape.
5. **Cluster propagation** — extend AWPP `WireEvent` to carry an
   optional watermark, or add a `WireWatermark` variant. Crosses the
   QUIC boundary exactly once per remote partition per emission tick.
6. **Window subsystem** (the actual F1 work) — `Windowed<P: Processor>`
   wrapper sits between source and user processor, owns window state
   in `aeon-state`, consumes watermarks from the tracker, calls the
   user processor's `process_batch` only when a window triggers.
   Tumbling + sliding + session assigners. Trigger hook.

Items 1–5 are F2 and reuse heavily. Item 6 is genuinely new.

---

## 6. Effort Estimate (Revised)

Original estimates in `docs/POSITIONING.md` § 6:

| Gap | Original |
|---|---|
| Windowing / CEP primitives | 3–6 months |

Revised once reuse is accounted for:

| Item | Revised | Notes |
|---|---|---|
| F2 — Watermarks + event-time | **3–4 weeks** | Shrunk from "whole new subsystem" to "new signal type + propagation + strategy trait" because per-partition monotonic tracking and min-across-inputs reducer already exist (`AckSeqTracker`). |
| F1 — Windowing primitives (tumbling + sliding + session) | **6–10 weeks** | Window-keyed state index is new. Trigger machinery is new. Allowed-lateness + side-output is new. Does not shrink from existing plumbing. |
| F1 + F2 combined, shipped together | **~3 months** | End-to-end, including RD + cloud validation. Honest number. |

CEP patterns and streaming SQL sit on top of F1+F2 and are separate
future multi-month efforts each.

---

## 7. Trade-offs to Decide When We Revisit

- **Interleaved ring-buffer messages vs. parallel side channel** for
  watermark propagation. Affects hot-path dispatch cost.
- **Watermark-carrying `WireEvent` vs. separate `WireWatermark` AWPP
  frame**. Affects cluster fan-out frequency.
- **Window state backend** — L1 DashMap only (fast, bounded), L2 mmap
  segments (scales, crash-safe), L3 redb (durable, slow). Likely
  L1+L2 with L3 as checkpoint snapshot.
- **Default watermark strategy** — `BoundedOutOfOrderness(5s)` is
  Flink's implicit default and a safe starting point. Configurable
  per-source in the pipeline manifest.
- **Processing-time vs. event-time as the default** — Flink defaults
  to event-time since 1.12. Aeon should too, with processing-time as
  an explicit opt-in. Crypto chain workloads care about event-time
  almost universally.
- **Relationship to PoH chain** — the PoH chain already gives us a
  deterministic event order per partition. Watermarks don't replace
  it; they decorate it with "safe to close" information. Worth
  sketching whether the chain sequence number can double as a
  monotonic ingest-time signal for the processing-time watermark
  path. Likely yes; not required.

---

## 8. Known Not-Doing (Deferred Deliberately)

- **Allowed lateness > 0** can ship in a second cut. Many users are
  fine with "strict close at watermark + late-side-output."
- **Session windows** can lag behind tumbling + sliding in delivery
  order; they're the most state-heavy of the three.
- **Trigger DSL** (Flink's `Trigger` class hierarchy) — ship with
  only watermark-based triggers first. Count / time / custom can follow.
- **Merging windows** — session window merge logic when two gaps
  close is subtle. Out of scope for a first cut.

---

## 9. When to Revisit

Prerequisites that should close before re-opening this:

1. Gate 2 complete (Phase 3.5 V2–V6 + DOKS re-spin).
2. Session B done; ceiling number published.
3. v0.1 cut.
4. At least one production user asking for it. Until then, the crypto
   chain wedge + Rust-native latency story are the pitch. Windowing
   is a broadening move, not the wedge.

When the above is true, the F1+F2 plan above gets promoted to a real
task breakdown and the `AckSeqTracker` reuse is validated empirically
before committing to the shape in § 4.3.

---

*See also:* [`POSITIONING.md`](POSITIONING.md) § 6 (competitive
framing) · [`EO-2-DURABILITY-DESIGN.md`](EO-2-DURABILITY-DESIGN.md)
(the `AckSeqTracker` / L2 GC pattern this design proposes to reuse) ·
[`ARCHITECTURE.md`](ARCHITECTURE.md) · [`ROADMAP.md`](ROADMAP.md)
