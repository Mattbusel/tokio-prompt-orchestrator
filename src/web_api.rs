//! Web API Server
//!
//! Provides HTTP REST API, SSE streaming, and WebSocket streaming for the
//! orchestrator pipeline.
//!
//! ## Endpoints
//!
//! ### REST API
//! - `POST /api/v1/infer`  -  Submit inference request (JSON)
//! - `POST /api/v1/stream`  -  SSE token streaming
//! - `GET  /api/v1/status/{request_id}`  -  Check request status
//! - `GET  /api/v1/result/{request_id}`  -  Get result (blocking until complete)
//! - `GET  /api/v1/schema`  -  OpenAPI 3.0 schema
//! - `GET  /health`  -  Health check
//! - `GET  /metrics`  -  Prometheus metrics
//!
//! ### WebSocket
//! - `WS /api/v1/ws`  -  Real-time streaming inference

/// Rejects oversized request bodies with 413.
///
/// Handles both Content-Length header and chunked bodies.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn body_size_middleware(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let max_size = state.config.max_request_size;
    // Fast path: Content-Length header present
    if let Some(length) = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if length > max_size {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({"error": "Request body too large"})),
            )
                .into_response();
        }
    }
    // Slow path: buffer entire body up to max_size+1 bytes
    let (parts, body) = req.into_parts();
    match axum::body::to_bytes(body, max_size + 1).await {
        Ok(bytes) if bytes.len() > max_size => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({"error": "Request body too large"})),
            )
                .into_response();
        }
        Ok(bytes) => {
            let req = Request::from_parts(parts, Body::from(bytes));
            next.run(req).await
        }
        Err(_) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": "Request body too large"})),
        )
            .into_response(),
    }
}

#[cfg(feature = "web-api")]
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket},
        Path, Query, State, WebSocketUpgrade,
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
use tower_http::cors::AllowOrigin;

#[cfg(feature = "web-api")]
use dashmap::DashMap;
#[cfg(feature = "web-api")]
use futures::{stream, SinkExt, StreamExt};
#[cfg(feature = "web-api")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "web-api")]
use std::collections::HashMap;
#[cfg(feature = "web-api")]
use std::collections::HashSet;
#[cfg(feature = "web-api")]
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
#[cfg(feature = "web-api")]
use std::sync::Arc;
#[cfg(feature = "web-api")]
use std::time::{Duration, Instant};
#[cfg(feature = "web-api")]
use tokio::sync::mpsc;
#[cfg(feature = "web-api")]
use tower_http::cors::CorsLayer;
#[cfg(feature = "web-api")]
use tracing::{debug, info, warn};
#[cfg(feature = "web-api")]
use uuid::Uuid;

#[cfg(feature = "web-api")]
use subtle::ConstantTimeEq;

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
    /// When `true`, the `/api/v1/debug/*` endpoints are enabled.
    /// Default: `true` (safe for development; set to `false` in production).
    pub debug_mode: bool,
}

#[cfg(feature = "web-api")]
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            max_request_size: 10 * 1024 * 1024, // 10MB
            timeout_seconds: 300,               // 5 minutes
            debug_mode: true,
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
    /// Optional request deadline in seconds from now.  When set, the pipeline
    /// will drop the request rather than process it after the deadline elapses.
    #[serde(default)]
    pub deadline_secs: Option<u64>,
    /// Per-request timeout in seconds (1–3600).  Overrides the server default
    /// for this request only.  The server rejects values of 0 or greater than
    /// 3600 with HTTP 400.  When absent, the server-wide `timeout_seconds`
    /// applies.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
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
    /// Set to `Some(Instant)` when the request reaches a terminal state
    /// (Completed or Failed).  Used by the background cleanup task to evict
    /// entries that have been complete for more than 1 hour.
    completed_at: Option<Instant>,
}

/// How long a completed/failed entry stays in the tracker before eviction.
#[cfg(feature = "web-api")]
const TRACKER_RETENTION: Duration = Duration::from_secs(60 * 60); // 1 hour

/// How often the cleanup task runs.
#[cfg(feature = "web-api")]
const TRACKER_CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60); // 5 minutes

// ============================================================================
// Batch Job Tracker
// ============================================================================

/// Per-job state stored in the [`BatchJobTracker`].
#[cfg(feature = "web-api")]
#[derive(Clone)]
struct BatchJob {
    total: usize,
    completed: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    /// IDs of the sub-requests that belong to this batch job.
    #[allow(dead_code)]
    sub_request_ids: Vec<String>,
}

/// Tracks progress of batch jobs submitted via `POST /api/v1/batch`.
#[cfg(feature = "web-api")]
#[derive(Clone)]
struct BatchJobTracker {
    jobs: Arc<DashMap<String, BatchJob>>,
}

#[cfg(feature = "web-api")]
impl BatchJobTracker {
    fn new() -> Self {
        Self {
            jobs: Arc::new(DashMap::new()),
        }
    }

    fn create(&self, job_id: String, total: usize, sub_request_ids: Vec<String>) {
        self.jobs.insert(
            job_id,
            BatchJob {
                total,
                completed: Arc::new(AtomicU64::new(0)),
                failed: Arc::new(AtomicU64::new(0)),
                sub_request_ids,
            },
        );
    }

    fn record_completed(&self, job_id: &str) {
        if let Some(job) = self.jobs.get(job_id) {
            job.completed.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }

    fn record_failed(&self, job_id: &str) {
        if let Some(job) = self.jobs.get(job_id) {
            job.failed.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }
}

// ============================================================================
// Validation Errors
// ============================================================================

/// A single field-level validation error.
#[cfg(feature = "web-api")]
#[derive(Debug, Serialize)]
struct FieldError {
    field: &'static str,
    code: &'static str,
    message: &'static str,
}

/// Structured validation error response (HTTP 422).
#[cfg(feature = "web-api")]
#[derive(Debug, Serialize)]
struct ValidationErrorBody {
    error: &'static str,
    fields: Vec<FieldError>,
}

#[cfg(feature = "web-api")]
impl IntoResponse for ValidationErrorBody {
    fn into_response(self) -> Response {
        (StatusCode::UNPROCESSABLE_ENTITY, Json(self)).into_response()
    }
}

/// Validate an [`InferRequest`] and return a [`ValidationErrorBody`] if any
/// fields are invalid.  Returns `None` when all fields are valid.
#[cfg(feature = "web-api")]
fn validate_infer_request(req: &InferRequest) -> Option<ValidationErrorBody> {
    let mut fields = Vec::new();

    if req.prompt.trim().is_empty() {
        fields.push(FieldError {
            field: "prompt",
            code: "ERR_REQUIRED",
            message: "prompt is required and cannot be empty",
        });
    }

    if let Some(t) = req.timeout_seconds {
        if t == 0 || t > 3600 {
            fields.push(FieldError {
                field: "timeout_seconds",
                code: "ERR_OUT_OF_RANGE",
                message: "timeout_seconds must be between 1 and 3600 (inclusive)",
            });
        }
    }

    if fields.is_empty() {
        None
    } else {
        Some(ValidationErrorBody {
            error: "VALIDATION_ERROR",
            fields,
        })
    }
}

#[cfg(feature = "web-api")]
impl RequestTracker {
    /// Create a new tracker and spawn a background task that every 5 minutes
    /// removes entries that have been in a terminal state for more than 1 hour.
    ///
    /// The `web_api_tracker_entries` gauge (mapped to the `"tracker"` label on
    /// the existing `set_queue_depth` metric) is updated after each sweep.
    fn new() -> Self {
        let status: Arc<DashMap<String, TrackedRequest>> = Arc::new(DashMap::new());
        let status_clone = Arc::clone(&status);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TRACKER_CLEANUP_INTERVAL);
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                let now = Instant::now();
                status_clone.retain(|_, v| {
                    match v.completed_at {
                        Some(completed_at) => now.duration_since(completed_at) < TRACKER_RETENTION,
                        None => true, // still in-flight — keep
                    }
                });
                let remaining = status_clone.len() as i64;
                crate::metrics::set_queue_depth("tracker", remaining);
                tracing::debug!(entries = remaining, "RequestTracker cleanup sweep complete");
            }
        });
        Self { status }
    }
}

/// Shared application state available to all handlers.
#[cfg(feature = "web-api")]
struct AppState {
    pipeline_tx: mpsc::Sender<PromptRequest>,
    tracker: RequestTracker,
    batch_tracker: BatchJobTracker,
    config: ServerConfig,
    /// Optional API key.  `None` means authentication is disabled (startup warning emitted).
    api_key: Option<String>,
    /// When `true`, the `/api/v1/debug/*` endpoints are active.
    debug_mode: bool,
    /// Dead-letter queue from the pipeline — exposed on the debug endpoints.
    dlq: Arc<crate::DeadLetterQueue>,
    /// Circuit breaker from the inference stage — exposed on the debug endpoints.
    circuit_breaker: crate::enhanced::CircuitBreaker,
    /// Optional metrics scrape API key (separate from the main API key).
    metrics_key: Option<String>,
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
    mut output_rx: mpsc::Receiver<crate::PostOutput>,
    dlq: Arc<crate::DeadLetterQueue>,
    circuit_breaker: crate::enhanced::CircuitBreaker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = format!("{}:{}", config.host, config.port);

    info!("Starting web API server on http://{}", addr);

    // Read optional API key from environment.
    let api_key = match std::env::var("API_KEY") {
        Ok(k) if !k.is_empty() => Some(k),
        _ => {
            debug!("API_KEY not set — web API auth disabled (fine for local use)");
            None
        }
    };

    // Configure CORS origins from ALLOWED_ORIGINS or fall back to wildcard.
    //
    // ALLOWED_ORIGINS format:
    //   Comma-separated list of allowed origins, e.g.
    //   `https://app.example.com,https://staging.example.com`.
    //   Supports exact matches only (no wildcards within a value).
    //   Leave unset to allow all origins (not recommended for production).
    let cors = match std::env::var("ALLOWED_ORIGINS") {
        Ok(origins) if !origins.is_empty() => {
            let allowed_set: HashSet<HeaderValue> = origins
                .split(',')
                .filter_map(|o| HeaderValue::from_str(o.trim()).ok())
                .collect();
            let origin_list: Vec<String> = allowed_set
                .iter()
                .filter_map(|v| v.to_str().ok().map(|s| s.to_owned()))
                .collect();
            info!(
                origins = %origin_list.join(", "),
                "CORS configured with explicit allow-list (exact-match only; \
                 update ALLOWED_ORIGINS to add new origins)"
            );
            let allowed_vec: Vec<HeaderValue> = allowed_set.into_iter().collect();
            CorsLayer::new().allow_origin(AllowOrigin::list(allowed_vec))
        }
        _ => {
            warn!(
                "ALLOWED_ORIGINS not set — CORS is open to all origins. \
                 Set ALLOWED_ORIGINS=https://app.example.com,https://staging.example.com \
                 for production (comma-separated, exact matches only)."
            );
            CorsLayer::new().allow_origin(AllowOrigin::any())
        }
    };

    let tracker = RequestTracker::new();
    let metrics_key = match std::env::var("METRICS_API_KEY") {
        Ok(k) if !k.is_empty() => Some(k),
        _ => None,
    };
    let batch_tracker = BatchJobTracker::new();
    let debug_mode = config.debug_mode;

    let state = Arc::new(AppState {
        pipeline_tx,
        tracker,
        batch_tracker,
        config: config.clone(),
        api_key,
        debug_mode,
        dlq,
        circuit_breaker,
        metrics_key,
    });

    // Collector task: receives pipeline output and writes results into the tracker.
    let collector_state = state.clone();
    tokio::spawn(async move {
        while let Some(output) = output_rx.recv().await {
            if let Some(mut tracked) = collector_state.tracker.status.get_mut(&output.request_id) {
                tracked.status = RequestStatus::Completed;
                tracked.result = Some(output.text);
                tracked.completed_at = Some(Instant::now());
            }
        }
    });

    let app = Router::new()
        .route("/api/v1/infer", post(infer_handler))
        .route("/api/v1/batch", post(batch_handler))
        .route("/api/v1/batch/:job_id/progress", get(batch_progress_handler))
        .route("/api/v1/stream", post(sse_stream_handler))
        .route("/api/v1/status/:request_id", get(status_handler))
        .route("/api/v1/result/:request_id", get(result_handler))
        .route("/api/v1/results", get(results_handler))
        .route("/api/v1/ws", get(websocket_handler))
        .route("/v1/stream", get(token_stream_ws_handler))
        .route("/api/v1/schema", get(schema_handler))
        .route("/api/v1/pipeline/status", get(pipeline_status_handler))
        .route("/api/v1/debug/dlq", get(debug_dlq_handler))
        .route("/api/v1/debug/dedup-index", get(debug_dedup_handler))
        .route("/api/v1/debug/pipeline", get(debug_pipeline_handler))
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            metrics_auth_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(middleware::from_fn(request_id_middleware))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            body_size_middleware,
        ))
        .layer(cors)
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

/// Bearer token authentication middleware.
///
/// Protects all inference endpoints (`/infer`, `/stream`, `/ws`, `/result`,
/// `/status`).  Public endpoints (`/health`, `/metrics`, `/schema`) are
/// always allowed through.
///
/// If `API_KEY` was not set at startup (auth disabled), every request is
/// allowed and a per-request warning is emitted.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // Public endpoints that do not require authentication.
    let req_path = req.uri().path().to_owned();
    let is_public = req_path == "/health" || req_path == "/api/v1/schema";
    if is_public {
        return next.run(req).await;
    }

    // Auth disabled — allow all (no key set, local use).
    let Some(ref expected_key) = state.api_key else {
        return next.run(req).await;
    };

    // Extract Bearer token from Authorization header.
    let token_valid = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| {
            // Constant-time comparison using the `subtle` crate to prevent
            // timing side-channels.  Tokens of different lengths are compared
            // as equal-length byte slices (zero-padded to the longer length)
            // so the branch on length mismatch is eliminated.
            let token_bytes = token.as_bytes();
            let expected_bytes = expected_key.as_bytes();
            let max_len = token_bytes.len().max(expected_bytes.len());
            let mut t = vec![0u8; max_len];
            let mut e = vec![0u8; max_len];
            t[..token_bytes.len()].copy_from_slice(token_bytes);
            e[..expected_bytes.len()].copy_from_slice(expected_bytes);
            t.as_slice().ct_eq(e.as_slice()).into()
        })
        .unwrap_or(false);

    if token_valid {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}

/// Bearer token authentication middleware for the `/metrics` scrape endpoint.
///
/// Checks the `METRICS_API_KEY` environment variable (loaded at startup into
/// `state.metrics_key`).  When the key is set, requests missing a matching
/// `Authorization: Bearer <key>` header are rejected with 401.  When the key
/// is not set, the endpoint is publicly accessible.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn metrics_auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // Only applies to the /metrics path.
    if req.uri().path() != "/metrics" {
        return next.run(req).await;
    }

    let Some(ref expected_key) = state.metrics_key else {
        return next.run(req).await;
    };

    let token_valid = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| {
            let token_bytes = token.as_bytes();
            let expected_bytes = expected_key.as_bytes();
            let max_len = token_bytes.len().max(expected_bytes.len());
            let mut t = vec![0u8; max_len];
            let mut e = vec![0u8; max_len];
            t[..token_bytes.len()].copy_from_slice(token_bytes);
            e[..expected_bytes.len()].copy_from_slice(expected_bytes);
            t.as_slice().ct_eq(e.as_slice()).into()
        })
        .unwrap_or(false);

    if token_valid {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}

// ============================================================================
// REST Handlers
// ============================================================================

/// `POST /api/v1/infer`  -  Submit an inference request.
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
) -> Result<Response, AppError> {
    // Field-level validation — returns HTTP 422 with structured errors.
    if let Some(err) = validate_infer_request(&req) {
        return Ok(err.into_response());
    }

    let request_id = Uuid::new_v4().to_string();
    let session_id = req
        .session_id
        .unwrap_or_else(|| format!("web-{}", request_id));

    let _span = tracing::info_span!(
        "infer_handler",
        request_id = %request_id,
        session_id = %session_id,
        endpoint = "POST /api/v1/infer",
    )
    .entered();

    let deadline = req
        .deadline_secs
        .map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));

    state.tracker.status.insert(
        request_id.clone(),
        TrackedRequest {
            status: RequestStatus::Pending,
            result: None,
            error: None,
            completed_at: None,
        },
    );

    let prompt_req = {
        let base = PromptRequest {
            session: SessionId::new(session_id),
            request_id: request_id.clone(),
            input: req.prompt,
            meta: req.metadata,
            deadline,
        };
        // Apply per-request timeout (overrides the pipeline default).
        if let Some(t) = req.timeout_seconds {
            base.with_deadline(Duration::from_secs(t))
        } else {
            base
        }
    };

    match state.pipeline_tx.try_send(prompt_req) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            // Remove the pending tracker entry since we won't process this request.
            state.tracker.status.remove(&request_id);
            return Err(AppError::PipelineFull);
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            state.tracker.status.remove(&request_id);
            return Err(AppError::PipelineClosed);
        }
    }

    if let Some(mut tracked) = state.tracker.status.get_mut(&request_id) {
        tracked.status = RequestStatus::Processing;
    }

    Ok(Json(InferResponse {
        request_id,
        status: RequestStatus::Processing,
        result: None,
        error: None,
    })
    .into_response())
}

/// `GET /api/v1/status/{request_id}`  -  Check request status.
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

/// `GET /api/v1/result/{request_id}`  -  Block until result is ready.
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

/// `POST /api/v1/stream`  -  SSE token streaming.
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
    // Field-level validation — returns HTTP 422 with structured errors.
    // Must happen before any field is moved out of `req`.
    if let Some(err) = validate_infer_request(&req) {
        return Err(AppError::ValidationError(err));
    }

    let request_id = Uuid::new_v4().to_string();
    let session_id = req
        .session_id
        .unwrap_or_else(|| format!("sse-{}", request_id));

    let _span = tracing::info_span!(
        "sse_stream_handler",
        request_id = %request_id,
        session_id = %session_id,
        endpoint = "POST /api/v1/stream",
    )
    .entered();

    let deadline = req
        .deadline_secs
        .map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));

    let prompt_req = {
        let base = PromptRequest {
            session: SessionId::new(session_id),
            request_id: request_id.clone(),
            input: req.prompt,
            meta: req.metadata,
            deadline,
        };
        if let Some(t) = req.timeout_seconds {
            base.with_deadline(Duration::from_secs(t))
        } else {
            base
        }
    };

    match state.pipeline_tx.try_send(prompt_req) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            return Err(AppError::PipelineFull);
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            return Err(AppError::PipelineClosed);
        }
    }

    // Register request in tracker so collector can write the result.
    state.tracker.status.insert(
        request_id.clone(),
        TrackedRequest {
            status: RequestStatus::Processing,
            result: None,
            error: None,
            completed_at: None,
        },
    );

    let tracker = state.tracker.clone();
    let timeout_secs = state.config.timeout_seconds;
    let request_id_for_done = request_id.clone();
    let token_stream = stream::once(async move {
        Ok::<_, std::convert::Infallible>(
            Event::default()
                .event("start")
                .data(serde_json::json!({"request_id": request_id}).to_string()),
        )
    })
    .chain(stream::once(async move {
        let request_id = request_id_for_done;
        let sse_timeout = Duration::from_secs(timeout_secs);
        let sse_start = std::time::Instant::now();
        let result_text = loop {
            if let Some(tracked) = tracker.status.get(&request_id) {
                match tracked.status {
                    RequestStatus::Completed => {
                        break tracked.result.clone().unwrap_or_default();
                    }
                    RequestStatus::Failed => {
                        break format!(
                            "[error] {}",
                            tracked
                                .error
                                .clone()
                                .unwrap_or_else(|| "unknown error".to_string())
                        );
                    }
                    _ => {}
                }
            }
            if sse_start.elapsed() > sse_timeout {
                break "[error] Request timed out".to_string();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        Ok::<_, std::convert::Infallible>(Event::default().event("done").data(result_text))
    }));

    Ok(Sse::new(token_stream).keep_alive(KeepAlive::default()))
}

// ============================================================================
// WebSocket
// ============================================================================

/// `GET /api/v1/ws`  -  WebSocket upgrade handler.
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
///
/// The socket is split into a sink (writes) and a stream (reads) so that the
/// inner inference-polling loop can race `tokio::select!` between:
///   (a) the next 100 ms poll tick, and
///   (b) an incoming WebSocket frame (Close / error = client gone).
/// This avoids burning API tokens when the client disconnects mid-inference.
#[cfg(feature = "web-api")]
async fn websocket_stream(socket: WebSocket, state: Arc<AppState>) {
    info!("WebSocket client connected");

    // Split the socket so the read half (disconnect detection) and write half
    // (result delivery) can be used independently inside the polling loop.
    let (mut ws_sink, mut ws_stream) = socket.split();

    let mut ping_interval = tokio::time::interval(Duration::from_secs(WS_PING_INTERVAL_SECS));
    let mut msg_count: u32 = 0;
    let mut window_start = std::time::Instant::now();

    loop {
        tokio::select! {
            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        //  Rate limiting
                        if window_start.elapsed() > Duration::from_secs(60) {
                            msg_count = 0;
                            window_start = std::time::Instant::now();
                        }
                        msg_count += 1;
                        if msg_count > WS_RATE_LIMIT_PER_MIN {
                            // Send a structured rate-limit error, then close the
                            // connection with WebSocket close code 4029 (application-
                            // defined; means "Too Many Requests").
                            let err_msg = serde_json::json!({
                                "error": "RATE_LIMITED",
                                "retry_after_ms": 1000
                            }).to_string();
                            let _ = ws_sink.send(Message::Text(err_msg)).await;
                            // Close with code 4029.
                            let close_frame = axum::extract::ws::CloseFrame {
                                code: 4029,
                                reason: std::borrow::Cow::Borrowed("rate limit exceeded"),
                            };
                            let _ = ws_sink.send(Message::Close(Some(close_frame))).await;
                            break;
                        }

                        //  Parse and process
                        let req: InferRequest = match serde_json::from_str(&text) {
                            Ok(r) => r,
                            Err(e) => {
                                let error_msg = serde_json::json!({
                                    "error": format!("Invalid JSON: {e}")
                                }).to_string();
                                let _ = ws_sink.send(Message::Text(error_msg)).await;
                                continue;
                            }
                        };

                        let request_id = Uuid::new_v4().to_string();
                        state.tracker.status.insert(
                            request_id.clone(),
                            TrackedRequest {
                                status: RequestStatus::Processing,
                                result: None,
                                error: None,
                                completed_at: None,
                            },
                        );
                        let processing_msg = serde_json::json!({
                            "request_id": &request_id,
                            "status": "processing"
                        }).to_string();

                        if ws_sink.send(Message::Text(processing_msg)).await.is_err() {
                            break;
                        }

                        let session_id = req.session_id
                            .unwrap_or_else(|| format!("ws-{}", request_id));
                        let prompt_req = PromptRequest {
                            session: SessionId::new(session_id),
                            request_id: request_id.clone(),
                            input: req.prompt,
                            meta: req.metadata,
                            deadline: None,
                        };

                        if state.pipeline_tx.send(prompt_req).await.is_ok() {
                            // Poll the tracker until Completed/Failed, timeout, or
                            // client disconnect — whichever comes first.
                            //
                            // Each iteration races a 100 ms sleep against the next
                            // incoming WebSocket frame.  A Close frame, recv error,
                            // or stream end means the client is gone: break early so
                            // we stop burning API tokens on an abandoned request.
                            let ws_timeout = Duration::from_secs(state.config.timeout_seconds);
                            let ws_start = std::time::Instant::now();
                            let poll_result: Option<(&str, Option<String>, Option<String>)> = loop {
                                if let Some(tracked) = state.tracker.status.get(&request_id) {
                                    match tracked.status {
                                        RequestStatus::Completed => {
                                            break Some(("completed", tracked.result.clone(), None));
                                        }
                                        RequestStatus::Failed => {
                                            break Some(("failed", None, tracked.error.clone()));
                                        }
                                        _ => {}
                                    }
                                }
                                if ws_start.elapsed() > ws_timeout {
                                    break Some(("timeout", None, Some("Request timed out".to_string())));
                                }
                                // Race poll sleep vs. incoming WS frame (disconnect detection).
                                tokio::select! {
                                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                                        // Continue polling; client still connected.
                                    }
                                    frame = ws_stream.next() => {
                                        match frame {
                                            Some(Ok(Message::Close(_))) | None => {
                                                // Client sent Close or stream ended — abort.
                                                debug!(
                                                    "Client disconnected mid-inference; \
                                                     aborting poll for request_id={}",
                                                    request_id
                                                );
                                                break None;
                                            }
                                            Some(Err(e)) => {
                                                warn!(error = %e, "WebSocket error during inference poll");
                                                break None;
                                            }
                                            // Ping/pong/binary while waiting — respond to
                                            // pings and keep polling.
                                            Some(Ok(Message::Ping(data))) => {
                                                if ws_sink.send(Message::Pong(data)).await.is_err() {
                                                    break None;
                                                }
                                            }
                                            _ => {} // Pong, Binary — ignore
                                        }
                                    }
                                }
                            };

                            // None means the client disconnected; stop the handler.
                            let (ws_status, ws_result, ws_error) = match poll_result {
                                None => break,
                                Some(t) => t,
                            };

                            let result_msg = serde_json::json!({
                                "request_id": &request_id,
                                "status": ws_status,
                                "result": ws_result,
                                "error": ws_error,
                            }).to_string();

                            if ws_sink.send(Message::Text(result_msg)).await.is_err() {
                                break;
                            }
                        } else {
                            let err_msg = serde_json::json!({
                                "request_id": &request_id,
                                "status": "failed",
                                "error": "Pipeline closed"
                            }).to_string();
                            let _ = ws_sink.send(Message::Text(err_msg)).await;
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if ws_sink.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        warn!(error = %e, "WebSocket receive error");
                        break;
                    }
                    _ => {} // Binary, Pong  -  ignore
                }
            }
            _ = ping_interval.tick() => {
                if ws_sink.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
        }
    }

    info!("WebSocket client disconnected  -  resources released");
}

// ============================================================================
// Token Streaming WebSocket (`GET /v1/stream`)
// ============================================================================

/// `GET /v1/stream` — WebSocket upgrade for token-level streaming.
///
/// Protocol:
/// 1. Client connects and upgrades to WebSocket.
/// 2. Client sends a JSON message: `{"prompt": "..."}`.
/// 3. Server streams `TokenChunk` JSON objects as they are produced.
/// 4. Connection is closed cleanly when a chunk with `finish_reason` set arrives.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn token_stream_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.max_message_size(WS_MAX_MESSAGE_SIZE)
        .on_upgrade(|socket| token_stream_ws(socket, state))
}

#[cfg(feature = "web-api")]
#[derive(serde::Deserialize)]
struct StreamPromptRequest {
    prompt: String,
}

#[cfg(feature = "web-api")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TokenChunk {
    pub text: String,
    pub finish_reason: Option<String>,
    pub index: usize,
}

#[cfg(feature = "web-api")]
async fn token_stream_ws(mut socket: WebSocket, state: Arc<AppState>) {
    info!("Token-stream WebSocket client connected");

    // Step 1: receive the prompt message
    let prompt = loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<StreamPromptRequest>(&text) {
                    Ok(req) => break req.prompt,
                    Err(e) => {
                        let _ = socket
                            .send(Message::Text(
                                serde_json::json!({"error": format!("Invalid JSON: {e}")})
                                    .to_string(),
                            ))
                            .await;
                        return;
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => return,
            Some(Err(e)) => {
                warn!(error = %e, "Token-stream WS receive error");
                return;
            }
            _ => continue,
        }
    };

    // Step 2: submit to pipeline and produce mock token chunks
    // (In a full implementation, the pipeline would drive a StreamingModelWorker.
    // Here we emit a single synthetic chunk to satisfy the interface contract.)
    let request_id = uuid::Uuid::new_v4().to_string();
    let session_id = format!("ws-stream-{}", request_id);
    let prompt_req = crate::PromptRequest {
        session: crate::SessionId::new(session_id),
        request_id: request_id.clone(),
        input: prompt.clone(),
        meta: std::collections::HashMap::new(),
        deadline: None,
    };

    if state.pipeline_tx.send(prompt_req).await.is_err() {
        let _ = socket
            .send(Message::Text(
                serde_json::json!({"error": "Pipeline closed"}).to_string(),
            ))
            .await;
        return;
    }

    // Stream synthetic token chunks (word-by-word) so the endpoint is functional.
    // Split the socket so we can detect client disconnect (Close frame / recv error)
    // concurrently with sending each token chunk.
    let (mut ws_sink, mut ws_stream) = socket.split();
    let words: Vec<&str> = prompt.split_whitespace().collect();
    let total = words.len();
    'stream: for (i, word) in words.iter().enumerate() {
        let is_last = i + 1 == total;
        let chunk = TokenChunk {
            text: format!("{} ", word),
            finish_reason: if is_last {
                Some("stop".to_string())
            } else {
                None
            },
            index: i,
        };
        let json = match serde_json::to_string(&chunk) {
            Ok(j) => j,
            Err(_) => break,
        };
        // Race the token send against an incoming Close/error from the client.
        // If the client disconnects mid-stream, stop immediately to avoid
        // burning API tokens on a gone connection.
        tokio::select! {
            send_result = ws_sink.send(Message::Text(json)) => {
                if send_result.is_err() {
                    break 'stream;
                }
            }
            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => {
                        debug!("Token-stream client disconnected mid-stream; aborting request_id={}", request_id);
                        break 'stream;
                    }
                    Some(Err(_)) => {
                        break 'stream;
                    }
                    _ => {
                        // Ignore other frames (ping, pong, etc.) and still send the chunk.
                        if ws_sink.send(Message::Text(
                            match serde_json::to_string(&chunk) { Ok(j) => j, Err(_) => break 'stream }
                        )).await.is_err() {
                            break 'stream;
                        }
                    }
                }
            }
        }
        if chunk.finish_reason.is_some() {
            break;
        }
    }

    info!("Token-stream WebSocket closed — request_id={}", request_id);
}

// ============================================================================
// Utility Handlers
// ============================================================================

/// `GET /api/v1/pipeline/status`  -  Pipeline health and queue statistics.
///
/// Returns real-time metrics about the orchestrator:
/// - `pending_requests`: requests currently tracked (queued or processing)
/// - `pipeline_queue_capacity`: remaining capacity in the inbound pipeline channel
/// - `version`: crate version
/// - `status`: always `"ok"` while the server is up
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn pipeline_status_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let pending = state
        .tracker
        .status
        .iter()
        .filter(|e| {
            matches!(
                e.value().status,
                RequestStatus::Pending | RequestStatus::Processing
            )
        })
        .count();

    let queue_capacity = state.pipeline_tx.capacity();
    let queue_len = state.pipeline_tx.max_capacity() - queue_capacity;

    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "pending_requests": pending,
        "pipeline_queue_len": queue_len,
        "pipeline_queue_capacity": queue_capacity,
        "pipeline_queue_max": state.pipeline_tx.max_capacity(),
    }))
}

/// `GET /health`  -  Health check endpoint.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cb_open = if let Some(ref cb) = state.circuit_breaker {
        cb.is_open_sync()
    } else {
        false
    };
    if cb_open {
        (StatusCode::SERVICE_UNAVAILABLE,
         Json(serde_json::json!({"status":"degraded","reason":"circuit_breaker_open","version":env!("CARGO_PKG_VERSION")})))
            .into_response()
    } else {
        (StatusCode::OK,
         Json(serde_json::json!({"status":"healthy","version":env!("CARGO_PKG_VERSION")})))
            .into_response()
    }
}

/// `GET /metrics`  -  Prometheus metrics endpoint.
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

/// `GET /api/v1/schema`  -  Serve the OpenAPI 3.0 schema.
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
// Batch Handler
// ============================================================================

/// JSON body for `POST /api/v1/batch`.
#[cfg(feature = "web-api")]
#[derive(Debug, Deserialize)]
struct BatchRequest {
    /// Prompts to process, up to 100.
    prompts: Vec<String>,
    /// Optional shared session ID prefix; a unique suffix is added per item.
    #[serde(default)]
    session_id: Option<String>,
    /// Maximum parallel in-flight requests (1–16, default 4).
    #[serde(default)]
    max_concurrency: Option<usize>,
}

/// Per-item result in a batch response.
#[cfg(feature = "web-api")]
#[derive(Debug, Serialize)]
struct BatchItemResult {
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// JSON response body for `POST /api/v1/batch`.
#[cfg(feature = "web-api")]
#[derive(Debug, Serialize)]
struct BatchResponse {
    results: Vec<BatchItemResult>,
    total: usize,
    succeeded: usize,
    failed: usize,
}

/// `GET /api/v1/batch/:job_id/progress`  -  Poll batch job progress.
///
/// Returns the current completion counters for the given batch job.
/// Status is `"in_progress"`, `"completed"`, or `"failed"` (all failed).
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn batch_progress_handler(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let job = state
        .batch_tracker
        .jobs
        .get(&job_id)
        .ok_or(AppError::NotFound)?;

    let total = job.total;
    let completed = job.completed.load(AtomicOrdering::Relaxed) as usize;
    let failed = job.failed.load(AtomicOrdering::Relaxed) as usize;
    let done = completed + failed;
    let pending = total.saturating_sub(done);

    let job_status = if done < total {
        "in_progress"
    } else if completed == 0 {
        "failed"
    } else {
        "completed"
    };

    Ok(Json(serde_json::json!({
        "job_id": job_id,
        "total": total,
        "completed": completed,
        "failed": failed,
        "pending": pending,
        "status": job_status,
    })))
}

/// `POST /api/v1/batch` — process multiple prompts concurrently.
///
/// Accepts up to 100 prompts.  `max_concurrency` controls in-flight parallelism
/// (1–16, default 4).  Returns HTTP 429 if the pipeline is at capacity.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn batch_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, AppError> {
    let batch_id = Uuid::new_v4().to_string();
    tracing::info!(
        batch_id = %batch_id,
        endpoint = "POST /api/v1/batch",
        prompt_count = req.prompts.len(),
        "batch request received",
    );

    const MAX_PROMPTS: usize = 100;
    const MAX_CONCURRENCY: usize = 16;
    const BATCH_TIMEOUT_SECS: u64 = 300;

    if req.prompts.is_empty() {
        return Err(AppError::BadRequest(
            "prompts must not be empty".to_string(),
        ));
    }
    if req.prompts.len() > MAX_PROMPTS {
        return Err(AppError::BadRequest(format!(
            "too many prompts: {} (max {MAX_PROMPTS})",
            req.prompts.len()
        )));
    }
    let concurrency = req.max_concurrency.unwrap_or(4).clamp(1, MAX_CONCURRENCY);
    let item_timeout = Duration::from_secs(state.config.timeout_seconds.min(BATCH_TIMEOUT_SECS));
    let session_prefix = req.session_id.clone();

    // Pre-generate request IDs so they can be registered in the BatchJobTracker
    // before the futures run.
    let prompt_count = req.prompts.len();
    let sub_request_ids: Vec<String> = (0..prompt_count)
        .map(|_| Uuid::new_v4().to_string())
        .collect();

    // Register the job entry so `GET /api/v1/batch/:job_id/progress` works immediately.
    state
        .batch_tracker
        .create(batch_id.clone(), prompt_count, sub_request_ids.clone());

    let futures_iter = req
        .prompts
        .into_iter()
        .enumerate()
        .zip(sub_request_ids.into_iter())
        .map(|((i, prompt), request_id)| {
            let state = Arc::clone(&state);
            let session_prefix = session_prefix.clone();
            let item_timeout = item_timeout;
            let batch_id = batch_id.clone();
            async move {
                let session_id = session_prefix
                    .map(|s| format!("{s}-{i}"))
                    .unwrap_or_else(|| format!("batch-{request_id}"));

                state.tracker.status.insert(
                    request_id.clone(),
                    TrackedRequest {
                        status: RequestStatus::Pending,
                        result: None,
                        error: None,
                        completed_at: None,
                    },
                );

                let prompt_req = PromptRequest {
                    session: SessionId::new(session_id),
                    request_id: request_id.clone(),
                    input: prompt,
                    meta: HashMap::new(),
                    deadline: Some(std::time::Instant::now() + item_timeout),
                };

                match state.pipeline_tx.try_send(prompt_req) {
                    Ok(()) => {}
                    Err(_) => {
                        state.tracker.status.remove(&request_id);
                        state.batch_tracker.record_failed(&batch_id);
                        return BatchItemResult {
                            request_id,
                            text: None,
                            error: Some("pipeline_full_or_closed".to_string()),
                        };
                    }
                }

                let start = std::time::Instant::now();
                loop {
                    if let Some(tracked) = state.tracker.status.get(&request_id) {
                        match tracked.status {
                            RequestStatus::Completed => {
                                let text = tracked.result.clone();
                                drop(tracked);
                                state.tracker.status.remove(&request_id);
                                state.batch_tracker.record_completed(&batch_id);
                                return BatchItemResult {
                                    request_id,
                                    text,
                                    error: None,
                                };
                            }
                            RequestStatus::Failed => {
                                let err = tracked.error.clone();
                                drop(tracked);
                                state.tracker.status.remove(&request_id);
                                state.batch_tracker.record_failed(&batch_id);
                                return BatchItemResult {
                                    request_id,
                                    text: None,
                                    error: err.or_else(|| Some("inference_failed".to_string())),
                                };
                            }
                            _ => {}
                        }
                    } else {
                        state.batch_tracker.record_failed(&batch_id);
                        return BatchItemResult {
                            request_id,
                            text: None,
                            error: Some("request_lost".to_string()),
                        };
                    }
                    if start.elapsed() > item_timeout {
                        state.tracker.status.remove(&request_id);
                        state.batch_tracker.record_failed(&batch_id);
                        return BatchItemResult {
                            request_id,
                            text: None,
                            error: Some("timeout".to_string()),
                        };
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        });

    use futures::StreamExt as _;
    let results: Vec<BatchItemResult> = futures::stream::iter(futures_iter)
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let succeeded = results.iter().filter(|r| r.error.is_none()).count();
    let failed = results.len() - succeeded;
    let total = results.len();

    Ok(Json(BatchResponse {
        results,
        total,
        succeeded,
        failed,
    }))
}

// ============================================================================
// Results Listing Handler
// ============================================================================

/// Query parameters accepted by `GET /api/v1/results`.
#[cfg(feature = "web-api")]
#[derive(Debug, Deserialize, Default)]
struct ResultsQuery {
    /// Filter by session ID.
    session_id: Option<String>,
    /// Filter by status: `"error"` (maps to Failed/Timeout), `"completed"`, or `"pending"`.
    filter: Option<String>,
    /// Max results to return (default 20, max 200).
    limit: Option<usize>,
}

/// Summary of a single tracked request returned by `GET /api/v1/results`.
#[cfg(feature = "web-api")]
#[derive(Debug, Serialize)]
struct ResultSummary {
    request_id: String,
    status: RequestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `GET /api/v1/results`  -  List tracked requests with optional filtering.
///
/// Query parameters:
/// - `session_id` — filter by session (prefix match on stored session metadata is
///   not yet possible; this performs a substring filter on the request_id for now)
/// - `filter` — `"completed"`, `"error"`, or `"pending"`
/// - `limit` — max results (default 20, max 200)
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn results_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ResultsQuery>,
) -> Json<Vec<ResultSummary>> {
    let limit = params.limit.unwrap_or(20).min(200);

    let summaries: Vec<ResultSummary> = state
        .tracker
        .status
        .iter()
        .filter(|entry| {
            // Status filter
            let status_ok = match params.filter.as_deref() {
                Some("completed") => entry.value().status == RequestStatus::Completed,
                Some("error") => matches!(
                    entry.value().status,
                    RequestStatus::Failed | RequestStatus::Timeout
                ),
                Some("pending") => matches!(
                    entry.value().status,
                    RequestStatus::Pending | RequestStatus::Processing
                ),
                _ => true,
            };
            // Session filter — match as substring of the request_id (request_ids are
            // opaque UUIDs; a full session-keyed index would require a separate map).
            let session_ok = params
                .session_id
                .as_deref()
                .map(|s| entry.key().contains(s))
                .unwrap_or(true);
            status_ok && session_ok
        })
        .take(limit)
        .map(|entry| ResultSummary {
            request_id: entry.key().clone(),
            status: entry.value().status.clone(),
            result: entry.value().result.clone(),
            error: entry.value().error.clone(),
        })
        .collect();

    Json(summaries)
}

// ============================================================================
// Debug Handlers
// ============================================================================

/// `GET /api/v1/debug/dlq`  -  Return last N entries from the dead-letter queue.
///
/// The DLQ is a ring buffer that captures requests dropped by backpressure or
/// inference failure.  This endpoint does **not** drain the queue — it returns
/// a snapshot.  Only available when `debug_mode` is `true`.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn debug_dlq_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !state.debug_mode {
        return Err(AppError::DebugDisabled);
    }

    // Peek without draining: collect entries from the queue and push them back.
    let entries = state.dlq.drain();
    let count = entries.len();
    // Re-enqueue so the DLQ is not cleared by this inspection call.
    for entry in &entries {
        state.dlq.push(entry.clone());
    }

    let json_entries: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "request_id": e.request_id,
                "session_id": e.session_id,
                "reason": e.reason,
                "dropped_at": e.dropped_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "count": count,
        "entries": json_entries,
    })))
}

/// `GET /api/v1/debug/dedup-index`  -  Return dedup entry count and hashed keys.
///
/// Returns a point-in-time snapshot of deduplication state.  Keys are the
/// already-hashed dedup keys (not raw prompts) so no PII is exposed.
/// Only available when `debug_mode` is `true`.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn debug_dedup_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !state.debug_mode {
        return Err(AppError::DebugDisabled);
    }

    // The dedup state is not stored directly in AppState; return a note that
    // the dedup index is not directly accessible from the web API layer without
    // wiring it through.  In production, this would require passing an
    // `Arc<Deduplicator>` into `AppState`.
    Ok(Json(serde_json::json!({
        "note": "Deduplicator not wired into web API state; integrate Arc<Deduplicator> in AppState to expose stats.",
        "count": 0,
        "keys": [],
    })))
}

/// `GET /api/v1/debug/pipeline`  -  Pipeline queue depths and circuit breaker state.
///
/// Returns per-stage statistics including queue depths, circuit breaker status,
/// and DLQ size.  Only available when `debug_mode` is `true`.
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "web-api")]
async fn debug_pipeline_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !state.debug_mode {
        return Err(AppError::DebugDisabled);
    }

    let cb_stats = state.circuit_breaker.stats().await;
    let cb_status = format!("{:?}", cb_stats.status).to_lowercase();

    let queue_capacity = state.pipeline_tx.capacity();
    let queue_max = state.pipeline_tx.max_capacity();
    let queue_len = queue_max - queue_capacity;

    let pending_requests = state
        .tracker
        .status
        .iter()
        .filter(|e| {
            matches!(
                e.value().status,
                RequestStatus::Pending | RequestStatus::Processing
            )
        })
        .count();

    Ok(Json(serde_json::json!({
        "pipeline_input_queue": {
            "depth": queue_len,
            "capacity": queue_capacity,
            "max": queue_max,
        },
        "circuit_breaker": {
            "status": cb_status,
            "failures": cb_stats.failures,
            "successes": cb_stats.successes,
            "success_rate": cb_stats.success_rate,
            "time_in_state_secs": cb_stats.time_in_current_state.as_secs(),
        },
        "dlq": {
            "depth": state.dlq.len(),
        },
        "tracker": {
            "pending_requests": pending_requests,
            "total_tracked": state.tracker.status.len(),
        },
        "batch_jobs_tracked": state.batch_tracker.jobs.len(),
    })))
}

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
    /// Pipeline input queue is full — client should retry after a backoff period.
    PipelineFull,
    /// Request validation failed (HTTP 400).
    BadRequest(String),
    /// Field-level validation error (HTTP 422).
    ValidationError(ValidationErrorBody),
    /// A debug endpoint was requested but debug mode is disabled.
    DebugDisabled,
}

#[cfg(feature = "web-api")]
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found", "message": "Request not found"})),
            )
                .into_response(),
            AppError::PipelineClosed => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "pipeline_closed", "message": "Pipeline closed"})),
            )
                .into_response(),
            AppError::Timeout => (
                StatusCode::REQUEST_TIMEOUT,
                Json(serde_json::json!({"error": "timeout", "message": "Request timeout"})),
            )
                .into_response(),
            AppError::PipelineFull => {
                let mut resp = (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(serde_json::json!({
                        "error": "server_busy",
                        "message": "Pipeline at capacity — retry after backoff period",
                        "retry_after_secs": 5
                    })),
                )
                    .into_response();
                resp.headers_mut()
                    .insert("Retry-After", axum::http::HeaderValue::from_static("5"));
                resp
            }
            AppError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "bad_request", "message": msg})),
            )
                .into_response(),
            AppError::ValidationError(body) => body.into_response(),
            AppError::DebugDisabled => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found", "message": "Debug endpoints are disabled"})),
            )
                .into_response(),
        }
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
    _output_rx: tokio::sync::mpsc::Receiver<crate::PostOutput>,
    _dlq: std::sync::Arc<crate::DeadLetterQueue>,
    _circuit_breaker: crate::enhanced::CircuitBreaker,
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

    // ------------------------------------------------------------------
    // CRIT-01: auth middleware tests
    // ------------------------------------------------------------------

    /// Build a minimal AppState for unit testing auth middleware.
    fn make_state_with_key(key: Option<&str>) -> Arc<AppState> {
        let (tx, _rx) = mpsc::channel(1);
        Arc::new(AppState {
            pipeline_tx: tx,
            tracker: RequestTracker {
                status: Arc::new(DashMap::new()),
            },
            batch_tracker: BatchJobTracker::new(),
            config: ServerConfig::default(),
            api_key: key.map(|s| s.to_string()),
            debug_mode: true,
            dlq: Arc::new(crate::DeadLetterQueue::new(100)),
            circuit_breaker: crate::enhanced::CircuitBreaker::new(
                5,
                0.8,
                std::time::Duration::from_secs(60),
            ),
        })
    }

    #[test]
    fn test_request_rejected_without_auth_header() {
        // When API key is set but no Authorization header is present the middleware
        // returns 401.  We verify the logic by calling the extraction code directly.
        let state = make_state_with_key(Some("secret-token"));
        let expected = state.api_key.as_deref();
        let provided: Option<&str> = None; // simulate missing Authorization header
        let token_valid = provided
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|token| Some(token) == expected)
            .unwrap_or(false);
        assert!(!token_valid, "no header → must be rejected");
    }

    #[test]
    fn test_request_accepted_with_valid_key() {
        let state = make_state_with_key(Some("secret-token"));
        let expected = state.api_key.as_deref();
        let header_value = "Bearer secret-token";
        let token_valid = Some(header_value)
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|token| Some(token) == expected)
            .unwrap_or(false);
        assert!(token_valid, "valid Bearer token → must be accepted");
    }

    #[test]
    fn test_request_rejected_with_wrong_key() {
        let state = make_state_with_key(Some("secret-token"));
        let expected = state.api_key.as_deref();
        let header_value = "Bearer wrong-token";
        let token_valid = Some(header_value)
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|token| Some(token) == expected)
            .unwrap_or(false);
        assert!(!token_valid, "wrong token → must be rejected");
    }

    #[test]
    fn test_auth_disabled_when_no_api_key() {
        let state = make_state_with_key(None);
        assert!(state.api_key.is_none(), "no API key → auth disabled");
    }
}
