# Aeon Clustering Guide

> Comprehensive guide to configuring and operating Aeon clusters, from single-node
> development setups through multi-node production deployments.
>
> **Implementation status**: Single-node Raft is implemented and tested. Multi-node
> cluster validation was completed on a 3-node DigitalOcean Kubernetes (DOKS) cluster
> on 2026-04-12 — leader failover, log replication, node rejoin, and cross-node QUIC
> were all validated (all nodes reached consistent state: `log_idx=16, applied=16,
> term=25`). Remaining multi-node work (Gate 2 acceptance criteria, partition transfer
> CL-6, cluster metrics) is tracked in `docs/FAULT-TOLERANCE-ANALYSIS.md` Section 7
> (Pillar 3). Note that local Rancher Desktop K3s is single-node; multi-node testing
> requires DOKS/EKS/GKE or a real multi-node K3s cluster.
>
> **Gate 2 code-path status (2026-04-19):** all Aeon code blockers for
> horizontal scale-up/down are closed. G11.a/b/c shipped the transfer
> driver + installer hooks. G15 shipped the fix for the
> `handle_add_node` leader-self-equality check — `server::serve()` now
> takes `self_id: NodeId` sourced from `ClusterConfig::node_id`
> (authoritative for REST), so the leader-self check in
> `handle_add_node` / `handle_remove_node` no longer relies on the
> watch-channel `raft.metrics().borrow().id`, which can diverge from the
> configured id. A `tracing::warn!` stays on the reject path as a
> diagnostic if future divergence occurs. Regression test
> `g15_join_targets_actual_leader_of_three_node_cluster` in
> `crates/aeon-cluster/tests/multi_node.rs` asserts the QUIC join RPC
> succeeds against the actual leader of a 3-node cluster bootstrapped
> via `initial_members`. End-to-end T2/T3 verification on DOKS
> (3→5→3→1 STS events with live T1 load + zero event loss) folds into
> the next DOKS re-spin; see `docs/ROADMAP.md` "Phase 3b" +
> `docs/GATE2-ACCEPTANCE-PLAN.md § 10.9` for the post-bundle evidence.

---

## Table of Contents

1. [Overview](#1-overview)
2. [Single-Node Configuration](#2-single-node-configuration)
3. [Multi-Node Cluster Setup](#3-multi-node-cluster-setup)
4. [Adding Nodes to an Existing Cluster](#4-adding-nodes-to-an-existing-cluster)
5. [Pipeline Distribution](#5-pipeline-distribution)
6. [Adding Pipeline Configurations](#6-adding-pipeline-configurations)
7. [Multi-Pipeline Configuration Examples](#7-multi-pipeline-configuration-examples)
8. [Monitoring a Cluster](#8-monitoring-a-cluster)
9. [Capacity Planning](#9-capacity-planning)

---

## 1. Overview

Aeon uses a distributed consensus architecture built on two core technologies:

- **openraft** -- A Rust implementation of the Raft consensus algorithm. Raft provides
  leader election, log replication, and membership changes. Every Aeon node participates
  in Raft, even in single-node deployments.

- **QUIC transport (quinn)** -- All inter-node communication uses QUIC (RFC 9000) via
  the quinn library. QUIC provides multiplexed streams without head-of-line blocking,
  0-RTT reconnection, and built-in TLS via rustls + aws-lc-rs.

### What goes through Raft consensus

The Raft log is intentionally small. It replicates only coordination metadata:

- **Partition assignment table** -- which node owns which partitions
- **Partition transfer state transitions** -- `Owned(node)` -> `Transferring(source, target)` -> `Owned(target)`
- **Membership changes** -- adding/removing voters and learners
- **Global PoH checkpoints** -- periodic cryptographic ordering proofs
- **Configuration changes** -- cluster-wide settings
- **Pipeline and processor registry** -- pipeline definitions, processor versions

### What does NOT go through Raft

- Event data (flows through the pipeline engine, not consensus)
- Per-partition PoH chains (local to the owning node)
- Metrics and health status (direct query or gossip)

This design keeps the Raft log small (kilobytes for snapshots) regardless of event
throughput, and ensures consensus overhead does not affect data plane performance.

### Architecture diagram

```
+-----------------------------------------------------------+
|                        Aeon Node                          |
|                                                           |
|  +-------------+    +--------------+    +---------------+ |
|  | Pipeline    |    | PoH          |    | Merkle Log    | |
|  | Engine      |--->| Recorder     |--->| (append-only) | |
|  +-------------+    +--------------+    +---------------+ |
|                                                           |
|  +-----------------------------------------------------+ |
|  | Cluster Layer (always active)                        | |
|  |                                                      | |
|  |  +------------+  +--------------+  +---------------+ | |
|  |  | Raft       |  | Partition    |  | QUIC          | | |
|  |  | (openraft) |  | Manager      |  | Transport     | | |
|  |  +------------+  +--------------+  +---------------+ | |
|  +-----------------------------------------------------+ |
+-----------------------------------------------------------+
```

### Network ports

| Port | Protocol | Purpose | When active |
|------|----------|---------|-------------|
| **4470** | UDP | QUIC inter-node transport (Raft RPCs, partition transfers, mTLS) | Multi-node only (no listener on single-node) |
| **4471** | TCP | HTTP management plane (REST API, `/health`, `/ready`, `/metrics`) | Always |

---

## 2. Single-Node Configuration

A single-node deployment is the simplest way to run Aeon. It is suitable for development,
testing, small workloads, and single-instance performance benchmarking.

### Design principle: Always-Raft

Aeon **always runs Raft**, even on a single node. There is no separate "standalone mode."
A single-node deployment is a Raft cluster of size 1 where:

- The node is trivially the leader
- Commits are instant (quorum of 1)
- There is zero network overhead
- No QUIC listener is started (no listening socket, no background tasks)
- The Partition Manager owns all partitions

This eliminates the "mode switch" problem. Scaling from 1 to 3 to 5 nodes is a Raft
membership change, not an architecture transition. This is the same approach used by
etcd, CockroachDB, and TiKV.

### Minimal single-node manifest

Create a file `aeon.yaml`:

```yaml
cluster:
  node_id: 1
  bind: "0.0.0.0:4470"
  num_partitions: 16

http:
  bind: "0.0.0.0:4471"
```

The `bind` address for the cluster is declared even though no QUIC listener starts in
single-node mode. This address is recorded in the Raft membership so that when you later
scale to multiple nodes, peers know how to reach this node.

### Key parameters

| Parameter | Description | Default |
|-----------|-------------|---------|
| `node_id` | Unique positive integer identifying this node. Must be > 0. | Required |
| `bind` | Socket address for the QUIC transport. | `0.0.0.0:4470` |
| `num_partitions` | Total partitions. Immutable after cluster creation. | Required |

### Partition count guidance

The partition count is decided once at cluster creation and **never changes**. Choose a
count large enough to accommodate future scaling:

| Expected max nodes | Recommended partitions | Partitions per node (min) |
|--------------------|------------------------|---------------------------|
| 1-3 | 16 | 5 |
| 3-7 | 32 | 4 |
| 7+ | 64 or 128 | 9+ |

Rule of thumb: `num_partitions >= max_expected_nodes * 4`.

Even if you start with a single node, choose a partition count that accommodates your
maximum expected cluster size. You cannot change it later.

### Raft timing (election timeouts and heartbeats)

Aeon exposes three Raft timing parameters on `ClusterConfig.raft_timing`:

| Field | Default | Meaning |
|-------|---------|---------|
| `heartbeat_ms` | 500 | Leader → follower heartbeat interval. |
| `election_min_ms` | 1500 | Lower bound of the randomized election timeout. |
| `election_max_ms` | 3000 | Upper bound of the randomized election timeout. |

When a follower doesn't hear a heartbeat within its per-round randomized
timeout (drawn uniformly from `[election_min_ms, election_max_ms]`), it
becomes a candidate and starts an election.

#### Why the jitter window matters

If two followers time out within a few milliseconds of each other, they
both become candidates, each votes for itself, neither wins, and the
term is incremented — a **split vote**. With `N` followers and a window
width `W = election_max_ms - election_min_ms`, the split-vote probability
per election round is approximately:

```
P(split_vote) ≈ N · (N-1) · (RTT / W)
```

A 3-node cluster on a VPC with ~100 ms RTT sees:

| Window W | Split-vote probability | Worst-case failover |
|----------|------------------------|---------------------|
| 1500 ms (default) | ~40% | ~3 s |
| 4000 ms (prod_recommended) | ~15% | ~6 s |
| 9000 ms (flaky_network) | ~7% | ~12 s |

A wider window is the mitigation for the absence of **pre-vote** in
openraft 0.9.x (see `docs/FAULT-TOLERANCE-ANALYSIS.md` FT-4). Pre-vote
would eliminate split votes entirely by running a dry-run round before
incrementing the term; without it, widening the jitter window is the
next-best option.

#### Presets

Three builders are provided on `RaftTiming`:

```rust
RaftTiming::default()           // 500 / 1500 / 3000  — fast failover, dev default
RaftTiming::prod_recommended()  // 500 / 2000 / 6000  — 3-node VPC, reliable network
RaftTiming::flaky_network()     // 500 / 3000 / 12000 — cross-region / noisy neighbour
```

In a YAML manifest:

```yaml
cluster:
  raft_timing:
    heartbeat_ms: 500
    election_min_ms: 2000
    election_max_ms: 6000
```

Pick the preset that matches your network. If DOKS stress tests show
stable `term` counters (no term churn under load), the default is fine.
If you see frequent leadership changes without actual node failures,
move up to `prod_recommended`. Only use `flaky_network` if your latency
distribution has multi-second tails.

#### QUIC transport timeouts (derived from `raft_timing`)

The Raft-layer timing above drives two QUIC-level knobs on every
inter-node connection. Operators don't set these directly — they're
derived automatically — but knowing the values matters when reading
QUIC metrics or diagnosing stale connections:

| QUIC knob | Value | Why |
|-----------|-------|-----|
| `keep_alive_interval` | `heartbeat_ms` (500 ms default) | Forces PING frames at the Raft heartbeat cadence so a silently-gone peer is noticed at the transport layer, not only via openraft's heartbeat-miss. |
| `max_idle_timeout` | `2 × election_max_ms` (6 s default, 12 s prod, 24 s flaky) | Closes orphaned connections after a peer crashes. Sized well above the election window so normal traffic never trips it. |

Raising `election_max_ms` automatically widens the idle-timeout, so the
three presets scale together — there's no separate QUIC knob to forget.

### Connection retry and backoff — layering

Aeon uses connection-retry with exponential backoff + jitter
(`aeon_types::BackoffPolicy`), but the retry policy is layered
intentionally. Understanding which layer retries is important when
debugging slow failover or spurious elections.

| Layer | Retries? | Notes |
|-------|----------|-------|
| **Connectors** (Kafka, MQTT, MongoDB CDC, NATS, RabbitMQ, …) | Yes | On transport error, back off and reconnect. Resets on any successful message/event so the next outage starts again at `initial_ms`. |
| **Cluster bootstrap / join RPC** (`QuicEndpoint::connect_with_backoff`) | Yes (opt-in, bounded) | Seed node may still be starting; callers pass `BackoffPolicy` + `max_attempts`. |
| **openraft Raft RPC path** (`QuicEndpoint::connect` plain) | **No — fail-fast** | openraft has its own protocol-level retry. Adding another retry layer here would block the Raft client task long enough to delay heartbeats and trigger spurious elections. This is a deliberate design decision. |
| **Pipeline / sink writes** | Controlled by `BatchFailurePolicy` | Application concern (retry / skip / DLQ), decoupled from transport. |

**If you see spurious leadership changes**, first check the Raft timing
(above — widen the jitter window). Do **not** be tempted to add retries
to `QuicNetworkConnection::append_entries` etc.; that will make the
problem worse, not better.

### Starting a single-node instance

```bash
aeon serve --addr 0.0.0.0:4471 --artifact-dir ./data
```

Or with environment variable overrides (the binary reads cluster shape
from env on startup; the manifest above is loaded at pipeline-apply
time, not engine-start time):

```bash
AEON_CLUSTER_NODE_ID=1 \
AEON_CLUSTER_BIND=0.0.0.0:4470 \
AEON_CLUSTER_NUM_PARTITIONS=16 \
AEON_API_ADDR=0.0.0.0:4471 \
aeon serve
```

### Verifying the node is running

```bash
# Health check
curl http://localhost:4471/health
# {"status":"ok","version":"0.1.0"}

# Readiness check
curl http://localhost:4471/ready
# {"status":"ready","version":"0.1.0"}
```

---

## 3. Multi-Node Cluster Setup

This section walks through deploying a 3-node production cluster from scratch.

### Prerequisites

- Three machines (or containers) with network connectivity between them
- UDP port 4470 open between all nodes (QUIC transport)
- TCP port 4471 open for management API access
- TLS certificates for mTLS between nodes (see Section 3.3)

### 3.1 Node configuration

Each node needs its own configuration file with a unique `node_id` and the addresses
of all peers.

**Node 1** (`aeon-node1.yaml`):

```yaml
cluster:
  node_id: 1
  bind: "0.0.0.0:4470"
  num_partitions: 16
  peers:
    - "10.0.0.2:4470"
    - "10.0.0.3:4470"
  tls:
    cert: /etc/aeon/tls/node1.pem
    key: /etc/aeon/tls/node1.key
    ca: /etc/aeon/tls/ca.pem

http:
  bind: "0.0.0.0:4471"
```

**Node 2** (`aeon-node2.yaml`):

```yaml
cluster:
  node_id: 2
  bind: "0.0.0.0:4470"
  num_partitions: 16
  peers:
    - "10.0.0.1:4470"
    - "10.0.0.3:4470"
  tls:
    cert: /etc/aeon/tls/node2.pem
    key: /etc/aeon/tls/node2.key
    ca: /etc/aeon/tls/ca.pem

http:
  bind: "0.0.0.0:4471"
```

**Node 3** (`aeon-node3.yaml`):

```yaml
cluster:
  node_id: 3
  bind: "0.0.0.0:4470"
  num_partitions: 16
  peers:
    - "10.0.0.1:4470"
    - "10.0.0.2:4470"
  tls:
    cert: /etc/aeon/tls/node3.pem
    key: /etc/aeon/tls/node3.key
    ca: /etc/aeon/tls/ca.pem

http:
  bind: "0.0.0.0:4471"
```

**Critical**: The `num_partitions` value must be identical across all nodes. Mismatched
values will cause the cluster to reject the joining node.

### 3.2 Bootstrap process

Start all three nodes. The first node to start initiates the Raft cluster. The other
nodes join as learners, receive the Raft log via snapshot, and are promoted to voters
once they are caught up.

```bash
# Each node reads its identity + peer list from env vars (or the helm
# chart's `values-local.yaml` / `values-doks.yaml` which sets them via
# the StatefulSet template). On a bare-VM bootstrap:

# On node 1 (10.0.0.1)
AEON_CLUSTER_NODE_ID=1 AEON_CLUSTER_PEERS="10.0.0.2:4470,10.0.0.3:4470" \
  aeon serve --addr 0.0.0.0:4471 --artifact-dir /var/lib/aeon/n1

# On node 2 (10.0.0.2)
AEON_CLUSTER_NODE_ID=2 AEON_CLUSTER_PEERS="10.0.0.1:4470,10.0.0.3:4470" \
  aeon serve --addr 0.0.0.0:4471 --artifact-dir /var/lib/aeon/n2

# On node 3 (10.0.0.3)
AEON_CLUSTER_NODE_ID=3 AEON_CLUSTER_PEERS="10.0.0.1:4470,10.0.0.2:4470" \
  aeon serve --addr 0.0.0.0:4471 --artifact-dir /var/lib/aeon/n3

# Confirm Raft formed (run on any node)
aeon cluster status --api http://10.0.0.1:4471
# Expect: leader present, members [{id:1},{id:2},{id:3}], term > 0.
```

The bootstrap sequence:

1. Node 1 initializes a single-node Raft cluster and becomes leader
2. Nodes 2 and 3 connect to Node 1 via QUIC and join as **learners**
3. Learners receive the Raft log (via snapshot for fast catch-up)
4. Once caught up, learners are promoted to **voters** (one at a time)
5. The leader computes a balanced partition assignment (~5-6 partitions per node)
6. Partitions are transferred to their new owners via QUIC

After bootstrap, the partition distribution looks approximately like:

```
Node 1 (Leader):    P0  P1  P2  P3  P4  P15
Node 2 (Follower):  P5  P6  P7  P8  P9
Node 3 (Follower):  P10 P11 P12 P13 P14
```

### 3.3 mTLS certificate setup

All inter-node QUIC connections require mutual TLS (mTLS). Each node presents its own
certificate, and verifies the peer's certificate against a shared Certificate Authority (CA).

#### Generate a CA

```bash
# Generate CA private key
openssl genpkey -algorithm RSA -out ca.key -pkeyopt rsa_keygen_bits:4096

# Generate self-signed CA certificate (valid 10 years)
openssl req -x509 -new -nodes -key ca.key -sha256 -days 3650 \
    -out ca.pem -subj "/CN=Aeon Cluster CA"
```

#### Generate node certificates

Repeat for each node, replacing `node1` and the IP/DNS as appropriate:

```bash
# Generate node private key
openssl genpkey -algorithm RSA -out node1.key -pkeyopt rsa_keygen_bits:2048

# Generate certificate signing request
openssl req -new -key node1.key -out node1.csr \
    -subj "/CN=aeon-node1"

# Create an extensions file for SAN (Subject Alternative Names)
cat > node1-ext.cnf <<EOF
[v3_req]
subjectAltName = @alt_names

[alt_names]
IP.1 = 10.0.0.1
DNS.1 = aeon-node1
DNS.2 = aeon-node1.example.com
EOF

# Sign with the CA
openssl x509 -req -in node1.csr -CA ca.pem -CAkey ca.key \
    -CAcreateserial -out node1.pem -days 365 -sha256 \
    -extfile node1-ext.cnf -extensions v3_req
```

#### Distribute certificates

On each node, place the following files:

```
/etc/aeon/tls/
  ca.pem       # Same CA cert on all nodes
  nodeN.pem    # This node's certificate
  nodeN.key    # This node's private key (restrict permissions: chmod 600)
```

The TLS configuration in the YAML manifest points to these paths:

```yaml
cluster:
  tls:
    cert: /etc/aeon/tls/node1.pem
    key: /etc/aeon/tls/node1.key
    ca: /etc/aeon/tls/ca.pem
```

#### Security notes

- The TLS stack is rustls + aws-lc-rs, which provides a FIPS 140-3 certification path.
- All cluster nodes mutually authenticate via mTLS -- unauthorized nodes cannot join.
- Certificate rotation requires restarting nodes with updated certificate files.
- In development/testing, TLS can be omitted (leave `tls` unset), but this is not
  recommended for production.

---

## 4. Adding Nodes to an Existing Cluster

### 4.1 Scaling from single-node to multi-node

If you started with a single-node deployment and want to expand to a 3-node cluster,
the new nodes use `seed_nodes` instead of `peers` to discover the existing cluster.

**New node configuration** (`aeon-node4.yaml`):

```yaml
cluster:
  node_id: 4
  bind: "0.0.0.0:4470"
  num_partitions: 16          # Must match existing cluster
  seed_nodes:
    - "10.0.0.1:4470"        # Any existing cluster member
  tls:
    cert: /etc/aeon/tls/node4.pem
    key: /etc/aeon/tls/node4.key
    ca: /etc/aeon/tls/ca.pem

http:
  bind: "0.0.0.0:4471"
```

The seed node returns the current Raft membership. The new node joins as a learner
through the leader. You only need one reachable seed node, though listing multiple
increases resilience against a single seed being temporarily unavailable.

For Kubernetes deployments, `seed_nodes` can point to a headless Service DNS name
that resolves to existing pod IPs.

### Add via CLI

```bash
# Add a node (joins as learner, auto-promotes, auto-rebalances)
aeon cluster add 10.0.0.4:4470

# Check cluster state
aeon cluster status

# Manually trigger a rebalance (usually automatic)
aeon cluster rebalance
```

### The join sequence

1. New node contacts seed node via QUIC
2. Seed returns current Raft membership and leader address
3. New node joins as a **learner** through the leader
4. Learner receives Raft snapshot (fast catch-up -- kilobytes, not gigabytes)
5. Once caught up, learner is promoted to **voter** (one at a time)
6. Leader computes new partition assignment and transfers partitions via QUIC

### Removing a node

```bash
# Remove a node (drains partitions first, then removes from Raft)
aeon cluster remove 10.0.0.4:4470
```

The removal sequence:

1. The departing node's partitions are transferred to remaining nodes via QUIC
2. Each partition: pause -> transfer state + PoH tip + Source-Anchor offset -> resume on target
3. Once the departing node has zero partitions, it is removed from the Raft voter set
4. The node shuts down cleanly after removal is committed

### 4.2 Best practice: odd number of nodes

Aeon enforces **odd-number cluster sizes** for Raft consensus. This is validated at
startup -- even-number configurations are rejected with an error:

```
error: cluster size must be odd (got 4). Even-number clusters waste a node
without improving fault tolerance
```

#### Why odd numbers are required

Raft requires a **quorum** (strict majority) of nodes to agree on every committed
operation. The quorum formula is:

```
quorum = floor(N / 2) + 1
```

Where N is the total number of voting members.

With odd-numbered clusters, every additional pair of nodes increases fault tolerance
by exactly one:

| Nodes | Quorum | Fault tolerance | Use case |
|-------|--------|-----------------|----------|
| 1 | 1 | 0 failures | Development, small workloads |
| 3 | 2 | 1 failure | Minimum production cluster |
| 5 | 3 | 2 failures | Recommended production |
| 7 | 4 | 3 failures | Large-scale, geo-distributed |

The fault tolerance is `N - quorum`, which equals `floor((N - 1) / 2)` for odd N.

### 4.3 What happens with even numbers (the even-number trap)

Even-number clusters provide **no additional fault tolerance** over the next smaller
odd-number cluster, while adding network overhead and increasing split-brain risk.

| Nodes | Quorum | Fault tolerance | Equivalent odd cluster |
|-------|--------|-----------------|------------------------|
| 2 | 2 | 0 failures | Worse than 1 node (added complexity, same tolerance) |
| 4 | 3 | 1 failure | Same as 3 nodes (wastes 1 node) |
| 6 | 4 | 2 failures | Same as 5 nodes (wastes 1 node) |
| 8 | 5 | 3 failures | Same as 7 nodes (wastes 1 node) |

#### The 2-node case: worse than single-node

A 2-node cluster requires quorum = 2 (both nodes must agree). If either node fails,
the cluster loses quorum and becomes **unavailable**. This is strictly worse than a
single-node deployment, which at least continues operating as long as the one node
is alive. A 2-node cluster adds network latency for Raft replication with zero
fault tolerance benefit.

#### The 4-node case: same as 3, more overhead

A 4-node cluster requires quorum = 3. It can tolerate exactly 1 failure -- the same
as a 3-node cluster. But the extra node:

- Consumes resources (CPU, memory, network)
- Increases Raft replication fan-out (leader must replicate to 3 followers instead of 2)
- Increases the chance of a split-brain scenario during network partitions

**Concrete example**: If you have 4 nodes and 2 fail simultaneously, quorum is lost
(only 2 of 4 alive, need 3). With 3 nodes and 1 failure, you still have quorum (2
of 3 alive, need 2). The 4-node cluster tolerates the same number of failures as
the 3-node cluster, but costs 33% more infrastructure.

#### Split-brain risk with even numbers

During a network partition, an even-number cluster can split into two equal halves
(e.g., 2|2 in a 4-node cluster). Neither half has a majority, so the entire cluster
becomes unavailable. An odd-number cluster always has an unambiguous majority side
(e.g., 2|1 in a 3-node cluster -- the side with 2 retains quorum).

### 4.4 Scaling safety rules

1. **One membership change at a time** -- never add or remove two voters simultaneously.
   This is a Raft safety invariant enforced by openraft.
2. **Learner-first on join** -- new nodes sync the Raft log as learners before becoming voters.
3. **Drain-first on leave** -- departing nodes transfer all partitions before removal from
   the voter set.
4. **Odd-number enforcement** -- transitions pass through even-number states briefly during
   individual promotions/removals, but the **target** cluster size must be odd.
5. **No data loss** -- partition transfer uses two-phase sync. Source-Anchor offsets are
   transferred with partition ownership.
6. **Pipeline continuity** -- partitions not being transferred continue processing normally.
   Only partitions being moved experience a brief pause during cutover (milliseconds).
7. **Leader failure during scaling** -- if the leader dies mid-transfer, the new leader
   checks the target's acknowledgement state. If bulk sync is complete, proceed to cutover.
   Otherwise, abort and revert to `Owned(source)`. Transfers are idempotent and can be retried.

---

## 5. Pipeline Distribution

### 5.1 Leader election

Raft leader election is automatic. When a cluster starts or when the current leader
becomes unavailable, a new leader is elected using Raft's term-based voting protocol.

Key Raft timing parameters (configured in `ClusterNode`):

| Parameter | Value | Description |
|-----------|-------|-------------|
| `heartbeat_interval` | 500ms | Leader sends heartbeats to followers |
| `election_timeout_min` | 1500ms | Minimum time before a follower starts an election |
| `election_timeout_max` | 3000ms | Maximum election timeout (randomized for stagger) |

The leader is responsible for:

- Accepting Raft write proposals (partition assignments, pipeline creation, config changes)
- Computing partition rebalance plans
- Coordinating partition transfers between nodes
- Producing global PoH checkpoints

Followers forward write requests to the leader. Read requests can be served by any node.

### 5.2 Pipeline scheduling and load balancing

Each pipeline is assigned to a specific node via the `assigned_node` field in its
`PipelineDefinition`. The leader determines the assignment based on partition ownership
and load balancing.

Partitions are distributed across nodes using a round-robin algorithm during initial
assignment. The `compute_rebalance` function in the Partition Manager computes the
minimum set of partition moves to balance load when nodes join or leave:

- Each node should own approximately `num_partitions / num_nodes` partitions
- If the division is uneven, some nodes get one extra partition
- Rebalancing minimizes moves -- only partitions on overloaded nodes are transferred

### 5.3 What happens during node failure

When a node becomes unreachable:

1. **Raft detects the failure** via missed heartbeats (within `election_timeout_max`, ~3s)
2. **If the failed node was the leader**, a new leader is elected automatically
3. **Partition reassignment** -- the new leader reassigns the failed node's partitions
   to the remaining healthy nodes
4. **Pipeline recovery** -- pipelines assigned to the failed node are rescheduled to
   healthy nodes. Source-Anchor offsets ensure exactly-once processing restarts from
   the last committed position (uncommitted Kafka offsets replay naturally)
5. **Rebalancing** -- the leader distributes orphaned partitions evenly across remaining nodes

If the failed node returns later, it rejoins as a learner, syncs the Raft log, and
may receive partitions back during a subsequent rebalance.

In a network partition (e.g., 2|3 in a 5-node cluster), the majority side (3 nodes)
retains quorum and reassigns orphaned partitions from the unreachable minority. When
the partition heals, minority-side nodes rejoin as learners.

---

## 6. Adding Pipeline Configurations

Pipelines are the unit of deployment in Aeon. Each pipeline connects a source, a
processor, and a sink. Pipelines can be created via the REST API, the CLI, or
declarative YAML manifests.

### 6.1 Via REST API

**Create a pipeline:**

```bash
curl -X POST http://localhost:4471/api/v1/pipelines \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer ${AEON_API_TOKEN}" \
  -d '{
    "name": "enrichment-pipeline",
    "source": {
      "type": "kafka",
      "topic": "raw-events",
      "partitions": [0, 1, 2, 3],
      "config": {
        "bootstrap.servers": "redpanda:9092",
        "group.id": "aeon-enrichment"
      }
    },
    "processor": {
      "name": "event-enricher",
      "version": "1.0.0"
    },
    "sink": {
      "type": "kafka",
      "topic": "enriched-events",
      "config": {
        "bootstrap.servers": "redpanda:9092"
      }
    },
    "upgrade_strategy": "drain-swap"
  }'
```

**List pipelines:**

```bash
curl http://localhost:4471/api/v1/pipelines \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"
```

**Inspect a pipeline:**

```bash
curl http://localhost:4471/api/v1/pipelines/enrichment-pipeline \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"
```

**Start a pipeline:**

```bash
curl -X POST http://localhost:4471/api/v1/pipelines/enrichment-pipeline/start \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"
```

**Stop a pipeline:**

```bash
curl -X POST http://localhost:4471/api/v1/pipelines/enrichment-pipeline/stop \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"
```

**Delete a pipeline:**

```bash
curl -X DELETE http://localhost:4471/api/v1/pipelines/enrichment-pipeline \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"
```

**View pipeline lifecycle history:**

```bash
curl http://localhost:4471/api/v1/pipelines/enrichment-pipeline/history \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"
```

**Upgrade a pipeline's processor:**

```bash
curl -X POST http://localhost:4471/api/v1/pipelines/enrichment-pipeline/upgrade \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer ${AEON_API_TOKEN}" \
  -d '{
    "processor": {
      "name": "event-enricher",
      "version": "2.0.0"
    }
  }'
```

> **Note**: When `AEON_API_TOKEN` environment variable is not set, authentication is
> disabled (development mode). Health endpoints (`/health`, `/ready`) always bypass
> authentication.

### 6.2 Via CLI

```bash
# Create a pipeline from a JSON definition
aeon pipeline create --name enrichment-pipeline \
  --source kafka --source-topic raw-events \
  --processor event-enricher:1.0.0 \
  --sink kafka --sink-topic enriched-events

# Start a pipeline
aeon pipeline start enrichment-pipeline

# Stop a pipeline
aeon pipeline stop enrichment-pipeline

# List all pipelines
aeon pipeline list

# Inspect a pipeline
aeon pipeline inspect enrichment-pipeline

# Delete a pipeline
aeon pipeline delete enrichment-pipeline
```

### 6.3 Via declarative YAML manifest

The `aeon apply` command applies a declarative manifest containing one or more pipeline
definitions. This is the recommended approach for production deployments and GitOps
workflows.

**Single pipeline manifest** (`pipeline.yaml`):

```yaml
pipelines:
  - name: enrichment-pipeline
    source:
      type: kafka
      topic: raw-events
      partitions: [0, 1, 2, 3]
      config:
        bootstrap.servers: "redpanda:9092"
    processor:
      name: event-enricher
      version: "1.0.0"
    sink:
      type: kafka
      topic: enriched-events
      config:
        bootstrap.servers: "redpanda:9092"
    upgrade_strategy: drain-swap
```

Apply the manifest:

```bash
# Apply (creates pipelines that do not exist, skips existing ones)
aeon apply -f pipeline.yaml

# Dry run -- show what would change without applying
aeon apply -f pipeline.yaml --dry-run
```

The manifest maximum size is 1 MB (enforced to prevent resource exhaustion).

### 6.4 Multiple pipelines in a single manifest

```yaml
pipelines:
  - name: ingest-raw
    source:
      type: kafka
      topic: raw-events
      config:
        bootstrap.servers: "redpanda:9092"
    processor:
      name: validator
      version: "1.0.0"
    sink:
      type: kafka
      topic: validated-events
      config:
        bootstrap.servers: "redpanda:9092"

  - name: enrich-validated
    source:
      type: kafka
      topic: validated-events
      config:
        bootstrap.servers: "redpanda:9092"
    processor:
      name: enricher
      version: "2.1.0"
    sink:
      type: kafka
      topic: enriched-events
      config:
        bootstrap.servers: "redpanda:9092"

  - name: analytics-sink
    source:
      type: kafka
      topic: enriched-events
      config:
        bootstrap.servers: "redpanda:9092"
    processor:
      name: analytics-router
      version: "1.0.0"
    sink:
      type: stdout
```

---

## 7. Multi-Pipeline Configuration Examples

### 7.1 Kafka-to-Kafka pipeline with Wasm processor

A common pattern: consume from a Kafka/Redpanda topic, process with a Wasm module,
and produce to another topic.

```yaml
pipelines:
  - name: fraud-detection
    source:
      type: kafka
      topic: transactions
      partitions: [0, 1, 2, 3, 4, 5, 6, 7]
      config:
        bootstrap.servers: "redpanda-0:9092,redpanda-1:9092,redpanda-2:9092"
        auto.offset.reset: earliest
    processor:
      name: fraud-scorer
      version: "3.2.1"
    sink:
      type: kafka
      topic: fraud-alerts
      config:
        bootstrap.servers: "redpanda-0:9092,redpanda-1:9092,redpanda-2:9092"
        acks: all
    upgrade_strategy: canary
```

Register the Wasm processor first:

```bash
# Register the Wasm processor artifact
curl -X POST http://localhost:4471/api/v1/processors \
  -H "Authorization: Bearer ${AEON_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "fraud-scorer",
    "description": "ML-based transaction fraud scoring",
    "version": "3.2.1",
    "processor_type": "wasm",
    "platform": "wasm32",
    "sha512": "<sha512-of-wasm-artifact>",
    "size_bytes": 245760
  }'
```

### 7.2 HTTP webhook to Redis Streams

Receive events via an HTTP webhook source and push to Redis Streams.

```yaml
pipelines:
  - name: webhook-to-redis
    source:
      type: http
      config:
        bind: "0.0.0.0:8080"
        path: "/webhooks/events"
        max_body_size: "1048576"
    processor:
      name: webhook-normalizer
      version: "1.0.0"
    sink:
      type: redis
      config:
        url: "redis://redis-cluster:6379"
        stream: "incoming-events"
        maxlen: "100000"
```

### 7.3 NATS to File with native processor

Consume from a NATS subject, process with a native Rust shared library, and write
to files on disk.

```yaml
pipelines:
  - name: nats-archiver
    source:
      type: nats
      config:
        url: "nats://nats-server:4222"
        subject: "telemetry.>"
        queue_group: "aeon-archiver"
    processor:
      name: telemetry-compressor
      version: "2.0.0"
    sink:
      type: file
      config:
        path: "/data/archive/telemetry"
        format: parquet
        rotation: "1h"
        max_file_size: "256MB"
```

### 7.4 PostgreSQL CDC to Kafka

Capture changes from PostgreSQL via Change Data Capture and publish to Kafka.

```yaml
pipelines:
  - name: pg-cdc-to-kafka
    source:
      type: postgres-cdc
      config:
        connection_string: "postgresql://aeon:secret@pg-primary:5432/mydb"
        slot_name: "aeon_cdc_slot"
        publication: "aeon_pub"
        tables: "orders,customers,inventory"
    processor:
      name: cdc-transformer
      version: "1.1.0"
    sink:
      type: kafka
      topic: cdc-events
      config:
        bootstrap.servers: "kafka:9092"
        acks: all
```

### 7.5 Multiple pipelines in a single manifest (complete example)

A production manifest combining multiple pipeline types:

```yaml
pipelines:
  # Pipeline 1: Core event enrichment (Kafka -> Kafka)
  - name: event-enrichment
    source:
      type: kafka
      topic: raw-events
      partitions: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
      config:
        bootstrap.servers: "redpanda:9092"
    processor:
      name: enricher
      version: "4.0.0"
    sink:
      type: kafka
      topic: enriched-events
      config:
        bootstrap.servers: "redpanda:9092"
    upgrade_strategy: blue-green

  # Pipeline 2: Fraud detection (Kafka -> Kafka)
  - name: fraud-detection
    source:
      type: kafka
      topic: enriched-events
      config:
        bootstrap.servers: "redpanda:9092"
    processor:
      name: fraud-scorer
      version: "3.2.1"
    sink:
      type: kafka
      topic: fraud-alerts
      config:
        bootstrap.servers: "redpanda:9092"
    upgrade_strategy: canary

  # Pipeline 3: Archive to object storage (Kafka -> File)
  - name: event-archiver
    source:
      type: kafka
      topic: enriched-events
      config:
        bootstrap.servers: "redpanda:9092"
    processor:
      name: parquet-converter
      version: "1.0.0"
    sink:
      type: file
      config:
        path: "/data/archive"
        format: parquet
        rotation: "1h"

  # Pipeline 4: Real-time monitoring sink (Kafka -> NATS)
  - name: monitoring-fanout
    source:
      type: kafka
      topic: enriched-events
      config:
        bootstrap.servers: "redpanda:9092"
    processor:
      name: metrics-extractor
      version: "2.0.0"
    sink:
      type: nats
      config:
        url: "nats://nats:4222"
        subject: "monitoring.events"
```

Apply the full manifest:

```bash
aeon apply -f production-pipelines.yaml
```

---

## 8. Monitoring a Cluster

### 8.1 Health checks

Every Aeon node exposes health and readiness endpoints on the HTTP management port (4471).
These endpoints do **not** require authentication.

```bash
# Health check -- returns 200 if the process is alive
curl http://10.0.0.1:4471/health
# {"status":"ok","version":"0.1.0"}

# Readiness check -- returns 200 if the node is ready to serve traffic
curl http://10.0.0.1:4471/ready
# {"status":"ready","version":"0.1.0"}
```

**Kubernetes probes** (example):

```yaml
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

### 8.2 Cluster status

```bash
# CLI cluster status (shows all nodes, roles, partition counts)
aeon cluster status
```

### 8.3 Pipeline status

```bash
# List all pipelines with their states
curl http://localhost:4471/api/v1/pipelines \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"

# Inspect a specific pipeline (includes assigned_node, state, processor version)
curl http://localhost:4471/api/v1/pipelines/enrichment-pipeline \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"

# Delivery tracking status
curl http://localhost:4471/api/v1/pipelines/enrichment-pipeline/delivery \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"
```

### 8.4 Processor registry

```bash
# List all registered processors
curl http://localhost:4471/api/v1/processors \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"

# Inspect a processor (all versions)
curl http://localhost:4471/api/v1/processors/enricher \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"

# List versions of a specific processor
curl http://localhost:4471/api/v1/processors/enricher/versions \
  -H "Authorization: Bearer ${AEON_API_TOKEN}"
```

### 8.5 Key metrics to monitor

When running Aeon in production, track the following:

| Metric | Description | Alert threshold |
|--------|-------------|-----------------|
| Raft leader changes | Number of leader elections | > 2 per hour |
| Raft log lag | Follower log index behind leader | > 1000 entries |
| Partition transfer duration | Time for partition moves | > 30 seconds |
| Pipeline state | Pipelines in Failed state | Any |
| Event processing latency (P99) | End-to-end per-event latency | > 100ms |
| QUIC connection pool | Active connections per node | Drops to 0 unexpectedly |
| Health endpoint response | HTTP status code | != 200 |

---

## 9. Capacity Planning

### 9.1 Node count vs pipeline count

Each Aeon node can run multiple pipelines. The primary constraints are:

- **CPU cores**: Each pipeline's hot path (source -> processor -> sink) benefits from
  dedicated cores. Plan for 2-4 cores per active pipeline.
- **Memory**: Base Aeon overhead is ~100MB. Each pipeline adds memory for SPSC ring buffers,
  processor state, and Kafka consumer/producer buffers. Plan for 256MB-1GB per pipeline.
- **Network bandwidth**: Dominated by event throughput to/from Kafka/Redpanda. The QUIC
  inter-node traffic is minimal (Raft log entries are small).

### 9.2 Partition count guidelines

| Expected max nodes | Recommended partitions | Rationale |
|--------------------|------------------------|-----------|
| 1-3 | 16 | 5+ partitions per node provides good parallelism |
| 3-7 | 32 | 4+ partitions per node; room to grow |
| 7-15 | 64 | 4-9 partitions per node |
| 15+ | 128 | Future-proofed for large clusters |

Partition count is **immutable** after cluster creation. Choosing too few partitions
limits your maximum cluster size (partitions cannot be split). Choosing too many adds
minor overhead for partition management metadata. Err on the side of more partitions.

### 9.3 Recommended cluster sizes

| Deployment | Nodes | Pipelines | Use case |
|------------|-------|-----------|----------|
| Development | 1 | 1-5 | Local development, testing |
| Small production | 3 | 5-15 | Low-to-medium throughput workloads |
| Medium production | 5 | 15-50 | High throughput, 2-failure tolerance |
| Large production | 7 | 50+ | Mission-critical, geo-distributed |

### 9.4 Hardware recommendations

**Per node (production)**:

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 8 cores | 16+ cores |
| Memory | 16 GB | 64+ GB |
| Disk | SSD (for state store) | NVMe |
| Network | 1 Gbps | 10 Gbps |

**Aggregate throughput target**: Aeon targets 1-2M events/sec per instance. A 10-node
cluster can achieve approximately 10-20M events/sec aggregate, with the goal that Aeon
is never the bottleneck -- infrastructure (Kafka, network) determines absolute throughput.

### 9.5 Cost optimization

- Start with 3 nodes and scale up based on observed load
- Use the partition count formula (`num_partitions >= max_expected_nodes * 4`) to
  future-proof without over-provisioning nodes
- Monitor per-node CPU utilization: if consistently below 30%, you may be over-provisioned
- Remember the even-number trap (Section 4.3): always use odd node counts to avoid
  wasting infrastructure on nodes that do not improve fault tolerance
