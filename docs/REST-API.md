# Aeon REST API Reference

REST API for managing processors, pipelines, and delivery tracking in Aeon.

**Default port**: 4471  
**Base URL**: `http://<host>:4471`  
**Content-Type**: `application/json` for all request and response bodies  

---

## Table of Contents

1. [Authentication](#authentication)
2. [Security](#security)
3. [Input Validation](#input-validation)
4. [Health Endpoints](#health-endpoints)
5. [Processor Endpoints](#processor-endpoints)
6. [Pipeline Endpoints](#pipeline-endpoints)
7. [Upgrade Endpoints](#upgrade-endpoints)
8. [Delivery Endpoints](#delivery-endpoints)
9. [Error Responses](#error-responses)
10. [Examples](#examples)

---

## Authentication

Authentication uses Bearer tokens via the `Authorization` header.

- Set the `AEON_API_TOKEN` environment variable to enable authentication.
- When `AEON_API_TOKEN` is **not set**, authentication is disabled (dev mode). All API endpoints are accessible without credentials.
- When `AEON_API_TOKEN` is set, all `/api/v1/` endpoints require a valid Bearer token.
- Health endpoints (`/health`, `/ready`) always bypass authentication.

**Header format:**

```
Authorization: Bearer <token>
```

**Responses when authentication fails:**

| Scenario | Status | Body |
|----------|--------|------|
| Missing or malformed header | `401 Unauthorized` | `{"error": "missing or malformed Authorization header (expected: Bearer <token>)"}` |
| Invalid token | `401 Unauthorized` | `{"error": "invalid bearer token"}` |

---

## Security

All responses include the following security headers:

| Header | Value | Purpose |
|--------|-------|---------|
| `X-Content-Type-Options` | `nosniff` | Prevents MIME type sniffing (OWASP A05) |
| `X-Frame-Options` | `DENY` | Prevents clickjacking via iframes (OWASP A05) |
| `Cache-Control` | `no-store` | Prevents caching of sensitive API responses |

**Request body limit**: 10 MB maximum. Requests exceeding this limit are rejected.

**Request logging**: All requests are logged via tower-http TraceLayer (OWASP A09).

---

## Input Validation

Resource names (processor names, pipeline names) are validated against the following rules:

- Must be 1--128 characters long.
- Must not contain `..`, `/`, or `\`.
- Must not start with `.` or `-`.
- Must not contain control characters.

Requests that violate these rules receive a `400 Bad Request` response.

---

## Health Endpoints

Health endpoints do not require authentication, even when `AEON_API_TOKEN` is set.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Liveness check |
| GET | `/ready` | Readiness check |

### GET /health

Returns the server health status and version.

**Response** `200 OK`:

```json
{
  "status": "ok",
  "version": "0.1.0"
}
```

### GET /ready

Returns the server readiness status and version.

**Response** `200 OK`:

```json
{
  "status": "ready",
  "version": "0.1.0"
}
```

---

## Processor Endpoints

All processor endpoints require authentication when `AEON_API_TOKEN` is set.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/processors` | List all processors |
| GET | `/api/v1/processors/{name}` | Get processor details |
| GET | `/api/v1/processors/{name}/versions` | List all versions of a processor |
| POST | `/api/v1/processors` | Register a processor (metadata + artifact) |
| DELETE | `/api/v1/processors/{name}/versions/{version}` | Delete a specific version |

### GET /api/v1/processors

List all registered processors.

**Response** `200 OK`:

```json
[
  {
    "name": "my-processor",
    "version_count": 3,
    "latest_version": "1.2.0"
  }
]
```

Returns an empty array `[]` when no processors are registered.

### GET /api/v1/processors/{name}

Get full details of a processor, including all versions.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `name` | string | Processor name |

**Response** `200 OK`:

```json
{
  "name": "my-processor",
  "description": "Event enrichment processor",
  "versions": {
    "1.0.0": {
      "version": "1.0.0",
      "sha512": "abc123...",
      "size_bytes": 1048576,
      "processor_type": "wasm",
      "platform": "wasm32",
      "status": "available",
      "registered_at": 1700000000000,
      "registered_by": "cli"
    }
  },
  "created_at": 1700000000000,
  "updated_at": 1700000000000
}
```

**Response** `404 Not Found`:

```json
{
  "error": "processor 'my-processor' not found"
}
```

**Field reference -- ProcessorVersion:**

| Field | Type | Description |
|-------|------|-------------|
| `version` | string | Semantic version (e.g., "1.2.3") |
| `sha512` | string | SHA-512 hash of the artifact |
| `size_bytes` | integer | Artifact size in bytes |
| `processor_type` | string | `"wasm"` or `"native-so"` |
| `platform` | string | Target platform (e.g., "wasm32", "linux-x86_64") |
| `status` | string | `"available"`, `"active"`, or `"archived"` |
| `registered_at` | integer | Unix epoch milliseconds |
| `registered_by` | string | Operator ID or "cli" |

### GET /api/v1/processors/{name}/versions

List all versions of a specific processor.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `name` | string | Processor name |

**Response** `200 OK`: Returns a map of version strings to `ProcessorVersion` objects (same structure as in the processor detail response).

**Response** `404 Not Found` if the processor does not exist.

### DELETE /api/v1/processors/{name}/versions/{version}

Delete a specific processor version.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `name` | string | Processor name |
| `version` | string | Version string (e.g., "1.0.0") |

**Response** `200 OK`:

```json
{
  "status": "deleted"
}
```

**Response** `400 Bad Request` if the version cannot be deleted (e.g., currently active).

---

## Pipeline Endpoints

All pipeline endpoints require authentication when `AEON_API_TOKEN` is set.

### CRUD and Lifecycle

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/pipelines` | List all pipelines |
| GET | `/api/v1/pipelines/{name}` | Get pipeline details |
| POST | `/api/v1/pipelines` | Create a pipeline |
| DELETE | `/api/v1/pipelines/{name}` | Delete a pipeline |
| POST | `/api/v1/pipelines/{name}/start` | Start a pipeline |
| POST | `/api/v1/pipelines/{name}/stop` | Stop a pipeline |
| GET | `/api/v1/pipelines/{name}/history` | Get lifecycle history |

### GET /api/v1/pipelines

List all pipelines with their current state.

**Response** `200 OK`:

```json
[
  {
    "name": "my-pipeline",
    "state": "running"
  }
]
```

**Pipeline states:** `created`, `running`, `stopped`, `upgrading`, `failed`

### GET /api/v1/pipelines/{name}

Get full pipeline definition and current state.

**Response** `200 OK`:

```json
{
  "name": "my-pipeline",
  "source": {
    "type": "kafka",
    "topic": "input-events",
    "partitions": [0, 1, 2],
    "config": {}
  },
  "processor": {
    "name": "my-processor",
    "version": "1.0.0"
  },
  "sink": {
    "type": "kafka",
    "topic": "output-events",
    "config": {}
  },
  "upgrade_strategy": "drain-swap",
  "state": "running",
  "created_at": 1700000000000,
  "updated_at": 1700000000000,
  "assigned_node": null
}
```

**Response** `404 Not Found` if the pipeline does not exist.

### POST /api/v1/pipelines

Create a new pipeline. The pipeline is created in the `created` state and must be started separately.

**Request body:**

```json
{
  "name": "my-pipeline",
  "source": {
    "type": "kafka",
    "topic": "input-events",
    "partitions": [0, 1, 2],
    "config": {}
  },
  "processor": {
    "name": "my-processor",
    "version": "1.0.0"
  },
  "sink": {
    "type": "kafka",
    "topic": "output-events",
    "config": {}
  },
  "upgrade_strategy": "drain-swap",
  "state": "created",
  "created_at": 1700000000000,
  "updated_at": 1700000000000
}
```

**Request body fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Unique pipeline name (1--128 chars, validated) |
| `source` | object | yes | Source configuration |
| `source.type` | string | yes | Source type: `"kafka"`, `"memory"`, `"blackhole"` |
| `source.topic` | string | no | Topic name (required for Kafka sources) |
| `source.partitions` | array of integers | no | Partition assignments |
| `source.config` | object | no | Additional key-value configuration |
| `processor` | object | yes | Processor reference |
| `processor.name` | string | yes | Processor name in the registry |
| `processor.version` | string | yes | Version string (e.g., "1.0.0" or "latest") |
| `sink` | object | yes | Sink configuration |
| `sink.type` | string | yes | Sink type: `"kafka"`, `"blackhole"`, `"stdout"` |
| `sink.topic` | string | no | Topic name (required for Kafka sinks) |
| `sink.config` | object | no | Additional key-value configuration |
| `upgrade_strategy` | string | no | `"drain-swap"` (default), `"blue-green"`, or `"canary"` |
| `state` | string | yes | Initial state (typically `"created"`) |
| `created_at` | integer | yes | Unix epoch milliseconds |
| `updated_at` | integer | yes | Unix epoch milliseconds |

**Response** `201 Created`:

```json
{
  "status": "created"
}
```

**Response** `400 Bad Request` if validation fails or the pipeline name already exists.

### DELETE /api/v1/pipelines/{name}

Delete a pipeline.

**Response** `200 OK`:

```json
{
  "status": "deleted"
}
```

### POST /api/v1/pipelines/{name}/start

Start a stopped or newly created pipeline.

**Response** `200 OK`:

```json
{
  "status": "started"
}
```

### POST /api/v1/pipelines/{name}/stop

Stop a running pipeline.

**Response** `200 OK`:

```json
{
  "status": "stopped"
}
```

### GET /api/v1/pipelines/{name}/history

Get the lifecycle history of a pipeline, ordered chronologically.

**Response** `200 OK`:

```json
[
  {
    "timestamp": 1700000000000,
    "action": "created",
    "actor": "api",
    "from_state": "created",
    "to_state": "created",
    "details": null
  },
  {
    "timestamp": 1700000001000,
    "action": "started",
    "actor": "api",
    "from_state": "created",
    "to_state": "running",
    "details": null
  }
]
```

**Pipeline actions:** `created`, `started`, `stopped`, `upgrade-started`, `upgrade-completed`, `upgrade-rolled-back`, `failed`, `blue-green-started`, `blue-green-cutover`, `canary-started`, `canary-promoted`, `canary-completed`

---

## Upgrade Endpoints

All upgrade endpoints require authentication when `AEON_API_TOKEN` is set. The pipeline must be in `running` state to initiate an upgrade.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/pipelines/{name}/upgrade` | Drain-swap upgrade |
| POST | `/api/v1/pipelines/{name}/upgrade/blue-green` | Start blue-green upgrade |
| POST | `/api/v1/pipelines/{name}/upgrade/canary` | Start canary upgrade |
| POST | `/api/v1/pipelines/{name}/cutover` | Cut over blue-green to new version |
| POST | `/api/v1/pipelines/{name}/rollback` | Roll back an in-progress upgrade |
| POST | `/api/v1/pipelines/{name}/promote` | Promote canary to next traffic step |
| GET | `/api/v1/pipelines/{name}/canary-status` | Get canary upgrade status |

### POST /api/v1/pipelines/{name}/upgrade

Perform a drain-swap upgrade. Drains in-flight events, swaps the processor, and resumes. Typical pause is under 100ms.

**Request body:**

```json
{
  "processor_name": "my-processor",
  "processor_version": "2.0.0"
}
```

**Response** `200 OK`:

```json
{
  "status": "upgraded"
}
```

### POST /api/v1/pipelines/{name}/upgrade/blue-green

Start a blue-green upgrade. The old (blue) and new (green) processors run in parallel. Use the cutover endpoint to switch live traffic to the new version.

**Request body:**

```json
{
  "processor_name": "my-processor",
  "processor_version": "2.0.0"
}
```

**Response** `200 OK`:

```json
{
  "status": "blue-green-started"
}
```

### POST /api/v1/pipelines/{name}/upgrade/canary

Start a canary upgrade with gradual traffic shifting and configurable thresholds for auto-rollback.

**Request body:**

```json
{
  "processor_name": "my-processor",
  "processor_version": "2.0.0",
  "steps": [10, 50, 100],
  "thresholds": {
    "max_error_rate": 0.01,
    "max_p99_latency_ms": 100,
    "min_throughput_ratio": 0.8
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `processor_name` | string | yes | -- | Processor name |
| `processor_version` | string | yes | -- | Target version |
| `steps` | array of integers | no | `[10, 50, 100]` | Traffic percentage steps (0--100) |
| `thresholds.max_error_rate` | float | no | `0.01` | Max error rate before auto-rollback (0.0--1.0) |
| `thresholds.max_p99_latency_ms` | integer | no | `100` | Max P99 latency (ms) before auto-rollback |
| `thresholds.min_throughput_ratio` | float | no | `0.8` | Min throughput ratio (canary/baseline) before auto-rollback |

**Response** `200 OK`:

```json
{
  "status": "canary-started"
}
```

### POST /api/v1/pipelines/{name}/cutover

Cut over a blue-green deployment to the new (green) processor. Only valid when a blue-green upgrade is in progress.

**Request body:** None.

**Response** `200 OK`:

```json
{
  "status": "cutover-complete"
}
```

### POST /api/v1/pipelines/{name}/rollback

Roll back an in-progress upgrade (blue-green or canary) to the original processor version.

**Request body:** None.

**Response** `200 OK`:

```json
{
  "status": "rolled-back"
}
```

### POST /api/v1/pipelines/{name}/promote

Promote a canary deployment to the next traffic step. When the final step is reached and promoted, the canary completes and the new processor becomes the active version.

**Request body:** None.

**Response** `200 OK`:

```json
{
  "status": "promoted"
}
```

### GET /api/v1/pipelines/{name}/canary-status

Get the current status of a canary upgrade.

**Response** `200 OK`:

```json
{
  "baseline_processor": {
    "name": "my-processor",
    "version": "1.0.0"
  },
  "canary_processor": {
    "name": "my-processor",
    "version": "2.0.0"
  },
  "traffic_pct": 10,
  "steps": [10, 50, 100],
  "current_step": 0,
  "thresholds": {
    "max_error_rate": 0.01,
    "max_p99_latency_ms": 100,
    "min_throughput_ratio": 0.8
  },
  "started_at": 1700000000000
}
```

**Response** `404 Not Found` if no canary upgrade is in progress:

```json
{
  "error": "no canary upgrade in progress for 'my-pipeline'"
}
```

---

## Delivery Endpoints

Delivery endpoints provide visibility into event delivery tracking per pipeline. They require authentication when `AEON_API_TOKEN` is set.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/pipelines/{name}/delivery` | Get delivery status |
| POST | `/api/v1/pipelines/{name}/delivery/retry` | Retry failed events |

### GET /api/v1/pipelines/{name}/delivery

Get delivery tracking status for a pipeline, including counts and details of failed events.

**Response** `200 OK`:

```json
{
  "pipeline": "my-pipeline",
  "pending_count": 5,
  "failed_count": 2,
  "total_tracked": 10000,
  "total_acked": 9993,
  "total_failed": 2,
  "oldest_pending_age_ms": 1500,
  "failed_entries": [
    {
      "event_id": "01912345-6789-7abc-def0-123456789abc",
      "partition": 0,
      "source_offset": 12345,
      "reason": "timeout",
      "attempts": 3
    }
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `pipeline` | string | Pipeline name |
| `pending_count` | integer | Events currently in-flight (tracked but not yet acked or failed) |
| `failed_count` | integer | Events currently in failed state |
| `total_tracked` | integer | Total events tracked since pipeline start |
| `total_acked` | integer | Total events successfully acknowledged |
| `total_failed` | integer | Total events that have failed |
| `oldest_pending_age_ms` | integer or null | Age of the oldest pending event in milliseconds |
| `failed_entries` | array | Details of currently failed events |

**Response** `404 Not Found` if no delivery ledger exists for the pipeline:

```json
{
  "error": "no delivery ledger for pipeline 'my-pipeline'"
}
```

### POST /api/v1/pipelines/{name}/delivery/retry

Mark specific failed events as eligible for retry. The events are removed from the failed state; the pipeline re-enqueues them for delivery.

**Request body:**

```json
{
  "event_ids": [
    "01912345-6789-7abc-def0-123456789abc",
    "01912345-6789-7abc-def0-123456789def"
  ]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event_ids` | array of strings | yes | UUIDs of failed events to retry |

**Response** `200 OK`:

```json
{
  "retried": 1,
  "not_found": 1
}
```

| Field | Type | Description |
|-------|------|-------------|
| `retried` | integer | Number of events successfully marked for retry |
| `not_found` | integer | Number of event IDs not found or with invalid UUID format |

**Response** `404 Not Found` if no delivery ledger exists for the pipeline.

---

## Error Responses

All error responses share a common JSON format:

```json
{
  "error": "description of the error"
}
```

### Common HTTP Status Codes

| Status | Meaning | Typical Cause |
|--------|---------|---------------|
| `200 OK` | Success | Request processed successfully |
| `201 Created` | Resource created | Pipeline created successfully |
| `400 Bad Request` | Invalid request | Validation failure, duplicate name, invalid state transition |
| `401 Unauthorized` | Authentication failed | Missing, malformed, or invalid Bearer token |
| `404 Not Found` | Resource not found | Processor, pipeline, or delivery ledger does not exist |

---

## Examples

All examples assume the API is running on `localhost:4471`. Replace `$TOKEN` with your API token value.

### Check health (no auth required)

```bash
curl http://localhost:4471/health
```

### List processors

```bash
curl -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/processors
```

### Inspect a processor

```bash
curl -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/processors/my-processor
```

### List processor versions

```bash
curl -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/processors/my-processor/versions
```

### Delete a processor version

```bash
curl -X DELETE -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/processors/my-processor/versions/1.0.0
```

### Create a pipeline

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "events-pipeline",
    "source": {
      "type": "kafka",
      "topic": "raw-events",
      "partitions": [0, 1, 2, 3],
      "config": {}
    },
    "processor": {
      "name": "enrichment",
      "version": "1.0.0"
    },
    "sink": {
      "type": "kafka",
      "topic": "enriched-events",
      "config": {}
    },
    "upgrade_strategy": "drain-swap",
    "state": "created",
    "created_at": 1700000000000,
    "updated_at": 1700000000000
  }' \
  http://localhost:4471/api/v1/pipelines
```

### Start a pipeline

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/start
```

### Stop a pipeline

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/stop
```

### Drain-swap upgrade

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"processor_name": "enrichment", "processor_version": "2.0.0"}' \
  http://localhost:4471/api/v1/pipelines/events-pipeline/upgrade
```

### Blue-green upgrade workflow

```bash
# 1. Start blue-green upgrade
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"processor_name": "enrichment", "processor_version": "2.0.0"}' \
  http://localhost:4471/api/v1/pipelines/events-pipeline/upgrade/blue-green

# 2. Validate the green processor, then cut over
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/cutover

# Or rollback if validation fails
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/rollback
```

### Canary upgrade workflow

```bash
# 1. Start canary with custom steps and thresholds
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "processor_name": "enrichment",
    "processor_version": "2.0.0",
    "steps": [10, 50, 100],
    "thresholds": {
      "max_error_rate": 0.005,
      "max_p99_latency_ms": 50,
      "min_throughput_ratio": 0.9
    }
  }' \
  http://localhost:4471/api/v1/pipelines/events-pipeline/upgrade/canary

# 2. Check canary status
curl -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/canary-status

# 3. Promote to next step (50%)
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/promote

# 4. Promote again to complete (100%)
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/promote

# Or rollback at any point
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/rollback
```

### Check delivery status

```bash
curl -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/delivery
```

### Retry failed events

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"event_ids": ["01912345-6789-7abc-def0-123456789abc"]}' \
  http://localhost:4471/api/v1/pipelines/events-pipeline/delivery/retry
```

### View pipeline history

```bash
curl -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline/history
```

### Delete a pipeline

```bash
curl -X DELETE -H "Authorization: Bearer $TOKEN" \
  http://localhost:4471/api/v1/pipelines/events-pipeline
```
