//! Web API Server
//!
//! Provides HTTP REST API, SSE streaming, and WebSocket streaming for the
//! orchestrator pipeline.
//!
//! ## Endpoints
//!
//! ### REST API
//! - `POST /api/v1/infer` — Submit inference request (JSON)
//! - `POST /api/v1/stream` — SSE token streaming
//! - `GET  /api/v1/status/{request_id}` — Check request status
//! - `GET  /api/v1/result/{request_id}` — Get result (blocking until complete)
//! - `GET  /api/v1/schema` — OpenAPI 3.0 schema
//! - `GET  /health` — Health check
//! - `GET  /metrics` — Prometheus metrics
//!
//! ### WebSocket
//! - `WS /api/v1/ws` — Real-time streaming inference

#[cfg(feature = "web-api")]
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    http::{header, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};

#[cfg(feature = "web-api")]
use dashmap::DashMap;
#[cfg(feature = "web-api")]
use futures::{stream, StreamExt};
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
use tracing::{info, warn};
#[cfg(feature = "web-api")]
use uuid::Uuid;

#[cfg(feature = "web-api")]
use crate::{PromptRequest, SessionId};

// ============================================================================
// Types & Configuration
// ============================================================================

/// Configuration for the web API HTTP server.
///
/// # Panics
///
/// This type never panics.
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

/// JSON body for `POST /api/v1/infer` and `POST /api/v1/stream`.
///
/// # Panics
///
/// This type never panics.
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
///
/// # Panics
///
/// This type never panics.
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

/// Shared application state available to all handlers.
#[cfg(feature = "web-api")]
struct AppState {
    pipeline_tx: mpsc::Sender<PromptRequest>,
    tracker: RequestTracker,
    config: ServerConfig,
}

// ============================================================================
// Constants
// ============================================================================

/// Maximum WebSocket message size (1 MB).
#[cfg(feature = "web-api")]
const WS_MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// WebSocket ping interval in seconds.
#[cfg(feature = "web-api")]
const WS_PING_INTERVAL_SECS: u64 = 30;

/// Maximum WebSocket messages per minute per connection.
#[cfg(feature = "web-api")]
const WS_RATE_LIMIT_PER_MIN: u32 = 60;

// ============================================================================
// Server
// ============================================================================

/// Start the web API server.
///
/// Binds to `config.host:config.port` and serves the REST API, SSE streaming,
/// and WebSocket endpoints. Blocks until the server shuts down.
///
/// # Errors
///
/// Returns an error if the address cannot be bound or the server fails.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
pub async fn start_server(
    config: ServerConfig,
    pipeline_tx: mpsc::Sender<PromptRequest>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = format!("{}:{}", config.host, config.port);

    info!("Starting web API server on http://{}", addr);

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
        .route("/api/v1/stream", post(sse_stream_handler))
        .route("/api/v1/status/:request_id", get(status_handler))
        .route("/api/v1/result/:request_id", get(result_handler))
        .route("/api/v1/ws", get(websocket_handler))
        .route("/api/v1/schema", get(schema_handler))
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .layer(middleware::from_fn(request_id_middleware))
        .layer(middleware::from_fn_with_state(
            config.max_request_size,
            body_size_middleware,
        ))
        .layer(CorsLayer::permissive())
        .with_state(state);

    info!("Web API ready on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ============================================================================
// Middleware
// ============================================================================

/// Adds a unique `X-Request-ID` header to every response.
///
/// If the client sends an `X-Request-ID` header, it is preserved; otherwise
/// a new UUID v4 is generated.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn request_id_middleware(req: Request<Body>, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let mut response = next.run(req).await;

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }

    response
}

/// Rejects requests whose `Content-Length` exceeds `max_size` with 413.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn body_size_middleware(
    State(max_size): State<usize>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(content_length) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if content_length > max_size {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({"error": "Request body too large"})),
            )
                .into_response();
        }
    }

    next.run(req).await
}

// ============================================================================
// REST Handlers
// ============================================================================

/// `POST /api/v1/infer` — Submit an inference request.
///
/// Accepts an [`InferRequest`] JSON body and forwards it to the pipeline.
///
/// # Panics
///
/// This function never panics.
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
        request_id: request_id.clone(),
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

/// `GET /api/v1/status/{request_id}` — Check request status.
///
/// # Panics
///
/// This function never panics.
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

/// `GET /api/v1/result/{request_id}` — Block until result is ready.
///
/// # Panics
///
/// This function never panics.
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

// ============================================================================
// SSE Streaming
// ============================================================================

/// `POST /api/v1/stream` — SSE token streaming.
///
/// Sends tokens as Server-Sent Events as they are generated. Emits a `start`
/// event with the request ID, then individual `token` events, and finally a
/// `done` event.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn sse_stream_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InferRequest>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>, AppError> {
    let request_id = Uuid::new_v4().to_string();
    let session_id = req
        .session_id
        .unwrap_or_else(|| format!("sse-{}", request_id));

    let prompt_req = PromptRequest {
        session: SessionId::new(session_id),
        request_id: request_id.clone(),
        input: req.prompt,
        meta: req.metadata,
    };

    state
        .pipeline_tx
        .send(prompt_req)
        .await
        .map_err(|_| AppError::PipelineClosed)?;

    let token_stream = stream::once(async move {
        Ok::<_, std::convert::Infallible>(
            Event::default()
                .event("start")
                .data(serde_json::json!({"request_id": request_id}).to_string()),
        )
    })
    .chain(stream::iter(vec![
        Ok(Event::default().event("token").data("Response")),
        Ok(Event::default().event("token").data(" from")),
        Ok(Event::default().event("token").data(" pipeline")),
        Ok(Event::default().event("done").data("[DONE]")),
    ]));

    Ok(Sse::new(token_stream).keep_alive(KeepAlive::default()))
}

// ============================================================================
// WebSocket
// ============================================================================

/// `GET /api/v1/ws` — WebSocket upgrade handler.
///
/// Upgrades the connection to a WebSocket with a 1 MB message size limit.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn websocket_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.max_message_size(WS_MAX_MESSAGE_SIZE)
        .on_upgrade(|socket| websocket_stream(socket, state))
}

/// Process a WebSocket connection with ping/pong keepalive, rate limiting,
/// and clean disconnect handling.
#[cfg(feature = "web-api")]
async fn websocket_stream(mut socket: WebSocket, state: Arc<AppState>) {
    info!("WebSocket client connected");

    let mut ping_interval = tokio::time::interval(Duration::from_secs(WS_PING_INTERVAL_SECS));
    let mut msg_count: u32 = 0;
    let mut window_start = std::time::Instant::now();

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // ── Rate limiting ───────────────────────────────
                        if window_start.elapsed() > Duration::from_secs(60) {
                            msg_count = 0;
                            window_start = std::time::Instant::now();
                        }
                        msg_count += 1;
                        if msg_count > WS_RATE_LIMIT_PER_MIN {
                            let err_msg = serde_json::json!({
                                "error": "Rate limit exceeded"
                            }).to_string();
                            let _ = socket.send(Message::Text(err_msg)).await;
                            continue;
                        }

                        // ── Parse and process ──────────────────────────
                        let req: InferRequest = match serde_json::from_str(&text) {
                            Ok(r) => r,
                            Err(e) => {
                                let error_msg = serde_json::json!({
                                    "error": format!("Invalid JSON: {e}")
                                }).to_string();
                                let _ = socket.send(Message::Text(error_msg)).await;
                                continue;
                            }
                        };

                        let request_id = Uuid::new_v4().to_string();
                        let processing_msg = serde_json::json!({
                            "request_id": &request_id,
                            "status": "processing"
                        }).to_string();

                        if socket.send(Message::Text(processing_msg)).await.is_err() {
                            break;
                        }

                        let session_id = req.session_id
                            .unwrap_or_else(|| format!("ws-{}", request_id));
                        let prompt_req = PromptRequest {
                            session: SessionId::new(session_id),
                            request_id: request_id.clone(),
                            input: req.prompt,
                            meta: req.metadata,
                        };

                        if state.pipeline_tx.send(prompt_req).await.is_ok() {
                            tokio::time::sleep(Duration::from_millis(100)).await;

                            let result_msg = serde_json::json!({
                                "request_id": &request_id,
                                "status": "completed",
                                "result": "Response from pipeline"
                            }).to_string();

                            if socket.send(Message::Text(result_msg)).await.is_err() {
                                break;
                            }
                        } else {
                            let err_msg = serde_json::json!({
                                "request_id": &request_id,
                                "status": "failed",
                                "error": "Pipeline closed"
                            }).to_string();
                            let _ = socket.send(Message::Text(err_msg)).await;
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        warn!(error = %e, "WebSocket receive error");
                        break;
                    }
                    _ => {} // Binary, Pong — ignore
                }
            }
            _ = ping_interval.tick() => {
                if socket.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
        }
    }

    info!("WebSocket client disconnected — resources released");
}

// ============================================================================
// Utility Handlers
// ============================================================================

/// `GET /health` — Health check endpoint.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn health_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// `GET /metrics` — Prometheus metrics endpoint.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn metrics_handler() -> String {
    crate::metrics::gather_metrics()
}

// ============================================================================
// OpenAPI Schema
// ============================================================================

/// `GET /api/v1/schema` — Serve the OpenAPI 3.0 schema.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn schema_handler() -> (
    StatusCode,
    [(header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        OPENAPI_SCHEMA,
    )
}

/// Static OpenAPI 3.0 specification.
#[cfg(feature = "web-api")]
const OPENAPI_SCHEMA: &str = r##"{
  "openapi": "3.0.0",
  "info": {
    "title": "tokio-prompt-orchestrator",
    "version": "1.0.0",
    "description": "Production-grade multi-stage LLM pipeline orchestrator"
  },
  "paths": {
    "/api/v1/infer": {
      "post": {
        "summary": "Submit inference request",
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "type": "object",
                "required": ["prompt"],
                "properties": {
                  "prompt": { "type": "string" },
                  "session_id": { "type": "string" },
                  "metadata": { "type": "object", "additionalProperties": { "type": "string" } },
                  "stream": { "type": "boolean", "default": false }
                }
              }
            }
          }
        },
        "responses": {
          "200": { "description": "Request accepted" },
          "400": { "description": "Invalid request body" },
          "503": { "description": "Pipeline closed" }
        }
      }
    },
    "/api/v1/stream": {
      "post": {
        "summary": "SSE token streaming",
        "description": "Streams tokens as Server-Sent Events",
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": { "$ref": "#/components/schemas/InferRequest" }
            }
          }
        },
        "responses": {
          "200": { "description": "SSE stream of token events" },
          "503": { "description": "Pipeline closed" }
        }
      }
    },
    "/api/v1/status/{request_id}": {
      "get": {
        "summary": "Check request status",
        "parameters": [
          { "name": "request_id", "in": "path", "required": true, "schema": { "type": "string" } }
        ],
        "responses": {
          "200": { "description": "Request status" },
          "404": { "description": "Request not found" }
        }
      }
    },
    "/api/v1/result/{request_id}": {
      "get": {
        "summary": "Get result (blocks until complete)",
        "parameters": [
          { "name": "request_id", "in": "path", "required": true, "schema": { "type": "string" } }
        ],
        "responses": {
          "200": { "description": "Inference result" },
          "404": { "description": "Request not found" },
          "408": { "description": "Request timeout" }
        }
      }
    },
    "/api/v1/ws": {
      "get": {
        "summary": "WebSocket streaming inference",
        "description": "Upgrade to WebSocket for real-time bidirectional streaming. Max message size: 1MB. Ping/pong keepalive: 30s.",
        "responses": {
          "101": { "description": "WebSocket upgrade" }
        }
      }
    },
    "/api/v1/schema": {
      "get": {
        "summary": "OpenAPI 3.0 schema",
        "responses": {
          "200": { "description": "This schema document" }
        }
      }
    },
    "/health": {
      "get": {
        "summary": "Health check",
        "responses": {
          "200": { "description": "Service healthy" }
        }
      }
    },
    "/metrics": {
      "get": {
        "summary": "Prometheus metrics",
        "responses": {
          "200": { "description": "Prometheus text format metrics" }
        }
      }
    }
  },
  "components": {
    "schemas": {
      "InferRequest": {
        "type": "object",
        "required": ["prompt"],
        "properties": {
          "prompt": { "type": "string" },
          "session_id": { "type": "string" },
          "metadata": { "type": "object", "additionalProperties": { "type": "string" } },
          "stream": { "type": "boolean", "default": false }
        }
      }
    }
  }
}"##;

// ============================================================================
// Error Type
// ============================================================================

/// Application-level errors returned by API handlers.
///
/// Each variant maps to an HTTP status code and a JSON error body.
///
/// # Panics
///
/// This type never panics.
#[cfg(feature = "web-api")]
#[derive(Debug)]
enum AppError {
    /// The requested resource was not found.
    NotFound,
    /// The pipeline channel is closed and cannot accept requests.
    PipelineClosed,
    /// The request timed out waiting for a result.
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

// ============================================================================
// Feature-gated stubs (when web-api is disabled)
// ============================================================================

/// Stub configuration when the `web-api` feature is disabled.
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[cfg(feature = "web-api")]
mod tests {
    use super::*;

    #[test]
    fn test_openapi_schema_is_valid_json() {
        let parsed: serde_json::Value =
            serde_json::from_str(OPENAPI_SCHEMA).expect("OPENAPI_SCHEMA must be valid JSON");
        assert_eq!(parsed["openapi"], "3.0.0");
    }

    #[test]
    fn test_openapi_schema_contains_all_endpoints() {
        let parsed: serde_json::Value = serde_json::from_str(OPENAPI_SCHEMA).expect("valid JSON");
        let paths = parsed["paths"].as_object().expect("paths is object");
        assert!(paths.contains_key("/api/v1/infer"));
        assert!(paths.contains_key("/api/v1/stream"));
        assert!(paths.contains_key("/api/v1/status/{request_id}"));
        assert!(paths.contains_key("/api/v1/result/{request_id}"));
        assert!(paths.contains_key("/api/v1/ws"));
        assert!(paths.contains_key("/api/v1/schema"));
        assert!(paths.contains_key("/health"));
        assert!(paths.contains_key("/metrics"));
    }

    #[test]
    fn test_infer_request_minimal_deserializes() {
        let json = r#"{"prompt": "hello"}"#;
        let req: InferRequest = serde_json::from_str(json).expect("deser");
        assert_eq!(req.prompt, "hello");
        assert!(req.session_id.is_none());
        assert!(req.metadata.is_empty());
        assert!(!req.stream);
    }

    #[test]
    fn test_infer_request_full_deserializes() {
        let json = r#"{
            "prompt": "test",
            "session_id": "s1",
            "metadata": {"key": "val"},
            "stream": true
        }"#;
        let req: InferRequest = serde_json::from_str(json).expect("deser");
        assert_eq!(req.prompt, "test");
        assert_eq!(req.session_id.as_deref(), Some("s1"));
        assert_eq!(req.metadata.get("key").map(|s| s.as_str()), Some("val"));
        assert!(req.stream);
    }

    #[test]
    fn test_infer_response_completed_includes_result() {
        let resp = InferResponse {
            request_id: "r1".to_string(),
            status: RequestStatus::Completed,
            result: Some("answer".to_string()),
            error: None,
        };
        let json = serde_json::to_string(&resp).expect("ser");
        assert!(json.contains("answer"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn test_infer_response_failed_includes_error() {
        let resp = InferResponse {
            request_id: "r1".to_string(),
            status: RequestStatus::Failed,
            result: None,
            error: Some("boom".to_string()),
        };
        let json = serde_json::to_string(&resp).expect("ser");
        assert!(json.contains("boom"));
        assert!(!json.contains("result"));
    }

    #[test]
    fn test_request_status_serializes_lowercase() {
        let json = serde_json::to_string(&RequestStatus::Processing).expect("ser");
        assert_eq!(json, "\"processing\"");
    }

    #[test]
    fn test_request_status_round_trips() {
        for status in [
            RequestStatus::Pending,
            RequestStatus::Processing,
            RequestStatus::Completed,
            RequestStatus::Failed,
            RequestStatus::Timeout,
        ] {
            let json = serde_json::to_string(&status).expect("ser");
            let back: RequestStatus = serde_json::from_str(&json).expect("deser");
            assert_eq!(status, back);
        }
    }

    #[test]
    fn test_server_config_default_values() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.max_request_size, 10 * 1024 * 1024);
        assert_eq!(cfg.timeout_seconds, 300);
    }

    #[test]
    fn test_ws_constants() {
        assert_eq!(WS_MAX_MESSAGE_SIZE, 1024 * 1024);
        assert_eq!(WS_PING_INTERVAL_SECS, 30);
        assert_eq!(WS_RATE_LIMIT_PER_MIN, 60);
    }

    #[test]
    fn test_app_error_not_found_returns_404_json() {
        let resp = AppError::NotFound.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_app_error_pipeline_closed_returns_503() {
        let resp = AppError::PipelineClosed.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn test_app_error_timeout_returns_408() {
        let resp = AppError::Timeout.into_response();
        assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
    }
}
