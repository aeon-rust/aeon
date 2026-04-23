# Aeon — Competitive Positioning

> **Honest framing, not marketing.** Aeon is pre-Gate-2 (2026-04-23).
> Throughput targets below are *targets*, not verified ceilings. Session B
> on AWS EKS will establish the real numbers. Until then, any comparison
> that cites an Aeon ceiling number is overstating the case.
>
> **Security / compliance audit landed 2026-04-23.** The S1–S10 workstream
> closed all critical and high audit findings (secrets, at-rest encryption,
> retention, GDPR erasure, inbound/outbound connector auth, audit channel,
> SSRF hardening, compliance-regime precondition validator). See
> `docs/ROADMAP.md` §Security for the complete landing table.

---

## 1. What Aeon Is (and Isn't)

**Aeon is a real-time stream processing engine** — Rust-native, pipeline-first, cluster-native, with a verifiable event chain (PoH / Merkle / MMR / Ed25519) built into the hot path.

**Aeon is not:**
- A CDC replication tool (use Debezium, Quest SharePlex, or similar).
- A managed ELT service (use Fivetran, Airbyte Cloud, or similar).
- A message broker (use Kafka/Redpanda, NATS, or MQTT brokers).
- A data warehouse or query engine (use ClickHouse, DuckDB, Snowflake, etc.).
- A general-purpose orchestrator (use Airflow, Dagster, Prefect, etc.).

Comparisons that mix these categories are not apples-to-apples. Section 3 below calls out the correct category for each common "vs" question.

---

## 2. Category Map

| Product | Category | Same category as Aeon? |
|---|---|---|
| **Apache Flink** | Stream processor | ✅ Direct competitor |
| **Arroyo** | Stream processor (SQL-first, Rust) | ✅ Direct competitor |
| **Apache Spark Streaming** | Micro-batch processor | ✅ Adjacent — different latency model |
| **Materialize** | Streaming SQL database | ⚠️ Overlap only on SQL workloads |
| **RisingWave** | Streaming SQL database | ⚠️ Overlap only on SQL workloads |
| **Quest SharePlex** | Oracle CDC replication | ❌ Different layer (source for Aeon) |
| **Debezium** | CDC source connectors | ❌ Different layer (source for Aeon) |
| **Fivetran** | Managed ELT SaaS | ❌ Different latency model (batch/micro-batch) |
| **Airbyte** | OSS ELT + managed | ❌ Different latency model |
| **EMQX / Mosquitto / HiveMQ** | MQTT broker | ❌ Different layer (source for Aeon) |
| **Kafka / Redpanda** | Durable log / broker | ❌ Different layer (source/sink for Aeon) |
| **NATS / JetStream** | Messaging + light streaming | ⚠️ Overlap on simple transforms only |

---

## 3. Head-to-Head — Stream Processors

### 3.1 vs Apache Flink

| Dimension | Aeon (target) | Flink (verified) |
|---|---|---|
| Runtime | Rust + Wasm guests | JVM (Java/Scala) |
| Per-event overhead (target) | <100 ns | ~1–10 µs |
| Throughput per node | 6.5M eps (floor, no-Kafka path, DOKS) | 1–5M eps/node typical |
| Latency | Sub-ms target | 10–100 ms typical |
| Delivery semantics | EO-2 (Aeon-native durability spine, tiered per sink) | Exactly-once via Chandy-Lamport checkpoints |
| Windowing / CEP | ⛔ Not yet | ✅ Mature (tumbling, sliding, session, CEP patterns) |
| SQL interface | ⛔ Not yet | ✅ Flink SQL (mature) |
| State backend | L1 DashMap + L2 mmap + L3 redb/RocksDB, tiered | RocksDB or HashMap |
| Consensus | openraft (always-on) | JobManager HA via ZK / K8s |
| Verifiable event chain | ✅ PoH + Merkle + MMR + Ed25519 | ❌ |
| At-rest encryption | ✅ AES-256-GCM per-segment DEK across L2 body + L3 checkpoints | ⚠️ Filesystem / backend only |
| Secret management | 🟡 Provider trait + dual KEK + envelope encryption + rotation shipped. Env / DotEnv / Literal providers shipped; Vault / OpenBao / KMS / SM adapters in-flight in `aeon-secrets` crate (task #35) | ❌ Config-file driven |
| Compliance regime enforcement | ✅ `validate_compliance` gate at pipeline start (PCI-DSS / HIPAA / GDPR) | ❌ Operator responsibility |
| GDPR right-to-erasure | ✅ Subject-id extraction + tombstone store + null-receipt export + deny-list | ❌ Manual / external tooling |
| Inbound connector auth | ✅ IP allow-list + API-key + HMAC + mTLS (per push source) | ⚠️ Operator responsibility |
| Outbound connector auth | ✅ Bearer / Basic / API-key / HMAC-sign / mTLS (per sink) | ⚠️ Connector-specific |
| Audit log channel | ✅ Separated from data-path tracing (tamper-resistant) | ❌ Application-level only |
| SSRF hardening | ✅ External-URL resolver with allow/deny + private-IP rejection | ❌ |
| Retention / redaction | ✅ Configurable L2/L3 retention + payload-never-in-logs tracing redaction | ⚠️ Manual |
| Ecosystem / connectors | 4 active (Memory, Blackhole, Stdout, Kafka) | 100+ |
| Operational complexity | Medium (pre-v0.1, evolving) | **High** (well-documented pain point) |
| Maturity | Pre-v0.1 | 10+ years, widely deployed |
| License | Apache-2.0 | Apache-2.0 |

**Where Aeon competes credibly today:**
- Workloads that need event-chain auditability (regulated data, financial, provenance-critical).
- Teams allergic to JVM ops overhead.
- Use cases where the per-event budget is <1 µs.
- Regulated workloads that need **compliance-regime preconditions enforced at pipeline start** (PCI-DSS / HIPAA / GDPR) rather than after-the-fact operator review.
- Pipelines that need **GDPR right-to-erasure with cryptographic null-receipts** as a first-class engine feature, not a bolt-on.
- Deployments adopting the provider-abstracted secret model — envelope encryption + dual KEK + KEK rotation are shipped; Vault / OpenBao / KMS / SM backends are in-flight in the `aeon-secrets` adapter crate.

**Where Flink still wins:**
- Anything requiring rich windowing or CEP.
- Teams that need SQL-first authoring.
- Organizations with existing Flink expertise.
- Broad connector ecosystem requirements.
- Production-proven at scale (Uber, Netflix, Alibaba).

### 3.2 vs Arroyo

| Dimension | Aeon | Arroyo |
|---|---|---|
| Runtime | Rust | Rust |
| Authoring model | Pipeline YAML + native/Wasm processors | SQL-first |
| State | Tiered L1/L2/L3 | State backend with checkpointing |
| Cluster-native | openraft always-on | K8s-native |
| Verifiable chain | ✅ | ❌ |
| Ecosystem | Small | Growing |
| License | Apache-2.0 | Apache-2.0 |
| Backing | Solo / small team | Arroyo Systems (VC-backed) |

**Where Aeon competes:** crypto-chain is the only meaningful differentiator today. Everything else is catch-up.

**Where Arroyo wins:** SQL-first dev experience, cloud-native packaging, commercial backing.

### 3.3 vs Materialize / RisingWave

These are **streaming SQL databases** — they materialize incremental views, not general-purpose stream processors. If the user's workload is "keep a SQL view fresh," those tools are a better fit than Aeon. Aeon is better for "apply arbitrary transform or native/Wasm processor code, maintain per-pipeline resource isolation, emit to arbitrary sinks."

---

## 4. Adjacent Categories — Where Aeon Does Not Compete

### 4.1 vs CDC Tools (Debezium, Quest SharePlex)

CDC tools tail database transaction logs. They are a **source type** Aeon should eventually consume, not compete with. A future Aeon CDC source connector is on the post-Gate-2 ecosystem expansion roadmap.

**Use CDC tools when:** Oracle→Oracle, Postgres→Postgres, or DB→warehouse replication with transactional fidelity is the end goal.

**Use Aeon + a CDC source when:** you want to apply arbitrary transforms (native or Wasm) on the change stream before it lands elsewhere, or emit to multiple downstream sinks.

### 4.2 vs Managed ELT (Fivetran, Airbyte)

ELT SaaS ingests from 400+ SaaS sources into a data warehouse, usually in minutes-to-hours batches. Aeon is a different latency/architecture model entirely.

**Use Fivetran when:** you need SaaS connectors (Salesforce, HubSpot, Stripe, etc.) into a warehouse with zero ops.

**Use Aeon when:** you're already in the real-time world (Kafka, MQTT, etc.) and need sub-ms transforms between systems.

### 4.3 vs MQTT Brokers (EMQX, Mosquitto, HiveMQ)

MQTT brokers do pub/sub for IoT/M2M. EMQX's rule engine does trivial transforms. Aeon is a downstream processor, not a broker.

**Use EMQX when:** IoT pub/sub is the workload.

**Use Aeon + EMQX** (post-Gate-2) when: you need real stream processing on MQTT-sourced telemetry.

### 4.4 vs Brokers (Kafka, Redpanda, NATS)

Brokers are durable logs. They are Aeon's sources and sinks, not competitors.

---

## 5. Aeon's Actual Wedge (Honest Version)

The pitch, broadened after the 2026-04-23 security/compliance audit close:

> **"Flink-class stream processing with a verifiable event chain, compliance enforcement at pipeline start, and Rust-class per-event overhead."**

Three things Aeon ships today that no established stream processor does natively:

1. **Verifiable event chain** on the hot path:
   - PoH (Proof of History) for deterministic event ordering
   - Merkle tree per partition for per-batch verification
   - MMR (Merkle Mountain Range) for chain-wide proofs
   - Ed25519 signatures for event-level authenticity

2. **Compliance-regime enforcement at pipeline start** (`compliance_validator::validate_compliance`):
   - PCI-DSS / HIPAA / GDPR regimes as first-class declarative config
   - Gate refuses to start pipelines missing required encryption / retention / erasure preconditions
   - Not "operator responsibility" and not a linter — a runtime precondition

3. **GDPR right-to-erasure + right-to-export** as engine primitives:
   - Subject-id extractor on ingest
   - Tombstone store + deny-list enforcement across L2 body and L3 checkpoints
   - Cryptographic null-receipt on erase — the chain still verifies
   - Configurable retention per-tier

These are backed by an at-rest encryption layer (AES-256-GCM per-segment DEK, dual-KEK envelope encryption, Vault/KMS/SM secret providers), an audit log channel separated from data-path tracing, per-source inbound auth (IP allow-list / API-key / HMAC / mTLS), per-sink outbound auth (Bearer / Basic / API-key / HMAC-sign / mTLS), and SSRF hardening on every external-URL resolver.

Target audience for this broadened wedge:
- **Regulated finance** — exchange surveillance, audit-trail-critical flows, PCI-DSS cardholder pipelines.
- **Healthcare** — HIPAA pipelines needing provenance + at-rest encryption + subject-id erasure as engine features.
- **Public-sector / compliance** — any pipeline where "how do we know this event wasn't tampered, and how do we prove subject X is erased" are both real questions.
- **EU data-protection workloads** — GDPR Article 17 erasure with cryptographic proof of null-receipt.
- **Blockchain-adjacent / Web3** — settlement pipelines, oracle networks, off-chain compute with on-chain proofs.

This wedge is defensible because retrofitting any one piece (crypto chain, compliance gate, erasure-with-null-receipt) into JVM-based Flink or SQL-based Arroyo would require a hot-path rewrite; shipping all three together would require a ground-up security rethink they are unlikely to undertake.

---

## 6. Known Gaps Before Aeon Can Credibly Pitch Against Flink/Arroyo

| Gap | Effort | Notes |
|---|---|---|
| **Session B throughput ceiling on AWS EKS** | Session B | Current numbers are DOKS floor, not ceiling — need NVMe + 10 Gbps to cite a real number |
| **SQL authoring interface** | Large (6–12 months) | Both Flink and Arroyo have this; Aeon doesn't |
| **Windowing / CEP primitives** | Revised: F1+F2 ~3 months together (see [`WINDOWING-WATERMARKS-DESIGN.md`](WINDOWING-WATERMARKS-DESIGN.md)) | Flink's differentiator; Aeon has none built-in. Watermark shrinks to 3–4 wks by reusing the `AckSeqTracker` pattern; windowing itself is the bulk of the work. |
| **Connector ecosystem (pull + push + SaaS)** | Continuous | 4 today → target 20+ for credible demos |
| **Benchmarks vs competitors on identical hardware** | Medium | Same infrastructure, same workload, verifiable numbers |
| **Production deployments outside of dev/test** | Whenever first user lands | No case study yet |
| **Documentation / tutorials / cookbook** | Medium | Scale-out docs exist; SDK docs are strong; lacks "write your first processor" funnel |
| **Direct PKCS#11 / HSM integration** | Deferred | FIPS-track claims are already satisfied via cloud KMS (AWS/GCP/Azure all FIPS 140-3 L3) and Vault/OpenBao-with-HSM-seal. Direct PKCS#11 only needed for air-gapped shops where Vault/OpenBao is not an option — trait stub (S1.4) is in place; real backend deferred until a customer asks. |
| **`aeon-secrets` adapter crate (Vault / OpenBao / KMS / SM backends)** | Medium | `SecretProvider` trait + Env / DotEnv / Literal providers ship today; production backends are trait slots. Task #35 delivers the adapter crate with Vault + OpenBao (API-compatible) as reference; AWS / GCP / Azure KMS + SM backends follow behind their own feature flags. |
| **Secret-provider production hardening** | Medium | Rotation runbook, error taxonomy, rate-limit handling across Vault / OpenBao / AWS KMS / AWS SM / GCP KMS (task #36) |

**Gaps closed by the 2026-04-23 audit landing (no longer on this list):**
- Secrets-in-config-files risk — closed by S1 (provider abstraction + envelope encryption)
- At-rest encryption of durability tiers — closed by S3 (AES-256-GCM per-segment DEK)
- Payload leakage into logs — closed by S2 (tracing redaction layer)
- SSRF via external-URL sources — closed by S7 (URL resolver hardening)
- Audit findings S8 (source_kind / on_ack_callback / RocksDB gate) — closed
- Inbound push-source auth — closed by S9 (IP allow-list + API-key + HMAC + mTLS)
- Outbound sink auth — closed by S10 (Bearer / Basic / API-key / HMAC-sign / mTLS) — aeon-cli factory wiring continuation in flight
- WebSocket / WebTransport mTLS TLS-layer — closed by task #34 (WS rustls `Connector`, WT `mtls_client_config_from_signer` helper, WT source post-handshake subject extraction)
- Configurable retention for L2 body + L3 ack — closed by S5
- GDPR subject-id + erasure + export — closed by S6
- Compliance regime precondition validator — closed by S4.2
- Audit log channel separation — closed by S2.5

---

## 7. One-Line Summary

**Aeon's pitch (today, honestly):**
> A Rust-native stream processing engine with a verifiable event chain, compliance regime enforcement at pipeline start, first-class GDPR erasure primitives, and a tiered state model — targeting Flink's correctness guarantees without JVM ops overhead, for workloads where event provenance, regulatory enforcement, and per-event latency all matter.

**Aeon's pitch (after Session B + SQL + windowing):**
> Drop-in alternative to Flink with cryptographic audit trails and compliance enforcement built in.

---

*See also:* [`ARCHITECTURE.md`](ARCHITECTURE.md) · [`ROADMAP.md`](ROADMAP.md) · [`GATE2-ACCEPTANCE-PLAN.md`](GATE2-ACCEPTANCE-PLAN.md) · [`EO-2-DURABILITY-DESIGN.md`](EO-2-DURABILITY-DESIGN.md) · [`WINDOWING-WATERMARKS-DESIGN.md`](WINDOWING-WATERMARKS-DESIGN.md) · [`SECURITY.md`](SECURITY.md) · [`COMPLIANCE.md`](COMPLIANCE.md)
