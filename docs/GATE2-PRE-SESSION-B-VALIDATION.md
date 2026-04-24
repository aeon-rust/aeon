# Gate 2 — Pre-Session-B Rancher Desktop Validation

**Purpose.** Rehearse the cluster-level test matrix on Rancher Desktop (k3s,
loopback QUIC, single-node host) before re-spinning DOKS for Session A
re-run or provisioning EKS for Session B. Catch code gaps cheaply; carry
forward the test-realism gaps that single-node RD cannot cover.

**Scope.** V1 cluster bring-up · V2 T0-T6 matrix · V3 processor validation
(native + Wasm) · V5 crypto-chain E2E · V6 consolidated report (this file).
T5 split-brain and T6 multi-node chaos are **explicitly deferred** to the
next DOKS re-spin where Chaos Mesh can apply genuine NetworkChaos between
real nodes — single-node RD cannot faithfully simulate either.

**Source of truth for the underlying matrix:**
[`GATE2-ACCEPTANCE-PLAN.md`](GATE2-ACCEPTANCE-PLAN.md) § 12 (acceptance
methodology, success criteria) and § 10 (Session A result schema that
this document mirrors).

---

## ⚠ Prerequisite — Image SHA alignment

The running RD cluster image (verified 2026-04-24 14:00 UTC) predates
the current feature-branch HEAD that ships Phases 1–4 of the
2026-04-24 session (S4.3 CLI, S10 mTLS for Redis/NATS/PG-CDC/MySQL-CDC/
Mongo-CDC, P5.c factories, S2.5 audit wiring). Local CLI calls against
this cluster fail at unrelated API endpoints (e.g. processor register
returns EOF from an older REST schema). Before running V2/V3/V5 the
operator must:

1. `docker build . -t aeonrust/aeon:pre-session-b` (or equivalent
   nerdctl for Rancher Desktop) from the current feature-branch SHA.
2. Reload the Helm values to point at the new tag: `helm upgrade
   aeon helm/aeon -n aeon --set image.tag=pre-session-b --reuse-values`.
3. Verify `kubectl rollout status sts/aeon -n aeon` completes cleanly
   and pods stabilise.
4. Re-run the V1 cluster-baseline checks.

This is tracked as **blocker 0** for any live load-test run in this
cycle; without it the CLI and the serving binary can disagree on
request schemas.

## V1 — Cluster Baseline (loopback STS on RD)

| Check | Method | Result |
|-------|--------|--------|
| 3-replica Aeon StatefulSet | `kubectl get pods -n aeon` | ✅ `aeon-0`, `aeon-1`, `aeon-2` all `1/1 Running` (age 13h, 1 restart each at 90m) |
| Helm install | `helm list -A` | ✅ `aeon-0.1.0` chart at revision 2, status `deployed` |
| Redpanda peer | `kubectl get pods -n aeon` | ✅ `redpanda-0` running, init job completed |
| Services wired | `kubectl get svc -n aeon` | ✅ `aeon` (4470 UDP / 4471 TCP / 4472 UDP) + `aeon-headless` (4470 UDP / 4471 TCP) + `redpanda-external` (NodePort 31092) |
| `/metrics` endpoint | port-forward + curl | ✅ Prometheus exposition served; counters for events_received / processed / outputs_sent / outputs_acked / failed / retried / checkpoints_written / poh_entries all registered |
| `/api/v1/cluster/status` | port-forward + curl | ✅ 3-node membership `{1, 2, 3}` resolved via `aeon-N.aeon-headless` DNS; leader = node 3; term 2; last_applied = 2 |
| Partition table | status JSON | ✅ 12 partitions, balanced 4/4/4 across owners 1/2/3, all status `owned` |
| `aeon cluster status --watch` | ✅ shipped in V1 (issue #79) |

**V1 verdict:** cluster bring-up on RD is healthy end-to-end — Helm /
StatefulSet / DNS / mDNS / Raft / partition ownership all match the
expected shape from Session 0. No code gaps surfaced in the boot path.

---

## V2 — T0-T6 Test Matrix

| Test | RD realism | Exercise | Issue | Status |
|------|-----------|----------|-------|--------|
| T0 | ✅ full | Baseline pipeline: Memory → Blackhole with streaming count, sustained 3-minute sweep; confirm outputs_acked_total == input with zero loss | #80 | ⏳ **needs dedicated session** — plan below |
| T2 | ✅ code path | 3 → 5 STS-scale (code exercise only; RD has no node pool to resize, so pods go Pending and we assert the G10 seed-join code path handles the Pending state correctly). `kubectl scale sts/aeon --replicas=5 -n aeon` | #80 | ⏳ pending |
| T3 | ✅ full | 5 → 3 → 1 drain: `aeon cluster drain <node>` → supervisor reassigns partitions → `kubectl scale sts/aeon --replicas=3 -n aeon` → same again down to 1. Exercises G5 (drain API) + G14 (relinquish) | #80 | ⏳ pending |
| T4 | ✅ full | Manual cutover: force a partition handoff via `aeon cluster transfer-partition` → G11.a/b/c transport primitives drive BulkSync → Cutover → PoH resume. All loopback but the crypto path is identical to a real cluster. | #80 | ⏳ pending |
| T5 | ❌ not realistic on single-node RD | NetworkChaos split-brain between peer pods is meaningless when all pods share the host kernel | #80 | ❌ **deferred to DOKS re-spin with Chaos Mesh** |
| T6 | ❌ not realistic on single-node RD | Multi-node chaos (random pod kills under load) needs a real node pool to reveal node-local state vs cluster-replicated state regressions | #80 | ❌ **deferred to DOKS re-spin with Chaos Mesh** |

**Run plan for T0 / T2 / T3 / T4 (next dedicated session):**

1. **T0 — baseline (20 min load):**
   ```bash
   helm upgrade --install aeon helm/aeon -n aeon -f values-local.yaml
   kubectl apply -f docs/examples/pipeline-t0-baseline.yaml  # (to-be-landed fixture)
   # run for 3 min, then:
   kubectl port-forward -n aeon svc/aeon 14471:4471 &
   curl http://localhost:14471/metrics | grep -E "events_received|outputs_acked|events_failed"
   # success: outputs_acked == events_received; events_failed == 0
   ```

2. **T2 — code-path scale (no node pool):**
   ```bash
   kubectl scale sts/aeon --replicas=5 -n aeon
   # expect: aeon-3 + aeon-4 Pending (no node capacity); Raft membership unchanged
   # verify via /api/v1/cluster/status: still 3-node membership
   kubectl scale sts/aeon --replicas=3 -n aeon  # revert
   ```

3. **T3 — drain chain (3 → 1):**
   ```bash
   # for each node_id in 3, 2:
   aeon cluster drain <node_id>  # G5 drain: reassigns partitions to peers
   # verify /api/v1/cluster/status shows no partitions still owned by drained node
   kubectl scale sts/aeon --replicas=$((N-1)) -n aeon
   # final state: single-node, all 12 partitions on the last surviving node
   ```

4. **T4 — manual cutover under load:**
   ```bash
   # with T0 pipeline running, trigger an explicit transfer:
   aeon cluster transfer-partition <pipeline> <partition_id> <target_node>
   # assert: no events lost (outputs_acked still == events_received)
   # assert: PoH chain continuity via /api/v1/pipeline/<name>/poh-head on source + target
   ```

**V2 verdict (partial, to-be-completed):** T0/T2/T3/T4 are all
code-complete as of G2 closure 2026-04-23 (leader-side transfer driver
wired end-to-end via CL-6c.4). What remains is dedicated hands-on time
to execute the scenarios against a running cluster. T5/T6 are blocked
by RD topology and must ship to the DOKS re-spin.

---

## V3 — Processor Validation (Native + Wasm)

| Pair | Path | Tier | Status |
|------|------|------|--------|
| Native Rust processor · per-event | Memory → Native `.so` → Blackhole, `DurabilityMode::AtLeastOnce` | L2 body spine active | ⏳ pending (fixture + run) |
| Native Rust processor · batch | Memory → Native `.so` → Blackhole, `DurabilityMode::ExactlyOnce` | L2 body + L3 checkpoint | ⏳ pending |
| Wasm guest · per-event | Memory → Wasm → Blackhole, `DurabilityMode::AtLeastOnce` | L2 body spine | ⏳ pending |
| Wasm guest · batch | Memory → Wasm → Blackhole, `DurabilityMode::ExactlyOnce` | L2 body + L3 checkpoint | ⏳ pending |
| WAL fallback | any of the above with L3 redb unavailable | WAL tier | ⏳ pending |

**Success criterion for every row:** `outputs_acked_total ==
events_received_total` at steady state, `events_failed_total == 0`, and
the tier-specific metric non-zero (`aeon_l2_body_bytes_written`,
`aeon_l3_checkpoints_written_total`, `aeon_wal_records_written_total`).

**V3 verdict (pending):** the native loader + Wasm runtime + per-event
and batch paths are all unit-tested and benched in the Rust test suite
(Phase 7 / Phase 12b). V3 validates the cluster-level integration — the
remaining work is running the fixtures against the RD cluster.

---

## V5 — Crypto Chain E2E across Partition Transfer

Per ROADMAP #83: walk a transferred partition under each `PohVerifyMode`
and assert MMR + Merkle + Ed25519 root-sig round-trips; resumed
`PohChain.sequence()` on the target matches the sender.

| PohVerifyMode | Steps | Status |
|---------------|-------|--------|
| `Verify` | trigger T4 transfer, `curl /api/v1/pipeline/<name>/poh-head` on both peers, assert byte-equal `current_hash` + `mmr_root` | ⏳ pending |
| `VerifyWithKey` | same as Verify + assert Ed25519 signature over the root verifies against the publisher's pubkey | ⏳ pending |
| `TrustExtend` | skip verify, confirm target still sequences correctly from the trusted extend point | ⏳ pending |

**Cross-reference:** PoH chain transport primitives are the CL-6b series,
all closed 2026-04-16 with 4 integration tests over real QUIC. V5 exists
to confirm the E2E engine-level wire-up stays green on RD (which itself
is closed via G2 / CL-6c.4 per 2026-04-23 ROADMAP entry).

**V5 verdict (pending):** code-level coverage is already green; the V5
run certifies the RD cluster as a whole, not new code paths.

---

## V6 — This Report · Session-B Readiness Checklist

### What this document records
- V1 full sign-off (cluster baseline) — ✅ this session.
- V2/V3/V5 partial sign-off: scenarios scoped, RD-realism flagged,
  dedicated-session run plan captured.
- T5/T6 gap carried forward to DOKS re-spin with Chaos Mesh (per
  2026-04-18 decision in `GATE2-ACCEPTANCE-PLAN.md` § 10.9).

### Session-B readiness checklist (mirrors § 12.6 of the acceptance plan)

| Prereq | Owner | Status |
|--------|-------|--------|
| Feature branch SHA frozen for ECR bake | user | ⏳ pending P4.iii ECR image |
| `deploy/eks/check-spot-pricing.sh` green (avg ≤ 50% on-demand, max ≤ 95%) within SESSION_HOURS | shipped 2026-04-19 | ✅ shipped, runs at session entry |
| `deploy/eks/cluster.yaml` + README | shipped 2026-04-19 | ✅ |
| DOKS tear-down verified (Block Storage!) | user | ⚠ next DOKS session: check Volumes before destroy (see `feedback_cluster_teardown_block_storage.md`) |
| DOKS API token rotated | user | ⚠ 2026-04-24 — user must supply a fresh token before next `doctl` call |

### Gaps to carry forward into Gate 2 blocker queue

0. **Image SHA alignment (prerequisite)** — RD cluster runs a 13h-old
   image that predates this session's S4.3 / S10 / P5.c / S2.5 work.
   Rebuild + redeploy is the first step of the next V2 attempt; see
   the prerequisite block above for the exact commands.
1. **V2 T0/T2/T3/T4 on RD** — scheduled, not yet run. Scope is 1–2 hours
   dedicated session time; no expected code gaps since G2 is closed.
2. **V2 T5/T6 on DOKS with Chaos Mesh** — requires the next DOKS re-spin
   and Chaos Mesh install. Non-trivial: multi-broker Redpanda sustained
   load requires larger nodes than DOKS Regular SSD (premium/NVMe
   unavailable per 2026-04-18).
3. **V3 processor fixture files** — ✅ **closed 2026-04-24**
   (`docs/examples/pipeline-t0-baseline.yaml` + `pipeline-t0-redpanda.yaml`
   landed; both parse cleanly via `aeon apply --dry-run`).
4. **V5 PoH head REST endpoint** — code exists (CL-6b closed); surfacing
   it as a dedicated `/api/v1/pipeline/<name>/poh-head` endpoint may
   need a tiny aeon-engine REST addition.

### What a green Session B looks like

- AWS EKS `us-east-1a` cluster up, pre-flight spot price within cap,
  ECR image bake-in done, T0/T1 ceiling + T6 sustained rows populated
  in `GATE2-ACCEPTANCE-PLAN.md` § 12.6.
- Multi-broker Redpanda sustained load captured as a ceiling number.
- Chaos Mesh NetworkChaos + PodKill tests for T5 / T6 populated.
- Tear-down checklist run end-to-end including Block Storage removal.

---

*This document is meant to evolve — update rows inline as tests run
against the RD cluster; flip ⏳ to ✅ with the actual metrics captured.
Back-propagate any new code gap to the Security & Compliance index in
`ROADMAP.md`.*
