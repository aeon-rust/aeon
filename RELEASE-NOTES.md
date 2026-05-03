# Aeon v0.1.0 — Release Notes

**Released: 2026-05-03**

> Aeon is a real-time data processing engine targeting 20M events/sec
> aggregate throughput. Language-agnostic processors (Rust native /
> Wasm / WebTransport / WebSocket), tiered state store (L1/L2/L3),
> EO-2 exactly-once durability, cryptographic event chain (PoH +
> Merkle + MMR + Ed25519), GDPR-aware compliance gating at pipeline
> start, multi-node Raft cluster, deploys on Kubernetes / VMs /
> baremetal.

## Try it in 60 seconds

```bash
docker pull ghcr.io/aeon-rust/aeon:latest
docker run -d --name aeon -p 4471:4471 -v aeon-data:/app/artifacts ghcr.io/aeon-rust/aeon:latest
curl http://localhost:4471/health
# → {"status":"ok","version":"0.1.0"}
```

Your first pipeline (Memory source → Blackhole sink, 10K events):

```bash
docker exec -i aeon /usr/local/bin/aeon apply -f - <<'EOF'
pipelines:
  - name: hello-aeon
    partitions: 1
    sources:
      - name: synth
        type: memory
        kind: push
        identity: { mode: random }
        event_time: { mode: aeon_ingest }
        count: "10000"
        payload_size: "256"
        batch_size: "256"
    processor:
      name: __identity
      version: "0.0.0"
    sinks:
      - name: discard
        type: blackhole
        eos_tier: t6_fire_and_forget
EOF

curl -s http://localhost:4471/metrics | grep aeon_pipeline_outputs
```

For the full developer onboarding (build from source, multi-node
cluster via Helm, per-connector recipes), see
[`docs/CONNECTOR-COOKBOOK.md`](docs/CONNECTOR-COOKBOOK.md) and the
README quickstart sections.

## What's in this release

### The wedge

Three things Aeon ships in v0.1.0 that no other established stream
processor offers natively:

1. **Verifiable event chain on the hot path** — PoH (Proof of History)
   for deterministic event ordering, Merkle tree per partition for
   per-batch verification, MMR (Merkle Mountain Range) for chain-wide
   proofs, Ed25519 root signatures for event-level authenticity. The
   chain is computed inline (not bolted on) — no separate audit
   pipeline, no replay-to-verify step.
2. **Compliance-regime enforcement at pipeline start** — PCI-DSS /
   HIPAA / GDPR regimes as first-class declarative config. The engine
   refuses to start a pipeline missing required encryption / retention
   / erasure preconditions. Not a linter; not operator responsibility;
   a runtime precondition checked by `compliance_validator`.
3. **GDPR right-to-erasure + right-to-export as engine primitives** —
   subject-id extractor on ingest; tombstone store + deny-list across
   L2 body and L3 checkpoints; cryptographic null-receipt on erase
   (the chain still verifies after deletion); configurable retention
   per-tier.

### Performance (Gate 1 — local validation)

- Per-event overhead: **<100 ns** (Gate 1 target met)
- Headroom ratio: **130x** (Gate 1 target ≥ 5x)
- CPU utilization: **18.7%** when Redpanda is saturated (Gate 1 target < 50%)
- Linear partition scaling verified 1 → 8 partitions

**Throughput ceiling on premium hardware (AWS EKS i4i / i3en):
pending.** Session B is queued; results land in v0.1.1 or v0.2 release
notes alongside the EKS bake.

### Connectors (CLI-registered, manifest-callable)

| Type key | Source | Sink |
|---|---|---|
| `memory` | ✅ | — |
| `kafka` (covers Redpanda) | ✅ | ✅ |
| `http-webhook` | ✅ | — |
| `http-polling` | ✅ | — |
| `http` | — | ✅ |
| `file` | ✅ | ✅ |
| `websocket` | ✅ | ✅ |
| `redis-streams` | ✅ | ✅ |
| `nats` | ✅ | ✅ |
| `postgres-cdc` | ✅ | — |
| `mysql-cdc` | ✅ | — |
| `mongodb-cdc` | ✅ | — |
| `blackhole` | — | ✅ |
| `stdout` | — | ✅ |

11 connectors usable as sources, 8 as sinks. Each has a
copy-pasteable recipe in [docs/CONNECTOR-COOKBOOK.md](docs/CONNECTOR-COOKBOOK.md)
and a runnable example fixture under `docs/examples/`.

**Implemented but not yet CLI-registered** (`aeon-connectors` Rust API
only — wiring follows in v0.1.x): MQTT, RabbitMQ, QUIC, WebTransport.

### Processor tiers (language-agnostic)

| Tier | Transport | Languages | Latency |
|---|---|---|---|
| **T1 Native** | In-process (C-ABI) | Rust, C/C++, .NET NativeAOT | ~240 ns |
| **T2 Wasm** | In-process (Wasmtime) | Rust, AssemblyScript | ~1.1 µs |
| **T3 WebTransport** | QUIC/UDP | Any with WT client | ~5–15 µs |
| **T4 WebSocket** | TCP/WS | Python, Go, Node.js, Java, PHP, C#, Rust | ~30–80 µs |

SDKs published for Python, Go, Node.js, C#/.NET, Java, PHP, C/C++,
Rust, AssemblyScript.

### Cluster correctness (closed in this release)

A class of bugs surfaced and closed during pre-v0.1 validation:

- **G9.b** — REST cluster-write requests against a Raft follower
  silently no-op'd because the original 307 redirect path wasn't
  followed by the `aeon-cli` HTTP client (per RFC 7231 §6.4.7). New
  `cluster_write_forwarder` middleware proxies every cluster-write
  request through to the leader transparently. Operators can call any
  pod (`kubectl exec` into any one) for any cluster-write op.
- **delete_processor_version Raft-replicated** — pre-fix, deleting
  a processor version landed only on the leader pod; followers kept
  the version, risking stale loads. Now uses the existing
  `RegistryCommand::DeleteVersion` Raft variant.
- **G9.c** — 7 lifecycle endpoints (blue-green, canary, cutover,
  rollback, promote, reconfigure-source, reconfigure-sink) gained 7
  new `RegistryCommand` variants + cluster_applier dispatch +
  supervisor side-effect on every node. Backfilled the existing
  `UpgradePipeline` to also propagate to followers.
- **P5.b** — per-sink ack-seq carry-on-transfer audit closed as
  covered-by-existing-design (Aeon's EOS tier matrix absorbs the
  duplicate-delivery edge case).

### Distribution

- Container images at `ghcr.io/aeon-rust/aeon` (GHCR; chosen over
  Docker Hub which discontinued free orgs in 2026)
- Tags published per release: `:vNN`, `:<short-sha>`, `:latest`
- OCI labels link the image to the source repo on the GitHub
  Packages tab
- Helm chart in-tree at `helm/aeon/` — supports Deployment
  (single-node) and StatefulSet (multi-node Raft cluster)

### Documentation (also in this release)

- [`docs/CONNECTOR-COOKBOOK.md`](docs/CONNECTOR-COOKBOOK.md) — single
  source of truth for connector wiring; 14 connectors with copy-paste
  fragments; flat-schema rule made explicit
- [`docs/STATEFUL-PROCESSING-EVOLUTION.md`](docs/STATEFUL-PROCESSING-EVOLUTION.md) —
  v0.2 trajectory framing Aeon's existing primitives in their own
  vocabulary (no Flink terminology dump); layered progression toward
  windowing
- README quickstart paths (A: GHCR pull / B: build from source /
  C: Helm cluster)
- [`docs/CLUSTERING.md`](docs/CLUSTERING.md) §3.2.1 documents the
  transparent leader-routing contract for cluster-write endpoints
- CLI verb drift across CLUSTERING / INSTALLATION / BUILD-FROM-SOURCE
  reconciled to the authoritative set in `aeon-cli/src/main.rs`

### Test coverage

- **1,717 Rust workspace tests pass** (lib + integration + doc tests
  across 14 crates), 0 failures, 16 ignored (`webtransport-host`
  feature-gated)
- clippy clean (`-D warnings`); rustfmt clean
- 31 Python SDK tests, 20 Go SDK tests, 32 Node.js SDK tests, 40
  C#/.NET SDK tests, 33 PHP SDK tests, 28 Java SDK tests, 22 C/C++
  SDK tests

## What's next (v0.2)

The integrated trajectory toward stateful processing — see
[`docs/STATEFUL-PROCESSING-EVOLUTION.md`](docs/STATEFUL-PROCESSING-EVOLUTION.md)
for the full layered plan:

- **Session B (AWS EKS)** — premium-hardware throughput ceiling
  number to substantiate the v0.1 wedge's "Rust-class per-event
  overhead" claim. Postponed; resumes when ECR bake is operationally
  scheduled.
- **G9.d (Layer 4)** — per-partition Raft-committed sequence boundary
  maps + partition-aware `PipelineControl::drain_partitions_at_seq()`.
  Foundation for L5/L6.
- **L5 (Layer 5)** — `WatermarkView` façade exposing Aeon's existing
  PoH chain head + AckSeqTracker + Event.timestamp derivations as a
  uniform per-partition watermark API.
- **L6 (Layer 6)** — Windowing F1+F2 (tumbling / sliding / session
  windows; per-key state in L1/L2/L3; watermark-based triggers) per
  the deferred design in
  [`docs/WINDOWING-WATERMARKS-DESIGN.md`](docs/WINDOWING-WATERMARKS-DESIGN.md).
  Keeps original prerequisites (Session B + at least one user demand).

## Acknowledgements

Aeon is developed using [Claude](https://claude.ai) (Anthropic) as an
AI coding partner via [Claude Code](https://claude.ai/claude-code).
The `.claude/` directory and `CLAUDE.md` contain the project
instructions and coding guidelines that shape how Claude assists with
development.

## License

Apache-2.0
