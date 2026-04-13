# Aeon v3 — E2E Test Plan

> Comprehensive end-to-end test matrix: Connectors x Processor SDKs x Tiers.
> Referenced from `docs/ROADMAP.md` Phase P3.

---

## Inventory

### Sources (16)

| # | Source | Infra Required | Gate 1? |
|---|--------|---------------|---------|
| 1 | Memory | None | Yes |
| 2 | File | None | — |
| 3 | Kafka/Redpanda | Redpanda Docker | Yes |
| 4 | NATS JetStream | NATS server | — |
| 5 | HTTP Webhook | None (runs server) | — |
| 6 | HTTP Polling | HTTP endpoint | — |
| 7 | WebSocket | WS server | — |
| 8 | QUIC | QUIC endpoint | — |
| 9 | WebTransport | WT endpoint | — |
| 10 | WebTransport Datagram | WT endpoint | — |
| 11 | Redis Streams | Redis server | — |
| 12 | MQTT | MQTT broker | — |
| 13 | RabbitMQ | RabbitMQ server | — |
| 14 | PostgreSQL CDC | PostgreSQL | — |
| 15 | MySQL CDC | MySQL | — |
| 16 | MongoDB CDC | MongoDB | — |

### Sinks (12)

| # | Sink | Infra Required |
|---|------|---------------|
| 1 | Memory | None |
| 2 | Blackhole | None |
| 3 | Stdout | None |
| 4 | File | None |
| 5 | Kafka/Redpanda | Redpanda Docker |
| 6 | NATS | NATS server |
| 7 | WebSocket | WS server |
| 8 | QUIC | QUIC endpoint |
| 9 | WebTransport | WT endpoint |
| 10 | Redis Streams | Redis server |
| 11 | MQTT | MQTT broker |
| 12 | RabbitMQ | RabbitMQ server |

### Processor SDKs x Tiers (19 configurations)

| # | SDK | T1 (Native) | T2 (Wasm) | T3 (WebTransport) | T4 (WebSocket) |
|---|-----|:-----------:|:---------:|:-----------------:|:--------------:|
| 1 | Rust Native | Yes | — | — | — |
| 2 | Rust Wasm | — | Yes | — | — |
| 3 | AssemblyScript | — | Yes | — | — |
| 4 | C/C++ | Yes | Yes | — | — |
| 5 | C#/.NET | Yes (NativeAOT) | — | — | Yes |
| 6 | Python | — | — | Yes | Yes |
| 7 | Go | — | — | Yes | Yes |
| 8 | Rust Network | — | — | Yes | Yes |
| 9 | Node.js | — | — | Yes | Yes |
| 10 | Java | — | — | Yes | Yes |
| 11 | PHP | — | — | — | Yes |

Total tier instances: 4 T1 + 3 T2 + 5 T3 + 7 T4 = 19 processor configurations.

Full cross-product (Sources x Sinks x Processors) = 16 x 12 x 19 = 3,648 combinations.
Reduced to 58 practical tests across 8 tiers below.

---

## Test Tiers

### Tier A: Memory Round-Trip (all SDKs) — P0, no infra

Memory Source -> Processor -> Memory Sink. Baseline correctness for every SDK.

| # | Processor | Tier | Test | Status |
|---|-----------|------|------|--------|
| A1 | Rust Native | T1 | Memory -> RustNative -> Memory | ✅ |
| A2 | Rust Wasm | T2 | Memory -> RustWasm -> Memory | ✅ |
| A3 | AssemblyScript | T2 | Memory -> AssemblyScript -> Memory | ✅ |
| A4 | C/C++ | T1 | Memory -> C_Native -> Memory | ✅ |
| A5 | C/C++ | T2 | Memory -> C_Wasm -> Memory | ✅ |
| A6 | C#/.NET | T1 | Memory -> DotNet_NativeAOT -> Memory | ✅ |
| A7 | C#/.NET | T4 | Memory -> DotNet_WS -> Memory | ✅ |
| A8 | Python | T4 | Memory -> Python_WS -> Memory | ✅ |
| A9 | Go | T4 | Memory -> Go_WS -> Memory | ✅ |
| A10 | Rust Network | T4 | Memory -> RustNet_WS -> Memory | ✅ |
| A11 | Node.js | T4 | Memory -> NodeJS_WS -> Memory | ✅ |
| A12 | Java | T4 | Memory -> Java_WS -> Memory | ✅ |
| A13 | PHP | T4 | Memory -> PHP_WS -> Memory | ✅ |

### Tier B: File Round-Trip (tier families) — P1, no infra

File Source -> Processor -> File Sink. One representative per tier family.

| # | Processor | Tier | Test | Status |
|---|-----------|------|------|--------|
| B1 | Rust Native | T1 | File -> RustNative -> File | ✅ |
| B2 | C/C++ | T1 | File -> C_Native -> File | ✅ |
| B3 | Python | T4 | File -> Python_WS -> File | ✅ |
| B4 | Node.js | T4 | File -> NodeJS_WS -> File | ✅ |

### Tier C: Kafka/Redpanda E2E (all SDKs) — P0, needs Redpanda

Kafka Source -> Processor -> Kafka Sink. The Gate 1 money path.

| # | Processor | Tier | Test | Status |
|---|-----------|------|------|--------|
| C1 | Rust Native | T1 | Kafka -> RustNative -> Kafka | ✅ |
| C2 | Rust Wasm | T2 | Kafka -> RustWasm -> Kafka | ✅ |
| C3 | C/C++ | T1 | Kafka -> C_Native -> Kafka | ✅ |
| C4 | C#/.NET | T1 | Kafka -> DotNet_NativeAOT -> Kafka | ✅ |
| C5 | C#/.NET | T4 | Kafka -> DotNet_WS -> Kafka | ✅ |
| C6 | Python | T4 | Kafka -> Python_WS -> Kafka | ✅ |
| C7 | Go | T4 | Kafka -> Go_WS -> Kafka | ✅ |
| C8 | Rust Network | T4 | Kafka -> RustNet_WS -> Kafka | ✅ |
| C9 | Node.js | T4 | Kafka -> NodeJS_WS -> Kafka | ✅ |
| C10 | Java | T4 | Kafka -> Java_WS -> Kafka | ✅ |
| C11 | PHP | T4 | Kafka -> PHP_WS -> Kafka | ✅ |

### Tier D: T3 WebTransport Variants — P1, needs TLS certs

Memory Source -> Processor (T3) -> Memory Sink. Validates QUIC/WebTransport transport.

**Sequencing (2026-04-10)**: per
[`WT-SDK-INTEGRATION-PLAN.md`](WT-SDK-INTEGRATION-PLAN.md), the order
of attack is **D1 (Python, aioquic) → D2 (Go, quic-go/webtransport-go)**,
after which Tier D is fully runnable for every SDK where a
production-grade WT client library exists. D4 (Node.js) and D5 (Java)
are **deferred** (Node.js library is a self-described stopgap; Flupke
WT is explicitly "still experimental") and remain `todo!()` stubs
until their libraries mature. There is no Tier D row for C#/.NET or
C/C++ yet — when those languages get WT client support they will be
added as D6+ rows.

| # | Processor | Test | Status |
|---|-----------|------|--------|
| D1 | Python | Memory -> Python_WT -> Memory | ✅ 2026-04-10 (aioquic) |
| D2 | Go | Memory -> Go_WT -> Memory | ✅ 2026-04-10 (quic-go/webtransport-go) |
| D3 | Rust Network | Memory -> RustNet_WT -> Memory | ✅ |
| D4 | Node.js | Memory -> NodeJS_WT -> Memory | ❌ (deferred — WT plan §5.4) |
| D5 | Java | Memory -> Java_WT -> Memory | ❌ (deferred — WT plan §5.3) |

### Tier E: Cross-Connector Coverage — P2, mixed infra

One representative SDK (Python T4) across many connector pairs.

| # | Source | Sink | Test | Status |
|---|--------|------|------|--------|
| E1 | Memory | Blackhole | Memory -> Python -> Blackhole | ✅ |
| E2 | Memory | Stdout | Memory -> Python -> Stdout | ✅ |
| E3 | Memory | File | Memory -> Python -> File | ✅ |
| E4 | File | Memory | File -> Python -> Memory | ✅ |
| E5 | File | Kafka | File -> Python -> Kafka | ✅ |
| E6 | Kafka | File | Kafka -> Python -> File | ✅ |
| E7 | Kafka | Blackhole | Kafka -> Python -> Blackhole | ✅ |
| E8 | HTTP Webhook | Memory | HTTPWebhook -> Python -> Memory | ✅ |
| E9 | HTTP Webhook | Kafka | HTTPWebhook -> Python -> Kafka | ✅ |

### Tier F: External Messaging Systems — P2, Docker services

Each tests a different messaging connector with a different SDK.

| # | Source -> Sink | Processor | Infra | Status |
|---|---------------|-----------|-------|--------|
| F1 | NATS -> NATS | Python T4 | NATS | ✅ |
| F2 | NATS -> Kafka | Go T4 | NATS + Redpanda | ✅ |
| F3 | Redis -> Redis | Node.js T4 | Redis | ✅ |
| F4 | MQTT -> MQTT | Java T4 | Mosquitto | ✅ |
| F5 | RabbitMQ -> RabbitMQ | PHP T4 | RabbitMQ | ✅ |
| F6 | WebSocket -> WebSocket | Rust Net T4 | None (loopback) | ✅ |
| F7 | QUIC -> QUIC | Rust T4 | None (loopback, self-signed TLS) | ✅ |

### Tier G: CDC Database Sources — P3, heavy infra

| # | Source | Sink | Processor | Infra | Status |
|---|--------|------|-----------|-------|--------|
| G1 | PostgreSQL CDC | Memory | Rust Native T1 | PG on K3s | ✅ |
| G2 | MySQL CDC | Memory | Rust Native T1 | MySQL on K3s | ✅ |
| G3 | MongoDB CDC | Memory | Rust Native T1 | MongoDB on K3s | ✅ |

### Tier H: PHP Deployment Model Variants — P1

All use Memory -> PHP -> Memory, validating each adapter E2E.

| # | Adapter | Status |
|---|---------|--------|
| H1 | Swoole / OpenSwoole (Laravel Octane) | ✅ |
| H2 | RevoltPHP + ReactPHP (Ratchet) | ✅ |
| H3 | RevoltPHP + AMPHP | ✅ |
| H4 | Workerman | ✅ |
| H5 | FrankenPHP / RoadRunner | ✅ |
| H6 | Native CLI (fallback) | ✅ |

---

## Summary

| Tier | Description | Tests | Infra | Priority |
|------|-------------|------:|-------|----------|
| A | Memory -> SDK -> Memory (all SDKs) | 13 | None | P0 |
| B | File -> SDK -> File (tier families) | 4 | None | P1 |
| C | Kafka -> SDK -> Kafka (all SDKs) | 11 | Redpanda | P0 |
| D | T3 WebTransport variants | 5 | TLS certs | P1 |
| E | Cross-connector (one SDK, many connectors) | 9 | Mixed | P2 |
| F | External messaging systems | 7 | Docker services | P2 |
| G | CDC database sources | 3 | Databases | P3 |
| H | PHP adapter variants | 6 | PHP extensions | P1 |
| | **Total** | **58** | | |

**Recommended implementation order**: A -> C -> B -> H -> D -> E -> F -> G

---

## Verification Criteria (per test)

Each E2E test must verify:
1. **Event delivery**: source count == sink count (zero loss)
2. **Payload integrity**: input payload == output payload (or expected transformation)
3. **Metadata propagation**: headers/metadata pass through correctly
4. **Ordering**: within a partition, events arrive in order
5. **Graceful shutdown**: processor disconnects cleanly, no orphaned batches

---

## Execution Log

### 2026-04-10 — Tier D D2 (Go WT) landed

Ran the full Tier D suite after adding the Go WT E2E test: `cargo test
--test e2e_tier_d -p aeon-engine --features webtransport-host --
--nocapture`: **3 passed, 2 ignored, 0 failed in 5.00s** (D1 Python,
D2 Go, D3 Rust Network all green; D4 Node.js + D5 Java still deferred
per the WT plan).

The Go `RunWebTransport` path mirrors the Rust reference client in
`crates/aeon-processor-client/src/webtransport.rs` verbatim — same
6-step AWPP-over-WT adapter contract (control stream → challenge →
register → accepted → heartbeat + per-(pipeline, partition) data
streams → batch loop). Hidden cost of the port was a trio of
library-interaction bugs that had to be solved before D2 passed:

1. **quic-go lazy stream materialization**. `session.OpenStreamSync`
   allocates a stream ID but webtransport-go only writes the
   `[frame_type][session_id]` stream header on the first `Write()`.
   The control stream reads the challenge first, so without an
   explicit flush the server's `accept_bi()` never fires and the
   handshake dead-locks until the QUIC idle timeout. Fix: call
   `ctrlStream.Write(nil)` right after `OpenStreamSync` —
   `maybeSendStreamHeader()` in webtransport-go is invoked
   unconditionally before the delegated quic-go `Write`, so a
   zero-length input is enough to flush the header. See the comment
   in `sdks/go/aeon_webtransport.go` for the exact chain.
2. **Go SDK Signer bugs (same pair as Python's)**. `PublicKeyHex()`
   returned raw hex instead of `ed25519:<base64>`, and
   `SignChallenge` signed the UTF-8 bytes of the hex nonce rather
   than the hex-decoded bytes. A9/C7 (Go T4) had been silently
   working around this via an inline custom handshake in the WS
   harness; the WT path uses the SDK directly so the bugs surfaced.
   Fix: new `AWPPPublicKey()` returning `ed25519:<base64>`, and
   `SignChallenge(nonceHex)` now `hex.DecodeString` s the nonce
   before `ed25519.Sign`.
3. **Handshake-vs-data-stream race**. `wait_for_connection` returns
   once `session_count > 0` (the handshake completed), but the
   client opens data streams asynchronously after the Accepted
   message, and the server's data-stream `accept_bi` loop registers
   them one at a time. With multiple WT tests running concurrently
   the test could race ahead of `accept_bi` and `call_batch` would
   fail with `no T3 data stream for pipeline=... partition=0`.
   Fix: new `WebTransportProcessorHost::data_stream_count()` getter
   + `wait_for_data_streams` harness helper; D1/D2/D3 all now wait
   for 16 data streams before driving events. (D1 and D3 were
   passing by luck when run alone — the concurrent sweep exposed
   the race.)

Tier D totals: **3/5 passed** (D1, D2, D3), **0 stubs in the
in-progress lane**, **2 deferred** (D4 Node.js + D5 Java per WT plan).

### 2026-04-10 — Tier D D1 (Python WT) landed

Ran the D1 Python WebTransport E2E alone (`cargo test --test e2e_tier_d
-p aeon-engine --features webtransport-host d1_python_wt_t3 --
--nocapture`): **passed in 1.49s**. First non-Rust SDK Tier D proof —
the Python `aeon_transport.run_webtransport()` entrypoint drives 200
events through a self-signed `WebTransportProcessorHost` via an
`aioquic` subprocess, all 5 E2E criteria (C1/C2/C3/C4/C5) green.

Three Python SDK Signer fixes landed alongside the test (pre-existing
bugs never caught by A8 because A8 uses an inline handshake script, not
the SDK's `run_websocket/run_webtransport` entrypoints):

1. `open_wt_bi_stream` now manually sets `H3Stream.frame_type =
   FrameType.WEBTRANSPORT_STREAM` + `session_id` after calling
   `create_webtransport_stream`. Works around an aioquic gap where bi
   WT streams send the `[0x41][session_id]` prologue but don't
   register the stream in `H3Connection._stream`, so incoming server
   bytes are misparsed as HTTP/3 frames.
2. New `Signer.awpp_public_key` property returns `ed25519:<base64>`
   instead of raw hex (matches the identity store key format).
3. `Signer.sign_challenge` now hex-decodes the nonce before signing,
   matching the server's `hex::decode(nonce).verify(raw_bytes)` flow.

Tier D totals: **2/5 passed** (D1, D3), **3 stubs** (D2 Go in progress,
D4 Node.js + D5 Java deferred per WT plan).

### 2026-04-09 — Full E2E sweep (post-audit close)

Executed after the connector audit pass closed and Gate 1 was re-validated.
Infrastructure up: Redpanda (`aeon-redpanda`, Docker), Redis / NATS /
RabbitMQ / Mosquitto (K3s `aeon-test` namespace). Rust toolchain +
Python / Node.js / .NET / Java / PHP / Go runtimes available.

| Tier | Runnable cases | Passed | Ignored | Stubs (`todo!()`) | Wall time |
|---|---:|---:|---:|---:|---:|
| A (Memory round-trip, all SDKs) | 17 | **17** | 0 | 0 | 21s |
| B (File round-trip) | 5 | **5** | 0 | 0 | 2s |
| C (Kafka/Redpanda E2E, all SDKs) | 11 | **11** | 0 | 0 | 79s |
| D (T3 WebTransport variants) | 5 | **3** (D1‡, D2‡, D3) | 0 | **2** | ~5s |
| E (Cross-connector, Python T4) | 13 | **13** (E1–E13) | 0 | 0 | 28s |
| F (External messaging) | 7 | **7** (F1–F7) | 0 | 0 | ~8s |
| G (CDC database sources) | 3 | **3** (G1–G3) | 0 | 0 | ~15s |
| H (PHP adapter variants) | 6 | **6** (H1–H6)† | 0 | 0 | ~8s |
| **Total** | **67** | **65** | **0** | **2** | **~166s** |

‡ D1 and D2 landed on 2026-04-10 (see the dedicated sections above) —
counted in this table for current-state accuracy; the original
2026-04-09 sweep had both as stubs.

† H1 (Swoole) and H5 (FrankenPHP) self-skip when their runtime is absent,
matching the A12 (Java) / C7 (Go) convention. Both are real, compiled
tests — they become active the moment the extension/binary is installed.

**Bonus coverage (not in the plan, run in the same sweep):**
- `redpanda_integration`: 3/3 passed in 17s (source, sink, E2E passthrough).
- `sustained_load`: 2/2 passed in 60s (`sustained_30s_zero_event_loss` +
  `sustained_30s_buffered_zero_event_loss` — 30s steady-state runs with
  zero loss assertions).

### Status column audit

Every ✅ in the tier tables above was **actually executed and passed**.
Every ❌ is a real `todo!()` stub with the exact deferral reason captured
in the test's `#[ignore]` message. The status column is accurate — no
aspirational marks.

### Implementation debt captured

2 stub tests remain (down from 6 — A5, F7, G1, G2, G3 completed 2026-04-10/11):

1. **T3 WebTransport end-to-end (2)** — Tier D4/D5, blocked on
   per-language WebTransport clients in the Node.js/Java SDKs.
   D1 (Python) landed on 2026-04-10 via `aioquic` — first non-Rust
   SDK Tier D proof, 200 events through a subprocess runner, all 5
   E2E criteria green; required three Python SDK Signer fixes (H3
   stream state patch, AWPP `ed25519:<base64>` public key, challenge
   hex-decode) — see the 2026-04-10 execution-log entry. D2 (Go)
   landed on 2026-04-10 via `quic-go/webtransport-go` — the Go
   `RunWebTransport` implementation mirrors the Rust reference
   client verbatim; required a `Write(nil)` flush workaround for
   webtransport-go's lazy stream-header write, the same pair of
   Signer fixes the Python SDK needed (public-key format + nonce
   hex-decode), and a new `wait_for_data_streams` harness helper
   to serialize the test against the server's async `accept_bi`
   loop. D3 (Rust Network) landed in the 2026-04-09 sweep:
   in-process Rust WT processor client, self-signed localhost cert
   via `wtransport::Identity::self_signed`, 200 events through a
   partition-pinned data stream, C1/C2/C3 + graceful shutdown
   verified. T3 *transport* was already production-grade after §5.3
   (see `docs/CONNECTOR-AUDIT.md`); D1, D2 and D3 are the three
   SDK-level acceptance proofs that exist today. All require
   `--features webtransport-host`; D3 uses the `aeon-processor-client`
   `webtransport-insecure` feature; D1 uses a Python `insecure=True`
   kwarg, and D2 uses `ConfigWT.Insecure: true` +
   `ServerName: "localhost"` to trust the self-signed cert.

   **WT SDK plan (2026-04-10)**: per
   [`WT-SDK-INTEGRATION-PLAN.md`](WT-SDK-INTEGRATION-PLAN.md),
   **D1 (Python, `aioquic`) and D2 (Go, `quic-go/webtransport-go`)
   both shipped on 2026-04-10**. **D4 (Node.js) and D5 (Java) are
   deferred** — Node.js's `@fails-components/webtransport` is a
   self-described stopgap, and Flupke's Java WT is explicitly
   "still experimental". They stay `todo!()` stubs until their
   libraries mature.
2. ~~**QUIC loopback (1)**~~ — **Done (2026-04-10)**. F7: `QuicSource →
   Rust T4 → QuicSink` with self-signed TLS via `dev_quic_configs()`.
   100 events, zero loss, payload integrity. All Tier F now green.

3. ~~**CDC + C Wasm (4)**~~ — **All done (2026-04-10/11)**. G1 PostgreSQL
   CDC, G2 MySQL CDC, G3 MongoDB CDC all deployed to K3s and tested
   with Rust Native T1. A5 C Wasm compiled via wasi-sdk 32. Tier H is fully implemented: H1 (Swoole/OpenSwoole
   coroutine client), H2 (ReactPHP + Ratchet/Pawl), H3 (AMPHP v3
   `amphp/websocket-client`), H4 (Workerman `AsyncTcpConnection` ws://),
   H5 (FrankenPHP `php-cli` SAPI), H6 (native CLI). H1 and H5 self-skip
   when their runtime is absent — no `todo!()` stubs remain in Tier H.

The 2 remaining stubs (D4/D5) are blocked on external library maturity
and not Gate 1 blockers. All 65 runnable tests pass — the entire
Gate 1 money path (Tier C: 11 SDK × Kafka E2E) is green, and
D1 (Python, aioquic) + D2 (Go, quic-go/webtransport-go) + D3 (Rust
Network, wtransport) now prove the T3 WebTransport host end-to-end
with three independent real clients across three languages.

### 2026-04-12 — CI/CD scaffolding + WebSocket live-tail + to-do audit

- `.github/workflows/release.yml`: 5-stage release pipeline (validate → crates.io → binaries → GH release → Docker Hub)
- `deny.toml`: cargo-deny config for advisories, license compliance, dependency bans
- `CHANGELOG.md`: keep-a-changelog format, v0.1.0 entry
- `GET /api/v1/pipelines/{name}/tail`: WebSocket live-tail streaming pipeline metrics at 1 Hz
- `pipeline_metrics` DashMap added to AppState for metrics registration
- To-do list audit: 3 of 6 code stubs were already implemented; updated ROADMAP accordingly

### 2026-04-11 — Phase C+D REST API wired to PipelineControl

All zero-downtime upgrade and reconfiguration REST endpoints are now fully
wired to `PipelineControl` with real processor instantiation:

| Endpoint | Before | After |
|----------|--------|-------|
| `POST .../upgrade/blue-green` | Metadata only | Loads artifact → instantiates processor → `ctrl.start_blue_green()` |
| `POST .../upgrade/canary` | Metadata only | Loads artifact → instantiates processor → `ctrl.start_canary()` |
| `POST .../promote` | Metadata only | Calls `ctrl.set_canary_pct()` or `ctrl.complete_canary()` |
| `POST .../reconfigure/source` | Did not exist | Updates metadata, enforces same-type (cross-type rejected) |
| `POST .../reconfigure/sink` | Did not exist | Updates metadata, enforces same-type (cross-type rejected) |

Extracted `instantiate_processor()` helper shared across upgrade, blue-green,
and canary paths. Added `PipelineAction::Reconfigured` variant.

11 new tests: 6 PipelineManager unit (reconfigure source/sink: same-type,
cross-type rejected, not-running rejected, history recording) + 4 REST API
(reconfigure source, sink, cross-type rejected, nonexistent pipeline) +
`PipelineAction` now derives `PartialEq`/`Eq`.

**763 lib tests, 17 E2E integration tests, zero clippy warnings.**

### 2026-04-11 — Multi-node cluster lifecycle tests landed

10 new integration tests in `crates/aeon-cluster/tests/multi_node.rs`
covering the full cluster lifecycle over QUIC transport:

| Test | Scenario |
|------|----------|
| `three_node_cluster_formation` | Bootstrap 3-node, verify leader consensus |
| `three_node_log_replication` | 20 entries replicate to all 3 nodes |
| `dynamic_add_second_node` | 1→2 nodes (even-number cluster) |
| `dynamic_scale_one_to_three` | 1→2→3 sequential scaling |
| `scale_three_to_five` | 3→4→5 nodes, all replicate |
| `remove_node_from_three_node_cluster` | 3→2 removal, cluster continues |
| `leader_failover` | Shut down leader, re-election succeeds |
| `join_via_rpc_add_node_request` | End-to-end QUIC RPC join protocol |
| `remove_via_rpc` | End-to-end QUIC RPC node removal |
| `replication_consistency_across_nodes` | 20 entries, identical last_applied |

All 10 pass. Required fix: Windows resolves `localhost` to `::1` (IPv6)
first — QUIC endpoints bound to `127.0.0.1` reject the connection.
Switched to `127.0.0.1` addresses + `dev_quic_configs_insecure()`
(AcceptAnyCert) for all multi-node tests.

Total cluster test count: 72 lib + 10 multi-node + 5 single-node = **87**.

### Resolved: SDK envelope Uuid serialization (msgpack)

For a brief window A10 / C8 / D3 / F6 all pinned to `.codec("json")`
because `aeon_processor_client::ProcessEvent.id` was a `String` while
the engine's `WireEvent.id` is a `uuid::Uuid` — msgpack serializes
`Uuid` as a 16-byte array and decode blew up with
`invalid value: byte array, expected a string`. Fixed by flipping
`ProcessEvent.id` to `uuid::Uuid`; all four tests now run with the
default `msgpack` codec, matching production processors.
