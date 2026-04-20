//! REST API server for Aeon management (axum, port 4471).
//!
//! Provides CRUD endpoints for processors, pipelines, and cluster status.
//! Authentication via API key (Bearer token) set by `AEON_API_TOKEN` env var.
//! When `AEON_API_TOKEN` is unset, authentication is disabled (dev mode).
//!
//! ## Security
//!
//! - **Authentication**: Bearer token via `Authorization` header (OWASP A01).
//! - **Security headers**: X-Content-Type-Options, X-Frame-Options (OWASP A05).
//! - **Request body limit**: 10 MB max (OWASP A04).
//! - **Request logging**: All requests logged via tower-http TraceLayer (OWASP A09).
//! - Health endpoints (`/health`, `/ready`) bypass authentication.
//!
//! ## Endpoints
//!
//! **Processors:**
//! - `GET    /api/v1/processors`                    — list all
//! - `GET    /api/v1/processors/:name`              — inspect
//! - `GET    /api/v1/processors/:name/versions`     — list versions
//! - `POST   /api/v1/processors`                    — register (JSON metadata + artifact)
//! - `DELETE /api/v1/processors/:name/versions/:ver` — delete version
//!
//! **Pipelines:**
//! - `GET    /api/v1/pipelines`                     — list all
//! - `GET    /api/v1/pipelines/:name`               — inspect
//! - `POST   /api/v1/pipelines`                     — create
//! - `POST   /api/v1/pipelines/:name/start`         — start
//! - `POST   /api/v1/pipelines/:name/stop`          — stop
//! - `POST   /api/v1/pipelines/:name/upgrade`       — upgrade processor
//! - `GET    /api/v1/pipelines/:name/history`        — lifecycle history
//! - `DELETE /api/v1/pipelines/:name`               — delete
//! - `GET    /api/v1/pipelines/:name/verify`        — PoH/Merkle integrity
//!
//! **System:**
//! - `GET    /health`                               — health check (no auth)
//! - `GET    /ready`                                — readiness check (no auth)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::delivery_ledger::DeliveryLedger;
use crate::identity_store::ProcessorIdentityStore;
use crate::pipeline::PipelineControl;
use crate::pipeline_manager::PipelineManager;
use crate::pipeline_supervisor::PipelineSupervisor;
use crate::registry::ProcessorRegistry;

/// Maximum request body size (10 MB).
const MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Shared application state for all handlers.
pub struct AppState {
    pub registry: Arc<ProcessorRegistry>,
    pub pipelines: Arc<PipelineManager>,
    /// Bridge from `PipelineManager`'s declarative state to actually-running
    /// tokio tasks. Handlers for `/start` and `/stop` consult this *after*
    /// updating the manager (or after the Raft commit in cluster mode) so
    /// that the process converges on what the manifest declares.
    pub supervisor: Arc<PipelineSupervisor>,
    /// Per-pipeline delivery ledgers (pipeline_name → ledger).
    /// Populated when pipelines run with delivery tracking enabled.
    pub delivery_ledgers: dashmap::DashMap<String, Arc<DeliveryLedger>>,
    /// Per-pipeline control handles for drain→swap→resume hot-swap.
    /// Registered when a managed pipeline starts, removed when it stops.
    pub pipeline_controls: dashmap::DashMap<String, Arc<PipelineControl>>,
    /// Per-pipeline metrics handles for live-tail streaming.
    /// Registered when a managed pipeline starts, removed when it stops.
    pub pipeline_metrics: dashmap::DashMap<String, Arc<crate::pipeline::PipelineMetrics>>,
    /// Per-pipeline PoH chain state. Populated when a pipeline runs with
    /// `poh_enabled` in its config. Used by the verify endpoint.
    #[cfg(feature = "processor-auth")]
    pub poh_chains: dashmap::DashMap<String, crate::pipeline::PohState>,
    /// Processor identity store (ED25519 public keys for T3/T4 auth).
    pub identities: Arc<ProcessorIdentityStore>,
    /// API key authenticator for Bearer token auth. `None` disables auth (dev mode).
    /// Uses constant-time comparison and supports multiple named API keys.
    #[cfg(feature = "processor-auth")]
    pub authenticator: Option<aeon_crypto::auth::ApiKeyAuthenticator>,
    /// Fallback: simple API token when processor-auth feature is disabled.
    #[cfg(not(feature = "processor-auth"))]
    pub api_token: Option<String>,
    /// T4 WebSocket processor host (feature-gated).
    #[cfg(feature = "websocket-host")]
    pub ws_host: Option<Arc<crate::transport::websocket_host::WebSocketProcessorHost>>,
    /// Cluster node reference for cluster status queries.
    /// `None` in standalone (non-cluster) mode.
    #[cfg(feature = "cluster")]
    pub cluster_node: Option<Arc<aeon_cluster::ClusterNode>>,
    /// Flipped to `true` when the process has received SIGINT/SIGTERM and is
    /// draining. `/ready` returns 503 while this is set so K8s removes the
    /// pod from service endpoints before the pipeline stops.
    pub shutting_down: Arc<AtomicBool>,
}

/// Build the axum Router with all API routes.
///
/// Health endpoints bypass authentication. All `/api/v1/` routes require
/// a valid Bearer token when `AEON_API_TOKEN` is set.
pub fn api_router(state: Arc<AppState>) -> Router {
    // Health routes — no auth required
    let health_routes = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics_prometheus));

    // API routes — auth required when token is configured
    let api_routes = Router::new()
        // Processors
        .route(
            "/api/v1/processors",
            get(list_processors).post(register_processor),
        )
        .route("/api/v1/processors/{name}", get(get_processor))
        .route(
            "/api/v1/processors/{name}/versions",
            get(list_processor_versions),
        )
        .route(
            "/api/v1/processors/{name}/versions/{version}",
            delete(delete_processor_version),
        )
        // Pipelines
        .route(
            "/api/v1/pipelines",
            get(list_pipelines).post(create_pipeline),
        )
        .route(
            "/api/v1/pipelines/{name}",
            get(get_pipeline).delete(delete_pipeline),
        )
        .route("/api/v1/pipelines/{name}/start", post(start_pipeline))
        .route("/api/v1/pipelines/{name}/stop", post(stop_pipeline))
        .route("/api/v1/pipelines/{name}/upgrade", post(upgrade_pipeline))
        .route(
            "/api/v1/pipelines/{name}/upgrade/blue-green",
            post(upgrade_blue_green),
        )
        .route(
            "/api/v1/pipelines/{name}/upgrade/canary",
            post(upgrade_canary),
        )
        .route("/api/v1/pipelines/{name}/cutover", post(cutover_pipeline))
        .route("/api/v1/pipelines/{name}/rollback", post(rollback_pipeline))
        .route("/api/v1/pipelines/{name}/promote", post(promote_canary))
        .route("/api/v1/pipelines/{name}/canary-status", get(canary_status))
        .route("/api/v1/pipelines/{name}/history", get(pipeline_history))
        // Reconfiguration (same-type source/sink swap)
        .route(
            "/api/v1/pipelines/{name}/reconfigure/source",
            post(reconfigure_source),
        )
        .route(
            "/api/v1/pipelines/{name}/reconfigure/sink",
            post(reconfigure_sink),
        )
        // Identities
        .route(
            "/api/v1/processors/{name}/identities",
            get(list_identities).post(register_identity),
        )
        .route(
            "/api/v1/processors/{name}/identities/{fingerprint}",
            delete(revoke_identity),
        )
        // Delivery
        .route("/api/v1/pipelines/{name}/delivery", get(delivery_status))
        .route(
            "/api/v1/pipelines/{name}/delivery/retry",
            post(delivery_retry),
        )
        // Integrity verification
        .route("/api/v1/pipelines/{name}/verify", get(verify_pipeline))
        // Cluster status
        .route("/api/v1/cluster/status", get(cluster_status))
        .route(
            "/api/v1/cluster/partitions/{partition}/transfer",
            post(transfer_partition),
        )
        .route("/api/v1/cluster/drain", post(cluster_drain))
        .route("/api/v1/cluster/rebalance", post(cluster_rebalance))
        .route("/api/v1/cluster/leave", post(cluster_leave))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // WebSocket processor connect — no Bearer auth (AWPP handshake handles auth)
    #[cfg(feature = "websocket-host")]
    let health_routes = health_routes
        .route("/api/v1/processors/connect", get(ws_processor_connect))
        .route("/api/v1/pipelines/{name}/tail", get(pipeline_tail));

    health_routes
        .merge(api_routes)
        .with_state(state)
        // Security headers (OWASP A05)
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        // Request body limit (OWASP A04)
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        // Request logging (OWASP A09)
        .layer(TraceLayer::new_for_http())
}

/// Bearer token authentication middleware.
///
/// When `processor-auth` feature is enabled, uses `ApiKeyAuthenticator` with
/// constant-time comparison and multi-key support. When the authenticator is
/// `None`, all requests pass through (dev mode).
#[cfg(feature = "processor-auth")]
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> impl IntoResponse {
    let Some(ref authenticator) = state.authenticator else {
        return next.run(req).await;
    };

    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(header) => match authenticator.validate_header(header) {
            Ok(_key_name) => next.run(req).await,
            Err(_) => (
                StatusCode::UNAUTHORIZED,
                Json(ApiError {
                    error: "invalid bearer token".into(),
                }),
            )
                .into_response(),
        },
        None => (
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "missing or malformed Authorization header (expected: Bearer <token>)"
                    .into(),
            }),
        )
            .into_response(),
    }
}

/// Fallback auth middleware when `processor-auth` feature is disabled.
#[cfg(not(feature = "processor-auth"))]
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> impl IntoResponse {
    let Some(ref expected_token) = state.api_token else {
        return next.run(req).await;
    };

    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(header) if header.starts_with("Bearer ") => {
            let token = &header[7..];
            if token == expected_token {
                next.run(req).await
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(ApiError {
                        error: "invalid bearer token".into(),
                    }),
                )
                    .into_response()
            }
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "missing or malformed Authorization header (expected: Bearer <token>)"
                    .into(),
            }),
        )
            .into_response(),
    }
}

/// Start the REST API server on the given address.
///
/// Installs a SIGINT/SIGTERM handler that drives graceful shutdown:
///   1. Flips `AppState.shutting_down` so `/ready` starts returning 503.
///   2. Sleeps `AEON_PRESTOP_DELAY_SECS` (default 5s) to let K8s endpoints
///      propagate the not-ready status through kube-proxy / iptables.
///   3. Stops every running pipeline via the supervisor (which drains SPSC
///      buffers and flushes sinks).
///   4. Shuts the Raft cluster node down cleanly so leadership and partition
///      ownership don't need to be rediscovered on restart.
///   5. Returns, letting axum's `with_graceful_shutdown` finish in-flight
///      HTTP requests before the server exits.
pub async fn serve(state: Arc<AppState>, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let auth_enabled = {
        #[cfg(feature = "processor-auth")]
        {
            state.authenticator.is_some()
        }
        #[cfg(not(feature = "processor-auth"))]
        {
            state.api_token.is_some()
        }
    };
    if auth_enabled {
        tracing::info!(addr = addr, "REST API server listening (auth enabled)");
    } else {
        tracing::warn!(
            addr = addr,
            "REST API server listening (auth DISABLED — configure api_keys to enable)"
        );
    }
    let app = api_router(Arc::clone(&state));
    let listener = TcpListener::bind(addr).await?;

    let shutdown_state = Arc::clone(&state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_state))
        .await?;
    Ok(())
}

/// Future that resolves when the process should begin draining.
///
/// Listens for SIGINT and (on Unix) SIGTERM. On the first received signal,
/// drives the drain sequence documented on `serve()`.
async fn shutdown_signal(state: Arc<AppState>) {
    wait_for_terminate().await;

    tracing::warn!("shutdown signal received — beginning graceful drain");
    state.shutting_down.store(true, Ordering::Relaxed);

    // G14 — relinquish Raft leadership in parallel with endpoint propagation.
    // If this pod is the leader, disabling heartbeats lets followers
    // election-timeout and pick a new leader while we're still alive to
    // answer their RPCs. Running it concurrently with `AEON_PRESTOP_DELAY_SECS`
    // means the handoff mostly hides inside the window K8s needs to propagate
    // the /ready=503 flip through kube-proxy.
    let prestop = std::env::var("AEON_PRESTOP_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5);

    let relinquish = async {
        #[cfg(feature = "cluster")]
        {
            if let Some(node) = &state.cluster_node {
                let budget = std::time::Duration::from_millis(
                    std::env::var("AEON_LEADER_RELINQUISH_MS")
                        .ok()
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(4_000),
                );
                if let Err(e) = node.relinquish_leadership(budget).await {
                    tracing::warn!(error = %e, "leader relinquish failed — proceeding with shutdown anyway");
                }
            }
        }
    };

    let prestop_sleep = async {
        if prestop > 0 {
            tracing::info!(
                secs = prestop,
                "waiting for endpoint propagation before stopping pipelines"
            );
            tokio::time::sleep(std::time::Duration::from_secs(prestop)).await;
        }
    };

    tokio::join!(relinquish, prestop_sleep);

    // Stop every running pipeline. PipelineSupervisor::stop flips the
    // per-pipeline shutdown flag, awaits the task handle, and lets the
    // pipeline drain SPSC buffers and flush sinks.
    let running = state.supervisor.list_running().await;
    if !running.is_empty() {
        tracing::info!(count = running.len(), "stopping pipelines");
        for name in running {
            if let Err(e) = state.supervisor.stop(&name).await {
                tracing::warn!(pipeline = %name, error = %e, "pipeline stop failed during shutdown");
            }
        }
    }

    // Shut down the cluster node so Raft logs are flushed and the QUIC
    // endpoint closes cleanly. Followers will notice our absence and let
    // openraft re-elect if we were leader.
    #[cfg(feature = "cluster")]
    {
        if let Some(node) = &state.cluster_node {
            if let Err(e) = node.shutdown().await {
                tracing::warn!(error = %e, "cluster node shutdown reported error");
            }
        }
    }

    tracing::info!("drain complete — axum will stop accepting new requests");
}

/// Wait for SIGINT or (on Unix) SIGTERM, whichever arrives first.
#[cfg(unix)]
async fn wait_for_terminate() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to install SIGTERM handler; falling back to SIGINT only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT"),
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
    }
}

#[cfg(not(unix))]
async fn wait_for_terminate() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("received Ctrl+C");
}

// ── Input validation (OWASP A03) ──────────────────────────────────────

/// Validate a resource name for path traversal and injection prevention.
fn validate_resource_name(name: &str, kind: &str) -> Result<(), (StatusCode, Json<ApiError>)> {
    if name.is_empty() || name.len() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{kind} name must be 1-128 characters"),
            }),
        ));
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{kind} name must not contain '..', '/', or '\\\\'"),
            }),
        ));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{kind} name must not start with '.' or '-'"),
            }),
        ));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{kind} name must not contain control characters"),
            }),
        ));
    }
    Ok(())
}

// ── Response types ─────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn api_error(status: StatusCode, msg: impl Into<String>) -> impl IntoResponse {
    (status, Json(ApiError { error: msg.into() }))
}

/// G9 — auto-forward cluster-write requests from a follower to the Raft leader.
///
/// Returns `Some(307 Temporary Redirect)` when this pod is not the current
/// leader and the leader's REST URL can be derived; the caller should
/// short-circuit the handler and return this response. Returns `None`
/// when this pod IS the leader (proceed locally), or when auto-forward
/// is not configured / leader is unknown (fall through to the existing
/// `NotLeader`-style error path).
///
/// Always emits `X-Leader-Node-Id` when a leader is known, so scripted
/// clients that don't follow 307 can still route themselves without
/// parsing redirect URLs.
#[cfg(feature = "cluster")]
fn maybe_forward_to_leader(
    state: &Arc<AppState>,
    uri: &axum::http::Uri,
) -> Option<axum::response::Response> {
    let node = state.cluster_node.as_ref()?;
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path());
    let location = node.leader_rest_url(path_and_query)?;

    let leader_id = node
        .raft()
        .metrics()
        .borrow()
        .current_leader
        .unwrap_or_default();

    let mut resp = axum::response::Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(axum::http::header::LOCATION, &location)
        .header("X-Leader-Node-Id", leader_id.to_string())
        .body(axum::body::Body::empty())
        .ok()?;
    // Don't let a UTF-8 oddity in the host silently drop the redirect.
    resp.headers_mut()
        .insert("X-Aeon-Forwarded", HeaderValue::from_static("1"));
    Some(resp)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

// ── Health endpoints ───────────────────────────────────────────────────

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, Json<HealthResponse>) {
    if state.shutting_down.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthResponse {
                status: "shutting_down",
                version: env!("CARGO_PKG_VERSION"),
            }),
        );
    }
    (
        StatusCode::OK,
        Json(HealthResponse {
            status: "ready",
            version: env!("CARGO_PKG_VERSION"),
        }),
    )
}

// ── Prometheus metrics endpoint ───────────────────────────────────────

/// `GET /metrics` — Prometheus exposition for the full Aeon node.
///
/// Emits per-pipeline aggregate counters (events received/processed/sent,
/// checkpoints, failures, retries, PoH entries) plus cluster-level Raft
/// metrics (term, leader, replication lag, membership size — see
/// `ClusterNode::cluster_metrics_prometheus` / CL-4) when cluster mode is on.
///
/// No auth — Prometheus scrapers are typically deployed inside the cluster
/// boundary. Sits on the health_routes sub-router which bypasses the Bearer
/// auth middleware.
async fn metrics_prometheus(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;

    let mut out = String::with_capacity(4096);

    // Single source of truth: the supervisor knows what's actually running
    // on this node regardless of whether start() came from the REST handler
    // or the Raft applier. (Previously this read from AppState.pipeline_metrics,
    // which was only populated by the REST path — cluster-mode pipelines
    // started via Raft had no Prometheus samples.)
    let pipelines = state.supervisor.metrics_snapshot().await;

    // Per-pipeline aggregates
    out.push_str("# HELP aeon_pipeline_events_received_total Events received by a pipeline's source\n");
    out.push_str("# TYPE aeon_pipeline_events_received_total counter\n");
    for (name, m) in &pipelines {
        out.push_str(&format!(
            "aeon_pipeline_events_received_total{{pipeline=\"{name}\"}} {}\n",
            m.events_received.load(Ordering::Relaxed)
        ));
    }

    out.push_str("# HELP aeon_pipeline_events_processed_total Events processed by a pipeline's processor\n");
    out.push_str("# TYPE aeon_pipeline_events_processed_total counter\n");
    for (name, m) in &pipelines {
        out.push_str(&format!(
            "aeon_pipeline_events_processed_total{{pipeline=\"{name}\"}} {}\n",
            m.events_processed.load(Ordering::Relaxed)
        ));
    }

    out.push_str("# HELP aeon_pipeline_outputs_sent_total Outputs the pipeline handed to its sink\n");
    out.push_str("# TYPE aeon_pipeline_outputs_sent_total counter\n");
    for (name, m) in &pipelines {
        out.push_str(&format!(
            "aeon_pipeline_outputs_sent_total{{pipeline=\"{name}\"}} {}\n",
            m.outputs_sent.load(Ordering::Relaxed)
        ));
    }

    out.push_str("# HELP aeon_pipeline_outputs_acked_total Outputs the downstream system confirmed (broker ack, HTTP 2xx, fsync)\n");
    out.push_str("# TYPE aeon_pipeline_outputs_acked_total counter\n");
    for (name, m) in &pipelines {
        out.push_str(&format!(
            "aeon_pipeline_outputs_acked_total{{pipeline=\"{name}\"}} {}\n",
            m.outputs_acked.load(Ordering::Relaxed)
        ));
    }

    out.push_str("# HELP aeon_pipeline_events_failed_total Events permanently failed (retry-exhausted or skip-to-dlq)\n");
    out.push_str("# TYPE aeon_pipeline_events_failed_total counter\n");
    for (name, m) in &pipelines {
        out.push_str(&format!(
            "aeon_pipeline_events_failed_total{{pipeline=\"{name}\"}} {}\n",
            m.events_failed.load(Ordering::Relaxed)
        ));
    }

    out.push_str("# HELP aeon_pipeline_events_retried_total Individual retry attempts\n");
    out.push_str("# TYPE aeon_pipeline_events_retried_total counter\n");
    for (name, m) in &pipelines {
        out.push_str(&format!(
            "aeon_pipeline_events_retried_total{{pipeline=\"{name}\"}} {}\n",
            m.events_retried.load(Ordering::Relaxed)
        ));
    }

    out.push_str("# HELP aeon_pipeline_checkpoints_written_total Checkpoint records appended\n");
    out.push_str("# TYPE aeon_pipeline_checkpoints_written_total counter\n");
    for (name, m) in &pipelines {
        out.push_str(&format!(
            "aeon_pipeline_checkpoints_written_total{{pipeline=\"{name}\"}} {}\n",
            m.checkpoints_written.load(Ordering::Relaxed)
        ));
    }

    out.push_str("# HELP aeon_pipeline_poh_entries_total PoH chain entries appended (per batch)\n");
    out.push_str("# TYPE aeon_pipeline_poh_entries_total counter\n");
    for (name, m) in &pipelines {
        out.push_str(&format!(
            "aeon_pipeline_poh_entries_total{{pipeline=\"{name}\"}} {}\n",
            m.poh_entries.load(Ordering::Relaxed)
        ));
    }

    // Cluster-level Raft metrics (CL-4)
    #[cfg(feature = "cluster")]
    if let Some(node) = state.cluster_node.as_ref() {
        out.push_str(&node.cluster_metrics_prometheus());
    }

    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}

// ── Cluster status endpoint ───────────────────────────────────────────

/// Returns cluster status: node ID, leader, partition assignments.
/// In standalone mode, returns a minimal response indicating no cluster.
async fn cluster_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    #[cfg(feature = "cluster")]
    {
        if let Some(ref node) = state.cluster_node {
            let leader = node.current_leader().await;
            let config = node.config();
            let pt = node.partition_table_snapshot().await;

            // Build partition assignment map: partition_id → owner_node_id
            let mut partitions = serde_json::Map::new();
            for (pid, ownership) in pt.iter() {
                let owner_str = match ownership {
                    aeon_cluster::types::PartitionOwnership::Owned(n) => {
                        serde_json::json!({ "owner": n, "status": "owned" })
                    }
                    aeon_cluster::types::PartitionOwnership::Transferring {
                        source, target, ..
                    } => {
                        serde_json::json!({
                            "source": source,
                            "target": target,
                            "status": "transferring"
                        })
                    }
                };
                partitions.insert(pid.as_u16().to_string(), owner_str);
            }

            let metrics = node.raft().metrics().borrow().clone();

            Json(serde_json::json!({
                "mode": "cluster",
                "node_id": config.node_id,
                "leader_id": leader,
                "num_partitions": config.num_partitions,
                "partitions": partitions,
                "raft": {
                    "state": format!("{:?}", metrics.state),
                    "current_term": metrics.current_term,
                    "last_applied": metrics.last_applied.map(|l| l.index),
                    "last_log_index": metrics.last_log_index,
                    "membership": format!("{:?}", metrics.membership_config.membership()),
                }
            }))
        } else {
            Json(serde_json::json!({
                "mode": "standalone",
                "message": "no cluster node configured"
            }))
        }
    }
    #[cfg(not(feature = "cluster"))]
    {
        let _ = state;
        Json(serde_json::json!({
            "mode": "standalone",
            "message": "cluster feature not enabled"
        }))
    }
}

// ── Partition transfer endpoint (CL-6) ─────────────────────────────────

/// Body for `POST /api/v1/cluster/partitions/{partition}/transfer`.
///
/// Initiates a partition handover to `target_node_id`. The source is read
/// from the committed partition table; callers don't need to know it.
#[derive(Deserialize)]
#[cfg_attr(not(feature = "cluster"), allow(dead_code))]
struct TransferPartitionBody {
    target_node_id: u64,
}

/// Operator-triggered CL-6 partition handover. Leader-only: followers reply
/// with `409 Conflict` and an `X-Leader-Id` header so the client can retry
/// against the actual leader. Measures Row 5 cutover latency in the Gate 2
/// plan (`docs/GATE2-ACCEPTANCE-PLAN.md §11.5`).
async fn transfer_partition(
    State(state): State<Arc<AppState>>,
    Path(partition): Path<u16>,
    #[cfg_attr(not(feature = "cluster"), allow(unused))] uri: axum::http::Uri,
    Json(body): Json<TransferPartitionBody>,
) -> axum::response::Response {
    #[cfg(feature = "cluster")]
    {
        let Some(ref node) = state.cluster_node else {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "cluster mode not enabled on this node",
            )
            .into_response();
        };
        if let Some(resp) = maybe_forward_to_leader(&state, &uri) {
            return resp;
        }

        let pid = aeon_types::PartitionId::new(partition);
        let status = match node
            .propose_partition_transfer(pid, body.target_node_id)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                    .into_response();
            }
        };

        use aeon_cluster::TransferStatus;
        match status {
            TransferStatus::Accepted { source, target } => (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "status": "accepted",
                    "partition": partition,
                    "source": source,
                    "target": target,
                })),
            )
                .into_response(),
            TransferStatus::NotLeader { current_leader } => {
                let mut resp = (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "not the Raft leader",
                        "current_leader": current_leader,
                    })),
                )
                    .into_response();
                if let Some(leader_id) = current_leader {
                    if let Ok(v) = HeaderValue::from_str(&leader_id.to_string()) {
                        resp.headers_mut().insert("X-Leader-Id", v);
                    }
                }
                resp
            }
            TransferStatus::UnknownPartition => api_error(
                StatusCode::NOT_FOUND,
                format!("partition {partition} not found in partition table"),
            )
            .into_response(),
            TransferStatus::AlreadyTransferring { source, target } => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "partition already has a transfer in flight",
                    "partition": partition,
                    "source": source,
                    "target": target,
                })),
            )
                .into_response(),
            TransferStatus::NoChange { owner } => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "no-change",
                    "partition": partition,
                    "owner": owner,
                })),
            )
                .into_response(),
            TransferStatus::Rejected(msg) => api_error(StatusCode::CONFLICT, msg).into_response(),
        }
    }
    #[cfg(not(feature = "cluster"))]
    {
        let _ = (state, partition, body, uri);
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "cluster feature not enabled in this build",
        )
        .into_response()
    }
}

// ── Drain / rebalance endpoints (P1.1g / G5) ─────────────────────────

/// Body for `POST /api/v1/cluster/drain`.
///
/// `node_id` is the node operators want to take out of rotation. The leader
/// reassigns every partition currently owned by that node to other live
/// members (round-robin in ascending NodeId order). Used by T3 (scale-down)
/// runs and any operator-driven evacuation.
#[derive(Deserialize)]
#[cfg_attr(not(feature = "cluster"), allow(dead_code))]
struct DrainBody {
    node_id: u64,
}

/// Operator-triggered bulk evacuation: drive every partition off `node_id`.
/// Leader-only — followers reply 409 with `X-Leader-Id` so the CLI can
/// retry against the real leader (mirrors `transfer_partition`).
async fn cluster_drain(
    State(state): State<Arc<AppState>>,
    #[cfg_attr(not(feature = "cluster"), allow(unused))] uri: axum::http::Uri,
    Json(body): Json<DrainBody>,
) -> axum::response::Response {
    #[cfg(feature = "cluster")]
    {
        if let Some(resp) = maybe_forward_to_leader(&state, &uri) {
            return resp;
        }
        cluster_bulk_transfer(&state, BulkPlan::Drain(body.node_id)).await
    }
    #[cfg(not(feature = "cluster"))]
    {
        let _ = (state, body, uri);
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "cluster feature not enabled in this build",
        )
        .into_response()
    }
}

/// Operator-triggered rebalance: redistribute partitions evenly across all
/// live cluster members. Used after T2 (scale-up) to push load onto the
/// freshly-joined nodes.
async fn cluster_rebalance(
    State(state): State<Arc<AppState>>,
    #[cfg_attr(not(feature = "cluster"), allow(unused))] uri: axum::http::Uri,
) -> axum::response::Response {
    #[cfg(feature = "cluster")]
    {
        if let Some(resp) = maybe_forward_to_leader(&state, &uri) {
            return resp;
        }
        cluster_bulk_transfer(&state, BulkPlan::Rebalance).await
    }
    #[cfg(not(feature = "cluster"))]
    {
        let _ = (state, uri);
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "cluster feature not enabled in this build",
        )
        .into_response()
    }
}

/// Graceful self-removal: the node hosting this endpoint asks the current
/// Raft leader to remove it from the voter set. Called from the K8s preStop
/// hook (see `helm/aeon/templates/statefulset.yaml`) so scale-down doesn't
/// leave a stale voter behind and partition ownership rebalances away before
/// the pod terminates.
///
/// Unlike drain/rebalance this endpoint does **not** forward to the leader —
/// the departing node must run the RPC from its own identity (the leader
/// derives the target `node_id` from the RPC source, not from the body).
/// The leader cannot remove itself, so if this node is the current leader
/// the endpoint returns 409 and the operator must transfer leadership first.
async fn cluster_leave(
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    #[cfg(feature = "cluster")]
    {
        let Some(ref node) = state.cluster_node else {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "cluster mode not enabled on this node",
            )
            .into_response();
        };

        let self_id = node.config().node_id;

        if node.is_leader().await {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "this node is the current Raft leader; transfer leadership before leaving",
                    "node_id": self_id,
                })),
            )
                .into_response();
        }

        let metrics = node.raft().metrics().borrow().clone();
        let Some(leader_id) = metrics.current_leader else {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no current Raft leader; cannot propose self-removal",
            )
            .into_response();
        };
        let Some(leader_addr) = metrics
            .membership_config
            .membership()
            .get_node(&leader_id)
            .cloned()
        else {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("leader {leader_id} has no address in Raft membership"),
            )
            .into_response();
        };
        let Some(endpoint) = node.endpoint() else {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "node has no QUIC endpoint (single-node bootstrap?)",
            )
            .into_response();
        };

        let req = aeon_cluster::types::RemoveNodeRequest { node_id: self_id };
        match aeon_cluster::transport::network::send_remove_request(
            endpoint,
            leader_id,
            &leader_addr,
            &req,
        )
        .await
        {
            Ok(resp) if resp.success => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "left",
                    "node_id": self_id,
                    "leader_id": resp.leader_id,
                    "message": resp.message,
                })),
            )
                .into_response(),
            Ok(resp) => api_error(
                StatusCode::CONFLICT,
                format!("leader rejected remove: {}", resp.message),
            )
            .into_response(),
            Err(e) => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("remove RPC to leader {leader_id} failed: {e}"),
            )
            .into_response(),
        }
    }
    #[cfg(not(feature = "cluster"))]
    {
        let _ = state;
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "cluster feature not enabled in this build",
        )
        .into_response()
    }
}

#[cfg(feature = "cluster")]
enum BulkPlan {
    Drain(u64),
    Rebalance,
}

/// Shared backbone for `cluster_drain` and `cluster_rebalance`. Builds the
/// transfer plan via `aeon_cluster::plan_drain` / `plan_rebalance`, then
/// loops the plan through `propose_partition_transfer`. Each per-partition
/// outcome is reported in the response so operators can see exactly what
/// landed and what fought an in-flight transfer.
#[cfg(feature = "cluster")]
async fn cluster_bulk_transfer(
    state: &Arc<AppState>,
    plan: BulkPlan,
) -> axum::response::Response {
    let Some(ref node) = state.cluster_node else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "cluster mode not enabled on this node",
        )
        .into_response();
    };

    // Leader-only — fail fast with the same `X-Leader-Id` hint that
    // `transfer_partition` returns so the CLI can re-route.
    if !node.is_leader().await {
        let current_leader = node.current_leader().await;
        let mut resp = (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "not the Raft leader",
                "current_leader": current_leader,
            })),
        )
            .into_response();
        if let Some(leader_id) = current_leader {
            if let Ok(v) = HeaderValue::from_str(&leader_id.to_string()) {
                resp.headers_mut().insert("X-Leader-Id", v);
            }
        }
        return resp;
    }

    let table = node.partition_table_snapshot().await;
    let members: Vec<u64> = match node.members().await {
        Ok(m) => m.keys().copied().collect(),
        Err(e) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read cluster membership: {e}"),
            )
            .into_response();
        }
    };

    let (op_label, transfers) = match plan {
        BulkPlan::Drain(target) => (
            "drain",
            aeon_cluster::plan_drain(&table, target, &members),
        ),
        BulkPlan::Rebalance => ("rebalance", aeon_cluster::plan_rebalance(&table, &members)),
    };

    if transfers.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "noop",
                "operation": op_label,
                "planned": 0,
                "results": [],
            })),
        )
            .into_response();
    }

    use aeon_cluster::TransferStatus;
    let mut results = Vec::with_capacity(transfers.len());
    let mut accepted: usize = 0;
    let mut rejected: usize = 0;
    let mut noop: usize = 0;

    for (pid, target) in transfers.iter().copied() {
        let status = match node.propose_partition_transfer(pid, target).await {
            Ok(s) => s,
            Err(e) => {
                rejected += 1;
                results.push(serde_json::json!({
                    "partition": pid.as_u16(),
                    "target": target,
                    "status": "error",
                    "message": e.to_string(),
                }));
                continue;
            }
        };
        let outcome = match status {
            TransferStatus::Accepted { source, target: t } => {
                accepted += 1;
                serde_json::json!({
                    "partition": pid.as_u16(),
                    "source": source,
                    "target": t,
                    "status": "accepted",
                })
            }
            TransferStatus::NoChange { owner } => {
                noop += 1;
                serde_json::json!({
                    "partition": pid.as_u16(),
                    "owner": owner,
                    "status": "no-change",
                })
            }
            TransferStatus::AlreadyTransferring { source, target: t } => {
                noop += 1;
                serde_json::json!({
                    "partition": pid.as_u16(),
                    "source": source,
                    "target": t,
                    "status": "already-transferring",
                })
            }
            TransferStatus::UnknownPartition => {
                rejected += 1;
                serde_json::json!({
                    "partition": pid.as_u16(),
                    "status": "unknown-partition",
                })
            }
            TransferStatus::Rejected(msg) => {
                rejected += 1;
                serde_json::json!({
                    "partition": pid.as_u16(),
                    "target": target,
                    "status": "rejected",
                    "message": msg,
                })
            }
            TransferStatus::NotLeader { current_leader } => {
                // Lost leadership mid-loop — surface a partial outcome and stop.
                rejected += 1;
                results.push(serde_json::json!({
                    "partition": pid.as_u16(),
                    "target": target,
                    "status": "lost-leadership",
                    "current_leader": current_leader,
                }));
                break;
            }
        };
        results.push(outcome);
    }

    let http_status = if rejected == 0 {
        StatusCode::ACCEPTED
    } else {
        StatusCode::MULTI_STATUS
    };
    (
        http_status,
        Json(serde_json::json!({
            "status": if rejected == 0 { "accepted" } else { "partial" },
            "operation": op_label,
            "planned": transfers.len(),
            "accepted": accepted,
            "noop": noop,
            "rejected": rejected,
            "results": results,
        })),
    )
        .into_response()
}

// ── Processor endpoints ────────────────────────────────────────────────

#[derive(Serialize)]
struct ProcessorListItem {
    name: String,
    version_count: usize,
    latest_version: Option<String>,
}

async fn list_processors(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let names = state.registry.list().await;
    let mut items = Vec::with_capacity(names.len());

    for name in names {
        if let Some(record) = state.registry.get(&name).await {
            let latest = record.latest_version().map(|v| v.version.clone());
            items.push(ProcessorListItem {
                name: record.name,
                version_count: record.versions.len(),
                latest_version: latest,
            });
        }
    }

    Json(items)
}

#[derive(Deserialize)]
struct RegisterProcessorRequest {
    name: String,
    #[serde(default)]
    description: String,
    version: aeon_types::registry::ProcessorVersion,
}

async fn register_processor(
    State(state): State<Arc<AppState>>,
    #[cfg_attr(not(feature = "cluster"), allow(unused))] uri: axum::http::Uri,
    Json(req): Json<RegisterProcessorRequest>,
) -> impl IntoResponse {
    let cmd = aeon_types::registry::RegistryCommand::RegisterProcessor {
        name: req.name,
        description: req.description,
        version: req.version,
    };

    // Cluster mode: replicate the register via Raft so every node's local
    // registry observes the new processor version.
    #[cfg(feature = "cluster")]
    if let Some(node) = state.cluster_node.as_ref() {
        if let Some(resp) = maybe_forward_to_leader(&state, &uri) {
            return resp;
        }
        return match node.propose_registry(cmd).await {
            Ok(aeon_types::RegistryResponse::ProcessorRegistered { name, version }) => (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "status": "registered",
                    "name": name,
                    "version": version,
                    "replicated": true,
                })),
            )
                .into_response(),
            Ok(aeon_types::RegistryResponse::Error { message }) => {
                api_error(StatusCode::BAD_REQUEST, message).into_response()
            }
            Ok(other) => (
                StatusCode::OK,
                Json(serde_json::to_value(&other).unwrap_or_default()),
            )
                .into_response(),
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    let resp = state.registry.apply(cmd).await;
    match resp {
        aeon_types::registry::RegistryResponse::ProcessorRegistered { name, version } => (
            StatusCode::CREATED,
            Json(serde_json::json!({"status": "registered", "name": name, "version": version})),
        )
            .into_response(),
        aeon_types::registry::RegistryResponse::Error { message } => {
            api_error(StatusCode::BAD_REQUEST, message).into_response()
        }
        other => (
            StatusCode::OK,
            Json(serde_json::to_value(&other).unwrap_or_default()),
        )
            .into_response(),
    }
}

async fn get_processor(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.registry.get(&name).await {
        Some(record) => {
            let json = serde_json::to_value(&record).unwrap_or_default();
            (StatusCode::OK, Json(json)).into_response()
        }
        None => api_error(
            StatusCode::NOT_FOUND,
            format!("processor '{name}' not found"),
        )
        .into_response(),
    }
}

async fn list_processor_versions(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.registry.versions(&name).await {
        Some(versions) => {
            let json = serde_json::to_value(&versions).unwrap_or_default();
            (StatusCode::OK, Json(json)).into_response()
        }
        None => api_error(
            StatusCode::NOT_FOUND,
            format!("processor '{name}' not found"),
        )
        .into_response(),
    }
}

async fn delete_processor_version(
    State(state): State<Arc<AppState>>,
    Path((name, version)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.registry.delete_version(&name, &version).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "deleted"})),
        )
            .into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── Pipeline endpoints ─────────────────────────────────────────────────

async fn list_pipelines(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let pipelines = state.pipelines.list_with_state().await;
    let items: Vec<serde_json::Value> = pipelines
        .into_iter()
        .map(|(name, pipeline_state)| {
            serde_json::json!({
                "name": name,
                "state": pipeline_state.to_string(),
            })
        })
        .collect();
    Json(items)
}

#[derive(Deserialize)]
struct CreatePipelineRequest {
    #[serde(flatten)]
    definition: aeon_types::registry::PipelineDefinition,
}

async fn create_pipeline(
    State(state): State<Arc<AppState>>,
    #[cfg_attr(not(feature = "cluster"), allow(unused))] uri: axum::http::Uri,
    Json(req): Json<CreatePipelineRequest>,
) -> impl IntoResponse {
    if let Err(e) = validate_resource_name(&req.definition.name, "pipeline") {
        return e.into_response();
    }

    // In cluster mode, replicate via Raft so every node observes the new
    // pipeline definition. Local apply happens on each node via the
    // registered `ClusterRegistryApplier` after the log entry commits.
    #[cfg(feature = "cluster")]
    if let Some(node) = state.cluster_node.as_ref() {
        if let Some(resp) = maybe_forward_to_leader(&state, &uri) {
            return resp;
        }
        let cmd = aeon_types::RegistryCommand::CreatePipeline {
            definition: Box::new(req.definition),
        };
        return match node.propose_registry(cmd).await {
            Ok(aeon_types::RegistryResponse::Error { message }) => {
                api_error(StatusCode::BAD_REQUEST, message).into_response()
            }
            Ok(_) => (
                StatusCode::CREATED,
                Json(serde_json::json!({"status": "created", "replicated": true})),
            )
                .into_response(),
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // Standalone mode — apply directly to the local PipelineManager.
    match state.pipelines.create(req.definition).await {
        Ok(_) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"status": "created"})),
        )
            .into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn get_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.pipelines.get(&name).await {
        Some(pipeline) => {
            let json = serde_json::to_value(&pipeline).unwrap_or_default();
            (StatusCode::OK, Json(json)).into_response()
        }
        None => api_error(
            StatusCode::NOT_FOUND,
            format!("pipeline '{name}' not found"),
        )
        .into_response(),
    }
}

async fn start_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    #[cfg_attr(not(feature = "cluster"), allow(unused))] uri: axum::http::Uri,
) -> impl IntoResponse {
    // Cluster path: go through Raft. The supervisor side-effect runs in
    // `ClusterRegistryApplier::apply` after the commit lands on every node,
    // so there's nothing for this handler to do beyond proposing.
    #[cfg(feature = "cluster")]
    if let Some(node) = state.cluster_node.as_ref() {
        if let Some(resp) = maybe_forward_to_leader(&state, &uri) {
            return resp;
        }
        let cmd = aeon_types::RegistryCommand::SetPipelineState {
            name: name.clone(),
            state: aeon_types::registry::PipelineState::Running,
        };
        return match node.propose_registry(cmd).await {
            Ok(aeon_types::RegistryResponse::Error { message }) => {
                api_error(StatusCode::BAD_REQUEST, message).into_response()
            }
            Ok(_) => Json(serde_json::json!({"status": "started", "replicated": true}))
                .into_response(),
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // Standalone path: flip the declarative state, then ask the supervisor
    // to spin up the real runtime task. Install the returned control /
    // metrics handles into AppState so the existing upgrade / metrics
    // endpoints can find them by pipeline name.
    if let Err(e) = state.pipelines.start(&name, "api").await {
        return api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    let def = match state.pipelines.get(&name).await {
        Some(d) => d,
        None => {
            return api_error(
                StatusCode::NOT_FOUND,
                format!("pipeline '{name}' not found after start"),
            )
            .into_response();
        }
    };
    match state.supervisor.start(&def).await {
        Ok((control, metrics)) => {
            state.pipeline_controls.insert(name.clone(), control);
            state.pipeline_metrics.insert(name.clone(), metrics);
            Json(serde_json::json!({"status": "started"})).into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn stop_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    #[cfg_attr(not(feature = "cluster"), allow(unused))] uri: axum::http::Uri,
) -> impl IntoResponse {
    #[cfg(feature = "cluster")]
    if let Some(node) = state.cluster_node.as_ref() {
        if let Some(resp) = maybe_forward_to_leader(&state, &uri) {
            return resp;
        }
        let cmd = aeon_types::RegistryCommand::SetPipelineState {
            name: name.clone(),
            state: aeon_types::registry::PipelineState::Stopped,
        };
        return match node.propose_registry(cmd).await {
            Ok(aeon_types::RegistryResponse::Error { message }) => {
                api_error(StatusCode::BAD_REQUEST, message).into_response()
            }
            Ok(_) => Json(serde_json::json!({"status": "stopped", "replicated": true}))
                .into_response(),
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    if let Err(e) = state.pipelines.stop(&name, "api").await {
        return api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    if let Err(e) = state.supervisor.stop(&name).await {
        tracing::warn!(pipeline = %name, error = %e, "supervisor stop returned error");
    }
    state.pipeline_controls.remove(&name);
    state.pipeline_metrics.remove(&name);
    Json(serde_json::json!({"status": "stopped"})).into_response()
}

#[derive(Deserialize)]
struct UpgradeRequest {
    processor_name: String,
    processor_version: String,
}

/// Load a processor artifact from the registry and instantiate it.
///
/// Returns the boxed processor on success, or an HTTP error response on failure.
/// Supports Wasm and NativeSo types; T3/T4 (WebTransport/WebSocket) processors
/// are managed externally and cannot be instantiated here.
async fn instantiate_processor(
    state: &AppState,
    processor_name: &str,
    processor_version: &str,
) -> Result<Box<dyn aeon_types::Processor + Send + Sync>, axum::response::Response> {
    // Load artifact from registry
    let artifact_bytes = state
        .registry
        .load_artifact(processor_name, processor_version)
        .await
        .map_err(|e| {
            api_error(
                StatusCode::BAD_REQUEST,
                format!("failed to load artifact: {e}"),
            )
            .into_response()
        })?;

    // Determine processor type from registry metadata
    let proc_type = match state.registry.get(processor_name).await {
        Some(record) => match record.get_version(processor_version) {
            Some(ver) => ver.processor_type,
            None => {
                return Err(api_error(
                    StatusCode::NOT_FOUND,
                    format!("version {processor_name}:{processor_version} not found"),
                )
                .into_response());
            }
        },
        None => {
            return Err(api_error(
                StatusCode::NOT_FOUND,
                format!("processor '{processor_name}' not found"),
            )
            .into_response());
        }
    };

    // Instantiate based on type
    match proc_type {
        aeon_types::registry::ProcessorType::Wasm => {
            let module = aeon_wasm::WasmModule::from_bytes(
                &artifact_bytes,
                aeon_wasm::WasmConfig::default(),
            )
            .map_err(|e| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    format!("failed to compile Wasm module: {e}"),
                )
                .into_response()
            })?;
            let p = aeon_wasm::WasmProcessor::new(std::sync::Arc::new(module)).map_err(|e| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    format!("failed to create Wasm processor: {e}"),
                )
                .into_response()
            })?;
            Ok(Box::new(p))
        }
        #[cfg(feature = "native-loader")]
        aeon_types::registry::ProcessorType::NativeSo => {
            let artifact_path =
                state
                    .registry
                    .artifact_path_for(processor_name, processor_version);
            let p = crate::native_loader::NativeProcessor::load(&artifact_path, &[]).map_err(
                |e| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        format!("failed to load native processor: {e}"),
                    )
                    .into_response()
                },
            )?;
            Ok(Box::new(p))
        }
        other => Err(api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "hot-swap not supported for processor type '{other}' — \
                 T3/T4 processors are managed externally"
            ),
        )
        .into_response()),
    }
}

async fn upgrade_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    #[cfg_attr(not(feature = "cluster"), allow(unused))] uri: axum::http::Uri,
    Json(req): Json<UpgradeRequest>,
) -> impl IntoResponse {
    let proc_ref = aeon_types::registry::ProcessorRef::new(
        req.processor_name.clone(),
        req.processor_version.clone(),
    );

    // Cluster mode: replicate the metadata upgrade via Raft so every node's
    // local PipelineManager reflects the new processor ref. The actual hot-swap
    // below (drain→swap→resume) still happens per-node — only the node(s) that
    // own the pipeline's PipelineControl handle can swap the running processor.
    #[cfg(feature = "cluster")]
    if let Some(node) = state.cluster_node.as_ref() {
        if let Some(resp) = maybe_forward_to_leader(&state, &uri) {
            return resp;
        }
        let cmd = aeon_types::RegistryCommand::UpgradePipeline {
            name: name.clone(),
            new_processor: proc_ref.clone(),
        };
        if let Err(e) = node.propose_registry(cmd).await {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }

    // If a PipelineControl handle exists, perform a real drain→swap→resume.
    // Otherwise, fall back to metadata-only upgrade.
    if let Some(control) = state.pipeline_controls.get(&name) {
        let new_processor =
            match instantiate_processor(&state, &req.processor_name, &req.processor_version).await
            {
                Ok(p) => p,
                Err(resp) => return resp,
            };

        // Drain→swap→resume
        if let Err(e) = control.drain_and_swap(new_processor).await {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("drain-and-swap failed: {e}"),
            )
            .into_response();
        }

        // Update metadata
        if let Err(e) = state.pipelines.upgrade(&name, proc_ref, "api").await {
            tracing::warn!(pipeline = %name, "processor swapped but metadata update failed: {e}");
        }

        Json(serde_json::json!({"status": "upgraded", "method": "hot-swap"})).into_response()
    } else {
        // No PipelineControl — metadata-only upgrade
        match state.pipelines.upgrade(&name, proc_ref, "api").await {
            Ok(()) => {
                Json(serde_json::json!({"status": "upgraded", "method": "metadata"}))
                    .into_response()
            }
            Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    }
}

// ── Blue-Green + Canary endpoints ──────────────────────────────────────

async fn upgrade_blue_green(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<UpgradeRequest>,
) -> impl IntoResponse {
    let proc_ref = aeon_types::registry::ProcessorRef::new(
        req.processor_name.clone(),
        req.processor_version.clone(),
    );
    if let Err(e) = state
        .pipelines
        .upgrade_blue_green(&name, proc_ref, "api")
        .await
    {
        return api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    // If a managed pipeline control exists, instantiate the green processor
    // and start real blue-green shadow mode.
    if let Some(ctrl) = state.pipeline_controls.get(&name) {
        let green =
            match instantiate_processor(&state, &req.processor_name, &req.processor_version).await
            {
                Ok(p) => p,
                Err(resp) => return resp,
            };
        if let Err(e) = ctrl.start_blue_green(green).await {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to start blue-green: {e}"),
            )
            .into_response();
        }
        Json(serde_json::json!({"status": "blue-green-started", "method": "managed"}))
            .into_response()
    } else {
        Json(serde_json::json!({"status": "blue-green-started", "method": "metadata"}))
            .into_response()
    }
}

#[derive(Deserialize)]
struct CanaryUpgradeRequest {
    processor_name: String,
    processor_version: String,
    #[serde(default = "default_canary_steps")]
    steps: Vec<u8>,
    #[serde(default)]
    thresholds: aeon_types::registry::CanaryThresholds,
}

fn default_canary_steps() -> Vec<u8> {
    vec![10, 50, 100]
}

async fn upgrade_canary(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<CanaryUpgradeRequest>,
) -> impl IntoResponse {
    let proc_ref = aeon_types::registry::ProcessorRef::new(
        req.processor_name.clone(),
        req.processor_version.clone(),
    );
    let initial_pct = req.steps.first().copied().unwrap_or(10);
    if let Err(e) = state
        .pipelines
        .upgrade_canary(&name, proc_ref, req.steps, req.thresholds, "api")
        .await
    {
        return api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    // If a managed pipeline control exists, instantiate the canary processor
    // and start real traffic splitting.
    if let Some(ctrl) = state.pipeline_controls.get(&name) {
        let canary =
            match instantiate_processor(&state, &req.processor_name, &req.processor_version).await
            {
                Ok(p) => p,
                Err(resp) => return resp,
            };
        if let Err(e) = ctrl.start_canary(canary, initial_pct).await {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to start canary: {e}"),
            )
            .into_response();
        }
        Json(serde_json::json!({"status": "canary-started", "method": "managed"})).into_response()
    } else {
        Json(serde_json::json!({"status": "canary-started", "method": "metadata"})).into_response()
    }
}

async fn cutover_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.pipelines.cutover(&name, "api").await {
        Ok(()) => {
            // If a managed pipeline control handle exists, trigger the actual
            // processor cutover (green → active). This is a no-op if blue-green
            // wasn't started via PipelineControl (metadata-only mode).
            let method = if let Some(ctrl) = state.pipeline_controls.get(&name) {
                let _ = ctrl.cutover_blue_green().await;
                "managed"
            } else {
                "metadata"
            };
            Json(serde_json::json!({"status": "cutover-complete", "method": method}))
                .into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn rollback_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.pipelines.rollback_upgrade(&name, "api").await {
        Ok(()) => {
            let method = if let Some(ctrl) = state.pipeline_controls.get(&name) {
                let _ = ctrl.rollback_upgrade().await;
                "managed"
            } else {
                "metadata"
            };
            Json(serde_json::json!({"status": "rolled-back", "method": method}))
                .into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn promote_canary(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.pipelines.promote_canary(&name, "api").await {
        Ok(()) => {
            // If managed, check whether the canary is now at 100% and complete it,
            // or just advance the canary percentage to the next step.
            let method = if let Some(ctrl) = state.pipeline_controls.get(&name) {
                // Check if canary is complete (promoted to 100%)
                if let Some(cs) = state.pipelines.canary_status(&name).await {
                    if cs.traffic_pct >= 100 {
                        let _ = ctrl.complete_canary().await;
                    } else {
                        let _ = ctrl.set_canary_pct(cs.traffic_pct).await;
                    }
                }
                "managed"
            } else {
                "metadata"
            };
            Json(serde_json::json!({"status": "promoted", "method": method}))
                .into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn canary_status(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.pipelines.canary_status(&name).await {
        Some(cs) => {
            let json = serde_json::to_value(&cs).unwrap_or_default();
            (StatusCode::OK, Json(json)).into_response()
        }
        None => api_error(
            StatusCode::NOT_FOUND,
            format!("no canary upgrade in progress for '{name}'"),
        )
        .into_response(),
    }
}

async fn pipeline_history(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let history = state.pipelines.history(&name).await;
    let json = serde_json::to_value(&history).unwrap_or_default();
    Json(json)
}

// ── Source/Sink Reconfiguration endpoints ──────────────────────────────

async fn reconfigure_source(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(new_source): Json<aeon_types::registry::SourceConfig>,
) -> impl IntoResponse {
    match state
        .pipelines
        .reconfigure_source(&name, new_source, "api")
        .await
    {
        Ok(()) => {
            Json(serde_json::json!({"status": "source-reconfigured"})).into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn reconfigure_sink(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(new_sink): Json<aeon_types::registry::SinkConfig>,
) -> impl IntoResponse {
    match state
        .pipelines
        .reconfigure_sink(&name, new_sink, "api")
        .await
    {
        Ok(()) => {
            Json(serde_json::json!({"status": "sink-reconfigured"})).into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn delete_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    #[cfg_attr(not(feature = "cluster"), allow(unused))] uri: axum::http::Uri,
) -> impl IntoResponse {
    #[cfg(feature = "cluster")]
    if let Some(node) = state.cluster_node.as_ref() {
        if let Some(resp) = maybe_forward_to_leader(&state, &uri) {
            return resp;
        }
        let cmd = aeon_types::RegistryCommand::DeletePipeline { name: name.clone() };
        return match node.propose_registry(cmd).await {
            Ok(aeon_types::RegistryResponse::Error { message }) => {
                api_error(StatusCode::BAD_REQUEST, message).into_response()
            }
            Ok(_) => Json(serde_json::json!({"status": "deleted", "replicated": true}))
                .into_response(),
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    match state.pipelines.delete(&name).await {
        Ok(()) => Json(serde_json::json!({"status": "deleted"})).into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── Delivery endpoints ────────────────────────────────────────────────

/// GET /api/v1/pipelines/:name/delivery — delivery status for a pipeline.
async fn delivery_status(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.delivery_ledgers.get(&name) {
        Some(ledger) => {
            let failed = ledger.failed_entries();
            let failed_json: Vec<serde_json::Value> = failed
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "event_id": f.event_id.to_string(),
                        "partition": f.partition.as_u16(),
                        "source_offset": f.source_offset,
                        "reason": f.reason,
                        "attempts": f.attempts,
                    })
                })
                .collect();

            let json = serde_json::json!({
                "pipeline": name,
                "pending_count": ledger.pending_count(),
                "failed_count": ledger.failed_count(),
                "total_tracked": ledger.total_tracked(),
                "total_acked": ledger.total_acked(),
                "total_failed": ledger.total_failed(),
                "oldest_pending_age_ms": ledger.oldest_pending_age()
                    .map(|d| d.as_millis() as u64),
                "failed_entries": failed_json,
            });
            (StatusCode::OK, Json(json)).into_response()
        }
        None => api_error(
            StatusCode::NOT_FOUND,
            format!("no delivery ledger for pipeline '{name}'"),
        )
        .into_response(),
    }
}

/// POST /api/v1/pipelines/:name/delivery/retry — re-enqueue specific failed events.
async fn delivery_retry(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<DeliveryRetryRequest>,
) -> impl IntoResponse {
    match state.delivery_ledgers.get(&name) {
        Some(ledger) => {
            let mut retried = 0u64;
            let mut not_found = 0u64;

            for id_str in &req.event_ids {
                match uuid::Uuid::parse_str(id_str) {
                    Ok(id) => {
                        // Reset failed entry back to pending state by removing and re-tracking.
                        // The actual re-enqueue to the sink happens at the pipeline level;
                        // this endpoint marks events as eligible for retry.
                        if ledger.remove(&id) {
                            retried += 1;
                        } else {
                            not_found += 1;
                        }
                    }
                    Err(_) => {
                        not_found += 1;
                    }
                }
            }

            let json = serde_json::json!({
                "retried": retried,
                "not_found": not_found,
            });
            (StatusCode::OK, Json(json)).into_response()
        }
        None => api_error(
            StatusCode::NOT_FOUND,
            format!("no delivery ledger for pipeline '{name}'"),
        )
        .into_response(),
    }
}

#[derive(Deserialize)]
struct DeliveryRetryRequest {
    event_ids: Vec<String>,
}

// ── Integrity verification endpoint ──────────────────────────────────

/// GET /api/v1/pipelines/:name/verify — PoH/Merkle chain integrity status.
///
/// Returns the current PoH chain state for the pipeline, including per-partition
/// chain heads, sequence numbers, and verification results.
///
/// When PoH tracking is not yet active for a pipeline, returns a status response
/// indicating the pipeline exists but PoH is not wired (Phase 14 runtime).
async fn verify_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Check pipeline exists
    match state.pipelines.get(&name).await {
        Some(_pipeline) => {
            // Check if this pipeline has an active PoH chain
            #[cfg(feature = "processor-auth")]
            {
                if let Some(poh_ref) = state.poh_chains.get(&name) {
                    let chain = poh_ref.value().lock().await;
                    let chain_state = chain.export_state();
                    let verification = chain.verify_recent();
                    let json = serde_json::json!({
                        "pipeline": name,
                        "poh_active": true,
                        "status": "ok",
                        "chain": {
                            "partition": chain_state.partition.as_u16(),
                            "sequence": chain_state.sequence,
                            "current_hash": hex::encode(chain_state.current_hash),
                            "mmr_root": hex::encode(chain.mmr_root()),
                            "recent_entries": chain.recent_entries().len(),
                            "verification": if verification.is_none() { "valid" } else { "invalid" },
                            "invalid_at_index": verification,
                        },
                        "modules": {
                            "poh": "active",
                            "merkle": "active",
                            "mmr": "active",
                            "signing": if chain.recent_entries().iter().any(|e| e.signed_root.is_some()) { "active" } else { "available" },
                        },
                    });
                    return (StatusCode::OK, Json(json)).into_response();
                }
            }

            // PoH not active for this pipeline
            let json = serde_json::json!({
                "pipeline": name,
                "poh_active": false,
                "status": "ok",
                "message": "Pipeline exists. PoH chain tracking activates when the pipeline runs with poh config enabled.",
                "modules": {
                    "poh": "available",
                    "merkle": "available",
                    "mmr": "available",
                    "signing": "available",
                },
                "chain_heads": [],
                "aggregate_hash": null,
            });
            (StatusCode::OK, Json(json)).into_response()
        }
        None => {
            if name == "all" {
                // "all" target: report system-wide verification status
                let pipelines = state.pipelines.list_with_state().await;
                let pipeline_statuses: Vec<serde_json::Value> = pipelines
                    .into_iter()
                    .map(|(pname, pstate)| {
                        #[cfg(feature = "processor-auth")]
                        let poh_active = state.poh_chains.contains_key(&pname);
                        #[cfg(not(feature = "processor-auth"))]
                        let poh_active = false;
                        serde_json::json!({
                            "name": pname,
                            "state": pstate.to_string(),
                            "poh_active": poh_active,
                        })
                    })
                    .collect();

                let json = serde_json::json!({
                    "status": "ok",
                    "pipelines": pipeline_statuses,
                    "modules": {
                        "poh": "available",
                        "merkle": "available",
                        "mmr": "available",
                        "signing": "available",
                    },
                });
                (StatusCode::OK, Json(json)).into_response()
            } else {
                api_error(
                    StatusCode::NOT_FOUND,
                    format!("pipeline '{name}' not found"),
                )
                .into_response()
            }
        }
    }
}

// ── Identity management endpoints ─────────────────────────────────────

#[derive(Deserialize)]
struct RegisterIdentityRequest {
    /// ED25519 public key ("ed25519:<base64>").
    public_key: String,
    /// Pipeline scope.
    #[serde(default)]
    allowed_pipelines: aeon_types::processor_identity::PipelineScope,
    /// Maximum concurrent connections.
    #[serde(default = "default_max_instances")]
    max_instances: u32,
}

fn default_max_instances() -> u32 {
    1
}

async fn register_identity(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<RegisterIdentityRequest>,
) -> impl IntoResponse {
    // Compute fingerprint from public key
    #[cfg(feature = "processor-auth")]
    let fingerprint = match crate::processor_auth::compute_fingerprint(&req.public_key) {
        Ok(fp) => fp,
        Err(e) => {
            return api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
    };

    #[cfg(not(feature = "processor-auth"))]
    let fingerprint = {
        // Fallback: use public_key as-is for fingerprint when auth feature disabled
        format!("SHA256:{}", &req.public_key)
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let identity = aeon_types::processor_identity::ProcessorIdentity {
        public_key: req.public_key,
        fingerprint: fingerprint.clone(),
        processor_name: name,
        allowed_pipelines: req.allowed_pipelines,
        max_instances: req.max_instances,
        registered_at: now,
        registered_by: "api".into(),
        revoked_at: None,
    };

    match state.identities.register(identity) {
        Ok(fp) => {
            let json = serde_json::json!({
                "fingerprint": fp,
                "status": "active",
            });
            (StatusCode::CREATED, Json(json)).into_response()
        }
        Err(e) => api_error(StatusCode::CONFLICT, e.to_string()).into_response(),
    }
}

async fn list_identities(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let identities = state.identities.list_for_processor(&name);
    let json = serde_json::json!({
        "processor": name,
        "identities": identities.iter().map(|id| {
            serde_json::json!({
                "fingerprint": id.fingerprint,
                "public_key": id.public_key,
                "allowed_pipelines": id.allowed_pipelines,
                "max_instances": id.max_instances,
                "registered_at": id.registered_at,
                "registered_by": id.registered_by,
                "revoked_at": id.revoked_at,
                "active_connections": state.identities.active_connections(&id.fingerprint),
            })
        }).collect::<Vec<_>>(),
    });
    Json(json)
}

async fn revoke_identity(
    State(state): State<Arc<AppState>>,
    Path((name, fingerprint)): Path<(String, String)>,
) -> impl IntoResponse {
    // Verify the identity belongs to this processor
    match state.identities.get(&fingerprint) {
        Some(id) if id.processor_name == name => {}
        Some(_) => {
            return api_error(
                StatusCode::NOT_FOUND,
                format!("identity {fingerprint} does not belong to processor '{name}'"),
            )
            .into_response();
        }
        None => {
            return api_error(
                StatusCode::NOT_FOUND,
                format!("identity {fingerprint} not found"),
            )
            .into_response();
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    match state.identities.revoke(&fingerprint, now) {
        Some(_) => {
            let json = serde_json::json!({
                "fingerprint": fingerprint,
                "revoked_at": now,
            });
            Json(json).into_response()
        }
        None => api_error(
            StatusCode::CONFLICT,
            format!("identity {fingerprint} is already revoked"),
        )
        .into_response(),
    }
}

// ── WebSocket Processor Connect (T4) ────────────────────────────────────

#[cfg(feature = "websocket-host")]
async fn ws_processor_connect(
    State(state): State<Arc<AppState>>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> impl IntoResponse {
    let Some(ref ws_host) = state.ws_host else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "WebSocket processor host not configured",
        )
            .into_response();
    };
    let host = ws_host.clone();
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = host.handle_upgrade(socket).await {
            tracing::debug!(error = %e, "T4 WebSocket session ended");
        }
    })
    .into_response()
}

// ── WebSocket Pipeline Live-Tail ─────────────────────────────────────────

/// WebSocket endpoint that streams pipeline metrics at ~1 Hz.
///
/// Sends JSON frames with current event counts, error rates, and pipeline state.
/// Requires `websocket-host` feature (which enables `axum/ws`).
#[cfg(feature = "websocket-host")]
async fn pipeline_tail(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> impl IntoResponse {
    // Verify pipeline exists before upgrading
    if state.pipelines.get(&name).await.is_none() {
        return api_error(StatusCode::NOT_FOUND, format!("pipeline '{name}' not found"))
            .into_response();
    }

    let pipelines = state.pipelines.clone();
    let supervisor = Arc::clone(&state.supervisor);
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        use std::sync::atomic::Ordering;

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;

            // Build metrics snapshot
            let pipeline_state = pipelines.get(&name).await;
            let state_str = pipeline_state
                .as_ref()
                .map(|p| format!("{}", p.state))
                .unwrap_or_else(|| "unknown".into());

            let metrics_json = if let Some(m) = supervisor.get_metrics(&name).await {
                serde_json::json!({
                    "pipeline": name,
                    "state": state_str,
                    "events_received": m.events_received.load(Ordering::Relaxed),
                    "events_processed": m.events_processed.load(Ordering::Relaxed),
                    "outputs_sent": m.outputs_sent.load(Ordering::Relaxed),
                    "outputs_acked": m.outputs_acked.load(Ordering::Relaxed),
                    "checkpoints_written": m.checkpoints_written.load(Ordering::Relaxed),
                    "events_failed": m.events_failed.load(Ordering::Relaxed),
                    "events_retried": m.events_retried.load(Ordering::Relaxed),
                    "timestamp": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64,
                })
            } else {
                serde_json::json!({
                    "pipeline": name,
                    "state": state_str,
                    "message": "no metrics available (pipeline not running with metrics registration)",
                    "timestamp": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64,
                })
            };

            let msg = Message::Text(metrics_json.to_string().into());
            if socket.send(msg).await.is_err() {
                break; // Client disconnected
            }

            // Check for close frame from client
            match tokio::time::timeout(
                std::time::Duration::from_millis(10),
                socket.recv(),
            )
            .await
            {
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                _ => {} // No message or timeout — continue
            }
        }
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_registry::{
        ConnectorRegistry, DynSink, DynSource, SinkFactory, SourceFactory,
    };
    use aeon_types::registry::{PipelineDefinition, ProcessorRef, SinkConfig, SourceConfig};
    use aeon_types::{AeonError, BatchResult, Event, Output, Sink, Source};
    use axum::body::Body;
    use axum::http::Request;
    use std::collections::BTreeMap;
    use tower::ServiceExt;

    /// Stub source that immediately returns empty (the pipeline exits on its
    /// own). Lets `supervisor.start()` succeed in REST lifecycle tests that
    /// only care about state transitions, not actual data flow.
    struct StubSource;
    impl Source for StubSource {
        async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
            Ok(vec![])
        }
    }
    struct StubSourceFactory;
    impl SourceFactory for StubSourceFactory {
        fn build(&self, _cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
            Ok(Box::new(StubSource))
        }
    }

    struct StubSink;
    impl Sink for StubSink {
        async fn write_batch(
            &mut self,
            outputs: Vec<Output>,
        ) -> Result<BatchResult, AeonError> {
            Ok(BatchResult::all_delivered(
                outputs.iter().map(|_| uuid::Uuid::nil()).collect(),
            ))
        }
        async fn flush(&mut self) -> Result<(), AeonError> {
            Ok(())
        }
    }
    struct StubSinkFactory;
    impl SinkFactory for StubSinkFactory {
        fn build(&self, _cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
            Ok(Box::new(StubSink))
        }
    }

    /// Build a `PipelineSupervisor` pre-registered with stub factories for
    /// the connector names the REST tests declare in their fixtures
    /// (currently `kafka`). Extend here if tests start referencing other
    /// source/sink types — the stubs only need to satisfy the lifecycle,
    /// not the data path.
    fn test_supervisor() -> Arc<PipelineSupervisor> {
        let mut reg = ConnectorRegistry::new();
        reg.register_source("kafka", Arc::new(StubSourceFactory));
        reg.register_sink("kafka", Arc::new(StubSinkFactory));
        Arc::new(PipelineSupervisor::new(Arc::new(reg)))
    }

    fn test_state() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!(
            "aeon-api-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Arc::new(AppState {
            registry: Arc::new(ProcessorRegistry::new(&dir).unwrap()),
            pipelines: Arc::new(PipelineManager::new()),
            supervisor: test_supervisor(),
            delivery_ledgers: dashmap::DashMap::new(),
            pipeline_controls: dashmap::DashMap::new(),
            pipeline_metrics: dashmap::DashMap::new(),
            #[cfg(feature = "processor-auth")]
            poh_chains: dashmap::DashMap::new(),
            identities: Arc::new(ProcessorIdentityStore::new()),
            #[cfg(feature = "processor-auth")]
            authenticator: None,
            #[cfg(not(feature = "processor-auth"))]
            api_token: None,
            #[cfg(feature = "websocket-host")]
            ws_host: None,
            #[cfg(feature = "cluster")]
            cluster_node: None,
            shutting_down: Arc::new(AtomicBool::new(false)),
        })
    }

    fn test_state_with_auth(token: &str) -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!(
            "aeon-api-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        // Set env var for ApiKeyAuthenticator and create authenticator
        let env_key = format!(
            "AEON_TEST_API_KEY_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        // SAFETY: test-only, single-threaded test setup before any concurrent access.
        unsafe { std::env::set_var(&env_key, token) };

        Arc::new(AppState {
            registry: Arc::new(ProcessorRegistry::new(&dir).unwrap()),
            pipelines: Arc::new(PipelineManager::new()),
            supervisor: test_supervisor(),
            delivery_ledgers: dashmap::DashMap::new(),
            pipeline_controls: dashmap::DashMap::new(),
            pipeline_metrics: dashmap::DashMap::new(),
            #[cfg(feature = "processor-auth")]
            poh_chains: dashmap::DashMap::new(),
            identities: Arc::new(ProcessorIdentityStore::new()),
            #[cfg(feature = "processor-auth")]
            authenticator: Some(
                aeon_crypto::auth::ApiKeyAuthenticator::from_config(&[
                    aeon_crypto::auth::ApiKeyEntry {
                        name: "test".into(),
                        key_env: env_key,
                    },
                ])
                .unwrap(),
            ),
            #[cfg(not(feature = "processor-auth"))]
            api_token: Some(token.to_string()),
            #[cfg(feature = "websocket-host")]
            ws_host: None,
            #[cfg(feature = "cluster")]
            cluster_node: None,
            shutting_down: Arc::new(AtomicBool::new(false)),
        })
    }

    #[tokio::test]
    async fn metrics_endpoint_exposes_pipeline_counters() {
        use std::sync::atomic::Ordering;

        let state = test_state();

        // Seed a fake pipeline's metrics so the exposition has something to emit.
        let m = Arc::new(crate::pipeline::PipelineMetrics::new());
        m.events_received.store(1234, Ordering::Relaxed);
        m.events_processed.store(1200, Ordering::Relaxed);
        m.outputs_sent.store(1180, Ordering::Relaxed);
        m.outputs_acked.store(1175, Ordering::Relaxed);
        m.events_failed.store(7, Ordering::Relaxed);
        state
            .supervisor
            .insert_metrics_for_test("demo", Arc::clone(&m))
            .await;

        let app = api_router(state);
        let resp = app
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .cloned();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();

        // Content-Type must be Prometheus text (otherwise scrapers reject it)
        assert!(
            ct.as_ref()
                .map(|v| v.to_str().unwrap_or("").contains("text/plain"))
                .unwrap_or(false),
            "metrics content-type should be text/plain, got {ct:?}"
        );

        // Help/type headers + per-pipeline labelled counters
        assert!(text.contains("# TYPE aeon_pipeline_events_received_total counter"));
        assert!(text.contains("aeon_pipeline_events_received_total{pipeline=\"demo\"} 1234"));
        assert!(text.contains("aeon_pipeline_events_processed_total{pipeline=\"demo\"} 1200"));
        assert!(text.contains("aeon_pipeline_outputs_sent_total{pipeline=\"demo\"} 1180"));
        assert!(text.contains("aeon_pipeline_outputs_acked_total{pipeline=\"demo\"} 1175"));
        assert!(text.contains("# TYPE aeon_pipeline_outputs_acked_total counter"));
        assert!(text.contains("aeon_pipeline_events_failed_total{pipeline=\"demo\"} 7"));
    }

    #[cfg(feature = "cluster")]
    #[tokio::test]
    async fn metrics_endpoint_includes_cluster_metrics_when_clustered() {
        use std::sync::atomic::Ordering;

        // Build a single-node cluster and plug it into AppState so the /metrics
        // handler reaches cluster_metrics_prometheus().
        let cfg = aeon_cluster::ClusterConfig::single_node(42, 4);
        let node = Arc::new(aeon_cluster::ClusterNode::bootstrap_single(cfg).await.unwrap());

        // Start from a standard test_state() but swap in the cluster_node field.
        let base = test_state();
        let state = Arc::new(AppState {
            registry: base.registry.clone(),
            pipelines: base.pipelines.clone(),
            supervisor: base.supervisor.clone(),
            delivery_ledgers: dashmap::DashMap::new(),
            pipeline_controls: dashmap::DashMap::new(),
            pipeline_metrics: dashmap::DashMap::new(),
            #[cfg(feature = "processor-auth")]
            poh_chains: dashmap::DashMap::new(),
            identities: base.identities.clone(),
            #[cfg(feature = "processor-auth")]
            authenticator: None,
            #[cfg(not(feature = "processor-auth"))]
            api_token: None,
            #[cfg(feature = "websocket-host")]
            ws_host: None,
            cluster_node: Some(Arc::clone(&node)),
            shutting_down: Arc::new(AtomicBool::new(false)),
        });

        // Touch a pipeline metric so the pipeline section is non-empty too.
        let m = Arc::new(crate::pipeline::PipelineMetrics::new());
        m.events_received.store(5, Ordering::Relaxed);
        state
            .supervisor
            .insert_metrics_for_test("demo", Arc::clone(&m))
            .await;

        let app = api_router(state);
        let resp = app
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();

        // Cluster gauges carry the node_id label and the configured NodeId (42)
        assert!(text.contains("aeon_raft_term{node_id=\"42\"}"));
        assert!(text.contains("aeon_raft_is_leader{node_id=\"42\"} 1"));
        assert!(text.contains("aeon_cluster_membership_size{node_id=\"42\"} 1"));
        assert!(text.contains("aeon_cluster_node_id 42"));
        // And the pipeline section is still emitted alongside
        assert!(text.contains("aeon_pipeline_events_received_total{pipeline=\"demo\"} 5"));

        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn health_endpoint() {
        let state = test_state();
        let app = api_router(state);

        let resp = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn ready_flips_to_503_when_shutting_down() {
        let state = test_state();
        let app = api_router(Arc::clone(&state));

        let resp = app
            .clone()
            .oneshot(Request::get("/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ready");

        state.shutting_down.store(true, Ordering::Relaxed);

        let resp = app
            .oneshot(Request::get("/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "shutting_down");
    }

    #[tokio::test]
    async fn list_processors_empty() {
        let state = test_state();
        let app = api_router(state);

        let resp = app
            .oneshot(
                Request::get("/api/v1/processors")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn get_processor_not_found() {
        let state = test_state();
        let app = api_router(state);

        let resp = app
            .oneshot(
                Request::get("/api/v1/processors/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn register_processor_via_api() {
        let state = test_state();
        let app = api_router(state.clone());

        let resp = app
            .oneshot(
                Request::post("/api/v1/processors")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{
                            "name": "my-proc",
                            "description": "test processor",
                            "version": {
                                "version": "1.0.0",
                                "sha512": "abc123",
                                "size_bytes": 1024,
                                "processor_type": "wasm",
                                "platform": "wasm32",
                                "status": "available",
                                "registered_at": 1000,
                                "registered_by": "test"
                            }
                        }"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "registered");
        assert_eq!(json["name"], "my-proc");
        assert_eq!(json["version"], "1.0.0");

        // Verify it shows up in list
        let app = api_router(state);
        let resp = app
            .oneshot(
                Request::get("/api/v1/processors")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["name"], "my-proc");
    }

    #[tokio::test]
    async fn list_pipelines_empty() {
        let state = test_state();
        let app = api_router(state);

        let resp = app
            .oneshot(
                Request::get("/api/v1/pipelines")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn pipeline_lifecycle_via_api() {
        let state = test_state();

        // Create pipeline directly via manager (simulating prior state)
        let def = PipelineDefinition::new(
            "test-pipe",
            SourceConfig {
                source_type: "kafka".into(),
                topic: Some("in".into()),
                partitions: vec![0],
                config: BTreeMap::new(),
            },
            ProcessorRef::new("proc", "1.0.0"),
            SinkConfig {
                sink_type: "kafka".into(),
                topic: Some("out".into()),
                config: BTreeMap::new(),
            },
            1000,
        );
        state.pipelines.create(def).await.unwrap();

        // Start via API
        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/test-pipe/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Verify running
        let pipeline = state.pipelines.get("test-pipe").await.unwrap();
        assert_eq!(pipeline.state, aeon_types::registry::PipelineState::Running);

        // Stop via API
        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/test-pipe/stop")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn pipeline_history_via_api() {
        let state = test_state();

        let def = PipelineDefinition::new(
            "hist-pipe",
            SourceConfig {
                source_type: "memory".into(),
                topic: None,
                partitions: vec![],
                config: BTreeMap::new(),
            },
            ProcessorRef::new("proc", "1.0.0"),
            SinkConfig {
                sink_type: "blackhole".into(),
                topic: None,
                config: BTreeMap::new(),
            },
            1000,
        );
        state.pipelines.create(def).await.unwrap();
        state.pipelines.start("hist-pipe", "test").await.unwrap();

        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::get("/api/v1/pipelines/hist-pipe/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Should have at least 2 entries: created + started
        assert!(json.as_array().unwrap().len() >= 2);
    }

    /// Helper: create a running pipeline for upgrade tests.
    async fn create_running_pipeline(state: &Arc<AppState>, name: &str) {
        let def = PipelineDefinition::new(
            name,
            SourceConfig {
                source_type: "kafka".into(),
                topic: Some("in".into()),
                partitions: vec![0],
                config: BTreeMap::new(),
            },
            ProcessorRef::new("proc", "1.0.0"),
            SinkConfig {
                sink_type: "kafka".into(),
                topic: Some("out".into()),
                config: BTreeMap::new(),
            },
            1000,
        );
        state.pipelines.create(def).await.unwrap();
        state.pipelines.start(name, "test").await.unwrap();
    }

    #[tokio::test]
    async fn blue_green_via_api() {
        let state = test_state();
        create_running_pipeline(&state, "bg-api").await;

        // Start blue-green upgrade
        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/bg-api/upgrade/blue-green")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"processor_name":"proc","processor_version":"2.0.0"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Cutover
        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/bg-api/cutover")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let p = state.pipelines.get("bg-api").await.unwrap();
        assert_eq!(p.processor.version, "2.0.0");
    }

    #[tokio::test]
    async fn rollback_via_api() {
        let state = test_state();
        create_running_pipeline(&state, "rb-api").await;

        // Start blue-green
        state
            .pipelines
            .upgrade_blue_green("rb-api", ProcessorRef::new("proc", "2.0.0"), "test")
            .await
            .unwrap();

        // Rollback via API
        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/rb-api/rollback")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let p = state.pipelines.get("rb-api").await.unwrap();
        assert_eq!(p.processor.version, "1.0.0");
    }

    #[tokio::test]
    async fn canary_via_api() {
        let state = test_state();
        create_running_pipeline(&state, "can-api").await;

        // Start canary
        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/can-api/upgrade/canary")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"processor_name":"proc","processor_version":"2.0.0","steps":[10,100]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Check canary status
        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::get("/api/v1/pipelines/can-api/canary-status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["traffic_pct"], 10);

        // Promote to 100%
        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/can-api/promote")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Promote again → complete
        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/can-api/promote")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let p = state.pipelines.get("can-api").await.unwrap();
        assert_eq!(p.processor.version, "2.0.0");
    }

    #[tokio::test]
    async fn delivery_status_no_ledger() {
        let state = test_state();
        let app = api_router(state);

        let resp = app
            .oneshot(
                Request::get("/api/v1/pipelines/nonexistent/delivery")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delivery_status_with_ledger() {
        let state = test_state();

        // Register a ledger with tracked events
        let ledger = Arc::new(DeliveryLedger::new(3));
        let id1 = uuid::Uuid::now_v7();
        let id2 = uuid::Uuid::now_v7();
        ledger.track(id1, aeon_types::PartitionId::new(0), 100);
        ledger.track(id2, aeon_types::PartitionId::new(0), 101);
        ledger.mark_acked(&id1);
        ledger.mark_failed(&id2, "timeout".into());

        state
            .delivery_ledgers
            .insert("test-pipe".to_string(), ledger);

        let app = api_router(state);
        let resp = app
            .oneshot(
                Request::get("/api/v1/pipelines/test-pipe/delivery")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["pending_count"], 0);
        assert_eq!(json["failed_count"], 1);
        assert_eq!(json["total_tracked"], 2);
        assert_eq!(json["total_acked"], 1);
        assert_eq!(json["failed_entries"].as_array().unwrap().len(), 1);
        assert_eq!(json["failed_entries"][0]["reason"], "timeout");
    }

    #[tokio::test]
    async fn delivery_retry_removes_failed() {
        let state = test_state();

        let ledger = Arc::new(DeliveryLedger::new(3));
        let id = uuid::Uuid::now_v7();
        ledger.track(id, aeon_types::PartitionId::new(0), 50);
        ledger.mark_failed(&id, "error".into());

        state
            .delivery_ledgers
            .insert("retry-pipe".to_string(), ledger.clone());

        let app = api_router(state);
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/retry-pipe/delivery/retry")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"event_ids":["{}"]}}"#, id)))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["retried"], 1);
        assert_eq!(json["not_found"], 0);

        // Ledger should be empty now
        assert!(ledger.is_empty());
    }

    // ── Authentication tests ──────────────────────────────────────────

    #[tokio::test]
    async fn auth_rejects_missing_token() {
        let state = test_state_with_auth("secret-token-123");
        let app = api_router(state);

        let resp = app
            .oneshot(
                Request::get("/api/v1/processors")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_rejects_wrong_token() {
        let state = test_state_with_auth("secret-token-123");
        let app = api_router(state);

        let resp = app
            .oneshot(
                Request::get("/api/v1/processors")
                    .header("authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_accepts_correct_token() {
        let state = test_state_with_auth("secret-token-123");
        let app = api_router(state);

        let resp = app
            .oneshot(
                Request::get("/api/v1/processors")
                    .header("authorization", "Bearer secret-token-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_bypasses_auth() {
        let state = test_state_with_auth("secret-token-123");
        let app = api_router(state);

        // Health should work without any token
        let resp = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn security_headers_present() {
        let state = test_state();
        let app = api_router(state);

        let resp = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            resp.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");
    }

    // ── Reconfigure endpoint tests ────────────────────────────────────

    #[tokio::test]
    async fn reconfigure_source_via_api() {
        let state = test_state();
        create_running_pipeline(&state, "reconf-src-api").await;

        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/reconf-src-api/reconfigure/source")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"type":"kafka","topic":"new-input","partitions":[0,1,2]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let p = state.pipelines.get("reconf-src-api").await.unwrap();
        assert_eq!(p.sources[0].topic.as_deref(), Some("new-input"));
        assert_eq!(p.sources[0].partitions, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn reconfigure_sink_via_api() {
        let state = test_state();
        create_running_pipeline(&state, "reconf-sink-api").await;

        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/reconf-sink-api/reconfigure/sink")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"type":"kafka","topic":"new-output"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let p = state.pipelines.get("reconf-sink-api").await.unwrap();
        assert_eq!(p.sinks[0].topic.as_deref(), Some("new-output"));
    }

    #[tokio::test]
    async fn reconfigure_cross_type_rejected_via_api() {
        let state = test_state();
        create_running_pipeline(&state, "reconf-cross-api").await;

        let app = api_router(state.clone());
        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/reconf-cross-api/reconfigure/source")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"type":"nats","topic":"stream"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn reconfigure_nonexistent_pipeline() {
        let state = test_state();
        let app = api_router(state);

        let resp = app
            .oneshot(
                Request::post("/api/v1/pipelines/ghost/reconfigure/source")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"type":"kafka","topic":"t"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cluster_status_standalone() {
        let state = test_state();
        let app = api_router(state);

        let resp = app
            .oneshot(
                Request::get("/api/v1/cluster/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["mode"].as_str().is_some());
    }
}
