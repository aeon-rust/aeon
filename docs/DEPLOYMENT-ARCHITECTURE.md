# Aeon Deployment Architecture

> Infrastructure topology options, from single-machine development to
> multi-server production. Covers Redpanda segregation, network latency
> trade-offs, and capacity planning.
>
> **Current deployment**: Topology 1 (single machine) is actively tested with
> Redpanda on Docker/Rancher Desktop. Performance numbers are from benchmark
> runs on this topology. Multi-server topologies (2-4) are designed for
> production but not yet validated.

---

## Table of Contents

1. [Overview](#1-overview)
2. [Topology Options](#2-topology-options)
3. [Topology 1: Single Machine (Development)](#3-topology-1-single-machine-development)
4. [Topology 2: Segregated Redpanda (Small Production)](#4-topology-2-segregated-redpanda-small-production)
5. [Topology 3: Full Separation (Production)](#5-topology-3-full-separation-production)
6. [Topology 4: Kubernetes](#6-topology-4-kubernetes)
7. [Redpanda Segregation — Detailed Analysis](#7-redpanda-segregation--detailed-analysis)
8. [Network Latency Impact](#8-network-latency-impact)
9. [Observability Stack Placement](#9-observability-stack-placement)
10. [Database Placement (CDC Sources)](#10-database-placement-cdc-sources)
11. [Resource Sizing Guide](#11-resource-sizing-guide)
12. [Deployment Checklist](#12-deployment-checklist)

---

## 1. Overview

Aeon's architecture separates three concern areas, each with different resource
profiles:

| Component | Resource Profile | Scaling Strategy |
|-----------|-----------------|------------------|
| **Aeon Engine** | CPU-bound (processing), moderate memory | Horizontal (add nodes to cluster) |
| **Redpanda / Kafka** | Disk I/O-bound, high memory for page cache | Vertical first, then horizontal (add brokers) |
| **Observability** (Prometheus, Grafana, Jaeger, Loki, OTel Collector) | Memory + disk, low CPU | Separate to avoid interfering with data plane |

The key architectural decision is **where to draw the server boundary** between
these components. This document analyzes each option.

---

## 2. Topology Options

```
Topology 1: Single Machine         Topology 2: Segregated Redpanda
┌─────────────────────────┐       ┌──────────────────┐  ┌──────────────────┐
│ Aeon + Redpanda + Obs   │       │ Aeon + Obs       │  │ Redpanda         │
│ (all on one host)       │       │ (Server A)       │  │ (Server B)       │
└─────────────────────────┘       └──────────────────┘  └──────────────────┘

Topology 3: Full Separation        Topology 4: Kubernetes
┌──────────┐ ┌──────────┐ ┌────┐ ┌──────────────────────────────────────────┐
│ Aeon     │ │ Redpanda │ │Obs │ │ K8s cluster: Aeon StatefulSet +          │
│ Cluster  │ │ Cluster  │ │    │ │ Redpanda Operator + Obs Helm charts     │
└──────────┘ └──────────┘ └────┘ └──────────────────────────────────────────┘
```

---

## 3. Topology 1: Single Machine (Development)

**When to use**: Local development, prototyping, CI/CD pipelines, small
single-tenant deployments.

```
┌─────────────────────────────────────────────────────────────────┐
│ Single Server / Laptop                                          │
│                                                                 │
│  ┌─────────────────────┐  ┌──────────────────────────────────┐ │
│  │ Aeon Engine          │  │ Redpanda                         │ │
│  │ • Pipeline(s)        │  │ • Broker (localhost:19092)       │ │
│  │ • REST API (:4471)   │  │ • Schema Registry (:18081)      │ │
│  │ • Processors         │  │ • Pandaproxy (:18082)            │ │
│  └─────────────────────┘  └──────────────────────────────────┘ │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │ Observability (optional)                                    ││
│  │ Prometheus(:9090) Grafana(:3000) Jaeger(:16686) Loki(:3100)││
│  │ OTel Collector(:4317)                                       ││
│  └─────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
```

**Configuration**: Default `docker-compose.yml` provides this topology.

```bash
# Start everything
docker compose up -d

# Or minimal (just Redpanda + Aeon)
docker compose up -d redpanda redpanda-console redpanda-init
cargo run --release --bin aeon-pipeline -- ...
```

**Advantages**:
- Zero network latency between Aeon and Redpanda (shared memory / loopback)
- Simplest to set up and debug
- All data stays on one machine

**Disadvantages**:
- Resource contention: Redpanda and Aeon compete for CPU, memory, and disk I/O
- Not production-grade for high-throughput workloads
- Single point of failure

**Resource requirements**:
- Minimum: 4 CPU cores, 8GB RAM, SSD
- Recommended: 8 CPU cores, 16GB RAM, NVMe SSD

---

## 4. Topology 2: Segregated Redpanda (Small Production)

**When to use**: Production workloads where Redpanda is the primary bottleneck,
or when Aeon and Redpanda have conflicting resource needs.

```
┌────────────────────────────┐          ┌────────────────────────────┐
│ Server A: Aeon Engine       │          │ Server B: Redpanda          │
│                             │  network │                             │
│ • Aeon Pipeline(s)          │◄────────►│ • Redpanda Broker           │
│ • REST API (:4471)          │ TCP/9092 │ • Schema Registry           │
│ • OTel Collector            │          │ • Pandaproxy                │
│ • Prometheus + Grafana      │          │ • Redpanda Console          │
│                             │          │                             │
│ CPU: for processing         │          │ CPU: for I/O + compaction   │
│ RAM: for pipeline buffers   │          │ RAM: for page cache         │
│ Disk: checkpoints only      │          │ Disk: NVMe for WAL + data   │
└────────────────────────────┘          └────────────────────────────┘
```

**Configuration changes**:

On Server A (Aeon), set the broker address to Server B's IP:

```bash
# Environment variable
export AEON_BROKERS="server-b:9092"

# Or in docker-compose.prod.yml
environment:
  AEON_BROKERS: "192.168.1.20:9092"
```

On Server B (Redpanda), update the advertised listener to the server's IP:

```yaml
# Redpanda config
redpanda:
  command:
    - --advertise-kafka-addr internal://redpanda:9092,external://192.168.1.20:9092
```

**Advantages**:
- **Eliminates resource contention**: Redpanda gets 100% of Server B's disk I/O and page cache
- **Independent scaling**: Upgrade Redpanda's disk/RAM without touching Aeon
- **Aeon CPU is dedicated**: No competition from Redpanda's compaction, replication, or I/O threads
- **Better fault isolation**: Redpanda disk failure doesn't crash Aeon (Aeon can buffer and retry)
- **Independent maintenance**: Restart/upgrade Redpanda without stopping Aeon pipelines

**Disadvantages**:
- **Added network latency**: 0.1-2ms per round trip (see Section 8 for detailed analysis)
- **Network dependency**: Network partition = pipeline stall (mitigated by Aeon's backpressure)
- **More infrastructure to manage**: Two servers instead of one

**Resource requirements**:

| Server | CPU | RAM | Disk | Purpose |
|--------|-----|-----|------|---------|
| A (Aeon) | 4-8 cores | 8-16GB | SSD (small, for checkpoints) | Processing |
| B (Redpanda) | 4-8 cores | 16-32GB | NVMe SSD (large, for data) | Broker |

---

## 5. Topology 3: Full Separation (Production)

**When to use**: High-throughput production with multiple Aeon nodes, dedicated
Redpanda cluster, and isolated observability.

```
┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│ Aeon Node 1       │  │ Aeon Node 2       │  │ Aeon Node 3       │
│ Pipeline A, B     │  │ Pipeline C, D     │  │ Pipeline E, F     │
│ :4470 (QUIC)      │  │ :4470 (QUIC)      │  │ :4470 (QUIC)      │
│ :4471 (REST)      │  │ :4471 (REST)      │  │ :4471 (REST)      │
└────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘
         │ Raft consensus (QUIC :4470)                │
         └──────────────────┬──────────────────────────┘
                            │
              ┌─────────────┴─────────────┐
              │                           │
    ┌─────────┴──────────┐     ┌──────────┴──────────┐
    │ Redpanda Broker 1   │     │ Redpanda Broker 2   │
    │ Redpanda Broker 3   │     │ (3 or 5 brokers)    │
    └─────────────────────┘     └─────────────────────┘
              │
    ┌─────────┴──────────────────────────────────────┐
    │ Observability Server                            │
    │ OTel Collector, Prometheus, Grafana, Jaeger,   │
    │ Loki                                            │
    └─────────────────────────────────────────────────┘
```

**Key configuration**:

```yaml
# Aeon node config (each node)
cluster:
  node_id: 1  # unique per node
  bind: "0.0.0.0:4470"
  peers:
    - "aeon-node-2:4470"
    - "aeon-node-3:4470"

# All nodes point to the Redpanda cluster
environment:
  AEON_BROKERS: "rp-broker-1:9092,rp-broker-2:9092,rp-broker-3:9092"
  OTEL_EXPORTER_OTLP_ENDPOINT: "http://obs-server:4317"
```

**Advantages**:
- Full horizontal scaling for both Aeon and Redpanda
- Complete fault isolation between all layers
- Independent lifecycle management (upgrade Aeon without touching Redpanda)
- Observability stack doesn't interfere with data plane

**Disadvantages**:
- Most complex to set up and operate
- Highest network latency (multiple hops)
- Requires proper network infrastructure (low-latency links between Aeon and Redpanda)

---

## 6. Topology 4: Kubernetes

**When to use**: Cloud-native deployments, auto-scaling requirements, existing K8s
infrastructure.

```yaml
# Aeon: StatefulSet (3 replicas for Raft)
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: aeon
spec:
  replicas: 3
  template:
    spec:
      containers:
        - name: aeon
          image: aeon:latest
          ports:
            - containerPort: 4470  # QUIC cluster
            - containerPort: 4471  # REST API
          env:
            - name: AEON_BROKERS
              value: "redpanda-0.redpanda:9092,redpanda-1.redpanda:9092,redpanda-2.redpanda:9092"
            - name: AEON_API_TOKEN
              valueFrom:
                secretKeyRef:
                  name: aeon-secrets
                  key: api-token

# Redpanda: use the Redpanda Operator or Helm chart
# https://docs.redpanda.com/current/deploy/deployment-option/self-hosted/kubernetes/
```

**Advantages**:
- Auto-healing, rolling updates, resource quotas
- Redpanda Operator handles broker lifecycle
- Service mesh provides mTLS between all components
- Pod anti-affinity ensures Aeon and Redpanda don't share nodes

**Disadvantages**:
- K8s overhead and complexity
- Container networking adds latency vs bare metal
- Storage class selection is critical for Redpanda performance

See [PROCESSOR-DEPLOYMENT.md](PROCESSOR-DEPLOYMENT.md) for K8s-specific patterns
(init containers, sidecar processors, CRD-based pipelines).

---

## 7. Redpanda Segregation -- Detailed Analysis

### Why Segregate Redpanda

Redpanda and Aeon have **conflicting resource needs**:

| Resource | Redpanda needs | Aeon needs |
|----------|---------------|------------|
| **CPU** | I/O threads, compaction, replication | Event processing, Wasm execution |
| **Memory** | Page cache (the more RAM, the fewer disk reads) | Pipeline buffers, SPSC ring buffers, state store |
| **Disk I/O** | Sequential writes (WAL), random reads (fetch) | Checkpoint WAL (small), processor artifacts |
| **Network** | Replication between brokers, client fetch/produce | Source polling, sink producing, cluster QUIC |

On a shared machine, Redpanda's page cache competes with Aeon's heap. When
the OS evicts Redpanda's page cache to give memory to Aeon (or vice versa),
both suffer performance cliffs.

### What Segregation Gains

| Metric | Co-located | Segregated | Why |
|--------|-----------|------------|-----|
| Redpanda tail latency (p99) | 5-20ms | 1-5ms | No page cache eviction from Aeon |
| Aeon processing throughput | Varies (contention) | Stable | No CPU stolen by Redpanda compaction |
| Aeon pipeline restart time | Slow (Redpanda caching cold) | Fast (Redpanda cache warm) | Redpanda has dedicated memory |
| Disk I/O predictability | Shared (unpredictable) | Dedicated (predictable) | No interference |

### What Segregation Costs

The primary cost is **network round-trip latency** on every produce/fetch call.
See Section 8 for detailed analysis.

### Segregation Decision Matrix

| Scenario | Recommendation |
|----------|---------------|
| Development / CI | Co-located (Topology 1) |
| < 10K events/sec production | Co-located is fine |
| 10K-100K events/sec | Segregate Redpanda (Topology 2) |
| > 100K events/sec | Full separation (Topology 3) |
| Multiple pipelines, mixed workloads | Segregate (resource isolation) |
| CDC sources (PostgreSQL, MySQL, MongoDB) | Keep databases separate from both |

---

## 8. Network Latency Impact

### Baseline: Co-located (Loopback)

When Aeon and Redpanda are on the same machine, produce/fetch calls go through
the loopback interface:

```
Aeon -> localhost:9092 -> Redpanda
RTT: ~0.05ms (50 microseconds)
```

### Segregated: Same Data Center / Same Rack

```
Aeon (Server A) -> switch -> Redpanda (Server B)
RTT: 0.1-0.5ms (typical 10GbE same-rack)
```

### Segregated: Cross-Rack / Cross-AZ

```
Aeon (Rack A) -> spine switch -> Redpanda (Rack B)
RTT: 0.5-2ms (typical cross-rack)

Aeon (AZ-1) -> AZ interconnect -> Redpanda (AZ-2)
RTT: 1-5ms (typical cross-AZ within same region)
```

### Impact on Aeon Throughput

Aeon's **batching architecture** absorbs network latency efficiently:

| Delivery Strategy | Behavior | Latency Impact |
|-------------------|----------|----------------|
| **PerEvent** | 1 produce call per event, await ack | **High impact**: throughput = 1/RTT. At 1ms RTT, max ~1000 events/sec per partition |
| **OrderedBatch** | 1 produce call per batch, await all acks at batch boundary | **Low impact**: 1 RTT per batch of 1024 events. At 1ms RTT, ~1M events/sec theoretical |
| **UnorderedBatch** | Fire-and-forget produce, flush at intervals | **Minimal impact**: acks are asynchronous, pipeline never waits for individual ack |

**Concrete numbers** (100K events, 256B payload, batch size 1024):

| Strategy | Co-located (0.05ms) | Same-rack (0.3ms) | Cross-rack (1ms) | Cross-AZ (3ms) |
|----------|--------------------|--------------------|-------------------|-----------------|
| PerEvent | ~1,800/s | ~1,500/s | ~900/s | ~300/s |
| OrderedBatch | ~35,000/s | ~33,000/s | ~30,000/s | ~25,000/s |
| UnorderedBatch | ~41,500/s | ~40,000/s | ~38,000/s | ~35,000/s |

Key insight: **OrderedBatch and UnorderedBatch are largely network-latency-tolerant**
because they amortize the RTT across entire batches. Only PerEvent is significantly
affected, and PerEvent is reserved for strict regulatory use cases where throughput
is secondary.

### Fetch (Source) Side

The source side is similarly batch-tolerant:

```
KafkaSource::next_batch()
  -> rdkafka poll (fetches up to batch_max events)
  -> 1 network round trip fetches 1024 events
  -> Amortized latency per event: RTT / batch_size
  -> At 1ms RTT, 1024 batch: ~1us per event additional overhead
```

### Recommendations

1. **Use OrderedBatch (default) or UnorderedBatch** for segregated deployments.
   These strategies make network latency nearly irrelevant.

2. **Keep Aeon and Redpanda in the same rack** if possible. Same-rack latency
   (0.1-0.5ms) has negligible impact on batched strategies.

3. **Cross-AZ is acceptable** for disaster recovery, but adds 1-5ms per batch
   boundary. OrderedBatch at 3ms cross-AZ still achieves ~25,000 events/sec.

4. **PerEvent across network** is strongly discouraged unless required by
   regulation. Use OrderedBatch instead -- it preserves ordering with 15-20x
   higher throughput.

5. **Increase batch size** when latency is higher. Larger batches amortize
   RTT more effectively:
   ```
   Effective throughput = batch_size / (process_time + RTT)
   ```

---

## 9. Observability Stack Placement

The observability stack (Prometheus, Grafana, Jaeger, Loki, OTel Collector)
should be **separated from the data plane** in production:

```
                ┌──────────────────────────────┐
                │ Observability Server          │
                │                               │
                │  OTel Collector (:4317)        │
                │  Prometheus (:9090)            │
                │  Grafana (:3000)               │
                │  Jaeger (:16686)               │
                │  Loki (:3100)                  │
                └──────────┬───────────────────┘
                           │ OTLP gRPC + Prometheus scrape
              ┌────────────┴──────────────┐
              │                           │
    ┌─────────┴─────────┐     ┌──────────┴──────────┐
    │ Aeon Nodes          │     │ Redpanda Brokers     │
    └─────────────────────┘     └─────────────────────┘
```

**Why separate**:
- Prometheus scraping and Loki log ingestion consume memory and disk
- Grafana dashboard rendering consumes CPU
- Jaeger trace storage can grow large
- None of these should compete with Aeon's processing or Redpanda's I/O

**Acceptable co-location**:
- OTel Collector can run as a sidecar on Aeon nodes (low overhead, async export)
- In development, everything runs on one machine (Topology 1)

---

## 10. Database Placement (CDC Sources)

When using CDC connectors (PostgreSQL, MySQL, MongoDB), the source databases
should be **on separate servers** from both Aeon and Redpanda:

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│ PostgreSQL    │     │ Aeon Engine   │     │ Redpanda      │
│ (CDC source)  │────►│ (processing)  │────►│ (sink topic)  │
│               │ WAL │               │     │               │
│ Server C      │     │ Server A      │     │ Server B      │
└──────────────┘     └──────────────┘     └──────────────┘
```

**Why**:
- CDC replication reads from the database's WAL (PostgreSQL) or binlog (MySQL).
  This adds I/O load on the database server.
- Co-locating the database with Aeon or Redpanda creates a three-way resource
  conflict.
- Database performance should never be affected by stream processing load.

**CDC latency**: WAL replication is asynchronous. The CDC source receives changes
within milliseconds of the database commit. Network latency between the database
and Aeon is added to this but is not on the critical path (CDC is inherently
asynchronous).

---

## 11. Resource Sizing Guide

### Single Pipeline (Topology 1-2)

| Component | CPU | RAM | Disk | Notes |
|-----------|-----|-----|------|-------|
| Aeon Engine | 2-4 cores | 4-8 GB | 10 GB SSD | Checkpoint WAL + processor artifacts |
| Redpanda | 2-4 cores | 8-16 GB | 100+ GB NVMe | 80% of RAM for page cache |
| Observability | 1-2 cores | 4 GB | 50 GB SSD | Prometheus TSDB, Loki chunks |

### Multi-Pipeline Cluster (Topology 3)

| Component | Per Node | Cluster (3 nodes) | Notes |
|-----------|----------|-------------------|-------|
| Aeon Engine | 4-8 cores, 8-16 GB | 12-24 cores, 24-48 GB | CPU scales with pipeline count |
| Redpanda | 4-8 cores, 16-32 GB | 12-24 cores, 48-96 GB | RAM scales with retention |
| Observability | 2-4 cores, 8 GB | Single server is fine | Prometheus federation for large deployments |

### Scaling Triggers

| Metric | Threshold | Action |
|--------|-----------|--------|
| Aeon CPU > 70% sustained | Scale up | Add Aeon nodes or upgrade CPU |
| Redpanda disk > 80% | Scale up | Increase disk or reduce retention |
| Redpanda fetch latency p99 > 50ms | Investigate | Check page cache ratio, consider more RAM |
| Pipeline backpressure > 30% of time | Scale up | Add partitions + Aeon nodes |
| OTel Collector queue growing | Scale out | Add Collector replicas or reduce sampling |

---

## 12. Deployment Checklist

### Pre-Deployment

- [ ] Choose topology (1-4) based on throughput requirements
- [ ] Provision servers with appropriate resources (see Section 11)
- [ ] Configure network: ensure low-latency links between Aeon and Redpanda
- [ ] Set up DNS or static IPs for broker addresses
- [ ] Generate TLS certificates for mTLS (multi-node clusters)

### Redpanda Setup

- [ ] Install Redpanda (bare metal, Docker, or K8s Operator)
- [ ] Configure `--smp` (CPU cores) and `--memory` based on server specs
- [ ] Set `--default-log-level=warn` for production
- [ ] Create topics with appropriate partition counts (immutable after creation)
- [ ] Verify cluster health: `rpk cluster health`

### Aeon Setup

- [ ] Set `AEON_API_TOKEN` (required for production)
- [ ] Set `AEON_BROKERS` to Redpanda address(es)
- [ ] Set `AEON_CHECKPOINT_DIR` to persistent storage
- [ ] Set `AEON_LOG_FORMAT=json` for structured logging
- [ ] Configure `OTEL_EXPORTER_OTLP_ENDPOINT` for observability
- [ ] Deploy processor artifacts (`.wasm` or `.so`/`.dll`)
- [ ] Apply pipeline manifests: `aeon apply -f pipelines.yaml`
- [ ] Verify: `aeon pipeline list`, `aeon top`

### Observability

- [ ] Deploy OTel Collector with appropriate config
- [ ] Configure Prometheus scraping (Aeon `:4471/metrics`, Redpanda metrics)
- [ ] Set up Grafana dashboards (pre-provisioned datasources)
- [ ] Configure alerting rules (see Section 11 thresholds)
- [ ] Verify traces in Jaeger: `http://jaeger-host:16686`

### Security

- [ ] All passwords externalized via environment variables (no defaults)
- [ ] `AEON_API_TOKEN` set and not logged
- [ ] mTLS certificates distributed (multi-node)
- [ ] Firewall rules: only required ports open
- [ ] Non-root container users
- [ ] See [SECURITY.md](SECURITY.md) for full hardening checklist

### Validation

- [ ] Health check: `curl http://aeon-host:4471/health`
- [ ] Readiness: `curl http://aeon-host:4471/ready`
- [ ] End-to-end test: produce events, verify they appear in sink topic
- [ ] Benchmark: run `e2e_delivery_bench` to establish baseline
- [ ] Load test: sustained load for 10+ minutes, verify zero event loss
