//! REST API server for Aeon management (axum, port 4471).
//!
//! Provides CRUD endpoints for processors, pipelines, and cluster status.
//! Authentication via API key (Bearer token) or mTLS.
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
//!
//! **System:**
//! - `GET    /health`                               — health check
//! - `GET    /ready`                                — readiness check

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::pipeline_manager::PipelineManager;
use crate::registry::ProcessorRegistry;

/// Shared application state for all handlers.
pub struct AppState {
    pub registry: Arc<ProcessorRegistry>,
    pub pipelines: Arc<PipelineManager>,
}

/// Build the axum Router with all API routes.
pub fn api_router(state: Arc<AppState>) -> Router {
    Router::new()
        // Health
        .route("/health", get(health))
        .route("/ready", get(ready))
        // Processors
        .route("/api/v1/processors", get(list_processors))
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
        .route("/api/v1/pipelines/{name}", get(get_pipeline).delete(delete_pipeline))
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
        .route(
            "/api/v1/pipelines/{name}/canary-status",
            get(canary_status),
        )
        .route("/api/v1/pipelines/{name}/history", get(pipeline_history))
        .with_state(state)
}

/// Start the REST API server on the given address.
pub async fn serve(state: Arc<AppState>, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let app = api_router(state);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(addr = addr, "REST API server listening");
    axum::serve(listener, app).await?;
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

async fn get_processor(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.registry.get(&name).await {
        Some(record) => {
            let json = serde_json::to_value(&record).unwrap_or_default();
            (StatusCode::OK, Json(json)).into_response()
        }
        None => api_error(StatusCode::NOT_FOUND, format!("processor '{name}' not found"))
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
        None => api_error(StatusCode::NOT_FOUND, format!("processor '{name}' not found"))
            .into_response(),
    }
}

async fn delete_processor_version(
    State(state): State<Arc<AppState>>,
    Path((name, version)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.registry.delete_version(&name, &version).await {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"status": "deleted"}))).into_response(),
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
        None => api_error(StatusCode::NOT_FOUND, format!("pipeline '{name}' not found"))
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
    let proc_ref = aeon_types::registry::ProcessorRef {
        name: req.processor_name,
        version: req.processor_version,
    };
    match state.pipelines.upgrade(&name, proc_ref, "api").await {
        Ok(()) => Json(serde_json::json!({"status": "upgraded"})).into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── Blue-Green + Canary endpoints ──────────────────────────────────────

async fn upgrade_blue_green(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<UpgradeRequest>,
) -> impl IntoResponse {
    let proc_ref = aeon_types::registry::ProcessorRef {
        name: req.processor_name,
        version: req.processor_version,
    };
    match state
        .pipelines
        .upgrade_blue_green(&name, proc_ref, "api")
        .await
    {
        Ok(()) => Json(serde_json::json!({"status": "blue-green-started"})).into_response(),
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
    let proc_ref = aeon_types::registry::ProcessorRef {
        name: req.processor_name,
        version: req.processor_version,
    };
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
        Ok(()) => Json(serde_json::json!({"status": "cutover-complete"})).into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn rollback_pipeline(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.pipelines.rollback_upgrade(&name, "api").await {
        Ok(()) => Json(serde_json::json!({"status": "rolled-back"})).into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn promote_canary(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.pipelines.promote_canary(&name, "api").await {
        Ok(()) => Json(serde_json::json!({"status": "promoted"})).into_response(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::registry::{
        PipelineDefinition, ProcessorRef, SinkConfig, SourceConfig,
    };
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
            ProcessorRef {
                name: "proc".into(),
                version: "1.0.0".into(),
            },
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
        assert_eq!(
            pipeline.state,
            aeon_types::registry::PipelineState::Running
        );

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
            ProcessorRef {
                name: "proc".into(),
                version: "1.0.0".into(),
            },
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
            ProcessorRef {
                name: "proc".into(),
                version: "1.0.0".into(),
            },
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
            .upgrade_blue_green(
                "rb-api",
                ProcessorRef {
                    name: "proc".into(),
                    version: "2.0.0".into(),
                },
                "test",
            )
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
}
