# Aeon — Installation, Configuration & Multi-Version Operation

> This document covers Aeon installation, default port assignments, instance
> configuration, multi-version side-by-side operation, and deployment models.
> Referenced from `docs/ROADMAP.md` and `docs/PROCESSOR-DEPLOYMENT.md`.
>
> Related: `docs/PROCESSOR-DEPLOYMENT.md` (processor registry, pipeline lifecycle,
> upgrade strategies, REST API, K8s patterns)
>
> **Status**: Port assignments and manifest schema are finalized. Single binary and
> Docker installation methods are designed; `cargo install aeon-cli` and Docker Hub
> images are not yet published (see `docs/PUBLISHING.md`). Currently, Aeon runs from
> source via `cargo run` or `cargo build --release`.

---

## 1. Default Ports

Aeon uses three sequential ports, chosen to avoid conflicts with popular applications
(Kafka 9092, Prometheus 9090, Grafana 3000, PostgreSQL 5432, Redis 6379, K8s API 6443,
HTTP proxies 8080, QUIC demos 4433, etc.). All ports are configurable.

| Port | Protocol | Purpose | Active When |
|------|----------|---------|-------------|
| **4470** | UDP | QUIC inter-node transport (Raft consensus, partition transfers, mTLS) | Multi-node cluster only (no listener on single-node) |
| **4471** | TCP | HTTP management plane (REST API, `/health`, `/ready`, `/metrics`) | Always |
| **4472** | UDP | QUIC external connectors (WebTransport streams/datagrams, raw QUIC sources/sinks) | When external QUIC connectors are configured |

### 1.1 Port Separation Rationale

The three ports serve three distinct security boundaries:

```
┌─────────────────────────────────────────────────────────┐
│ 4470 — Cluster-Internal (QUIC/UDP)                      │
│   mTLS required, never exposed outside cluster          │
│   Raft RPCs, partition transfers, registry replication   │
├─────────────────────────────────────────────────────────┤
│ 4471 — Management Plane (HTTP/TCP)                      │
│   REST API, health checks, Prometheus scrape target     │
│   mTLS or API key auth, exposed to operators/CI/CD      │
├─────────────────────────────────────────────────────────┤
│ 4472 — External Data Plane (QUIC/UDP)                   │
│   WebTransport, raw QUIC sources/sinks                  │
│   TLS required, exposed to external QUIC clients        │
│   Separate from cluster QUIC: different certs, ACLs,    │
│   rate limits                                           │
└─────────────────────────────────────────────────────────┘
```

### 1.2 Port Conflict Reference

Applications whose default ports were explicitly avoided:

| Port | Application | Why Avoided |
|------|-------------|-------------|
| 443 | HTTPS | Universal |
| 2379/2380 | etcd | Common in K8s clusters |
| 3000 | Grafana, Node.js dev | Aeon's own dev stack uses Grafana on 3000 |
| 3100 | Loki | Aeon's observability stack |
| 4222 | NATS | Future Aeon connector |
| 4317/4318 | OTLP gRPC/HTTP | Aeon's tracing (Jaeger) |
| 4433 | QUIC demos (quinn, quiche) | Too generic, easily confused |
| 5432 | PostgreSQL | Future Aeon connector |
| 5672 | RabbitMQ | Future Aeon connector |
| 6379 | Redis | Future Aeon connector |
| 6443 | Kubernetes API | K8s environments |
| 8080 | HTTP proxies, Tomcat, Jenkins, Redpanda Console | Extremely overloaded |
| 8443 | HTTPS alt, K8s dashboard | Common alt HTTPS |
| 9090 | Prometheus | Aeon's monitoring stack |
| 9092 | Kafka/Redpanda | Aeon's primary connector |
| 9200 | Elasticsearch | Common logging backend |
| 16686 | Jaeger UI | Aeon's tracing stack |
| 27017 | MongoDB | Future Aeon connector |

---

## 2. Instance Configuration

Each Aeon instance is fully self-contained: a binary, a manifest file, and a data
directory. No global state, no hardcoded paths, no shared resources between instances.

### 2.1 Manifest File (YAML)

The manifest is the single source of configuration for an Aeon instance. Every path,
port, and behavior is configurable.

```yaml
# /opt/aeon/manifest.yaml — complete instance configuration

# Instance identity and storage
instance:
  data_dir: /opt/aeon/data           # Raft log, state store, registry artifacts
  log_dir: /opt/aeon/logs            # Application logs
  pid_file: /opt/aeon/aeon.pid       # PID file (prevents duplicate startup)

# Cluster configuration
cluster:
  node_id: 1                         # Unique within cluster
  bind: "0.0.0.0:4470"              # QUIC inter-node (default: 4470)
  num_partitions: 16                 # Immutable after creation
  peers: []                          # Empty = single-node cluster
  # peers:                           # Multi-node:
  #   - "10.0.0.2:4470"
  #   - "10.0.0.3:4470"
  tls:
    mode: auto                       # none | auto | pem
    # mode: none                     # No TLS (dev only, single-node)
    # mode: auto                     # Auto-generate self-signed CA + node cert,
    #                                # persisted to data_dir/tls/ (single-node only;
    #                                # rejected if peers configured)
    # mode: pem                      # Load from PEM files (production, required multi-node)
    #   cert: /etc/aeon/tls/node.pem
    #   key: /etc/aeon/tls/node.key
    #   ca: /etc/aeon/tls/ca.pem

# HTTP management plane
http:
  bind: "0.0.0.0:4471"              # REST API + health + metrics (default: 4471)
  tls:
    mode: auto                       # none | auto | pem (same three modes)
  auth:
    mode: api-key                    # none | api-key | mtls
    api_key_file: /etc/aeon/api.key  # For api-key mode

# External QUIC connectors (optional, only when QUIC sources/sinks configured)
connectors:
  quic:
    bind: "0.0.0.0:4472"            # External QUIC (default: 4472)
    tls:
      mode: auto                     # auto | pem (no 'none' — QUIC requires TLS)
      # mode: pem
      #   cert: /etc/aeon/tls/external.pem
      #   key: /etc/aeon/tls/external.key

# Encryption at rest (opt-in)
encryption:
  at_rest:
    enabled: false                   # Default off (rely on filesystem/disk encryption)
    key_provider: env                # env | file (future: vault, hsm, cloud-kms)
    key_id: aeon_master              # Key ID to load from provider

# Processor registry storage
registry:
  storage_dir: /opt/aeon/data/registry    # Processor artifacts stored here
  max_versions_per_processor: 10          # Auto-archive older versions

# Pipelines (can also be managed via CLI/REST API)
pipelines:
  orders-pipeline:
    source:
      type: redpanda
      brokers: ["localhost:9092"]
      topic: orders
      partitions: [0, 1, 2, 3, 4, 5, 6, 7]
      tls:                           # Per-connector TLS (independent per source/sink)
        mode: none                   # none | system-ca | pem
    processor:
      name: my-enricher
      version: v3
    sink:
      type: redpanda
      brokers: ["localhost:9092"]
      topic: enriched-orders
      tls:
        mode: none                   # Sink can have different TLS config than source
    upgrade:
      strategy: canary
      initial_percent: 10
```

### 2.2 Minimal Configuration

The simplest possible manifest for getting started:

```yaml
# Minimum viable manifest — single-node, defaults for everything
cluster:
  node_id: 1
```

All defaults applied:
- `instance.data_dir`: `./data`
- `instance.log_dir`: `./logs`
- `cluster.bind`: `0.0.0.0:4470` (IPv4; use `[::]:4470` for IPv6/dual-stack)
- `cluster.tls.mode`: `auto` (self-signed CA + node cert, persisted to `data_dir/tls/`)
- `http.bind`: `0.0.0.0:4471` (IPv4; use `[::]:4471` for IPv6/dual-stack)
- `http.tls.mode`: `auto` (HTTPS with self-signed cert)
- `http.auth.mode`: `none` (dev mode)
- `encryption.at_rest.enabled`: `false`
- `cluster.num_partitions`: `16`
- Single-node Raft (quorum of 1, no QUIC listener on 4470)

### 2.3 Environment Variable Overrides

Every manifest field can be overridden via environment variables:

```bash
AEON_CLUSTER_NODE_ID=1
AEON_CLUSTER_BIND=0.0.0.0:4470
AEON_HTTP_BIND=0.0.0.0:4471
AEON_INSTANCE_DATA_DIR=/opt/aeon/data
AEON_INSTANCE_LOG_DIR=/opt/aeon/logs
```

Precedence: environment variable > manifest file > built-in default.

### 2.4 IPv4, IPv6, and Dual-Stack

All Aeon bind addresses accept both IPv4 and IPv6. The address format in `bind` fields
determines the IP version:

| Bind Address | Protocol | Behavior |
|-------------|----------|----------|
| `0.0.0.0:4470` | IPv4 only | Accepts IPv4 connections (default) |
| `[::]:4470` | IPv6 (+ IPv4 on most OSes) | Dual-stack if OS allows (`IPV6_V6ONLY=false`) |
| `[::1]:4470` | IPv6 loopback only | Local IPv6 connections only |
| `192.168.1.10:4470` | Specific IPv4 | Binds to one interface |
| `[fd00::1]:4470` | Specific IPv6 | Binds to one interface |

**Dual-stack note**: Binding to `[::]` enables dual-stack on most Linux configurations
(accepts both IPv4 and IPv6 on a single socket). On Windows and some hardened Linux
setups where `IPV6_V6ONLY=true`, binding to `[::]` only accepts IPv6. In that case,
run two Aeon instances or use a reverse proxy to serve both protocols.

**Connector addresses**: Source and sink connector addresses (e.g., `brokers`) support
IPv4 (`10.0.1.5:9092`), IPv6 (`[fd00::5]:9092`), and hostnames (`broker.local:9092`).
DNS resolution is handled by the underlying connector library.

**TLS auto-cert SANs**: When `tls.mode: auto`, the generated certificate includes
SANs for `localhost`, `127.0.0.1` (IPv4), `::1` (IPv6), the machine hostname, and
the specific bind IP (if not a wildcard address).

```yaml
# Example: IPv6 dual-stack configuration
cluster:
  node_id: 1
  bind: "[::]:4470"

http:
  bind: "[::]:4471"

connectors:
  quic:
    bind: "[::]:4472"
```

---

## 3. Installation Methods

### 3.1 Single Binary

Aeon is a single static binary. Download, configure, run.

```bash
# Download
curl -L https://github.com/aeonflow/aeon/releases/latest/download/aeon-linux-x86_64 -o aeon
chmod +x aeon

# Create minimal manifest
cat > manifest.yaml << 'EOF'
cluster:
  node_id: 1
EOF

# Run
./aeon run -f manifest.yaml
```

### 3.2 Docker (Single Container)

```dockerfile
# Dockerfile
FROM rust:1.82-slim AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --features kafka,wasm

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/aeon /usr/local/bin/aeon

# Default ports
EXPOSE 4470/udp 4471/tcp 4472/udp

# Default directories
VOLUME ["/data", "/processors"]

ENTRYPOINT ["aeon"]
CMD ["run", "-f", "/etc/aeon/manifest.yaml"]
```

```bash
# Run with Docker
docker run -d \
  --name aeon \
  -p 4471:4471 \
  -v ./manifest.yaml:/etc/aeon/manifest.yaml:ro \
  -v ./data:/data \
  -v ./processors:/processors \
  aeon:latest
```

### 3.3 Docker Compose (With Redpanda)

```yaml
# docker-compose.yaml — Aeon + Redpanda (Scenario 1)
services:
  redpanda:
    image: redpandadata/redpanda:latest
    command:
      - redpanda start
      - --smp 2
      - --memory 2G
      - --overprovisioned
      - --kafka-addr internal://0.0.0.0:9092,external://0.0.0.0:19092
      - --advertise-kafka-addr internal://redpanda:9092,external://localhost:19092
    ports:
      - "19092:19092"

  aeon:
    image: aeon:latest
    depends_on: [redpanda]
    ports:
      - "4471:4471"              # HTTP API + health + metrics
    volumes:
      - ./manifest.yaml:/etc/aeon/manifest.yaml:ro
      - ./processors:/processors:ro
      - aeon-data:/data
    environment:
      AEON_CLUSTER_NODE_ID: 1
    networks:
      - aeon-net

  # Observability (optional)
  prometheus:
    image: prom/prometheus:latest
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml:ro
    ports:
      - "9090:9090"

  grafana:
    image: grafana/grafana:latest
    ports:
      - "3000:3000"

volumes:
  aeon-data:

networks:
  aeon-net:
    driver: bridge
```

### 3.4 Kubernetes (Helm)

```bash
# Install via Helm (Phase 14)
helm repo add aeon https://charts.aeonflow.io
helm install my-aeon aeon/aeon \
  --set cluster.nodeId=1 \
  --set cluster.numPartitions=16 \
  --set http.auth.mode=mtls
```

---

## 4. Multi-Version Side-by-Side Operation

Multiple Aeon versions can run simultaneously on the same machine or cluster. Each
instance is fully isolated — own binary, own manifest, own data directory, own ports.

### 4.1 Same Machine, Different Ports

```
/opt/aeon/v1/
├── bin/aeon                # v1.0.0 binary
├── manifest.yaml           # cluster.bind: 4470, http.bind: 4471
├── data/                   # Raft log, state, registry
├── logs/
└── processors/             # .wasm / .so files for v1

/opt/aeon/v2/
├── bin/aeon                # v2.0.0 binary
├── manifest.yaml           # cluster.bind: 4480, http.bind: 4481
├── data/                   # Separate Raft log, state, registry
├── logs/
└── processors/             # .wasm / .so files for v2
```

```bash
# Run both simultaneously — completely independent
/opt/aeon/v1/bin/aeon run -f /opt/aeon/v1/manifest.yaml &
/opt/aeon/v2/bin/aeon run -f /opt/aeon/v2/manifest.yaml &

# Each has its own REST API
curl http://localhost:4471/health    # v1
curl http://localhost:4481/health    # v2

# Each has its own CLI target
aeon pipeline status --target localhost:4471    # v1 pipelines
aeon pipeline status --target localhost:4481    # v2 pipelines
```

### 4.2 Docker Compose, Different Ports

```yaml
services:
  aeon-v1:
    image: aeon:1.0.0
    ports:
      - "4470:4470/udp"     # QUIC (if multi-node)
      - "4471:4471"         # HTTP
    volumes:
      - ./v1/manifest.yaml:/etc/aeon/manifest.yaml:ro
      - ./v1/processors:/processors:ro
      - v1-data:/data

  aeon-v2:
    image: aeon:2.0.0
    ports:
      - "4480:4470/udp"     # Different host port → same container port
      - "4481:4471"         # Different host port → same container port
    volumes:
      - ./v2/manifest.yaml:/etc/aeon/manifest.yaml:ro
      - ./v2/processors:/processors:ro
      - v2-data:/data

volumes:
  v1-data:
  v2-data:
```

### 4.3 Canary Upgrade of Aeon Itself

For upgrading the Aeon engine (not processors — that's handled by the Processor
Registry without any Aeon restart), you can run two Aeon clusters and gradually
shift partition assignments:

```
Source connector (any type)
    │
    ├── Partitions 0-11  → Aeon v1 cluster (current, 75%)
    └── Partitions 12-15 → Aeon v2 cluster (canary, 25%)
                             │
                             ▼
                    Monitor metrics (v2 vs v1)
                             │
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
         Healthy:       Acceptable:    Problems:
         Shift more     Hold and       Rollback all
         partitions     observe        to v1
         to v2
```

This is connector-agnostic — it works at the Aeon partition assignment level. The
source and sink connectors are unaware that an engine upgrade is in progress.

**Steps:**
1. Deploy Aeon v2 cluster alongside v1 (different ports or different nodes)
2. Reassign a small set of partitions to v2: `aeon pipeline move-partitions --from v1 --to v2 --partitions 12,13,14,15`
3. Monitor v2 metrics via REST API or Grafana
4. If healthy, shift more partitions; if not, move them back to v1
5. Once v2 has all partitions, decommission v1

### 4.4 Key Design Points

- **No global state**: No `/var/lib/aeon`, no shared PID files, no system-wide config
- **All paths configurable**: `data_dir`, `log_dir`, `pid_file`, `registry.storage_dir`
- **All ports configurable**: `cluster.bind`, `http.bind`, `connectors.quic.bind`
- **Data directory isolation**: Each instance has its own Raft log, state store, and
  processor registry — no cross-contamination
- **PID file prevents accidental duplicates**: Two instances cannot share the same
  `pid_file` path (startup fails with clear error)

---

## 5. Systemd Service (Linux)

For production Linux installations running Aeon as a system service:

```ini
# /etc/systemd/system/aeon.service
[Unit]
Description=Aeon Stream Processing Engine
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=aeon
Group=aeon
ExecStart=/usr/local/bin/aeon run -f /etc/aeon/manifest.yaml
Restart=on-failure
RestartSec=5
LimitNOFILE=65535

# Security hardening
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/aeon /var/log/aeon
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

```bash
# Install and enable
sudo cp aeon /usr/local/bin/
sudo useradd --system --no-create-home aeon
sudo mkdir -p /etc/aeon /var/lib/aeon /var/log/aeon
sudo cp manifest.yaml /etc/aeon/
sudo systemctl daemon-reload
sudo systemctl enable --now aeon

# Multi-version: separate service files
sudo cp /etc/systemd/system/aeon.service /etc/systemd/system/aeon-v2.service
# Edit aeon-v2.service to point to v2 binary and v2 manifest (different ports)
```

---

## 6. Directory Layout Conventions

### 6.1 Single Binary Install

```
/opt/aeon/
├── bin/aeon                    # Binary
├── manifest.yaml               # Instance configuration
├── data/                       # Raft log, state store
│   ├── raft/                   # Raft log and snapshots
│   ├── state/                  # L1/L2/L3 state store
│   └── registry/               # Processor artifacts (.wasm, .so)
├── logs/                       # Application logs
├── processors/                 # Processor source/build artifacts (development)
└── tls/                        # Certificates (production)
    ├── node.pem                # Node certificate
    ├── node.key                # Node private key
    └── ca.pem                  # CA certificate
```

### 6.2 Docker Container

```
/usr/local/bin/aeon             # Binary (in image)
/etc/aeon/manifest.yaml         # Config (mounted or baked in)
/data/                          # Persistent volume
│   ├── raft/
│   ├── state/
│   └── registry/
/processors/                    # Mounted volume (Wasm/native artifacts)
/etc/aeon/tls/                  # Mounted secrets
```

### 6.3 Kubernetes

```
Pod: aeon-0
├── Container: aeon
│   ├── /usr/local/bin/aeon
│   ├── /etc/aeon/manifest.yaml       ← ConfigMap
│   ├── /data/                        ← PersistentVolumeClaim
│   ├── /processors/                  ← ConfigMap (wasm) or PVC (native .so)
│   └── /etc/aeon/tls/                ← Secret
└── InitContainer: fetch-processors   (optional, pulls from OCI/S3)
```

---

## 7. Health Check Endpoints

Available on the HTTP management port (default: 4471):

| Endpoint | Method | Purpose | Response |
|----------|--------|---------|----------|
| `/health` | GET | Liveness probe | `200 OK` if process is alive |
| `/ready` | GET | Readiness probe | `200 OK` if Raft is initialized and pipelines are accepting events |
| `/metrics` | GET | Prometheus scrape | Prometheus text format (throughput, latency, partition lag, etc.) |

```bash
# K8s probe configuration
livenessProbe:
  httpGet:
    path: /health
    port: 4471
  initialDelaySeconds: 5
  periodSeconds: 10

readinessProbe:
  httpGet:
    path: /ready
    port: 4471
  initialDelaySeconds: 10
  periodSeconds: 5
```

---

## 8. Configuration Precedence

Configuration values are resolved in this order (highest priority first):

```
1. CLI flags          aeon run -f manifest.yaml --http-bind 0.0.0.0:9999
2. Environment vars   AEON_HTTP_BIND=0.0.0.0:9999
3. Manifest file      http: { bind: "0.0.0.0:4471" }
4. Built-in defaults  4470 (QUIC), 4471 (HTTP), 4472 (external QUIC)
```

This follows the convention used by most modern infrastructure tools (12-factor app
compatible). CLI flags enable quick overrides for testing; environment variables
enable container orchestration; manifest files enable version-controlled configuration.
