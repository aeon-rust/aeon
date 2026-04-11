# Multi-Node Cluster & Deployment Strategy

> Discussion document: Aeon deployment models across IaaS, Kubernetes,
> and serverless platforms. Covers CPU pinning, Raft networking, and
> what works where.

---

## 1. Connector × Processor Test Coverage Audit (2026-04-11)

### Current State

**Source Connectors**: 16 implemented, 15 tested, 1 untested (WebTransportDatagramSource)
**Sink Connectors**: 13 implemented (HttpSink added), 13 tested, 0 untested
**Processor Types**: T1 (Rust native), T2 (Wasm), T3 (WebTransport), T4 (WebSocket)
**Languages**: Rust, Python, Go, Node.js, Java, C#/.NET, PHP, C/AssemblyScript

### Connector × Processor Matrix

```
                          T1 Rust    T2 Wasm    T3 WT       T4 WS
Source
├── MemorySource           A1         A2,A8      D1-D5       A3-A7,H1-H6
├── KafkaSource            C1         C2         —           C3-C11
├── FileSource             B1         —          —           B3,B4
├── HttpWebhookSource      —          —          —           E1,E9
├── RedisStreamsSource      —          —          —           F1
├── NatsSource             —          —          —           F2
├── MqttSource             —          —          —           F3
├── RabbitMqSource         —          —          —           F4
├── WebSocketSource        —          —          —           F5
├── QuicSource             —          —          —           F7
├── PostgresCdcSource      G1         —          —           —
├── MysqlCdcSource         G2         —          —           —
├── MongoDbCdcSource       G3         —          —           —
├── HttpPollingSource      E10        —          —           —
├── WebTransportSource     E12        —          —           —
└── WebTransportDgSource   ✗          ✗          ✗           ✗        ← NO TESTS (niche)

Sink
├── BlackholeSink          C1,G1-G3   C2         —           E3,E7,F1-F5
├── MemorySink             A1         A2,A8      D1-D5       A3-A7,H1-H6
├── StdoutSink             —          —          —           E4
├── KafkaSink              C1         C2         —           C3-C11,E5,E9
├── FileSink               B1         —          —           B3,B4,E2,E6
├── WebSocketSink          —          —          —           F5
├── RedisStreamsSink       —          —          —           F1
├── NatsSink               —          —          —           F2
├── MqttSink               —          —          —           F3
├── RabbitMqSink           —          —          —           F4
├── QuicSink               —          —          —           F7
├── WebTransportSink       E13        —          —           —
└── HttpSink               E11        —          —           —        ← NEW
```

### What's Proven

- **Gate 1 money path** (Kafka→Processor→Kafka): 11 tests across 7 languages, T1+T2+T4
- **All 8 SDK languages** validated via MemorySource with T4 WS (Tier A)
- **All 3 CDC sources** proven with T1 Rust (PostgreSQL, MySQL, MongoDB)
- **All 6 messaging brokers** tested in loopback (Redis, NATS, MQTT, RabbitMQ, WebSocket, QUIC)
- **Cross-connector mixing** validated in Tier E (9 combinations with Python T4)
- **PHP async models**: all 6 patterns tested (Swoole, ReactPHP, AMPHP, Workerman, FrankenPHP, Native CLI)

### Coverage Gaps (updated 2026-04-11)

| Gap | Severity | Status |
|-----|----------|--------|
| ~~HttpPollingSource — no test~~ | ~~Medium~~ | **Fixed**: E10 test |
| ~~WebTransportSource/Sink — no test~~ | ~~Low~~ | **Fixed**: E12, E13 tests |
| WebTransportDatagramSource — no test | Low | Niche unreliable datagram path; deferred |
| ~~No HttpSink exists~~ | ~~Medium~~ | **Fixed**: HttpSink + E11 test |
| Broker sources only with T4 WS | Low | Core path (Kafka) covers T1+T2; broker internals are connector-level |
| CDC only with MemorySink | Low | CDC→Kafka not tested; CDC correctness is source-side |

### Verdict

The test coverage is **strong for the primary use case** (Kafka→Processor→Kafka
with any language) and **adequate for secondary connectors** (each tested at least
once in loopback). The untested connectors (HttpPolling, raw WebTransport) are
edge cases that don't block multi-node work.

---

## 2. Multi-Node Cluster: Deployment Models

### 2.1 Bare Metal / VM (IaaS)

**How it works:**
- Each Aeon node runs as a systemd service (or equivalent) on a dedicated server
- Nodes discover each other via static seed list or DNS SRV records
- Raft consensus over QUIC (already implemented: `QuicNetworkFactory` in `aeon-cluster/src/transport/network.rs`)
- CPU pinning via `core_affinity` crate — direct hardware access, full control

**CPU pinning model** (from `aeon-engine/src/affinity.rs`):
```
Core 0: OS / runtime / Raft consensus
Core 1-3: Pipeline 0 (source, processor, sink)
Core 4-6: Pipeline 1 (source, processor, sink)
Core 7-9: Pipeline 2 (source, processor, sink)
...
```
Each partition pipeline gets 3 dedicated cores. A 32-core server handles ~10
partition pipelines with cores to spare for OS, Raft, and the REST API.

**Advantages:**
- Full control over NUMA topology, CPU pinning, hugepages
- No container overhead (no cgroup scheduling, no overlay networking)
- Predictable latency — no noisy neighbors
- Direct NIC access (kernel bypass with io_uring possible)

**Disadvantages:**
- Manual provisioning and capacity management
- Harder rolling upgrades (need orchestration tooling)
- No built-in health checks or auto-restart

**When to use:**
- Maximum performance workloads (20M+ events/sec target)
- Dedicated infrastructure with consistent hardware
- Regulated environments with strict hardware requirements

### 2.2 Kubernetes (Managed or Self-Hosted)

**How it works:**
- Aeon runs as a **StatefulSet** (not Deployment) for stable network identities
- Each pod = one Aeon node in the Raft cluster
- Headless Service for peer discovery (pod DNS: `aeon-0.aeon-headless.ns.svc.cluster.local`)
- QUIC transport between pods (UDP, needs `hostPort` or `NodePort` for cross-node)

**The CPU pinning question — does K8s pin to specific bare-metal cores?**

**Short answer: Not automatically, but you can get close.**

Kubernetes CPU management modes:
1. **Default (CFS shares)** — pods get a time-slice proportional to their CPU request.
   No pinning. The pod floats across all cores. **Not suitable for Aeon's hot path.**

2. **`static` CPU Manager policy** — when a pod requests **integer CPU** with
   Guaranteed QoS (requests == limits), kubelet assigns exclusive cores.
   ```yaml
   resources:
     requests:
       cpu: "6"      # Must be integer, not "6000m"
       memory: "8Gi"
     limits:
       cpu: "6"      # Must equal requests for Guaranteed QoS
       memory: "8Gi"
   ```
   With this config + `--cpu-manager-policy=static` on the kubelet:
   - The pod gets 6 **exclusive** physical cores (other pods cannot use them)
   - Aeon's `core_affinity::set_for_current(core)` works inside the container
     because it uses `sched_setaffinity` within the cgroup's allowed CPU set
   - The 6 cores are visible as cores 0-5 inside the container (mapped from
     the node's physical cores by the cgroup cpuset)

3. **Topology Manager** — `--topology-manager-policy=single-numa-node` ensures
   the assigned cores are all on the **same NUMA node**, preventing cross-socket
   memory access latency. Critical for Aeon's L1/L2 cache warmth strategy.

**Practical K8s configuration for Aeon:**
```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: aeon
spec:
  serviceName: aeon-headless
  replicas: 3
  template:
    spec:
      containers:
        - name: aeon
          resources:
            requests:
              cpu: "6"
              memory: "8Gi"
            limits:
              cpu: "6"
              memory: "8Gi"
          env:
            - name: AEON_NODE_ID
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name  # "aeon-0", "aeon-1", etc.
          ports:
            - containerPort: 4460
              protocol: UDP   # QUIC for Raft
            - containerPort: 4461
              protocol: TCP   # REST API
            - containerPort: 4462
              protocol: TCP   # Metrics
```

**Key insight:** On K8s, Aeon does NOT know or care which physical cores it gets.
The container sees cores 0-5 regardless of which physical cores the kubelet
assigned. `core_affinity::set_for_current(core)` pins to cgroup-relative cores.
This is correct and requires **no code changes** — the affinity module already
works in both bare-metal and containerized environments.

**What you're NOT validating on K8s vs bare-metal:**
- Cross-NUMA pinning: only matters with Topology Manager
- Hardware-specific tuning: hugepages, NIC IRQ affinity, kernel bypass
- These are performance optimizations, not correctness concerns

**Advantages:**
- Auto-healing (pod restarts on crash)
- Rolling upgrades via StatefulSet update strategy
- HPA for horizontal scaling (already implemented in Helm chart)
- Service mesh integration for mTLS, observability
- Multi-cloud portability

**Disadvantages:**
- Container overhead (~2-5% CPU for cgroup accounting)
- UDP (QUIC) across nodes requires careful networking (Calico, Cilium)
- Kubelet CPU Manager requires node-level config (not all managed K8s
  providers expose this — DigitalOcean DOKS does, EKS does, GKE does)

### 2.3 Serverless Platforms

**Can Aeon run on serverless?**

| Component | Serverless Viable? | Why |
|-----------|-------------------|-----|
| **Aeon Node (engine)** | **No** | Long-running process, Raft state, SPSC buffers, CPU pinning — fundamentally incompatible with ephemeral execution |
| **Aeon Cluster** | **No** | Raft consensus requires persistent connections and stable identities |
| **T1/T2 Processor** | **No** | Embedded in the Aeon process |
| **T3 WebTransport Processor** | **Technically possible, but impractical** | WT requires persistent QUIC connections; Lambda's 15-min timeout and cold starts (~200ms-2s) make this unreliable for real-time |
| **T4 WebSocket Processor** | **Marginal** | WS connections are long-lived; serverless platforms that support WS (API Gateway + Lambda) add 50-100ms latency per message and have concurrent connection limits |

**Where serverless DOES make sense — as an Aeon connector target:**

```
External Event Source
  → Lambda / Cloud Function (transform, enrich, filter)
    → HTTP POST to Aeon HttpWebhookSource
      → Aeon Pipeline (processor → sink)

OR:

Aeon Pipeline
  → HttpSink (future) / WebSocket / Webhook
    → Lambda / Cloud Function (post-processing, notification, fan-out)
```

Serverless is useful as a **glue layer around Aeon**, not as a host for Aeon
components:

1. **Ingest gateway**: Lambda behind API Gateway receives webhooks from third
   parties (Stripe, GitHub, Twilio), validates/transforms, forwards to Aeon's
   HttpWebhookSource or writes to Kafka/Redpanda topic.

2. **Fan-out sink**: Aeon writes to an SQS queue or SNS topic, Lambda
   consumers fan out to email, SMS, Slack, database writes, etc.

3. **Schema validation**: Lambda validates incoming payloads against a schema
   registry before they enter the Aeon pipeline.

4. **CDC trigger**: Lambda triggered by DynamoDB Streams or Aurora CDC,
   writes events to Kafka topic that Aeon consumes.

**Warmed-up Lambda as a webhook endpoint for Aeon:**
- Provisioned concurrency eliminates cold starts
- Response time: 5-15ms (warm) vs 200ms-2s (cold)
- Works well as an Aeon HttpWebhookSource target
- Cost-effective for bursty, low-throughput webhook ingestion
- NOT suitable for sustained high-throughput (> 10K events/sec) due to
  per-invocation cost and concurrency limits

### 2.4 Hybrid Architecture (Recommended for Production)

```
┌─────────────────────────────────────────────────────────┐
│  External Sources                                        │
│  (Webhooks, APIs, IoT)                                  │
│                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │ Lambda/CF    │  │ Lambda/CF    │  │ Direct Push  │  │
│  │ (validation) │  │ (transform)  │  │ (SDK client) │  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  │
│         │                  │                  │          │
│         ▼                  ▼                  ▼          │
│  ┌──────────────────────────────────────────────────┐   │
│  │              Kafka / Redpanda Cluster             │   │
│  │         (or direct HTTP/WS to Aeon)               │   │
│  └───────────────────────┬──────────────────────────┘   │
│                           │                              │
│  ┌────────────────────────▼─────────────────────────┐   │
│  │            Aeon Cluster (3-5 nodes)               │   │
│  │                                                    │   │
│  │   ┌─────────┐  ┌─────────┐  ┌─────────┐          │   │
│  │   │ aeon-0  │  │ aeon-1  │  │ aeon-2  │          │   │
│  │   │ (leader)│◄─┤(follower│◄─┤(follower│          │   │
│  │   │         │  │)        │  │)        │          │   │
│  │   └────┬────┘  └────┬────┘  └────┬────┘          │   │
│  │        │ QUIC/Raft   │            │               │   │
│  │        └─────────────┴────────────┘               │   │
│  │                                                    │   │
│  │   Partitions distributed across nodes:            │   │
│  │   aeon-0: P0, P3, P6    (3 pipelines × 3 cores)  │   │
│  │   aeon-1: P1, P4, P7    (3 pipelines × 3 cores)  │   │
│  │   aeon-2: P2, P5, P8    (3 pipelines × 3 cores)  │   │
│  └──────────────────────┬───────────────────────────┘   │
│                          │                               │
│  ┌───────────────────────▼──────────────────────────┐   │
│  │              Sink Targets                         │   │
│  │  Kafka, Database, S3, HTTP, NATS, Redis, ...     │   │
│  │                                                    │   │
│  │  ┌──────────────┐  ┌──────────────┐               │   │
│  │  │ Lambda/CF    │  │ Lambda/CF    │               │   │
│  │  │ (fan-out)    │  │ (alerts)     │               │   │
│  │  └──────────────┘  └──────────────┘               │   │
│  └──────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

---

## 3. Kubernetes Multi-Node: What Needs to Change

### 3.1 Already Implemented

- **Raft consensus**: openraft with `MemLogStore` + `StateMachineStore`
- **QUIC transport**: `QuicNetworkFactory` + `QuicNetworkConnection` in `aeon-cluster/src/transport/`
- **Multi-node tests**: `aeon-cluster/tests/multi_node.rs` — 3-node Raft leader election + log replication (in-process, loopback QUIC)
- **CPU affinity**: `aeon-engine/src/affinity.rs` — best-effort, works in containers
- **Helm chart**: Deployment (single-node) + StatefulSet (cluster) + headless Service + HPA
- **PoH chain continuity**: Designed for partition transfer between nodes
- **K8s peer discovery**: `discovery.rs` — pod name → node ID, headless Service DNS, env var parsing
- **`aeon serve` command**: REST API server entrypoint for containerized deployment

### 3.2 What's Missing for Real Multi-Node

| Component | Status | Work Needed |
|-----------|--------|-------------|
| ~~`ClusterNode` uses `StubNetworkFactory`~~ | **Done (P4b)** | `bootstrap_multi()` uses `QuicNetworkFactory` |
| ~~Helm StatefulSet~~ | **Done (P4c)** | StatefulSet + headless Service, dual-mode chart |
| ~~Peer discovery~~ | **Done (P4d)** | DNS-based via headless svc + `from_k8s_env()`, 8 tests |
| ~~Helm deploy on K3s~~ | **Done (P4e)** | `aeon serve`, pod Running 1/1, REST API validated |
| Partition assignment | Stub | Raft-based partition→node mapping (currently hardcoded) |
| PoH chain transfer | Designed | `PohChain::resume()` exists but transfer protocol untested |
| Checkpoint replication | Partial | `CheckpointWriter` writes locally; needs Raft replication |
| Cross-node QUIC | Tested (loopback) | Needs real network testing (separate pods/VMs) |

### 3.3 DigitalOcean DOKS Cluster Plan

For real multi-node validation, a 3-node DOKS cluster:

**Recommended node spec:**
- **Node pool**: 3× `s-4vcpu-8gb` ($48/mo each = $144/mo total)
  - 4 vCPU per node → 1 core OS + 1 pipeline (3 cores: source, proc, sink)
  - Or: 3× `c-8` (8 dedicated vCPU, $80/mo) for 2 pipelines per node
- **CPU-Optimized** nodes recommended for Aeon workloads (c-series, not s-series)
- DOKS supports `--cpu-manager-policy=static` via worker node config

**What to validate on real cluster:**
1. Raft leader election across 3 pods (separate nodes via pod anti-affinity)
2. Partition assignment: leader assigns partitions, followers start pipelines
3. Node failure: kill aeon-1, verify partition reassignment to surviving nodes
4. PoH chain transfer: partition moves from node A to node B, chain continues
5. Split-brain: network partition between nodes, verify Raft handles it
6. Sustained load: 30s→5min→1hr across 3 nodes with Redpanda (multi-broker)
7. CPU pinning verification: `taskset -pc <pid>` confirms exclusive cores
8. Cross-node QUIC latency: measure Raft RPC round-trip between pods

---

## 4. Serverless Integration Points

### 4.1 What Aeon Should NOT Be

Aeon is a **stateful, long-running, CPU-pinned stream processor**. It is
architecturally incompatible with serverless execution models because:

- Raft consensus requires persistent peer connections
- SPSC ring buffers require stable memory
- CPU pinning requires dedicated cores
- PoH chains require sequential, uninterrupted execution
- Pipeline state (checkpoints, delivery ledgers) lives in-process

### 4.2 What Aeon CAN Integrate With

Serverless functions work as **edge adapters** around the Aeon pipeline:

**Inbound (serverless → Aeon):**
| Pattern | How |
|---------|-----|
| Webhook relay | Lambda receives webhook → validates → POSTs to Aeon HttpWebhookSource |
| Event bridge | Lambda triggered by SQS/SNS → writes to Kafka topic → Aeon KafkaSource |
| CDC bridge | Lambda triggered by DynamoDB Streams → writes to Aeon-consumed topic |
| IoT gateway | Lambda@Edge processes IoT payloads → forwards to Aeon MQTT/WebSocket |

**Outbound (Aeon → serverless):**
| Pattern | How |
|---------|-----|
| Fan-out | Aeon KafkaSink → topic → Lambda consumer → email/SMS/Slack |
| Post-processing | Aeon writes to S3 (future FileSink variant) → S3 trigger → Lambda |
| Alerting | Aeon HttpSink (future) → Lambda → PagerDuty/Opsgenie |

**Processor-as-serverless (NOT recommended):**

While technically a T4 WebSocket processor could be a Lambda behind API Gateway
WebSocket, this defeats Aeon's latency guarantees:

```
Without serverless:  Event → WS to processor → response    ~1-5ms
With serverless:     Event → API GW → Lambda cold start →   ~200ms-2s
                     Lambda warm → process → API GW → Aeon   ~50-100ms
```

The 50-200ms overhead per event at the processor layer is unacceptable for
real-time stream processing. Processors must be co-located (T1/T2) or
connected via persistent connections (T3/T4) to dedicated compute.

### 4.3 Future: HttpSink for Serverless Integration

An `HttpSink` connector would be the natural bridge from Aeon to serverless:

```rust
pub struct HttpSink {
    client: reqwest::Client,
    endpoint: String,     // e.g., Lambda function URL
    batch_mode: bool,     // Send batch as JSON array vs individual POSTs
    retry_policy: RetryPolicy,
    auth: HttpAuth,       // Bearer, API key, SigV4
}
```

This would enable: `KafkaSource → Processor → HttpSink → Lambda Function URL`
without needing a Kafka topic as an intermediary.

---

## 5. Deployment Decision Matrix

| Deployment Model | Single-Node Aeon | Multi-Node Cluster | Processors |
|-----------------|------------------|-------------------|------------|
| **Bare Metal / VM** | Best performance. systemd service. Full CPU/NUMA control. | Best for 20M/s target. Static seed discovery. | T1/T2 embedded, T3/T4 co-located on same or adjacent servers |
| **Kubernetes** | Deployment (current Helm). Good for dev/staging. | StatefulSet + headless svc. `cpu-manager-policy=static`. Pod anti-affinity. | T1/T2 embedded, T3/T4 as sidecar or separate pod |
| **Managed K8s (DOKS, EKS, GKE)** | Easy. Use CPU-optimized nodes. | Same as K8s. Verify kubelet CPU manager is enabled. | Same as K8s |
| **Serverless** | **No.** | **No.** | **No** (as serverless functions). Use as edge adapters only. |
| **Hybrid** | Aeon on K8s/VM + serverless edge adapters | Recommended for production. Best of both worlds. | T1/T2 embedded, T3/T4 on dedicated compute, Lambda for fan-out |

---

## 6. Execution Plan — Local-First, Then Cloud

Strategy: maximize work on Rancher Desktop (single-node K3s on Windows)
before provisioning cloud. Avoids unplanned costs from development delays.

### Phase 1: Local (Rancher Desktop) — see ROADMAP.md P4a-P4e, P7

| Step | Roadmap | What | Validates |
|------|---------|------|-----------|
| 1 | P4a | Dockerfile + container image | `docker run aeonrust/aeon aeon --version` |
| 2 | P4b | Wire QuicNetworkFactory into ClusterNode | `multi_node.rs` tests pass with real QUIC |
| 3 | P4c | StatefulSet + headless Service Helm templates | `helm template --dry-run` clean |
| 4 | P4d | Peer discovery module (DNS SRV + static seed) | Unit tests with mock DNS |
| 5 | P4e | Full Helm deploy→run→verify cycle on K3s | Single-pod pipeline via REST API |
| 6 | P7a | HttpPollingSource E2E test | Polling + backoff logic validated |
| 7 | P7b | WebTransportSource/Sink E2E tests | Raw WT stream path validated |
| 8 | P7c | HttpSink connector | Aeon→webhook/Lambda fan-out path |

### Phase 2: Cloud (DigitalOcean DOKS) — see ROADMAP.md P4f

Triggered when cloud access is provisioned. All code is ready from Phase 1.

| Step | What | Validates |
|------|------|-----------|
| 1 | `helm install` with `replicas: 3` | 3-node Raft leader election |
| 2 | Partition assignment via Raft | Leader distributes partitions |
| 3 | Kill aeon-1 pod | Partition reassignment to survivors |
| 4 | PoH chain transfer | Chain continues on new node |
| 5 | Network partition test | Split-brain recovery |
| 6 | Multi-broker Redpanda sustained load | 30s→5min→1hr across 3 nodes |
| 7 | CPU pinning validation | `taskset -pc` confirms exclusive cores |
| 8 | Cross-node QUIC latency | Raft RPC round-trip measurement |

### Cloud infrastructure needed (when ready)

- 3-node DOKS cluster: `c-8` (8 dedicated vCPU, $80/mo each = $240/mo)
  - Or `c-4` (4 dedicated vCPU, $40/mo each = $120/mo) for initial validation
- Managed Redpanda (Redpanda Cloud) or self-hosted 3-broker on same cluster
- Estimated cloud cost: $120-$300/mo during active development
- Can spin down between sessions to minimize cost

---

## 7. Zero-Downtime Deployment — Summary & Status (updated 2026-04-11)

Full to-do list with item IDs: `docs/PROCESSOR-DEPLOYMENT.md` Section 13.

### 7.1 What Works Today (No Code Changes Needed)

| Scenario | How | Notes |
|----------|-----|-------|
| T3 WebTransport processor replacement | New processor connects, old disconnects, routing table auto-updates | External process — Aeon doesn't restart |
| T4 WebSocket processor replacement | Same as T3 | External process — Aeon doesn't restart |
| Pipeline config via REST API | `POST /pipelines`, `POST /pipelines/{name}/start`, etc. | All CRUD endpoints working |
| TLS certificate rotation | `CertificateStore::reload()` re-reads PEM files | Combine with cert-manager auto-renewal |

### 7.2 What Needs Code Changes (Before or During Cloud Phase)

| Priority | Items | What It Enables |
|----------|-------|-----------------|
| ~~**Must fix**~~ | ~~ZD-1 (POST route), ZD-2 (CLI serde), ZD-3 (SHA-512)~~ | ~~Processor registration via CLI/REST, artifact integrity~~ **Done (2026-04-11)** |
| ~~**High**~~ | ~~ZD-4 (hot-swap orchestrator)~~ | ~~Zero-downtime Wasm and Native .so processor upgrades~~ **Done (2026-04-11)** — `PipelineControl` + `run_buffered_managed()` |
| ~~**Medium**~~ | ~~ZD-5 (blue-green runtime), ZD-6 (canary traffic split)~~ | ~~Advanced upgrade strategies~~ **Done (2026-04-11)** — Blue-green shadow + cutover, canary probabilistic split via `PipelineControl` |
| ~~**Medium**~~ | ~~ZD-7, ZD-8 (same-type source/sink reconfig)~~ | ~~Connector config changes without pipeline restart~~ **Done (2026-04-11)** — `drain_and_swap_source()`/`drain_and_swap_sink()` via `Box<dyn Any>` downcast |
| **Low** | ZD-9 (cross-type via blue-green pipeline), ZD-10 (batch replay), ZD-11 (Wasm state), ~~ZD-12 (file watcher)~~ **Done**, ZD-13 (child process) | All deferred — edge cases, no user demand. See `PROCESSOR-DEPLOYMENT.md` §13.4 |

### 7.3 Deployment Environments — What Changes Where

| Environment | Processor Deploy | Source/Sink Config Change | Aeon Binary Upgrade |
|-------------|-----------------|--------------------------|---------------------|
| **VM / bare metal** | T2: `aeon deploy` → drain→swap (~1ms). T3/T4: reconnect. T1 .so: `dlclose`/`dlopen`. | Drain→swap via `PipelineControl` (ZD-7/8 done). | Rolling restart via systemd. Blue-green with LB (manual). |
| **Kubernetes (single)** | Same as VM, via REST API or `aeon deploy` | Same. K8s restarts pod if config changes. | Rolling update via Deployment. |
| **Kubernetes (cluster)** | Same, but registry is Raft-replicated across nodes | Same. Leader coordinates via Raft. | StatefulSet rolling update (one pod at a time). Raft handles leader re-election. |
| **AWS ECS** | Same as K8s single-node. Task re-deployment for binary updates. | Same. ECS service update. | ECS rolling deployment or blue-green via ALB. |
| **Docker Compose** | Same as VM. `docker exec aeon aeon deploy ...` | Same. `docker compose restart aeon`. | `docker compose pull && docker compose up -d`. |
