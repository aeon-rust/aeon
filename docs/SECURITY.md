# Aeon Security Documentation

This document describes the security measures implemented in Aeon, covering
authentication, authorization, input validation, processor integrity, container
hardening, and production deployment guidelines.

Aeon's security model follows OWASP Top 10 principles at every layer, from the
REST API surface to the Wasm sandbox boundary.

---

## Table of Contents

1. [Overview](#1-overview)
2. [Authentication](#2-authentication)
3. [Authorization](#3-authorization)
4. [Input Validation](#4-input-validation)
5. [Security Headers](#5-security-headers)
6. [Processor Integrity](#6-processor-integrity)
7. [Credential Management](#7-credential-management)
8. [Container Security](#8-container-security)
9. [Network Security](#9-network-security)
10. [Wasm Sandboxing](#10-wasm-sandboxing)
11. [Pipeline Data-Plane Security (S1–S10 Audit Landings)](#11-pipeline-data-plane-security-s1s10-audit-landings)
12. [Pre-Deployment Security Testing Toolchain](#12-pre-deployment-security-testing-toolchain)
13. [Production Hardening Checklist](#13-production-hardening-checklist)

---

## 1. Overview

Aeon is a real-time data processing engine designed with security as a core
concern, not an afterthought. The security architecture addresses the following
OWASP Top 10 (2021) categories:

| OWASP Category | Aeon Mitigation |
|---|---|
| A01 — Broken Access Control | Bearer token on the REST management plane; per-source inbound auth (IP allow-list / API-key / HMAC / mTLS — §11.9); per-sink outbound auth (Bearer / Basic / API-key / HMAC-sign / mTLS / broker-native — §11.10) |
| A02 — Cryptographic Failures | At-rest encryption of L2 body + L3 checkpoints (AES-256-GCM per-segment DEK — §11.3); dual-KEK envelope encryption with rotation (§11.1); secret provider abstraction (Vault / KMS / SM) keeps plaintext credentials off disk |
| A03 — Injection | Resource name validation, path traversal prevention; SSRF hardening on every external-URL resolver (§11.8) |
| A04 — Insecure Design | Request body size limits (10 MB), batch size bounds; compliance-regime precondition validator refuses to start pipelines missing required encryption / retention / erasure (§11.5) |
| A05 — Security Misconfiguration | Security headers on all responses, dev-mode warnings; dev-mode secret providers emit a warn log so they cannot silently reach prod |
| A08 — Software and Data Integrity | SHA-256 hash verification for native processor artifacts; PoH + Merkle + MMR + Ed25519 hot-path chain for event-level integrity |
| A09 — Security Logging and Monitoring | Full request logging via tower-http TraceLayer; tracing-layer payload redaction (§11.6); separated audit log channel (§11.7) |
| A10 — Server-Side Request Forgery | SSRF policy: allow/deny-list, private-IP rejection, URL redaction on error paths (§11.8) |

Key design principles:

- **Defense in depth.** Multiple layers of validation: CLI, REST API, and engine.
- **Secure by default.** Production requires explicit token configuration; dev mode
  logs a warning when authentication is disabled.
- **Least privilege.** Wasm processors run in a sandboxed environment with no
  filesystem access. Container images run as a non-root user.
- **Zero trust for artifacts.** Native processor libraries can be verified against
  a SHA-256 hash before loading into the process.

---

## 2. Authentication

Aeon uses Bearer token authentication for the REST API management plane.

### How It Works

The `AEON_API_TOKEN` environment variable controls authentication behavior:

- **Token set (production mode):** All `/api/v1/` endpoints require an
  `Authorization: Bearer <token>` header. Requests with a missing, malformed, or
  incorrect token receive a `401 Unauthorized` response.
- **Token unset (dev mode):** Authentication is disabled. The engine logs a warning
  at startup: `"REST API server listening (auth DISABLED -- set AEON_API_TOKEN to enable)"`.

### Request Format

```
Authorization: Bearer <your-secret-token>
```

### Error Responses

| Condition | Status | Message |
|---|---|---|
| Missing or malformed header | 401 | `missing or malformed Authorization header (expected: Bearer <token>)` |
| Invalid token | 401 | `invalid bearer token` |

### Implementation

The authentication middleware is defined in `crates/aeon-engine/src/rest_api.rs`
and applied as an axum `route_layer` on all `/api/v1/` routes. Health endpoints
are mounted on a separate router that bypasses the auth layer.

---

## 3. Authorization

### Endpoint Protection

All management endpoints under `/api/v1/` require authentication when
`AEON_API_TOKEN` is configured:

- `GET/POST/DELETE /api/v1/processors/...`
- `GET/POST/DELETE /api/v1/pipelines/...`
- `GET/POST /api/v1/pipelines/{name}/delivery/...`

### Health Endpoint Bypass

The following endpoints are always publicly accessible (no authentication required):

- `GET /health` -- liveness probe for orchestrators (Kubernetes, Docker).
- `GET /ready` -- readiness probe.

This is by design: health checks from load balancers and container orchestrators
must not require credentials.

### CLI Authentication

The `aeon` CLI reads `AEON_API_URL` (defaults to `http://localhost:4471`) and
passes credentials via the same Bearer token mechanism. Set `AEON_API_TOKEN` in
the shell environment or `.env` file before running CLI commands against a
secured engine.

---

## 4. Input Validation

### Resource Name Validation (OWASP A03)

All resource names (processor names, pipeline names, project names) are validated
at both the CLI layer and the REST API layer to prevent path traversal and
injection attacks.

Validation rules (applied in both `validate_name` in CLI and
`validate_resource_name` in REST API):

| Rule | Rationale |
|---|---|
| Length 1--128 characters | Prevents empty names and buffer abuse |
| Must not contain `..`, `/`, or `\` | Prevents path traversal attacks |
| Must not start with `.` or `-` | Prevents hidden file creation and flag injection |
| Must not contain control characters | Prevents terminal injection and log poisoning |
| Only alphanumeric, `-`, `_`, `.` allowed (CLI) | Restricts to safe filesystem characters |

### Request Body Limits (OWASP A04)

The REST API enforces a 10 MB maximum request body size via tower-http's
`RequestBodyLimitLayer`. This applies to all endpoints and prevents
denial-of-service via oversized payloads.

```rust
const MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024; // 10 MB
```

### Manifest Validation

YAML manifests submitted via `aeon apply` are parsed and validated before
execution. The `--dry-run` flag allows previewing changes without applying them.

---

## 5. Security Headers

Every HTTP response from the REST API includes the following security headers,
applied via tower-http `SetResponseHeaderLayer`:

| Header | Value | Purpose |
|---|---|---|
| `X-Content-Type-Options` | `nosniff` | Prevents MIME type sniffing (OWASP A05) |
| `X-Frame-Options` | `DENY` | Prevents clickjacking by disallowing iframe embedding |
| `Cache-Control` | `no-store` | Prevents caching of sensitive API responses |

These headers are applied globally to all responses, including health endpoints.

---

## 6. Processor Integrity

### SHA-256 Hash Verification for Native Processors (OWASP A08)

Native processors (`.so` on Linux, `.dll` on Windows) run in-process with full
memory access. Aeon provides SHA-256 integrity verification to ensure that only
trusted, untampered artifacts are loaded.

### Usage

**Verified loading (recommended for production):**

```rust
NativeProcessor::load_verified("path/to/processor.so", config, "expected_sha256_hex")?;
```

**Compute hash for a new artifact:**

```rust
let hash = NativeProcessor::compute_hash("path/to/processor.so")?;
// Store this hash in your processor registry or YAML manifest
```

**Standalone integrity check:**

```rust
NativeProcessor::verify_integrity(path, "expected_sha256_hex")?;
```

### How It Works

1. The loader reads the entire shared library file into memory.
2. It computes the SHA-256 digest using the `sha2` crate.
3. The computed hex digest is compared against the expected hash.
4. If the hashes do not match, loading is refused with an error:
   `"integrity check failed for '<path>': expected SHA-256 <expected>, got <actual>"`.
5. On success, the hash and path are logged at `info` level.

### Symbol Validation

Before creating a processor instance, `NativeProcessor::validate()` checks that
the shared library exports all required C-ABI symbols:

- `aeon_processor_create`
- `aeon_processor_destroy`
- `aeon_process`
- `aeon_process_batch`

Missing symbols produce a clear error before any code is executed.

### Best Practices

- Store expected hashes in your YAML manifest or processor registry.
- Compute hashes as part of CI/CD and embed them in deployment artifacts.
- Always use `load_verified` in production; reserve `load` for local development.

---

## 7. Credential Management

### Environment Variable Injection

Aeon follows the 12-Factor App methodology. All credentials are injected via
environment variables, never hardcoded or stored in configuration files.

Key environment variables:

| Variable | Purpose | Required in Production |
|---|---|---|
| `AEON_API_TOKEN` | Bearer token for REST API authentication | Yes |
| `AEON_API_URL` | Engine address for CLI | No (defaults to `http://localhost:4471`) |
| `AEON_CHECKPOINT_DIR` | Persistent checkpoint directory | Yes |
| `POSTGRES_PASSWORD` | PostgreSQL password | When using PostgreSQL |
| `MYSQL_ROOT_PASSWORD` | MySQL root password | When using MySQL |
| `MYSQL_PASSWORD` | MySQL user password | When using MySQL |
| `RABBITMQ_PASSWORD` | RabbitMQ password | When using RabbitMQ |
| `GRAFANA_ADMIN_PASSWORD` | Grafana admin password | When using Grafana |

### .env File Pattern

The repository includes `.env.example` as a template. Copy it to `.env` and fill
in actual values:

```bash
cp .env.example .env
# Edit .env with real credentials
```

The `.env` file should never be committed to version control (it is excluded via
`.gitignore`).

### Docker Compose Enforcement

The `docker-compose.yml` uses the `:?` syntax to enforce that required passwords
are set:

```yaml
POSTGRES_PASSWORD: ${POSTGRES_PASSWORD:?Set POSTGRES_PASSWORD in .env}
AEON_API_TOKEN: ${AEON_API_TOKEN:?Set AEON_API_TOKEN for production}
```

Docker Compose will refuse to start if these variables are missing, preventing
accidental deployment with empty credentials.

### Docker Secrets (Production)

For orchestrated deployments (Docker Swarm, Kubernetes), use Docker secrets or
Kubernetes secrets instead of environment variables:

```yaml
# Kubernetes Secret example
apiVersion: v1
kind: Secret
metadata:
  name: aeon-secrets
type: Opaque
stringData:
  AEON_API_TOKEN: "your-production-token"
```

Mount secrets as environment variables in your pod spec or Docker service
definition.

---

## 8. Container Security

### Non-Root Execution

The production Dockerfile (`docker/Dockerfile`) creates a dedicated `aeon` user
and group, and the container runs as this non-root user:

```dockerfile
RUN groupadd -r aeon && useradd -r -g aeon -d /app -s /sbin/nologin aeon
USER aeon
```

This limits the blast radius if the container is compromised -- the attacker
cannot escalate to root privileges within the container.

### Minimal Base Image

The multi-stage build produces a minimal runtime image:

- **Build stage:** `rust:1.94-bookworm` (full toolchain, discarded after build).
- **Runtime stage:** `debian:bookworm-slim` with only `ca-certificates` and
  `libssl3` installed.
- APT lists are cleaned after installation (`rm -rf /var/lib/apt/lists/*`).
- The final binary is stripped (`strip /build/target/release/aeon`).

### Read-Only Filesystem Compatibility

Application directories are created with proper ownership:

```dockerfile
RUN mkdir -p /app/artifacts /app/config /app/data \
    && chown -R aeon:aeon /app
```

In production, mount `/app/artifacts` and `/app/data` as named volumes with
appropriate permissions. The rest of the filesystem can be mounted read-only.

### Health Checks

The Dockerfile includes a health check so orchestrators can detect unhealthy
containers:

```dockerfile
HEALTHCHECK --interval=15s --timeout=5s --retries=3 \
    CMD ["/usr/local/bin/aeon", "--help"]
```

---

## 9. Network Security

### Port Separation

Aeon uses three distinct ports to separate concerns:

| Port | Purpose | Audience |
|---|---|---|
| 4470 | QUIC cluster communication | Internal only (node-to-node) |
| 4471 | REST API (management plane) | Operators, CI/CD |
| 4472 | Metrics (Prometheus scrape) | Monitoring systems |

In production, configure network policies to:

- **4470:** Allow only between Aeon cluster nodes (internal network).
- **4471:** Allow only from operator networks or CI/CD systems. Protect with
  `AEON_API_TOKEN` and optionally a reverse proxy with TLS termination.
- **4472:** Allow only from monitoring infrastructure (Prometheus, Grafana).

### TLS / mTLS Status

Aeon's key decisions lock in `rustls` + `aws-lc-rs` for TLS, providing a path
to FIPS 140-3 compliance. Current status by surface:

| Surface | TLS / mTLS status |
|---|---|
| QUIC cluster (port 4470) | TLS via rustls + auto-generated or configured certificates (always on) |
| REST API (port 4471) | Bearer token; TLS via reverse proxy recommended |
| Inbound push sources (HTTP webhook, WebSocket, WebTransport) | mTLS supported as an S9 auth mode alongside IP allow-list / API-key / HMAC |
| Outbound HTTP sink | mTLS supported — S10 mode `mtls`, client cert/key from secret provider |
| Outbound Kafka / Redpanda sink | mTLS via broker-native (rdkafka SASL/TLS) — S10 mode `broker_native` |
| Outbound WebSocket / WebTransport sink | mTLS supported — S10 mode `mtls`; WS wires the signer cert/key into the rustls `Connector`, WT exposes `webtransport::mtls_client_config_from_signer` to bake the identity into `wtransport::ClientConfig` before construction. Inbound WT source extracts peer-cert CN + SAN post-handshake for S9 `mtls` subject matching. |
| Inbound REST on port 4471 | Native TLS termination via rustls is future work; reverse proxy recommended |

Roadmap reference: FIPS-track claims are already available via the
cloud-KMS and Vault/OpenBao-with-HSM-seal paths (§11.2). Direct PKCS#11
integration (task #33) is deferred until a specific customer surfaces it.

### Kafka/Redpanda Security

When connecting to secured Kafka/Redpanda clusters, configure authentication via
rdkafka properties in the pipeline manifest:

- SASL/SCRAM or SASL/PLAIN for authentication.
- TLS for encryption in transit.
- ACLs on the broker side to restrict topic access per pipeline.

---

## 10. Wasm Sandboxing

Wasm processors run inside the Wasmtime sandbox with strict isolation guarantees.

### Fuel Metering

Every `process()` call is allocated a finite fuel budget (default: 1,000,000
fuel units). When fuel is exhausted, execution is terminated immediately with an
error. This prevents infinite loops and runaway computation.

```rust
WasmConfig {
    max_fuel: Some(1_000_000),   // Fuel per process() call
    max_memory_bytes: 64 * 1024 * 1024, // 64 MiB memory cap
    enable_simd: true,
    namespace: "default".to_string(),
}
```

Fuel is reset before each `process()` invocation, ensuring a consistent budget
per event.

### Memory Isolation

Each Wasm instance has a bounded linear memory space (default: 64 MiB). The
guest cannot access host memory, other Wasm instances, or any memory outside its
allocated region.

### No Filesystem Access

Wasm guests have no access to the host filesystem. The only host functions
available are:

- `state_get`, `state_put`, `state_delete` -- namespaced key-value state.
- `log_info`, `log_warn`, `log_error` -- structured logging through the host.
- `metrics_counter_inc`, `metrics_gauge_set` -- metrics emission.
- `clock_now_ms` -- wall clock time (read-only).

There is no `fd_open`, `fd_read`, `fd_write`, or any WASI filesystem capability
exposed.

### Namespace Isolation

Each Wasm processor operates within a configured namespace. State operations
(`state_get`, `state_put`, `state_delete`) are prefixed with the namespace,
preventing one processor from reading or writing another processor's state.

### Instance-Per-Event Execution Model

Each Wasm instance is single-threaded (protected by Mutex). For parallelism,
multiple instances of the same module are created -- one per pipeline thread.
This prevents shared-state race conditions at the Wasm layer.

---

## 11. Pipeline Data-Plane Security (S1–S10 Audit Landings)

Sections 2–10 cover the Aeon control plane (REST API, CLI, processor loading,
Wasm sandbox, container, ports). This section covers the pipeline data plane
— how events, checkpoints, and connector credentials are protected as data
flows through Aeon. All items below landed between 2026-04-19 and 2026-04-23
as the S1–S10 audit close-out. For regime-specific guidance (PCI-DSS / HIPAA /
GDPR), see [`COMPLIANCE.md`](COMPLIANCE.md).

### 11.1 Secret Management (S1)

Secrets (KEKs, DEKs, HMAC keys, connector passwords, API tokens, TLS private
keys) are resolved through a two-trait provider abstraction:

- **`SecretProvider`** — fetch-shape. `resolve(path) -> SecretBytes`. Fits
  env vars, `.env` files, Vault / OpenBao KV-v2, AWS Secrets Manager, GCP
  Secret Manager, Azure Key Vault reads.
- **`KekProvider`** — remote-op shape. `async wrap(dek)` /
  `async unwrap(wrapped)` without exposing KEK bytes to the Aeon process.
  Fits AWS KMS, GCP KMS, Azure KV crypto, Vault Transit, OpenBao Transit,
  and PKCS#11. The existing in-process AES-GCM path (reads KEK bytes via
  `SecretProvider`) is the `LocalKekProvider` and remains the default.

Backend status (2026-04-23):

| Backend | Trait | Status |
|---|---|---|
| Env vars | SecretProvider | ✅ shipped (`EnvProvider`) |
| `.env` file (`AEON_DOTENV_PATH`) | SecretProvider | ✅ shipped (`DotEnvProvider`) |
| Literal YAML | SecretProvider | ✅ shipped (`LiteralProvider`) — dev-only, warn-once |
| HashiCorp Vault KV-v2 | SecretProvider | 🟡 in-flight — `aeon-secrets` crate (task #35) |
| OpenBao KV-v2 | SecretProvider | 🟡 in-flight — ships under the Vault adapter (API-compatible) |
| AWS Secrets Manager | SecretProvider | ⬜ planned behind `aws-sm` feature (task #35 follow-up) |
| GCP Secret Manager | SecretProvider | ⬜ planned |
| Azure Key Vault (read) | SecretProvider | ⬜ planned |
| Local AES-GCM (KEK bytes via SecretProvider) | KekProvider | ✅ shipped (`LocalKekProvider` in `aeon-crypto::kek`) |
| AWS KMS | KekProvider | ⬜ planned behind `aws-kms` feature |
| GCP Cloud KMS | KekProvider | ⬜ planned |
| Azure Key Vault (crypto ops) | KekProvider | ⬜ planned |
| Vault Transit | KekProvider | ⬜ planned (follows KV-v2) |
| OpenBao Transit | KekProvider | ⬜ planned (shares Vault adapter) |
| PKCS#11 direct | KekProvider | 🟥 deferred (task #33) |

Until `aeon-secrets` lands, production deployments source KEK and connector
credentials via `EnvProvider` (K8s / Helm / Vault-sidecar-injected env vars
are the canonical path) with the envelope-encryption, dual-KEK, and rotation
machinery all already live. Moving to a native Vault or KMS backend is a
drop-in registration change, not a surface rewrite.

Envelope encryption model:

- DEK = per-segment 256-bit AES key, generated by the provider or Aeon.
- KEK = key-encryption-key held by the provider; wraps the DEK.
- Dual KEK supported: primary + next. During rotation, new segments use the
  next KEK while old segments continue to unwrap under the primary.

KEK rotation runbook:

1. Add the new KEK to the provider with a new key-id.
2. Configure Aeon with `security.kek.next = <new-key-id>`.
3. New L2 segment DEKs are wrapped under `next`; existing segments are
   unaffected.
4. Once all old segments age out (per S5 retention), promote `next` to
   `primary` and remove the old KEK.

Config-via-env-var-first: all provider endpoints, role-ids, and key-ids come
from env vars (see [`CONFIGURATION.md`](CONFIGURATION.md) for the canonical
list). Literal YAML values are dev-only.

### 11.2 HSM Custody (via Vault / OpenBao, not PKCS#11)

Aeon's chosen FIPS-track path is **indirect**: Aeon will talk to Vault or
OpenBao as its secret provider (§11.1), and Vault / OpenBao can themselves
be sealed with an HSM backend (Thales / Entrust / Utimaco / CloudHSM) when
direct hardware key custody is required. This keeps Aeon out of the
PKCS#11 driver business and lets the operator choose the HSM on Vault's
schedule, not ours.

> **Status (2026-04-23):** the Vault / OpenBao adapter in `aeon-secrets`
> is the in-flight deliverable for this path (task #35). Until it lands,
> the "indirect HSM" claim below describes the target design, not a
> shipped production path.

For deployments that cannot run Vault / OpenBao at all and need Aeon to
speak PKCS#11 directly to an HSM, the `aeon-crypto::hsm` trait exists as
an extension point (S1.4 landed 2026-04-23). A real PKCS#11 backend
(cryptoki + SoftHSM CI + CloudHSM / on-prem HSM) is deferred until a
specific customer surfaces the requirement (tracked as task #33). The
90%-of-shops path is "Aeon → OpenBao / Vault → optional HSM seal" and
that is what SECURITY.md and COMPLIANCE.md recommend.

FIPS 140-3 claim paths, in decreasing order of operator effort:

| Path | FIPS posture | Notes |
|---|---|---|
| Aeon → AWS KMS / GCP KMS / Azure KV | FIPS 140-3 L3 for KEK | Cloud provider HSMs are FIPS-validated. Default for cloud deployments. |
| Aeon → Vault / OpenBao (software seal) | FIPS module only | Software seal is not itself FIPS-validated; use when FIPS is not a hard requirement. |
| Aeon → Vault / OpenBao (HSM seal) | FIPS 140-3 L3 for KEK | Vault Enterprise HSM seal or OpenBao + PKCS#11 seal. Primary on-prem FIPS path. |
| Aeon → PKCS#11 (direct, via S1.4) | FIPS 140-3 L3 for KEK | Deferred. Only needed when Vault / OpenBao is not an option. |

### 11.3 At-Rest Encryption (S3)

Both durability tiers are encrypted at rest:

| Tier | Encryption |
|---|---|
| L2 body (segment files) | AES-256-GCM with a per-segment DEK. DEK is wrapped with the KEK and stored alongside the segment. |
| L3 checkpoints (redb / RocksDB) | AES-256-GCM wrapping at the store boundary; DEK wrapped with the KEK. |

GCM provides integrity out of the box — any tampering produces a decryption
failure rather than silent corruption.

Performance: encryption is applied in the hot path. Gate 1 throughput was
re-benched after S3 landed; no regression versus the pre-encryption baseline.

### 11.4 Retention (S5)

Per-tier, per-pipeline retention is configurable:

- L2 body: `state.l2.retention = <duration>` (default 7d)
- L3 ack: `state.l3.ack_retention = <duration>` (default 24h)

Retention is a precondition of several compliance regimes (HIPAA retention
floors, GDPR storage limitation). The compliance validator (§11.5) reads
these values at pipeline start and refuses to start if they don't meet the
declared regime's floor.

### 11.5 Compliance-Regime Enforcement (S4.2)

The `compliance_validator::validate_compliance` function runs at pipeline
start (before the first event is accepted) and gates startup on regime-driven
preconditions.

Supported regimes (declared per-pipeline in YAML):

| Regime | Minimum preconditions checked |
|---|---|
| PCI-DSS | At-rest encryption (§11.3) on; tracing redaction (§11.6) on; audit channel (§11.7) on; KEK rotation configured. |
| HIPAA | At-rest encryption on; retention floor configured; erasure API (§11.8) available for patient-data-tagged pipelines. |
| GDPR | At-rest encryption on; subject-id extractor configured; retention ceiling configured; erasure + right-to-export available. |

Failed preconditions produce a startup error with the specific missing control
named. Operators see the error in engine logs and the pipeline stays in
`Failed` state until the config is corrected.

See [`COMPLIANCE.md`](COMPLIANCE.md) for per-regime operator guidance.

### 11.6 Tracing Redaction (S2)

Data-path tracing uses a redaction layer that strips event payload bytes
before emission. Only structural metadata (event id, source, partition,
timestamps, headers with opted-in keys) reaches the log sink. The redaction
is applied in the tracing-subscriber layer, so any log line anywhere in the
codebase is subject to it — not just opt-in call sites.

External URLs written to log fields are passed through `redact_uri()` to
strip userinfo (`user:pass@…`) and query strings.

### 11.7 Audit Log Channel (S2.5)

Audit-relevant events (pipeline start/stop, config changes, erasure
operations, KEK rotation, authentication failures) emit to a separate audit
channel rather than the data-path tracing channel. The audit channel is
defined in `aeon-types::audit` and emitted via `aeon-observability::audit`.

Separation matters because:
- The data-path channel is high-volume and redacted — audit events must not
  get lost in the noise.
- Audit events are append-only and routed to a different sink (commonly an
  immutable store) so ops can investigate incidents without trusting the
  same store that an attacker might tamper with.

Call-site wiring is ongoing (see ROADMAP §Security follow-ups).

### 11.8 SSRF Hardening (S7)

Every connector that dials an external URL runs the target through an
`SsrfPolicy` check first. Defaults (production profile):

- Scheme allow-list: `http`, `https`, `ws`, `wss` (plus protocol-specific
  extensions like `kafka`, `nats`).
- Host deny-list: loopback (127.0.0.0/8, ::1), link-local (169.254.0.0/16,
  fe80::/10), broadcast, multicast, RFC1918 private ranges (configurable).
- DNS resolution happens *before* the TCP SYN — a hostname that resolves to
  a private IP is rejected.
- URLs in error paths are passed through `redact_uri()`.

Dev profiles can relax the deny-list (e.g., to allow `127.0.0.1` for local
integration tests). Production profile is on by default.

Connectors using the guard: HTTP source/sink, webhook source, WebSocket
source/sink, WebTransport source/sink, any processor making an egress call.

### 11.9 GDPR Subject-ID / Erasure / Export (S6)

Three primitives:

- **Subject-ID extractor** — per-pipeline config that extracts a stable
  subject identifier from each event (JSON path, header, or custom
  extractor). Events are indexed by subject ID in the L3 tier.
- **Erasure API** — `POST /api/v1/pipelines/{name}/erase` takes a subject ID
  and writes a tombstone to the deny-list store. Subsequent reads of events
  for that subject are suppressed; future writes for that subject are
  rejected at ingest (deny-list enforcement).
- **Right-to-export** — `GET /api/v1/pipelines/{name}/export?subject=...`
  returns all events for a given subject ID across the retention window.
  Includes a cryptographic null-receipt when the subject has been erased
  (the PoH/Merkle chain still verifies; the payload is proven absent).

The erasure policy type (`ErasurePolicy`) declares the retention window and
the tombstone TTL. See [`COMPLIANCE.md`](COMPLIANCE.md) for the GDPR
operator workflow.

### 11.10 Inbound Connector Auth (S9)

Per-push-source authentication, configured under `auth.mode` on the source
YAML. Available modes:

| Mode | Description |
|---|---|
| `none` | No auth (dev only — allowed sources log a warn if used outside `127.0.0.1`). |
| `ip_allow_list` | CIDR-based peer-IP allow-list checked at accept. |
| `api_key` | Header-based API key; constant-time compare against secret-provider value. |
| `hmac` | Signature + timestamp header; HMAC-SHA256 / SHA512; replay window enforced. |
| `mtls` | Peer certificate required; trust store configurable. |

The modes compose: the source can require `mtls` AND `hmac` simultaneously.
Credentials resolve through the S1 secret provider.

Applies to: HTTP webhook source, WebSocket source, WebTransport source.

### 11.11 Outbound Connector Auth (S10)

Per-sink outbound authentication, configured under `auth.mode` on the sink
YAML. Available modes:

| Mode | Description |
|---|---|
| `none` | Legitimate default for trusted-VPC pulls (Kafka, CDC) where upstream whitelists Aeon's egress IPs. |
| `bearer` | `Authorization: Bearer <token>` header. |
| `basic` | `Authorization: Basic <user:pass>` header. |
| `api_key` | Custom header name + key value (e.g. `X-Api-Key: <value>`). |
| `hmac_sign` | Request body + path signed with HMAC-SHA256 / SHA512; timestamp + signature headers. |
| `mtls` | Client cert + key presented on the TLS handshake. |
| `broker_native` | Protocol-native auth (e.g. Kafka SASL/SCRAM, NATS credentials, Redis ACL). |

All credentials resolve through the S1 secret provider. One mode per sink —
no interlocking.

Applies to: HTTP sink, Kafka/Redpanda sink (broker-native), NATS sink,
Redis Streams sink, WebSocket sink, WebTransport sink.

For WebSocket sinks and sources, `mtls` mode plumbs the signer cert/key
into a rustls `Connector` and `tokio_tungstenite::connect_async_tls_with_config`.
For WebTransport, callers build the TLS-wired `ClientConfig` via
`aeon_connectors::webtransport::mtls_client_config_from_signer(&signer)`
and pass it to `WebTransportSinkConfig::new(url, client_config)`.
Inbound WT sources extract peer-cert CN + SAN after the QUIC handshake
via `aeon_crypto::tls::CertificateStore::parse_cert_subjects` and feed
them into `InboundAuthVerifier`'s `mtls` subject check.

---

## 12. Pre-Deployment Security Testing Toolchain

Every release candidate passes a seven-tool scan tranche on Rancher
Desktop before it is promoted to a staging / DOKS / EKS environment.
The suite splits into four layers — **supply chain** (what's in the
image), **network surface** (what's exposed to the world), **TLS
hygiene** (how we terminate), and **DAST** (active attack against a
running instance). All tools run locally against a freshly-rebuilt
Aeon Docker image inside Rancher Desktop, so there is no cloud spend
to the scan loop and no registry round-trip before the image is
declared fit to push.

### 12.1 Supply-chain: Trivy (image CVEs)

Scope: OS-package + language-package CVE enumeration against the
built `aeon:<tag>` image.

```bash
trivy image --severity HIGH,CRITICAL --exit-code 1 aeon:latest
```

Gate: HIGH/CRITICAL must be zero, or carry an explicit waiver entry
(with justification + expected fix window) in the release notes.
Runs before the image is tagged for push.

### 12.2 Supply-chain: Syft + Grype (SBOM + dependency CVEs)

Syft generates a CycloneDX SBOM the project ships alongside every
image (auditable artefact; useful both to operators and to third
parties under SLSA-style provenance claims). Grype consumes that SBOM
and cross-references NVD/OSV for vulnerabilities — the second opinion
on Trivy's result, used to catch divergence between scanner feeds.

```bash
syft aeon:latest -o cyclonedx-json > sbom.json
grype sbom:sbom.json --fail-on high
```

Gate: matches Trivy's HIGH/CRITICAL gate. Any Grype-only finding or
Trivy-only finding must be explained.

### 12.3 Network surface: Nuclei (template vuln scan)

Nuclei runs the ProjectDiscovery community template set plus
Aeon-specific templates against the live container. In-scope surfaces:

- `:4471` — REST API, WebSocket source/sink, `/metrics`, `/healthz`.
- `:4472` — WebTransport + external QUIC.

Custom templates under `security/nuclei/` (to be authored) cover
Aeon-specific expectations — e.g. `/metrics` requires S9 auth, 404
behaviour on unauthenticated management paths, header presence from
§5 Security Headers.

Gate: zero unexplained HIGH/CRITICAL matches. Template version and
template run-id captured in the release's scan report.

### 12.4 TLS hygiene: testssl.sh

Validates cipher suites, protocol versions, cert chain, and HSTS
behaviour on every TLS-facing port:

- `:4471` (when rustls is enabled end-to-end).
- `:4472` QUIC/HTTP/3 — via the testssl.sh `--quic` probe.

Cross-checks the results with the S2 (redaction), S7 (SSRF), S9
(inbound auth), and S10 (outbound auth) expectations in §11. Any
deviation from the rustls + aws-lc-rs hardening assumption (FIPS 140-3
path — see CLAUDE.md Key Decisions) flagged as a release blocker.

### 12.5 DAST: PortSwigger Dastardly (baseline)

Dastardly is the Burp-engine-subset, CI-friendly scanner that Port-
Swigger ship for GitHub Actions. We run it first as a fast baseline
against the REST API on `:4471` — it catches the Burp-style classes
(missing security headers, cookie flags, obvious injection shapes)
at < 10 minutes wall-clock. Dastardly is the gate for every release;
it is fast enough to run on every build.

Gate: zero HIGH, a justified-or-fixed list for MEDIUM. LOW advisory
only.

### 12.6 DAST: OWASP ZAP (deep scan)

ZAP runs the deep complement to Dastardly — baseline scan + full
active scan against both REST and WebSocket on `:4471`, using an
authenticated context primed with an API key issued via the S9
inbound-auth path.

Gate: same as Dastardly, but runs only on release candidates (not on
every build — too slow for the tight loop). Findings triage either
resolves against SECURITY.md §11 behaviour or opens an issue tagged
`security/dast`.

### 12.7 Parked for future consideration

- **Pentagi** — agentic pen-test orchestrator. Interesting for
  producing narrative reports and chaining tool output, but adds an
  LLM dependency to the scan pipeline that we are not ready to take on
  until the core seven-tool suite is stable and we have a clear per-
  release budget for token spend.
- **Kali Linux** — full offensive toolkit. Too broad for a CI gate;
  more suitable for periodic targeted manual pen-test engagements
  (e.g. pre-GA hardening sprint). Revisit alongside any third-party
  security audit.

### 12.8 Where the tranche runs in the release flow

1. `cargo clippy --workspace --all-features --all-targets -- -D warnings`
2. `cargo test --workspace --all-features`
3. Rebuild image: `docker build -f docker/Dockerfile -t aeon:<tag> .`
4. Rancher Desktop V2..V6 functional validation.
5. **Security tranche** — Trivy → Syft+Grype → Nuclei → testssl.sh →
   Dastardly → (release candidate only) ZAP.
6. Tag + push image; SBOM + scan reports published alongside.
7. DOKS / EKS acceptance re-runs.

---

## 13. Production Hardening Checklist

Follow this checklist before deploying Aeon to production.

### Authentication and Authorization

- [ ] Set `AEON_API_TOKEN` to a strong, unique token (minimum 32 characters,
      cryptographically random).
- [ ] Verify authentication is active: the engine startup log should show
      `"REST API server listening (auth enabled)"`.
- [ ] Rotate `AEON_API_TOKEN` periodically (at least quarterly).

### Credentials

- [ ] Never commit `.env` files to version control.
- [ ] Use Docker secrets or Kubernetes secrets for credential injection in
      orchestrated deployments.
- [ ] Change all default passwords in `.env.example` before first deployment.
- [ ] Set strong passwords for all infrastructure services (PostgreSQL, MySQL,
      RabbitMQ, Grafana).

### Network

- [ ] Do not expose port 4470 (QUIC cluster) to the public internet.
- [ ] Restrict port 4471 (REST API) to operator networks via firewall rules or
      network policies.
- [ ] Restrict port 4472 (metrics) to monitoring infrastructure only.
- [ ] Place a TLS-terminating reverse proxy in front of port 4471 for
      encryption in transit.
- [ ] Configure Kafka/Redpanda with SASL authentication and TLS.

### Container

- [ ] Use the production Dockerfile (`docker/Dockerfile`), which runs as
      non-root user `aeon`.
- [ ] Do not run containers with `--privileged` or add unnecessary Linux
      capabilities.
- [ ] Mount the container filesystem as read-only where possible
      (`--read-only`), with writable volumes only for `/app/data` and
      `/app/artifacts`.
- [ ] Pin image tags to specific versions (avoid `latest` in production).
- [ ] Scan container images for vulnerabilities before deployment.

### Processor Artifacts

- [ ] Use `NativeProcessor::load_verified()` for all native processor loading in
      production.
- [ ] Compute and store SHA-256 hashes in CI/CD as part of the build pipeline.
- [ ] Store hashes in the processor registry or YAML manifest alongside the
      artifact reference.
- [ ] For Wasm processors, configure appropriate fuel limits based on expected
      workload. Lower fuel limits reduce the impact of buggy or malicious code.

### Monitoring and Logging

- [ ] Enable structured JSON logging (`AEON_LOG_FORMAT=json`).
- [ ] Configure OTLP export to your observability stack
      (`OTEL_EXPORTER_OTLP_ENDPOINT`).
- [ ] Monitor for `401 Unauthorized` responses (potential credential attacks).
- [ ] Monitor for integrity check failures (potential artifact tampering).
- [ ] Set up alerts for processor fuel exhaustion (indicates runaway computation
      or insufficient fuel budget).

### Wasm Security

- [ ] Set `max_fuel` appropriate to your workload (default 1,000,000).
- [ ] Set `max_memory_bytes` appropriate to your workload (default 64 MiB).
- [ ] Use distinct namespaces for each processor to ensure state isolation.
- [ ] Audit Wasm modules before deploying to production.

### Data-Plane Security (S1–S10)

- [ ] Configure a production-grade **secret provider** (Vault / AWS KMS /
      Secrets Manager / GCP SM). Do not use literal YAML secrets or bare
      env vars beyond CI/Helm injection (see §11.1).
- [ ] Configure both **primary and next KEK** so rotation can be performed
      without downtime (see §11.1).
- [ ] Verify **at-rest encryption is on** for both L2 body and L3
      checkpoints (`state.l2.encryption=on`, `state.l3.encryption=on`) —
      §11.3.
- [ ] Set **retention** on both tiers to match your regulatory floor /
      ceiling — §11.4.
- [ ] Declare the **compliance regime** on each regulated pipeline
      (`compliance.regime: pci_dss | hipaa | gdpr`) so the validator (§11.5)
      enforces preconditions at start.
- [ ] Verify **payload-never-in-logs** by spot-checking structured log
      output on a sample pipeline (§11.6).
- [ ] Route the **audit channel** to an immutable or write-protected sink
      separate from data-path tracing (§11.7).
- [ ] Review **SSRF policy** for every source / sink that dials external
      URLs. Leave the production profile on unless you have a specific
      reason to relax it (§11.8).
- [ ] Configure **inbound auth** on every push source — never deploy
      `auth.mode: none` outside a trusted-VPC where upstream peer-auth is
      already enforced (§11.10).
- [ ] Configure **outbound auth** on every sink — prefer `mtls` or
      `broker_native` over `bearer` where the protocol supports it
      (§11.11).
- [ ] For pipelines handling EU personal data, test the **erasure API** and
      **right-to-export** paths (§11.9) as part of pre-prod acceptance.

### General

- [ ] Run `cargo clippy --workspace -- -D warnings` to catch code quality
      issues.
- [ ] Keep Aeon and all dependencies up to date with security patches.
- [ ] Review the `docker-compose.prod.yml` configuration before production
      deployment.
- [ ] Test your deployment with `aeon apply --dry-run` before applying
      manifests.
- [ ] Maintain an incident response plan for security events.
