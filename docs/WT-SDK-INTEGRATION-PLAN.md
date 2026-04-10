# WebTransport (T3) SDK Integration Plan

**Status**: Active plan — as of 2026-04-10 (D1 Python shipped)
**Scope**: Extending real T3 WebTransport client support into the
per-language processor SDKs, beyond `aeon-processor-client` (Rust, D3)
and `sdks/python` (aioquic, D1) — the two SDKs that ship a real T3
client today.
**Owner**: Aeon engine + SDK workstream
**Related docs**:
- `docs/ROADMAP.md` — Phase 12b Language SDKs + Language SDK Status tables
- `docs/E2E-TEST-PLAN.md` — Tier D (T3 WebTransport variants)
- `docs/ARCHITECTURE.md` §3 (core traits), §5 (tier model)
- `docs/FOUR-TIER-PROCESSORS.md` — T1/T2/T3/T4 definitions
- `docs/CONNECTOR-AUDIT.md` §5.3, §4.6 — transport-level WT status
- `crates/aeon-processor-client/src/webtransport.rs` — canonical
  reference implementation of the AWPP-over-WT adapter

---

## 1. Why this plan exists

Before 2026-04-10 the `Phase 12b Language SDKs` table in ROADMAP marked
every language SDK as `T3 + T4`. A 2026-04-10 audit of the SDK source
trees revealed that **only `aeon-processor-client` (Rust) has a real T3
WebTransport implementation**. Every other SDK (Python, Go, Node.js,
Java, C#/.NET, C/C++, PHP) is T4 WebSocket-only. The `T3 + T4` column
was aspirational on those rows. ROADMAP has since been corrected
(2026-04-10, commits `d6ca69e` / `d688137`) to show shipped vs pending
T3 per SDK.

Filling the gap is not a uniform exercise. Each target language has
its own WebTransport library situation, and at least three of the seven
have **no production-grade client-side WT implementation available at
all as of 2026-04-10**. A single tracking doc is needed so that the
decision about which SDKs to attempt, which to defer, and which to
return to later is made once, written down, and can be re-evaluated
on a cadence instead of relitigated every session.

This plan captures:

1. The library maturity audit (per language, with sources).
2. The approved sequencing and deferral decisions.
3. The shared AWPP-over-WT adapter contract every SDK must implement.
4. Per-SDK detailed plans (library, deps, files, risks, gates).
5. The documentation update pattern.
6. Re-evaluation triggers for deferred SDKs.

---

## 2. Current state (2026-04-10)

### 2.1 T3 shipped today

| SDK | Crate / path | Library | Tier D E2E |
|---|---|---|---|
| Rust Network (`aeon-processor-client`) | `crates/aeon-processor-client/` | `wtransport 0.6.1` | ✅ D3 (2026-04-10) |
| Python (`sdks/python`) | `sdks/python/aeon_transport.py` | `aioquic` (H3 + WT) | ✅ D1 (2026-04-10) |
| Go (`sdks/go`) | `sdks/go/aeon_webtransport.go` | `quic-go/webtransport-go 0.9.0` | ✅ D2 (2026-04-10) |

**Canonical reference implementation**:
`crates/aeon-processor-client/src/webtransport.rs`.
Every non-Rust SDK WT client should be a faithful port of the same
6-step pattern (see §4).

### 2.2 T3 pending / deferred

| SDK | Status after this plan | Reason |
|---|---|---|
| Java | **Deferred** (revisit when Flupke WT leaves experimental) | `Flupke` explicitly flags WT support as "still experimental (draft-ietf-webtrans-http3-13)" |
| Node.js | **Deferred** (last-attempt language) | `@fails-components/webtransport` README self-describes as "duct tape until Node ships native WT" |
| C#/.NET | **Deferred** (parallel to PHP) | No client-side WT in .NET today; tracked for .NET 11 (dotnet/runtime#43641, dotnet/aspnetcore#65406); only experimental Kestrel server-side exists |
| C/C++ | **Deferred** (parallel to PHP) | No first-party WT client library; `quiche` #1114 open; `msquic` is QUIC-only; `libwtf` is too new |
| PHP | **Deferred** (pre-existing decision) | Already deferred for all 6 deployment models — no usable PHP WT client library |

Approved sequencing is therefore **Python → Go**, with Java/C/C++/C#/Node.js
held until their library situations mature. **Python and Go both
landed 2026-04-10** (D1 + D2 E2E green).

---

## 3. WebTransport library maturity audit

Research was conducted via WebSearch on 2026-04-10 across 8 parallel
queries covering each target language. Each row below states the
verdict, the candidate library, evidence, and the maturity tier.

**Maturity tiers**:
- **Tier 1 — Production-ready**: multi-year track record, 1.x+ release,
  extensive test coverage, WT support explicitly documented as stable.
- **Tier 2 — Active, usable**: actively maintained, 0.x release OK,
  WT is a real feature (not a toy), has users in production.
- **Tier 3 — Experimental**: WT support exists but is flagged
  experimental or follows an old draft spec; may work for a demo but
  not a release we'd ship.
- **Tier 4 — Not available**: no usable client-side WT library exists,
  or the only option is a research prototype / unmaintained.

### 3.1 Python — Tier 2

- **Library**: [`aioquic`](https://github.com/aiortc/aioquic)
  (CPython, async, BSD-3) + optional
  [`PyWebTransport`](https://pypi.org/project/pywebtransport/) wrapper.
- **Evidence**: `aioquic.h3.connection.H3Connection.enable_webtransport`
  is documented and used in production by aiortc ecosystem. Still 0.x
  but multi-year release history. `PyWebTransport` is a newer
  higher-level convenience wrapper on top of it.
- **Verdict**: **GO** — this is the most mature open-source WT client
  across all our target languages after Rust. `aioquic` is the right
  dependency (avoid the extra layer of `PyWebTransport` until we need
  it).

### 3.2 Go — Tier 2

- **Library**: [`github.com/quic-go/webtransport-go`](https://github.com/quic-go/webtransport-go)
  built on `quic-go` (the same QUIC stack most of the Go cloud-native
  ecosystem uses).
- **Evidence**: Latest release 2026-01-12; WT is a first-class feature;
  API is stable enough that HTTP/3 proxies and CDNs use it.
- **Verdict**: **GO** — de-facto standard. 0.x but the only game in
  town and well-maintained.

### 3.3 Java — Tier 3 (experimental)

- **Library**: [`kwik`](https://github.com/ptrd/kwik) (QUIC) +
  [`Flupke`](https://github.com/ptrd/flupke) (HTTP/3 + WT on top of
  kwik).
- **Evidence**: Both are actively maintained by the same author (Peter
  Doornbosch). Flupke's README **explicitly flags WebTransport as
  "still experimental (draft-ietf-webtrans-http3-13)"**. The JDK has no
  native HTTP/3 client yet; Netty's incubator HTTP/3 codec exists but
  does not provide a WT client layer.
- **Verdict**: **DEFER** — per the user's directive, if Java's support
  is not stable we defer it the way PHP is deferred. Flupke's own
  documentation places it in Tier 3. Revisit when Flupke releases a
  WT stable milestone or when Netty ships WT support.

### 3.4 Node.js — Tier 3 (explicitly experimental by its own maintainer)

- **Library**: [`@fails-components/webtransport`](https://github.com/fails-components/webtransport).
- **Evidence**: Author's own README describes the package as a stopgap
  "until Node ships native WT". The project is real and usable for
  demos but has a small maintainer base, native addon compilation
  requirements on multiple platforms, and a history of breaking-change
  releases.
- **Verdict**: **DEFER** — the user originally preferred Node.js **first**
  in the general SDK priority order, but for WT specifically flipped it
  to **last** because of the library wobble. We hold until either
  `@fails-components/webtransport` stabilises a 1.0 or Node.js core
  ships a native WT client (tracked upstream; no ETA).

### 3.5 C#/.NET — Tier 4 (no usable client-side WT)

- **Library**: none first-party. `System.Net.Quic` is QUIC-only;
  client-side WebTransport is **not implemented**.
- **Evidence**: [dotnet/runtime#43641](https://github.com/dotnet/runtime/issues/43641)
  tracks WebTransport support in .NET (still open); planned for .NET
  11+. [dotnet/aspnetcore#65406](https://github.com/dotnet/aspnetcore/issues/65406)
  tracks the ASP.NET side. Kestrel has an experimental server-side WT
  implementation (draft-02, no datagrams), but that's server-only —
  a processor SDK needs client-side.
- **Verdict**: **DEFER** — parallel to PHP. This was a reversal from
  my initial (incorrect) recommendation to start with .NET; WebSearch
  revealed the actual state. C#/.NET is now the **last** language we
  will attempt via the native stack; before .NET 11 ships we would
  have to either (a) host a Rust shim via P/Invoke or (b) wait. Either
  approach is outside this plan's current scope.

### 3.6 C/C++ — Tier 4 (no usable client-side WT)

- **Library**: no first-party option. Candidates examined:
  - [`cloudflare/quiche`](https://github.com/cloudflare/quiche) —
    WebTransport tracked in open issue
    [#1114](https://github.com/cloudflare/quiche/issues/1114); not
    implemented.
  - [`microsoft/msquic`](https://github.com/microsoft/msquic) —
    QUIC-only, not WT.
  - `libwtf` — third-party, very new, not evaluated as
    production-grade.
- **Verdict**: **DEFER** — parallel to PHP. Revisit when `quiche`
  #1114 closes or when a production-grade C/C++ WT client library
  emerges.

### 3.7 PHP — Tier 4 (pre-existing decision)

- Already deferred for all 6 deployment models (Swoole, RevoltPHP+ReactPHP,
  RevoltPHP+AMPHP, Workerman, FrankenPHP/RoadRunner, Native CLI). No
  usable PHP WT client library exists. T4 WebSocket remains the
  shipped path (12b-13).
- **Verdict**: **DEFER** (unchanged).

### 3.8 Summary table

| Language | Library | Maturity tier | Decision |
|---|---|---|---|
| Rust (Network) | `wtransport` 0.6.1 | Tier 2 | ✅ Shipped (D3) |
| Python | `aioquic` | Tier 2 | ✅ Shipped (D1, 2026-04-10) |
| Go | `quic-go/webtransport-go` | Tier 2 | ✅ Shipped (D2, 2026-04-10) |
| Java | Flupke on kwik | Tier 3 (explicitly experimental) | Defer |
| Node.js | `@fails-components/webtransport` | Tier 3 (self-described stopgap) | Defer |
| C#/.NET | (none) — waits on .NET 11 | Tier 4 | Defer |
| C/C++ | (none) — `quiche` #1114, `msquic` QUIC-only | Tier 4 | Defer |
| PHP | (none) | Tier 4 | Defer (pre-existing) |

---

## 4. AWPP-over-WT adapter contract

Every non-Rust SDK WT client must be a faithful port of
`crates/aeon-processor-client/src/webtransport.rs`. The engine host
(`crates/aeon-engine/src/transport/webtransport_host.rs`) is already
production-grade — it treats the client as a black box that speaks
AWPP. The 6-step pattern below is the black-box contract.

### 4.1 Six-step pattern

1. **Connect** to `https://{host}:{port}` from `ProcessorConfig.url`.
   Use self-signed-accepting mode in tests, native certs in production.
2. **Open ONE control stream** (bidirectional). Perform the AWPP
   handshake: read `Challenge`, write `Register` (with ED25519
   `challenge_signature`), read `Accepted` or `Rejected`.
   All control-stream messages are **length-prefixed JSON**:
   `[4B len LE][utf8 JSON]`.
3. **Spawn a heartbeat task** that writes `Heartbeat` messages on the
   control stream at `Accepted.heartbeat_interval_ms`.
4. **For each (pipeline, partition) in `Accepted.pipelines`**: open a
   NEW bidirectional data stream, write the **routing header**
   `[4B name_len LE][name bytes][2B partition LE]`, then enter a
   read→process→write loop on length-prefixed **batch wire frames**.
5. **Wire format per `crates/aeon-engine/src/batch_wire.rs`**:

   ```
   Request:  [8B batch_id LE][4B count LE][per-event: 4B len LE + WireEvent bytes]
             [4B CRC32 LE]
             [64B ED25519 sig]?    ← only if Accepted.batch_signing == true
   Response: [8B batch_id LE][4B count LE][per-output: 4B len LE + WireOutput bytes]
             [4B CRC32 LE]
             [64B ED25519 sig]?    ← only if Accepted.batch_signing == true
   ```

   Each `WireEvent` / `WireOutput` is encoded using `Accepted.transport_codec`
   — **MsgPack by default**, JSON as fallback. Note that
   `uuid::Uuid`'s serde impl branches on `is_human_readable()`, so
   ensure the SDK's UUID decode path treats msgpack UUIDs as 16-byte
   arrays and JSON UUIDs as strings (this is what broke Rust until
   2026-04-10 — see ROADMAP 2026-04-10 note).

6. **Graceful shutdown**: when any data stream task ends (connection
   close, error, drain), tear down the remaining data-stream tasks and
   cancel the heartbeat task. Return cleanly.

### 4.2 Contract invariants

- **Control stream is long-lived**. The client does NOT open a new
  control stream per batch. The engine's WT host spawns exactly one
  `WtControlChannel` per session and expects it to stay up.
- **Data streams are long-lived**. One bidirectional stream per
  (pipeline, partition). The client writes the routing header **once**
  at stream open, then frames requests/responses forever.
- **Length framing is 4-byte little-endian** everywhere: control
  messages, data batch frames, and the name-length inside the routing
  header. Partition IDs are 2-byte LE.
- **Heartbeats are JSON on the control stream**, not on data streams.
- **Batch signing is negotiated in the `Accepted` message** — don't
  assume always-on or always-off.
- **CRC32 is IEEE**. Standard `crc32fast` / `zlib`-compatible
  polynomial. Covers `[batch_id][count][events]`, excludes the CRC and
  signature bytes themselves.

### 4.3 Why this contract matters

The first WT client write (`aeon-processor-client` before commit
`f8cf41f`) violated the "data streams are long-lived" invariant by
opening a stream per batch. The server accepts one stream per (pipeline,
partition) and waits on `accept_bi`, so both sides deadlocked. That bug
cost several hours. **Any new SDK WT client MUST open one data stream
per (pipeline, partition) and reuse it**, full stop. This is the single
most important invariant of the contract.

---

## 5. Per-SDK detailed plans

Each plan lists: library, new dependencies, files to touch, risks,
validation gates, and the Tier D test id that blocks on it. Follow
the shared contract in §4 verbatim.

### 5.1 Python (D1) — ✅ SHIPPED (2026-04-10)

- **Library**: `aioquic` (0.9.x+).
- **New dep** (`sdks/python/setup.py`): add `aioquic>=0.9` as an
  extras requirement under `extras_require={"webtransport": [...]}`
  so existing T4-only users aren't forced to pull QUIC.
- **Files to touch**:
  - `sdks/python/aeon_transport.py` — add `run_webtransport()` and
    `run_webtransport_batch()` entrypoints next to `run_websocket()`.
    Reuse the existing `wire_encode_batch_response`/`wire_decode_batch_request`
    helpers (they're codec-agnostic).
  - `sdks/python/test_transport.py` — add unit tests for:
    - Routing header encoding (matches `[4B name_len LE][name][2B part LE]`).
    - Length-prefixed read/write helpers.
    - AWPP handshake JSON shape.
    - End-to-end against a mock `aioquic` server instance in-process.
- **Risks**:
  - `aioquic` is async-first; ensure it plays well with the decorator
    pattern Python SDK users already know from `run_websocket()`. Plan:
    expose both sync and async wrappers (mirroring WS).
  - TLS: production users need `ssl.create_default_context()` with a
    real cert chain; tests need a `verify_mode=ssl.CERT_NONE`
    escape hatch gated by an explicit `insecure=True` kwarg. Mirror
    the `webtransport-insecure` cargo feature in Rust.
- **Validation gates**:
  - `python -m pytest sdks/python/test_transport.py -k webtransport`
    all green.
  - Rust Tier D D1 test green against a Python subprocess runner.
- **Tier D test**: D1 (`crates/aeon-engine/tests/e2e_tier_d.rs`).
- **Landed 2026-04-10**: D1 passes in ~1.5s. Three Python SDK Signer
  fixes landed alongside the test (pre-existing bugs never caught by
  A8, which uses an inline handshake script rather than the SDK's
  `run_*` entrypoints):
  1. `open_wt_bi_stream` manually patches `H3Stream.frame_type =
     FrameType.WEBTRANSPORT_STREAM` + `session_id` after
     `create_webtransport_stream`. Works around an aioquic gap where
     bi WT streams send the `[0x41][session_id]` prologue on the wire
     but don't register in `H3Connection._stream`, so incoming server
     bytes are misparsed as HTTP/3 frames.
  2. New `Signer.awpp_public_key` property returns `ed25519:<base64>`
     to match the identity store key format (raw hex was rejected
     with `KEY_NOT_FOUND`).
  3. `Signer.sign_challenge` now hex-decodes the nonce before signing
     — server verifies against raw bytes via `hex::decode(nonce)`.
  The Rust side of D1 spawns a Python subprocess via
  `run_webtransport(insecure=True, server_name="localhost")` (mirror
  of the Rust `webtransport-insecure` feature) and drives 200 events
  through partition 0. All 5 E2E criteria (C1–C5) verified.

### 5.2 Go (D2) — ✅ SHIPPED (2026-04-10)

- **Library**: `github.com/quic-go/webtransport-go` v0.9.0 (pulls
  `github.com/quic-go/quic-go` v0.53.0 transitively). Pinned in
  `sdks/go/go.mod`; `go 1.23` required.
- **Files**: new `sdks/go/aeon_webtransport.go` (~430 lines, full
  client) + new `sdks/go/aeon_webtransport_test.go` (4 wire-helper
  unit tests). Engine side: new `go_wt_passthrough_project` helper
  in `crates/aeon-engine/tests/e2e_wt_harness.rs`, new
  `wait_for_data_streams` in the same harness + new
  `WebTransportProcessorHost::data_stream_count()` getter. Test
  body: `crates/aeon-engine/tests/e2e_tier_d.rs` `d2_go_wt_t3`.
- **Entry point**:
  `aeon.RunWebTransport(ConfigWT{URL, Name, Version, PrivateKeyPath,
  Pipelines, Codec, Processor|BatchProcessor, Insecure, ServerName})`.
  `ConfigWT.Insecure` + `ConfigWT.ServerName` are the test-mode
  escape hatches for self-signed certs; both default to `false` /
  `""` (no insecure mode in production). Mirrors the Python kwargs
  of the same name and the Rust `webtransport-insecure` feature.
- **Landing summary**: the Go implementation mirrors the Rust
  reference client verbatim (same 6-step AWPP-over-WT contract).
  Three library-interaction bugs had to be solved before D2 passed:
  1. **quic-go lazy stream materialization**. `OpenStreamSync`
     allocates a stream ID but webtransport-go only writes the
     `[frame_type][session_id]` prologue on the first `Write()`.
     Fix: call `ctrlStream.Write(nil)` right after `OpenStreamSync`
     — the webtransport-go wrapper calls `maybeSendStreamHeader()`
     unconditionally before the delegated quic-go `Write`, so a
     zero-length input flushes the header. See the comment in
     `sdks/go/aeon_webtransport.go` citing the exact lines in both
     libraries.
  2. **Go SDK Signer bugs (same pair as Python's)**: `PublicKeyHex`
     returned raw hex instead of `ed25519:<base64>`, and
     `SignChallenge` signed the UTF-8 bytes of the hex nonce rather
     than the hex-decoded bytes. Fix: new `AWPPPublicKey()` method
     and `SignChallenge(nonceHex string) (string, error)` that
     `hex.DecodeString`-s before signing. A9/C7 (the Go T4 tests)
     keep passing because they use an inline custom handshake
     instead of the SDK's `Signer` methods.
  3. **Handshake-vs-data-stream race**: `wait_for_connection` only
     tracks `session_count`, but data streams are opened
     asynchronously after the Accepted message. Fix: new
     `wait_for_data_streams(expected, timeout)` harness helper
     backed by `WebTransportProcessorHost::data_stream_count()`;
     D1/D2/D3 all now wait for 16 data streams before driving
     events.
- **Tier D test**: D2 — all 5 E2E criteria (C1–C5) green in ~5s.
- **Test counts after landing**: 22 Go SDK tests (up from 18),
  `go build`/`go test`/`go vet` clean.

### 5.3 Java (D5) — DEFER

- **Library (candidate)**: Flupke + kwik.
- **Reason for deferral**: Flupke's own README flags WT support as
  "still experimental (draft-ietf-webtrans-http3-13)". The user's
  directive is explicit: "if Java's support is not stable too, defer
  Java too, when implementing webtransport support in those programming
  languages, the way we have deferred webtransport for PHP". Flupke's
  "still experimental" label triggers the defer clause.
- **Revisit trigger**: Flupke publishes a WT stable milestone, or Netty
  HTTP/3 incubator adds a WT client layer, or an entirely new JVM WT
  client emerges (the JDK itself is unlikely to ship WT soon).
- **Tier D test**: D5 remains a `todo!()` stub.

### 5.4 Node.js (D4) — DEFER

- **Library (candidate)**: `@fails-components/webtransport`.
- **Reason for deferral**: the maintainer's own README describes it as
  a stopgap, native addon compilation requirements across platforms
  (esp. Windows, where the user primarily develops), and history of
  breaking 0.x releases.
- **Revisit trigger**: `@fails-components/webtransport` reaches 1.0 OR
  Node.js core lands a native WT client (upstream discussion ongoing;
  no ETA).
- **Note**: despite the user's general preference for Node.js as top
  SDK priority, WT specifically is deferred. See
  `~/.claude/projects/.../memory/feedback_wt_sdk_sequencing.md`.
- **Tier D test**: D4 remains a `todo!()` stub.

### 5.5 C#/.NET — DEFER (parallel to PHP)

- **Library (candidate)**: none today. Waits on .NET 11+
  ([dotnet/runtime#43641](https://github.com/dotnet/runtime/issues/43641),
  [dotnet/aspnetcore#65406](https://github.com/dotnet/aspnetcore/issues/65406)).
- **Reason for deferral**: no client-side WT in .NET stack today;
  Kestrel server-side is experimental and server-only. The alternative
  (P/Invoke a Rust shim) is out of scope for this plan.
- **Revisit trigger**: `System.Net.Quic` ships WT client or ASP.NET
  Core ships a supported client WT API.
- **Tier D test**: no existing Tier D test for .NET (not in the
  original 5-test Tier D plan). When revived, D6 could be added.

### 5.6 C/C++ — DEFER (parallel to PHP)

- **Library (candidate)**: none today. `quiche` #1114 tracks WT; not
  implemented. `msquic` is QUIC-only. `libwtf` is too new to evaluate.
- **Reason for deferral**: no production-grade C/C++ WT client library.
- **Revisit trigger**: `quiche` #1114 closes with a WT implementation,
  or a new C/C++ WT library reaches production quality.
- **Tier D test**: no existing Tier D test for C/C++ (not in the
  original 5-test Tier D plan). When revived, a new row could be added.

### 5.7 PHP — DEFER (pre-existing)

- No change from current status. All 6 deployment models stay T4 WS
  only. Revisit only if a usable PHP WT client library emerges — none
  exists as of 2026-04-10.

---

## 6. Documentation update pattern (per SDK landing)

When a SDK WT client lands and its Tier D test flips ❌→✅, update docs
in this order (one focused commit bundle per SDK):

1. **Plan doc** (this file): move the SDK's row in §2.2 from
   "Implementing" to "Shipped", add a shipped-date annotation.
2. **ROADMAP.md**:
   - `Phase 12b Language SDKs` table: flip `T4` → `T3 + T4` in the
     "Tiers (shipped)" column for that SDK. Update the Notes column
     with the library used + test count.
   - `Language SDK Status` table (Outstanding Work section): flip the
     `T3 status` column to `✅ shipped (D{n} E2E YYYY-MM-DD)`.
   - `Latest updates` subsection: add a dated bullet referencing the
     commit hash and the plan doc.
3. **E2E-TEST-PLAN.md**:
   - Tier D table: flip the status of the landed row ❌→✅.
   - Execution Log: bump Tier D `Runnable`/`Passed`/`Stubs` totals.
   - Implementation debt list: remove the SDK from the "blocked on WT
     clients" bullet.

One commit per SDK landing (per the user's per-test commit cadence
preference): `feat: sdk-{lang} — T3 WebTransport client + Tier D D{n}`.
Follow-up doc-only commit if needed:
`docs: flip D{n} ✅ + update ROADMAP/plan`.

Never touch `docs/aeon-dev-notes.txt`.

---

## 7. Re-evaluation cadence

Deferred SDKs should be re-checked for library maturity at these
triggers (not on a fixed calendar):

- Any time a deferred-SDK user asks for WT support explicitly.
- When another language SDK lands and frees up bandwidth.
- When a major library release is announced (e.g.
  `@fails-components/webtransport` 1.0, Flupke WT stable, .NET 11
  preview).
- At the start of any new phase that touches SDKs broadly.

Each re-evaluation updates §2.2 and §3 of this document. Keep the
`as of YYYY-MM-DD` stamp fresh when the verdict changes.

---

## 8. Open questions

1. **Cert distribution for Tier D tests**: D3 uses
   `wtransport::Identity::self_signed(["localhost"])` and the client
   trusts it via the `webtransport-insecure` feature. For D1/D2 we
   need the Python/Go SDKs to expose a test-mode "trust self-signed
   for `localhost`" path. Plan: mirror the Rust feature — add
   `ProcessorConfig.insecure: bool` (Python kwarg, Go field), default
   `False`/`false`, set to `True`/`true` only in the Tier D harness.
2. **Subprocess harness for D1/D2**: D3 runs the WT client in-process
   because both client and engine are Rust. D1/D2 need to spawn a
   Python / Go subprocess. Proposal: mirror the A6 (Python) / A7 (Go)
   pattern — reuse those subprocess harness helpers but wire them to
   `WtTestServer` instead of `WsTestServer`. Capture stdout/stderr for
   debugging.
3. **Certificate SAN for `127.0.0.1`**: D3 binds to `127.0.0.1:0` with
   a SAN for `["localhost"]` and connects via a URL containing
   `127.0.0.1`. Windows IPv6 localhost trap means we can't just switch
   to `localhost`. The Python `aioquic` client needs the same
   treatment: connect by IPv4 address, supply the expected server
   name `"localhost"` (or whatever the cert's SAN is), and disable
   hostname verification when `insecure=True`.

---

## 9. References

- **Canonical Rust impl**: `crates/aeon-processor-client/src/webtransport.rs`
- **Engine WT host**: `crates/aeon-engine/src/transport/webtransport_host.rs`
- **Test harness**: `crates/aeon-engine/tests/e2e_wt_harness.rs`
- **Wire format**: `crates/aeon-engine/src/batch_wire.rs`
- **AWPP wire types**: `crates/aeon-processor-client/src/wire.rs`
- **Tier D spec**: `docs/E2E-TEST-PLAN.md` §Tier D
- **Original sequencing research**: WebSearch queries run 2026-04-10;
  findings summarised in §3 above.
- **Phase 12b overview**: `docs/ROADMAP.md` §"Phase 12b Language SDKs"

---

## 10. Change log

| Date | Change |
|---|---|
| 2026-04-10 | Initial version. Captures WebSearch audit, maturity tiers, approved Python→Go sequencing, Java/Node.js/C#/C/C++ deferrals. |
| 2026-04-10 | D1 (Python / aioquic) shipped. Moved §2.1 row; flipped §2.2, §3.8, §5.1 status to ✅. Captured the three Signer fixes that unblocked the handshake (H3 stream state patch, AWPP `ed25519:<base64>` public key, challenge hex-decode). |
| 2026-04-10 | D2 (Go / quic-go/webtransport-go) shipped. Added §2.1 row; removed §2.2 Go row; flipped §3.8, §5.2 to ✅ SHIPPED. Captured the three bugs solved: quic-go lazy stream materialization (`Write(nil)` flush), Go SDK Signer pair (same as Python's), handshake-vs-data-stream race (`wait_for_data_streams` helper). |
