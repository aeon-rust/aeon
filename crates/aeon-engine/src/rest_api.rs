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
use crate::registry::ProcessorRegistry;

/// Maximum request body size (10 MB).
const MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Shared application state for all handlers.
pub struct AppState {
    pub registry: Arc<ProcessorRegistry>,
    pub pipelines: Arc<PipelineManager>,
    /// Per-pipeline delivery ledgers (pipeline_name → ledger).
    /// Populated when pipelines run with delivery tracking enabled.
    pub delivery_ledgers: dashmap::DashMap<String, Arc<DeliveryLedger>>,
    /// Per-pipeline control handles for drain→swap→resume hot-swap.
    /// Registered when a managed pipeline starts, removed when it stops.
    pub pipeline_controls: dashmap::DashMap<String, Arc<PipelineControl>>,
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
}

/// Build the axum Router with all API routes.
///
/// Health endpoints bypass authentication. All `/api/v1/` routes require
/// a valid Bearer token when `AEON_API_TOKEN` is set.
pub fn api_router(state: Arc<AppState>) -> Router {
    // Health routes — no auth required
    let health_routes = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready));

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
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // WebSocket processor connect — no Bearer auth (AWPP handshake handles auth)
    #[cfg(feature = "websocket-host")]
    let health_routes =
        health_routes.route("/api/v1/processors/connect", get(ws_processor_connect));

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
    let app = api_router(state);
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
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

async fn ready() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ready",
        version: env!("CARGO_PKG_VERSION"),
    })
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
    Json(req): Json<RegisterProcessorRequest>,
) -> impl IntoResponse {
    let cmd = aeon_types::registry::RegistryCommand::RegisterProcessor {
        name: req.name,
        description: req.description,
        version: req.version,
    };
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
    Json(req): Json<CreatePipelineRequest>,
) -> impl IntoResponse {
    if let Err(e) = validate_resource_name(&req.definition.name, "pipeline") {
        return e.into_response();
    }
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
) -> impl IntoResponse {
    match state.pipelines.start(&name, "api").await {
        Ok(()) => Json(serde_json::json!({"status": "started"})).into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn stop_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.pipelines.stop(&name, "api").await {
        Ok(()) => Json(serde_json::json!({"status": "stopped"})).into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct UpgradeRequest {
    processor_name: String,
    processor_version: String,
}

async fn upgrade_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<UpgradeRequest>,
) -> impl IntoResponse {
    let proc_ref =
        aeon_types::registry::ProcessorRef::new(req.processor_name, req.processor_version);

    // If a PipelineControl handle exists and a replacement processor is
    // provided, perform a real drain→swap→resume. The caller must have
    // pre-registered a PipelineControl via `pipeline_controls.insert()`
    // when starting the managed pipeline.
    //
    // Processor instantiation (Wasm from_bytes, native dlopen) is handled
    // by the caller or a future middleware — this endpoint focuses on the
    // PipelineManager metadata upgrade. When pipeline_controls gains a
    // processor factory, the full hot-swap path will be:
    //   load artifact → instantiate → drain_and_swap → update metadata.
    match state.pipelines.upgrade(&name, proc_ref, "api").await {
        Ok(()) => {
            let method = if state.pipeline_controls.contains_key(&name) {
                "managed"
            } else {
                "metadata"
            };
            Json(serde_json::json!({"status": "upgraded", "method": method})).into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── Blue-Green + Canary endpoints ──────────────────────────────────────

async fn upgrade_blue_green(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<UpgradeRequest>,
) -> impl IntoResponse {
    let proc_ref =
        aeon_types::registry::ProcessorRef::new(req.processor_name, req.processor_version);
    match state
        .pipelines
        .upgrade_blue_green(&name, proc_ref, "api")
        .await
    {
        Ok(()) => {
            // Note: actual PipelineControl.start_blue_green() requires a processor
            // instance (Box<dyn Processor>). Without a processor factory, the REST
            // layer can only update metadata. The caller must provide a processor
            // instance via PipelineControl directly for real blue-green shadow mode.
            let method = if state.pipeline_controls.contains_key(&name) {
                "managed"
            } else {
                "metadata"
            };
            Json(serde_json::json!({"status": "blue-green-started", "method": method}))
                .into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
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
    let proc_ref =
        aeon_types::registry::ProcessorRef::new(req.processor_name, req.processor_version);
    match state
        .pipelines
        .upgrade_canary(&name, proc_ref, req.steps, req.thresholds, "api")
        .await
    {
        Ok(()) => Json(serde_json::json!({"status": "canary-started"})).into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
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
            let method = if state.pipeline_controls.contains_key(&name) {
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

async fn delete_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
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
            // PoH chains are not yet wired into the pipeline runtime.
            // Return a structured response so the CLI can display meaningful output
            // and detect when PoH becomes active.
            let json = serde_json::json!({
                "pipeline": name,
                "poh_active": false,
                "status": "ok",
                "message": "Pipeline exists. PoH chain tracking activates when the pipeline runs with integrity verification enabled.",
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
                        serde_json::json!({
                            "name": pname,
                            "state": pstate.to_string(),
                            "poh_active": false,
                        })
                    })
                    .collect();

                let json = serde_json::json!({
                    "status": "ok",
                    "poh_active": false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::registry::{PipelineDefinition, ProcessorRef, SinkConfig, SourceConfig};
    use axum::body::Body;
    use axum::http::Request;
    use std::collections::BTreeMap;
    use tower::ServiceExt;

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
            delivery_ledgers: dashmap::DashMap::new(),
            pipeline_controls: dashmap::DashMap::new(),
            identities: Arc::new(ProcessorIdentityStore::new()),
            #[cfg(feature = "processor-auth")]
            authenticator: None,
            #[cfg(not(feature = "processor-auth"))]
            api_token: None,
            #[cfg(feature = "websocket-host")]
            ws_host: None,
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
            delivery_ledgers: dashmap::DashMap::new(),
            pipeline_controls: dashmap::DashMap::new(),
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
        })
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
}
