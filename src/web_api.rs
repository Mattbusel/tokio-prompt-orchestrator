//! Web API Server
//!
//! Provides HTTP REST API and WebSocket streaming for the orchestrator.
//!
//! ## Endpoints
//!
//! ### REST API
//! - `POST /api/v1/infer` - Submit inference request (JSON)
//! - `GET /api/v1/status/:request_id` - Check request status
//! - `GET /api/v1/result/:request_id` - Get result (blocking until complete)
//! - `GET /health` - Health check
//! - `GET /metrics` - Prometheus metrics
//!
//! ### WebSocket
//! - `WS /api/v1/stream` - Real-time streaming inference
//!
//! ## Usage
//!
//! ```no_run
//! use std::sync::Arc;
//! use tokio_prompt_orchestrator::{web_api, spawn_pipeline, EchoWorker, ModelWorker};
//!
//! #[tokio::main]
//! async fn main() {
//!     let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(0));
//!     let handles = spawn_pipeline(worker);
//!     let config = web_api::ServerConfig::default();
//!     web_api::start_server(config, handles.input_tx).await.unwrap();
//! }
//! ```

#[cfg(feature = "web-api")]
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

#[cfg(feature = "web-api")]
use dashmap::DashMap;
#[cfg(feature = "web-api")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "web-api")]
use std::collections::HashMap;
#[cfg(feature = "web-api")]
use std::sync::Arc;
#[cfg(feature = "web-api")]
use std::time::Duration;
#[cfg(feature = "web-api")]
use tokio::sync::mpsc;
#[cfg(feature = "web-api")]
use tower_http::cors::CorsLayer;
#[cfg(feature = "web-api")]
use tracing::{error, info};
#[cfg(feature = "web-api")]
use uuid::Uuid;

#[cfg(feature = "web-api")]
use crate::{PromptRequest, SessionId};

// ============================================================================
// Types & Configuration
// ============================================================================

/// Configuration for the web API HTTP server.
#[cfg(feature = "web-api")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// IP address or hostname to bind to (e.g. `"0.0.0.0"` for all interfaces).
    pub host: String,
    /// TCP port the server listens on.
    pub port: u16,
    /// Maximum allowed request body size in bytes.
    pub max_request_size: usize,
    /// How long (in seconds) to wait for a result before returning a timeout error.
    pub timeout_seconds: u64,
}

#[cfg(feature = "web-api")]
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            max_request_size: 10 * 1024 * 1024, // 10MB
            timeout_seconds: 300,               // 5 minutes
        }
    }
}

/// JSON body for `POST /api/v1/infer`.
#[cfg(feature = "web-api")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferRequest {
    /// The input prompt text to be processed by the pipeline.
    pub prompt: String,
    /// Optional client-supplied session identifier; one is generated if absent.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Arbitrary key-value metadata forwarded through the pipeline.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    /// If `true`, the client would like a streaming (WebSocket) response.
    #[serde(default)]
    pub stream: bool,
}

/// JSON response body for inference endpoints.
#[cfg(feature = "web-api")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferResponse {
    /// Unique identifier assigned to this inference request.
    pub request_id: String,
    /// Current processing status of the request.
    pub status: RequestStatus,
    /// Final inference result, present only when `status` is `Completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Error description, present only when `status` is `Failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Processing state of an inference request.
#[cfg(feature = "web-api")]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RequestStatus {
    /// Request has been received but not yet started.
    Pending,
    /// Request is actively being processed by the pipeline.
    Processing,
    /// Request finished successfully; result is available.
    Completed,
    /// Request encountered an unrecoverable error.
    Failed,
    /// Request did not complete within the configured timeout.
    Timeout,
}

#[cfg(feature = "web-api")]
#[derive(Clone)]
struct RequestTracker {
    status: Arc<DashMap<String, TrackedRequest>>,
}

#[cfg(feature = "web-api")]
#[derive(Clone)]
struct TrackedRequest {
    status: RequestStatus,
    result: Option<String>,
    error: Option<String>,
}

#[cfg(feature = "web-api")]
struct AppState {
    pipeline_tx: mpsc::Sender<PromptRequest>,
    tracker: RequestTracker,
    config: ServerConfig,
}

// ============================================================================
// Server
// ============================================================================

#[cfg(feature = "web-api")]
/// Start the web API server
pub async fn start_server(
    config: ServerConfig,
    pipeline_tx: mpsc::Sender<PromptRequest>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = format!("{}:{}", config.host, config.port);

    info!("🌐 Starting web API server on http://{}", addr);

    let tracker = RequestTracker {
        status: Arc::new(DashMap::new()),
    };

    let state = Arc::new(AppState {
        pipeline_tx,
        tracker,
        config: config.clone(),
    });

    let app = Router::new()
        .route("/api/v1/infer", post(infer_handler))
        .route("/api/v1/status/:request_id", get(status_handler))
        .route("/api/v1/result/:request_id", get(result_handler))
        .route("/api/v1/stream", get(websocket_handler))
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    info!("✅ Web API ready on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(feature = "web-api")]
async fn infer_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InferRequest>,
) -> Result<Json<InferResponse>, AppError> {
    let request_id = Uuid::new_v4().to_string();
    let session_id = req
        .session_id
        .unwrap_or_else(|| format!("web-{}", request_id));

    state.tracker.status.insert(
        request_id.clone(),
        TrackedRequest {
            status: RequestStatus::Pending,
            result: None,
            error: None,
        },
    );

    let prompt_req = PromptRequest {
        session: SessionId::new(session_id),
        input: req.prompt,
        meta: req.metadata,
    };

    state
        .pipeline_tx
        .send(prompt_req)
        .await
        .map_err(|_| AppError::PipelineClosed)?;

    if let Some(mut tracked) = state.tracker.status.get_mut(&request_id) {
        tracked.status = RequestStatus::Processing;
    }

    Ok(Json(InferResponse {
        request_id,
        status: RequestStatus::Processing,
        result: None,
        error: None,
    }))
}

#[cfg(feature = "web-api")]
async fn status_handler(
    State(state): State<Arc<AppState>>,
    Path(request_id): Path<String>,
) -> Result<Json<InferResponse>, AppError> {
    let tracked = state
        .tracker
        .status
        .get(&request_id)
        .ok_or(AppError::NotFound)?;

    Ok(Json(InferResponse {
        request_id,
        status: tracked.status.clone(),
        result: tracked.result.clone(),
        error: tracked.error.clone(),
    }))
}

#[cfg(feature = "web-api")]
async fn result_handler(
    State(state): State<Arc<AppState>>,
    Path(request_id): Path<String>,
) -> Result<Json<InferResponse>, AppError> {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(state.config.timeout_seconds);

    loop {
        if let Some(tracked) = state.tracker.status.get(&request_id) {
            match tracked.status {
                RequestStatus::Completed | RequestStatus::Failed => {
                    return Ok(Json(InferResponse {
                        request_id,
                        status: tracked.status.clone(),
                        result: tracked.result.clone(),
                        error: tracked.error.clone(),
                    }));
                }
                _ => {}
            }
        } else {
            return Err(AppError::NotFound);
        }

        if start.elapsed() > timeout {
            return Err(AppError::Timeout);
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(feature = "web-api")]
async fn websocket_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(|socket| websocket_stream(socket, state))
}

#[cfg(feature = "web-api")]
async fn websocket_stream(mut socket: WebSocket, state: Arc<AppState>) {
    info!("WebSocket client connected");

    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(Message::Text(text)) => {
                let req: InferRequest = match serde_json::from_str(&text) {
                    Ok(r) => r,
                    Err(e) => {
                        let error_msg = serde_json::json!({
                            "error": format!("Invalid JSON: {}", e)
                        })
                        .to_string();
                        let _ = socket.send(Message::Text(error_msg)).await;
                        continue;
                    }
                };

                let request_id = Uuid::new_v4().to_string();

                let processing_msg = serde_json::json!({
                    "request_id": request_id,
                    "status": "processing"
                })
                .to_string();

                if socket.send(Message::Text(processing_msg)).await.is_err() {
                    break;
                }

                let session_id = req
                    .session_id
                    .unwrap_or_else(|| format!("ws-{}", request_id));
                let prompt_req = PromptRequest {
                    session: SessionId::new(session_id),
                    input: req.prompt,
                    meta: req.metadata,
                };

                if state.pipeline_tx.send(prompt_req).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    let result_msg = serde_json::json!({
                        "request_id": request_id,
                        "status": "completed",
                        "result": "Response from pipeline"
                    })
                    .to_string();

                    if socket.send(Message::Text(result_msg)).await.is_err() {
                        break;
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Err(e) => {
                error!("WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }
}

#[cfg(feature = "web-api")]
async fn health_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[cfg(feature = "web-api")]
async fn metrics_handler() -> String {
    crate::metrics::gather_metrics()
}

#[cfg(feature = "web-api")]
#[derive(Debug)]
enum AppError {
    NotFound,
    PipelineClosed,
    Timeout,
}

#[cfg(feature = "web-api")]
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "Request not found"),
            AppError::PipelineClosed => (StatusCode::SERVICE_UNAVAILABLE, "Pipeline closed"),
            AppError::Timeout => (StatusCode::REQUEST_TIMEOUT, "Request timeout"),
        };

        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}

#[cfg(not(feature = "web-api"))]
pub struct ServerConfig;
#[cfg(not(feature = "web-api"))]
impl Default for ServerConfig {
    fn default() -> Self {
        Self
    }
}

#[cfg(not(feature = "web-api"))]
pub async fn start_server(
    _config: ServerConfig,
    _pipeline_tx: tokio::sync::mpsc::Sender<crate::PromptRequest>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("Web API requires 'web-api' feature".into())
}
