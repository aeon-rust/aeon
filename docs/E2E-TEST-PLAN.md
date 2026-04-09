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
| A5 | C/C++ | T2 | Memory -> C_Wasm -> Memory | ❌ |
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

| # | Processor | Test | Status |
|---|-----------|------|--------|
| D1 | Python | Memory -> Python_WT -> Memory | ❌ |
| D2 | Go | Memory -> Go_WT -> Memory | ❌ |
| D3 | Rust Network | Memory -> RustNet_WT -> Memory | ❌ |
| D4 | Node.js | Memory -> NodeJS_WT -> Memory | ❌ |
| D5 | Java | Memory -> Java_WT -> Memory | ❌ |

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
| F1 | NATS -> NATS | Python T4 | NATS | ❌ |
| F2 | NATS -> Kafka | Go T4 | NATS + Redpanda | ❌ |
| F3 | Redis -> Redis | Node.js T4 | Redis | ❌ |
| F4 | MQTT -> MQTT | Java T4 | Mosquitto | ❌ |
| F5 | RabbitMQ -> RabbitMQ | PHP T4 | RabbitMQ | ❌ |
| F6 | WebSocket -> WebSocket | Rust Net T4 | None (loopback) | ✅ |
| F7 | QUIC -> QUIC | Go T3 | None (loopback) | ❌ |

### Tier G: CDC Database Sources — P3, heavy infra

| # | Source | Sink | Processor | Infra | Status |
|---|--------|------|-----------|-------|--------|
| G1 | PostgreSQL CDC | Kafka | Rust Native T1 | PG + Redpanda | ❌ |
| G2 | MySQL CDC | Memory | Python T4 | MySQL | ❌ |
| G3 | MongoDB CDC | Memory | Node.js T4 | MongoDB | ❌ |

### Tier H: PHP Deployment Model Variants — P1

All use Memory -> PHP -> Memory, validating each adapter E2E.

| # | Adapter | Status |
|---|---------|--------|
| H1 | Swoole / OpenSwoole (Laravel Octane) | ❌ |
| H2 | RevoltPHP + ReactPHP (Ratchet) | ❌ |
| H3 | RevoltPHP + AMPHP | ❌ |
| H4 | Workerman | ❌ |
| H5 | FrankenPHP / RoadRunner | ❌ |
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
