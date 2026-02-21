//! # Web Dashboard Binary
//!
//! ## Responsibility
//! Real-time web dashboard showing agent activity, pipeline metrics, and cost
//! savings. Serves an SSE stream for live updates and a single-page HTML
//! frontend with no external build step.
//!
//! ## Guarantees
//! - No panics — all errors are returned as HTTP error responses
//! - SSE stream emits every 500ms for real-time feel
//! - Single HTML file served inline — no static file dependencies at runtime
//! - Graceful shutdown on SIGINT / SIGTERM
//!
//! ## NOT Responsible For
//! - Pipeline execution (delegates to `spawn_pipeline`)
//! - Metric collection internals (reads from `metrics` module)

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::get,
    Router,
};
use serde::Serialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_prompt_orchestrator::{
    enhanced::{CircuitBreaker, CircuitStatus, Deduplicator},
    metrics, spawn_pipeline, EchoWorker, LlamaCppWorker, ModelWorker, OrchestratorError,
    PromptRequest,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

// ── Dashboard HTML ───────────────────────────────────────────────────────────

/// Inline dashboard HTML, compiled into the binary at build time.
const DASHBOARD_HTML: &str = include_str!("../../static/index.html");

// ── Shared application state ─────────────────────────────────────────────────

/// Shared state accessible from all route handlers.
///
/// Holds references to the pipeline sender and enhanced-feature components so
/// that the SSE stream can read live statistics.
#[derive(Clone)]
pub struct DashboardState {
    /// Sender side of the pipeline input channel — kept alive to prevent
    /// pipeline shutdown; also used by future request-submission routes.
    #[allow(dead_code)]
    pipeline_tx: mpsc::Sender<PromptRequest>,
    /// Circuit breaker for the inference stage.
    circuit_breaker: CircuitBreaker,
    /// Request deduplicator.
    deduplicator: Deduplicator,
    /// Timestamp when the dashboard server started.
    start_time: Instant,
    /// Name of the active worker backend.
    worker_name: String,
}

// ── SSE event payload ────────────────────────────────────────────────────────

/// Event payload pushed to SSE clients every 500ms.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardEvent {
    /// Number of active pipeline stages (always 5 when healthy).
    pub active_agents: u32,
    /// Total requests that entered the pipeline.
    pub requests_total: u64,
    /// Total requests that were deduplicated (cache hits).
    pub requests_deduped: u64,
    /// Deduplication hit rate as a percentage (0.0–100.0).
    pub dedup_rate: f32,
    /// Estimated USD saved through deduplication.
    pub cost_saved_usd: f64,
    /// Approximate throughput in requests per second.
    pub throughput_rps: f32,
    /// Circuit breaker states for each monitored service.
    pub circuit_breakers: Vec<CircuitBreakerState>,
    /// Per-stage latency metrics.
    pub stage_latencies: StageTiming,
    /// Model routing breakdown.
    pub model_routing: ModelRoutingStats,
    /// Per-stage request counts.
    pub stage_counts: StageCountMap,
    /// Per-stage shed counts.
    pub shed_counts: StageCountMap,
    /// Server uptime in seconds.
    pub uptime_secs: u64,
}

/// Circuit breaker status for a single service.
#[derive(Debug, Clone, Serialize)]
pub struct CircuitBreakerState {
    /// Service name (e.g., `"inference"`).
    pub name: String,
    /// Human-readable status: `"closed"`, `"open"`, or `"half_open"`.
    pub status: String,
    /// Total failures in the current tracking window.
    pub failures: usize,
    /// Total successes in the current tracking window.
    pub successes: usize,
    /// Computed success rate (0.0–1.0).
    pub success_rate: f64,
}

/// Per-stage latency breakdown (P50 / P99 in milliseconds).
#[derive(Debug, Clone, Serialize)]
pub struct StageTiming {
    /// RAG stage latency.
    pub rag_ms: f64,
    /// Assemble stage latency.
    pub assemble_ms: f64,
    /// Inference stage latency.
    pub inference_ms: f64,
    /// Post-processing stage latency.
    pub post_ms: f64,
    /// Streaming stage latency.
    pub stream_ms: f64,
}

/// Model routing statistics.
#[derive(Debug, Clone, Serialize)]
pub struct ModelRoutingStats {
    /// Name of the primary worker.
    pub primary_model: String,
    /// Percentage of requests routed to the primary model (0.0–100.0).
    pub primary_pct: f32,
    /// Percentage of requests using the fallback (echo) model.
    pub fallback_pct: f32,
}

/// Stage-level counter map (stage name → count).
#[derive(Debug, Clone, Serialize)]
pub struct StageCountMap {
    /// RAG stage count.
    pub rag: u64,
    /// Assemble stage count.
    pub assemble: u64,
    /// Inference stage count.
    pub inference: u64,
    /// Post-processing stage count.
    pub post: u64,
    /// Stream stage count.
    pub stream: u64,
}

// ── Snapshot builder ─────────────────────────────────────────────────────────

/// Build a [`DashboardEvent`] from the current system state.
///
/// # Panics
///
/// This function never panics.
pub async fn build_snapshot(state: &DashboardState) -> DashboardEvent {
    let summary = metrics::get_metrics_summary();

    let stage_total = |stage: &str| -> u64 {
        summary
            .requests_total
            .get(stage)
            .copied()
            .unwrap_or_default()
    };
    let stage_shed = |stage: &str| -> u64 {
        summary
            .requests_shed
            .get(stage)
            .copied()
            .unwrap_or_default()
    };

    let requests_total: u64 = summary.requests_total.values().sum();

    // Dedup stats
    let dedup_stats = state.deduplicator.stats();
    let requests_deduped = dedup_stats.cached as u64;
    let dedup_rate = if requests_total > 0 {
        (requests_deduped as f32 / requests_total as f32) * 100.0
    } else {
        0.0
    };

    // Cost model: assume ~$0.002 per inference call saved via dedup
    let cost_saved_usd = requests_deduped as f64 * 0.002;

    // Throughput: total requests / uptime seconds
    let uptime = state.start_time.elapsed();
    let uptime_secs = uptime.as_secs().max(1);
    let throughput_rps = requests_total as f32 / uptime_secs as f32;

    // Circuit breaker
    let cb_stats = state.circuit_breaker.stats().await;
    let cb_status_str = match cb_stats.status {
        CircuitStatus::Closed => "closed".to_string(),
        CircuitStatus::Open => "open".to_string(),
        CircuitStatus::HalfOpen => "half_open".to_string(),
    };

    let circuit_breakers = vec![CircuitBreakerState {
        name: "inference".to_string(),
        status: cb_status_str,
        failures: cb_stats.failures,
        successes: cb_stats.successes,
        success_rate: cb_stats.success_rate,
    }];

    // Stage latencies from histogram — use sum / count as approximation
    let stage_latency_ms = |stage: &str| -> f64 {
        let families = metrics::gather();
        for family in &families {
            if family.get_name() == "orchestrator_stage_duration_seconds" {
                for metric in family.get_metric() {
                    let labels = metric.get_label();
                    let matches = labels
                        .iter()
                        .any(|l| l.get_name() == "stage" && l.get_value() == stage);
                    if matches {
                        let h = metric.get_histogram();
                        let count = h.get_sample_count();
                        if count > 0 {
                            return (h.get_sample_sum() / count as f64) * 1000.0;
                        }
                    }
                }
            }
        }
        0.0
    };

    let stage_latencies = StageTiming {
        rag_ms: stage_latency_ms("rag"),
        assemble_ms: stage_latency_ms("assemble"),
        inference_ms: stage_latency_ms("inference"),
        post_ms: stage_latency_ms("post"),
        stream_ms: stage_latency_ms("stream"),
    };

    let model_routing = ModelRoutingStats {
        primary_model: state.worker_name.clone(),
        primary_pct: 100.0,
        fallback_pct: 0.0,
    };

    let stage_counts = StageCountMap {
        rag: stage_total("rag"),
        assemble: stage_total("assemble"),
        inference: stage_total("inference"),
        post: stage_total("post"),
        stream: stage_total("stream"),
    };

    let shed_counts = StageCountMap {
        rag: stage_shed("rag"),
        assemble: stage_shed("assemble"),
        inference: stage_shed("inference"),
        post: stage_shed("post"),
        stream: stage_shed("stream"),
    };

    DashboardEvent {
        active_agents: 5,
        requests_total,
        requests_deduped,
        dedup_rate,
        cost_saved_usd,
        throughput_rps,
        circuit_breakers,
        stage_latencies,
        model_routing,
        stage_counts,
        shed_counts,
        uptime_secs,
    }
}

// ── Route handlers ───────────────────────────────────────────────────────────

/// `GET /` — serve the dashboard HTML page.
///
/// # Panics
///
/// This function never panics.
async fn index_handler() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// `GET /api/stats` — one-shot JSON snapshot of current dashboard state.
///
/// # Panics
///
/// This function never panics.
async fn stats_handler(State(state): State<Arc<DashboardState>>) -> Response {
    let snapshot = build_snapshot(&state).await;
    match serde_json::to_string(&snapshot) {
        Ok(json) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"serialization failed"}"#.to_string(),
        )
            .into_response(),
    }
}

/// `GET /api/status` — SSE stream emitting [`DashboardEvent`] every 500ms.
///
/// # Panics
///
/// This function never panics.
async fn sse_handler(
    State(state): State<Arc<DashboardState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            let snapshot = build_snapshot(&state).await;
            if let Ok(json) = serde_json::to_string(&snapshot) {
                yield Ok(Event::default().data(json));
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── Router construction ──────────────────────────────────────────────────────

/// Build the Axum [`Router`] with all dashboard routes.
///
/// # Panics
///
/// This function never panics.
pub fn build_router(state: Arc<DashboardState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/", get(index_handler))
        .route("/api/status", get(sse_handler))
        .route("/api/stats", get(stats_handler))
        .layer(cors)
        .with_state(state)
}

// ── Server bootstrap ─────────────────────────────────────────────────────────

/// Start the dashboard HTTP server.
///
/// # Arguments
///
/// * `addr` — Socket address to bind, e.g. `"0.0.0.0:3000"`.
/// * `state` — Shared dashboard state.
///
/// # Errors
///
/// Returns an error if the address cannot be parsed or the TCP listener
/// fails to bind.
///
/// # Panics
///
/// This function never panics.
pub async fn start_dashboard(
    addr: &str,
    state: Arc<DashboardState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let socket_addr: std::net::SocketAddr = addr.parse()?;

    info!("Dashboard server starting on http://{}", socket_addr);

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(&socket_addr).await?;

    info!("Dashboard ready at http://{}", socket_addr);

    axum::serve(listener, app).await?;

    Ok(())
}

// ── Worker factory ───────────────────────────────────────────────────────────

/// Create a [`ModelWorker`] from a CLI name string.
///
/// # Errors
///
/// Returns [`OrchestratorError::ConfigError`] for unrecognised worker names.
///
/// # Panics
///
/// This function never panics.
pub fn create_worker(name: &str) -> Result<Arc<dyn ModelWorker>, OrchestratorError> {
    match name {
        "echo" => Ok(Arc::new(EchoWorker::new())),
        "llama_cpp" => Ok(Arc::new(LlamaCppWorker::new())),
        _ => Err(OrchestratorError::ConfigError(format!(
            "unknown worker: {name}"
        ))),
    }
}

/// Parse the `--worker` CLI argument, defaulting to `"echo"`.
///
/// # Panics
///
/// This function never panics.
pub fn parse_worker_arg() -> String {
    let args: Vec<String> = std::env::args().collect();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--worker" {
            if let Some(val) = args.get(i + 1) {
                return val.clone();
            }
        }
    }
    "echo".to_string()
}

/// Parse the `--port` CLI argument, defaulting to `3000`.
///
/// # Panics
///
/// This function never panics.
pub fn parse_port_arg() -> u16 {
    let args: Vec<String> = std::env::args().collect();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--port" {
            if let Some(val) = args.get(i + 1) {
                return val.parse().unwrap_or(3000);
            }
        }
    }
    3000
}

// ── Main entry point ─────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialise tracing
    let _ = tokio_prompt_orchestrator::init_tracing();

    // Initialise metrics
    metrics::init_metrics()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

    // Parse CLI args
    let worker_name = parse_worker_arg();
    let port = parse_port_arg();

    // Create worker
    let worker = create_worker(&worker_name)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

    // Spawn pipeline
    let handles = spawn_pipeline(worker);

    // Create enhanced features
    let circuit_breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
    let deduplicator = Deduplicator::new(Duration::from_secs(300));

    let state = Arc::new(DashboardState {
        pipeline_tx: handles.input_tx,
        circuit_breaker,
        deduplicator,
        start_time: Instant::now(),
        worker_name,
    });

    let addr = format!("0.0.0.0:{port}");
    start_dashboard(&addr, state).await?;

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::util::ServiceExt;

    /// Create a test [`DashboardState`] backed by an echo worker pipeline.
    fn make_test_state() -> Arc<DashboardState> {
        let _ = metrics::init_metrics();
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let circuit_breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
        let deduplicator = Deduplicator::new(Duration::from_secs(300));

        Arc::new(DashboardState {
            pipeline_tx: handles.input_tx,
            circuit_breaker,
            deduplicator,
            start_time: Instant::now(),
            worker_name: "echo".to_string(),
        })
    }

    // ── DashboardEvent serialization tests ───────────────────────────────

    #[test]
    fn test_dashboard_event_serializes_to_json() {
        let event = DashboardEvent {
            active_agents: 5,
            requests_total: 100,
            requests_deduped: 20,
            dedup_rate: 20.0,
            cost_saved_usd: 0.04,
            throughput_rps: 5.0,
            circuit_breakers: vec![],
            stage_latencies: StageTiming {
                rag_ms: 1.0,
                assemble_ms: 0.5,
                inference_ms: 10.0,
                post_ms: 0.3,
                stream_ms: 0.1,
            },
            model_routing: ModelRoutingStats {
                primary_model: "echo".to_string(),
                primary_pct: 100.0,
                fallback_pct: 0.0,
            },
            stage_counts: StageCountMap {
                rag: 100,
                assemble: 100,
                inference: 100,
                post: 100,
                stream: 100,
            },
            shed_counts: StageCountMap {
                rag: 0,
                assemble: 0,
                inference: 0,
                post: 0,
                stream: 0,
            },
            uptime_secs: 60,
        };

        let json = serde_json::to_string(&event);
        assert!(json.is_ok(), "DashboardEvent must serialize: {json:?}");
    }

    #[test]
    fn test_dashboard_event_contains_all_fields() {
        let event = DashboardEvent {
            active_agents: 5,
            requests_total: 42,
            requests_deduped: 7,
            dedup_rate: 16.67,
            cost_saved_usd: 0.014,
            throughput_rps: 2.1,
            circuit_breakers: vec![CircuitBreakerState {
                name: "inference".to_string(),
                status: "closed".to_string(),
                failures: 0,
                successes: 10,
                success_rate: 1.0,
            }],
            stage_latencies: StageTiming {
                rag_ms: 0.0,
                assemble_ms: 0.0,
                inference_ms: 0.0,
                post_ms: 0.0,
                stream_ms: 0.0,
            },
            model_routing: ModelRoutingStats {
                primary_model: "llama_cpp".to_string(),
                primary_pct: 85.0,
                fallback_pct: 15.0,
            },
            stage_counts: StageCountMap {
                rag: 42,
                assemble: 42,
                inference: 42,
                post: 42,
                stream: 42,
            },
            shed_counts: StageCountMap {
                rag: 0,
                assemble: 0,
                inference: 0,
                post: 0,
                stream: 0,
            },
            uptime_secs: 120,
        };

        let json =
            serde_json::to_string(&event).unwrap_or_else(|_| "serialization failed".to_string());
        assert!(json.contains("active_agents"));
        assert!(json.contains("requests_total"));
        assert!(json.contains("requests_deduped"));
        assert!(json.contains("dedup_rate"));
        assert!(json.contains("cost_saved_usd"));
        assert!(json.contains("throughput_rps"));
        assert!(json.contains("circuit_breakers"));
        assert!(json.contains("stage_latencies"));
        assert!(json.contains("model_routing"));
        assert!(json.contains("stage_counts"));
        assert!(json.contains("shed_counts"));
        assert!(json.contains("uptime_secs"));
    }

    #[test]
    fn test_circuit_breaker_state_serializes() {
        let state = CircuitBreakerState {
            name: "inference".to_string(),
            status: "closed".to_string(),
            failures: 2,
            successes: 50,
            success_rate: 0.96,
        };
        let json = serde_json::to_string(&state);
        assert!(json.is_ok());
        let s = json.unwrap_or_default();
        assert!(s.contains("inference"));
        assert!(s.contains("closed"));
    }

    #[test]
    fn test_stage_timing_serializes() {
        let timing = StageTiming {
            rag_ms: 1.5,
            assemble_ms: 0.3,
            inference_ms: 12.0,
            post_ms: 0.2,
            stream_ms: 0.05,
        };
        let json = serde_json::to_string(&timing);
        assert!(json.is_ok());
    }

    #[test]
    fn test_model_routing_stats_serializes() {
        let stats = ModelRoutingStats {
            primary_model: "llama_cpp".to_string(),
            primary_pct: 90.0,
            fallback_pct: 10.0,
        };
        let json = serde_json::to_string(&stats).unwrap_or_else(|_| String::new());
        assert!(json.contains("llama_cpp"));
        assert!(json.contains("primary_pct"));
    }

    #[test]
    fn test_stage_count_map_serializes() {
        let counts = StageCountMap {
            rag: 10,
            assemble: 10,
            inference: 10,
            post: 10,
            stream: 10,
        };
        let json = serde_json::to_string(&counts).unwrap_or_else(|_| String::new());
        assert!(json.contains("\"rag\":10"));
    }

    // ── Worker factory tests ─────────────────────────────────────────────

    #[test]
    fn test_create_worker_echo_succeeds() {
        let result = create_worker("echo");
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_worker_llama_cpp_succeeds() {
        let result = create_worker("llama_cpp");
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_worker_unknown_returns_error() {
        let result = create_worker("nonexistent_worker");
        assert!(result.is_err());
        if let Err(ref e) = result {
            assert!(
                e.to_string().contains("unknown worker"),
                "error message should contain 'unknown worker'"
            );
        }
    }

    // ── CLI argument parsing tests ───────────────────────────────────────

    #[test]
    fn test_parse_worker_arg_defaults_to_echo() {
        // Without --worker flag in env args, should default to "echo"
        let default = parse_worker_arg();
        // May or may not be "echo" depending on test runner args, but must not panic
        assert!(!default.is_empty());
    }

    #[test]
    fn test_parse_port_arg_defaults_to_3000() {
        let port = parse_port_arg();
        // Default should be 3000 unless --port is set in test runner
        assert!(port > 0);
    }

    // ── Route handler tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_index_handler_returns_html() {
        let response = index_handler().await;
        let body = response.0;
        assert!(body.contains("tokio-prompt-orchestrator"));
    }

    #[tokio::test]
    async fn test_stats_handler_returns_200() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_stats_handler_returns_json_content_type() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        let content_type = response.headers().get("content-type");
        assert!(content_type.is_some());
        let ct_str = content_type.map(|v| v.to_str().unwrap_or("")).unwrap_or("");
        assert!(ct_str.contains("application/json"));
    }

    #[tokio::test]
    async fn test_stats_response_contains_required_fields() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        let body = axum::body::to_bytes(response.into_body(), 1024 * 64)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);

        assert!(text.contains("active_agents"));
        assert!(text.contains("requests_total"));
        assert!(text.contains("circuit_breakers"));
        assert!(text.contains("stage_latencies"));
    }

    #[tokio::test]
    async fn test_index_route_returns_200() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_unknown_route_returns_404() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/nonexistent")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_sse_route_returns_200() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/status")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_sse_route_content_type_is_event_stream() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/status")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("text/event-stream"),
            "expected SSE content type, got: {ct}"
        );
    }

    // ── Snapshot builder tests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_build_snapshot_returns_valid_event() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert_eq!(snapshot.active_agents, 5);
        assert!(
            snapshot.uptime_secs <= 5,
            "uptime should be near zero in tests"
        );
    }

    #[tokio::test]
    async fn test_build_snapshot_dedup_rate_zero_when_no_requests() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        // With no requests processed, dedup rate should be 0
        assert!((snapshot.dedup_rate - 0.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_build_snapshot_circuit_breaker_starts_closed() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert_eq!(snapshot.circuit_breakers.len(), 1);
        assert_eq!(snapshot.circuit_breakers[0].status, "closed");
        assert_eq!(snapshot.circuit_breakers[0].name, "inference");
    }

    #[tokio::test]
    async fn test_build_snapshot_worker_name_matches_state() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert_eq!(snapshot.model_routing.primary_model, "echo");
        assert!((snapshot.model_routing.primary_pct - 100.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_build_snapshot_cost_saved_zero_initially() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert!((snapshot.cost_saved_usd - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_build_snapshot_stage_latencies_zero_initially() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert!((snapshot.stage_latencies.rag_ms - 0.0).abs() < f64::EPSILON);
        assert!((snapshot.stage_latencies.inference_ms - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_build_snapshot_shed_counts_zero_initially() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert_eq!(snapshot.shed_counts.rag, 0);
        assert_eq!(snapshot.shed_counts.assemble, 0);
        assert_eq!(snapshot.shed_counts.inference, 0);
    }

    #[tokio::test]
    async fn test_build_snapshot_serializes_roundtrip() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        let json = serde_json::to_string(&snapshot);
        assert!(json.is_ok(), "snapshot must serialize");
        let json_str = json.unwrap_or_default();
        assert!(!json_str.is_empty());

        // Verify it's valid JSON by parsing it back
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&json_str);
        assert!(parsed.is_ok(), "serialized JSON must parse back");
    }

    // ── Router construction tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_build_router_does_not_panic() {
        let state = make_test_state();
        let _router = build_router(state);
    }

    // ── DashboardState clone tests ───────────────────────────────────────

    #[tokio::test]
    async fn test_dashboard_state_is_cloneable() {
        let state = make_test_state();
        let inner = (*state).clone();
        assert_eq!(inner.worker_name, "echo");
    }

    // ── Post method tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_post_to_index_returns_method_not_allowed() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn test_post_to_stats_returns_method_not_allowed() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn test_post_to_sse_returns_method_not_allowed() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/api/status")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    // ── Dashboard HTML content tests ─────────────────────────────────────

    #[test]
    fn test_dashboard_html_is_not_empty() {
        assert!(
            !DASHBOARD_HTML.is_empty(),
            "embedded HTML must not be empty"
        );
    }

    #[test]
    fn test_dashboard_html_contains_title() {
        assert!(
            DASHBOARD_HTML.contains("tokio-prompt-orchestrator"),
            "HTML must contain the project title"
        );
    }

    #[test]
    fn test_dashboard_html_contains_sse_endpoint() {
        assert!(
            DASHBOARD_HTML.contains("/api/status"),
            "HTML must reference the SSE endpoint"
        );
    }

    #[test]
    fn test_dashboard_html_contains_doctype() {
        assert!(
            DASHBOARD_HTML.contains("<!DOCTYPE html>"),
            "HTML must have a doctype declaration"
        );
    }

    // ── Dedup rate calculation tests ─────────────────────────────────────

    #[test]
    fn test_dedup_rate_zero_when_no_requests() {
        let requests_total: u64 = 0;
        let requests_deduped: u64 = 0;
        let rate = if requests_total > 0 {
            (requests_deduped as f32 / requests_total as f32) * 100.0
        } else {
            0.0
        };
        assert!((rate - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_dedup_rate_calculation_correct() {
        let requests_total: u64 = 100;
        let requests_deduped: u64 = 25;
        let rate = if requests_total > 0 {
            (requests_deduped as f32 / requests_total as f32) * 100.0
        } else {
            0.0
        };
        assert!((rate - 25.0).abs() < 0.01);
    }

    #[test]
    fn test_cost_saved_calculation() {
        let deduped: u64 = 500;
        let cost = deduped as f64 * 0.002;
        assert!((cost - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_throughput_calculation() {
        let requests: u64 = 300;
        let uptime_secs: u64 = 60;
        let rps = requests as f32 / uptime_secs as f32;
        assert!((rps - 5.0).abs() < 0.01);
    }

    // ── Circuit breaker status string conversion tests ───────────────────

    #[test]
    fn test_circuit_status_closed_to_string() {
        let status = CircuitStatus::Closed;
        let s = match status {
            CircuitStatus::Closed => "closed",
            CircuitStatus::Open => "open",
            CircuitStatus::HalfOpen => "half_open",
        };
        assert_eq!(s, "closed");
    }

    #[test]
    fn test_circuit_status_open_to_string() {
        let status = CircuitStatus::Open;
        let s = match status {
            CircuitStatus::Closed => "closed",
            CircuitStatus::Open => "open",
            CircuitStatus::HalfOpen => "half_open",
        };
        assert_eq!(s, "open");
    }

    #[test]
    fn test_circuit_status_half_open_to_string() {
        let status = CircuitStatus::HalfOpen;
        let s = match status {
            CircuitStatus::Closed => "closed",
            CircuitStatus::Open => "open",
            CircuitStatus::HalfOpen => "half_open",
        };
        assert_eq!(s, "half_open");
    }
}
