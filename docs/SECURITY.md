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
11. [Production Hardening Checklist](#11-production-hardening-checklist)

---

## 1. Overview

Aeon is a real-time data processing engine designed with security as a core
concern, not an afterthought. The security architecture addresses the following
OWASP Top 10 (2021) categories:

| OWASP Category | Aeon Mitigation |
|---|---|
| A01 — Broken Access Control | Bearer token authentication on all management endpoints |
| A03 — Injection | Resource name validation, path traversal prevention |
| A04 — Insecure Design | Request body size limits (10 MB), batch size bounds |
| A05 — Security Misconfiguration | Security headers on all responses, dev-mode warnings |
| A08 — Software and Data Integrity | SHA-256 hash verification for native processor artifacts |
| A09 — Security Logging and Monitoring | Full request logging via tower-http TraceLayer |

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

- **Build stage:** `rust:1.85-bookworm` (full toolchain, discarded after build).
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

### mTLS Path (Planned)

Aeon's key decisions lock in `rustls` + `aws-lc-rs` for TLS, providing a path
to FIPS 140-3 compliance. The planned security progression:

1. **Current:** Bearer token over HTTP (suitable for internal networks).
2. **Next:** TLS termination via reverse proxy (nginx, Traefik, Envoy).
3. **Future:** Native mTLS on all ports using rustls, with certificate-based
   mutual authentication between cluster nodes (QUIC via quinn).

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

## 11. Production Hardening Checklist

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

### General

- [ ] Run `cargo clippy --workspace -- -D warnings` to catch code quality
      issues.
- [ ] Keep Aeon and all dependencies up to date with security patches.
- [ ] Review the `docker-compose.prod.yml` configuration before production
      deployment.
- [ ] Test your deployment with `aeon apply --dry-run` before applying
      manifests.
- [ ] Maintain an incident response plan for security events.
