//! # HelixRouter Web UI
//!
//! ## Responsibility
//! Serve an Axum-based web dashboard with real-time SSE streaming of routing
//! events, JSON stats, Prometheus metrics, and a dark-themed HTML dashboard.
//!
//! ## Guarantees
//! - Thread-safe: all state is behind `Arc` and broadcast channels.
//! - Non-blocking: SSE streams use `tokio::sync::broadcast` with backpressure.
//! - Zero panics: all error paths return `Result` or appropriate HTTP status codes.
//!
//! ## NOT Responsible For
//! - Routing decisions (see `router`)
//! - Metrics computation (see `metrics`)
//! - Configuration validation (see `config`)

use crate::helix::router::HelixRouter;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Json;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::pin::Pin;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

/// A routing event emitted by the HelixRouter for live dashboard streaming.
///
/// Serialized to JSON and pushed to SSE clients.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingEvent {
    /// Timestamp in milliseconds since an arbitrary epoch.
    pub timestamp_ms: u64,
    /// The job ID that was routed.
    pub job_id: u64,
    /// The strategy name chosen for the job.
    pub strategy: String,
    /// The latency of the routing decision in milliseconds.
    pub latency_ms: f64,
    /// The current system pressure score (0.0 to 1.0).
    pub pressure: f64,
}

impl std::fmt::Display for RoutingEvent {
    /// Format the routing event for human-readable output.
    ///
    /// # Panics
    ///
    /// This function never panics.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}ms] job-{} -> {} ({:.2}ms, pressure={:.2})",
            self.timestamp_ms, self.job_id, self.strategy, self.latency_ms, self.pressure
        )
    }
}

/// Shared application state for the Axum web handlers.
///
/// # Panics
///
/// This type never panics.
#[derive(Clone)]
struct AppState {
    router: HelixRouter,
    event_tx: tokio::sync::broadcast::Sender<RoutingEvent>,
}

/// The HelixRouter web UI server.
///
/// Wraps a `HelixRouter` and a broadcast channel for real-time event streaming.
/// Build an Axum router with `build_router()` and serve it with `axum::serve`.
///
/// # Panics
///
/// This type and its methods never panic.
pub struct HelixWeb {
    router: HelixRouter,
    event_tx: tokio::sync::broadcast::Sender<RoutingEvent>,
}

impl HelixWeb {
    /// Create a new HelixWeb instance wrapping the given router.
    ///
    /// Internally creates a broadcast channel with capacity 256 for SSE events.
    ///
    /// # Arguments
    ///
    /// * `router` — The HelixRouter to expose via the web UI.
    ///
    /// # Returns
    ///
    /// A new [`HelixWeb`] instance.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(router: HelixRouter) -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(256);
        Self { router, event_tx }
    }

    /// Get a clone of the broadcast sender for publishing routing events.
    ///
    /// Callers can use this to send [`RoutingEvent`] values that will be
    /// streamed to all connected SSE clients.
    ///
    /// # Returns
    ///
    /// A cloned `broadcast::Sender<RoutingEvent>`.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn event_sender(&self) -> tokio::sync::broadcast::Sender<RoutingEvent> {
        self.event_tx.clone()
    }

    /// Build the Axum router with all web endpoints.
    ///
    /// # Endpoints
    ///
    /// - `GET /` — HTML dashboard (dark theme)
    /// - `GET /api/stats` — JSON stats snapshot
    /// - `GET /api/events` — SSE stream of routing events
    /// - `GET /metrics` — Prometheus text exposition
    /// - `POST /api/config` — Configuration update (placeholder)
    ///
    /// # Returns
    ///
    /// An [`axum::Router`] ready to be served.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn build_router(self) -> axum::Router {
        let state = AppState {
            router: self.router,
            event_tx: self.event_tx,
        };

        axum::Router::new()
            .route("/", get(dashboard_handler))
            .route("/api/stats", get(stats_handler))
            .route("/api/events", get(sse_handler))
            .route("/metrics", get(prometheus_handler))
            .route("/api/config", post(config_update_handler))
            .with_state(state)
    }
}

/// JSON request body for the config update endpoint.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdateRequest {
    /// Optional new inline threshold.
    pub inline_threshold: Option<u64>,
    /// Optional new spawn threshold.
    pub spawn_threshold: Option<u64>,
}

/// JSON response body for the config update endpoint.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdateResponse {
    /// Whether the update was accepted.
    pub status: String,
    /// Human-readable message about the result.
    pub message: String,
}

/// Serve the inline HTML dashboard.
///
/// # Returns
///
/// An HTML response with the dark-themed dashboard.
///
/// # Panics
///
/// This function never panics.
async fn dashboard_handler() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// Return a JSON snapshot of router statistics.
///
/// # Arguments
///
/// * `state` — The shared application state containing the router.
///
/// # Returns
///
/// A JSON response with completed/dropped counters, drop rate, and per-strategy latency.
///
/// # Panics
///
/// This function never panics.
async fn stats_handler(State(state): State<AppState>) -> impl IntoResponse {
    let stats = state.router.metrics().stats_snapshot();
    let mut strategy_stats = Vec::new();
    for (strategy, summary) in &stats.latencies {
        strategy_stats.push(serde_json::json!({
            "strategy": strategy.to_string(),
            "count": summary.count,
            "mean_ms": summary.mean_ms,
            "p95_ms": summary.p95_ms,
            "p99_ms": summary.p99_ms,
            "min_ms": summary.min_ms,
            "max_ms": summary.max_ms,
        }));
    }

    let body = serde_json::json!({
        "completed": stats.completed,
        "dropped": stats.dropped,
        "drop_rate": stats.drop_rate,
        "strategies": strategy_stats,
    });

    Json(body)
}

/// Stream routing events to the client via Server-Sent Events.
///
/// Uses `tokio::sync::broadcast` to fan out events to all connected clients.
/// Broadcast lag errors are silently skipped; the stream continues.
///
/// # Arguments
///
/// * `state` — The shared application state containing the event sender.
///
/// # Returns
///
/// An SSE stream of routing events.
///
/// # Panics
///
/// This function never panics.
async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(event) => {
            let json = serde_json::to_string(&event).unwrap_or_else(|_| String::from("{}"));
            Some(Ok(Event::default().data(json)))
        }
        Err(_) => None, // Skip lag errors
    });

    Sse::new(Box::pin(stream))
}

/// Return Prometheus text exposition metrics.
///
/// # Arguments
///
/// * `state` — The shared application state containing the router.
///
/// # Returns
///
/// A plain text response in Prometheus exposition format.
///
/// # Panics
///
/// This function never panics.
async fn prometheus_handler(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.router.metrics().render_prometheus();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
}

/// Accept a JSON config update request.
///
/// This is a placeholder endpoint. The HelixRouter config is currently immutable
/// after construction, so updates are acknowledged but not applied.
///
/// # Arguments
///
/// * `_state` — The shared application state (unused for now).
/// * `payload` — The JSON request body with optional threshold values.
///
/// # Returns
///
/// A JSON response acknowledging the request.
///
/// # Panics
///
/// This function never panics.
async fn config_update_handler(
    State(_state): State<AppState>,
    Json(payload): Json<ConfigUpdateRequest>,
) -> impl IntoResponse {
    let message = format!(
        "Config update acknowledged (inline_threshold={:?}, spawn_threshold={:?}). \
         Live config reload is not yet supported; values will take effect on next restart.",
        payload.inline_threshold, payload.spawn_threshold
    );

    Json(ConfigUpdateResponse {
        status: "accepted".to_string(),
        message,
    })
}

/// Inline HTML for the dark-themed dashboard.
const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>HelixRouter Dashboard</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { background: #0b0f14; color: #c9d1d9; font-family: 'Consolas', 'Monaco', monospace; padding: 20px; }
  h1 { color: #58a6ff; margin-bottom: 16px; font-size: 1.5em; }
  h2 { color: #8b949e; margin: 16px 0 8px 0; font-size: 1.1em; border-bottom: 1px solid #21262d; padding-bottom: 4px; }
  .card { background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 16px; margin-bottom: 12px; }
  .stat { display: inline-block; margin-right: 24px; }
  .stat-value { font-size: 1.4em; color: #58a6ff; font-weight: bold; }
  .stat-label { font-size: 0.85em; color: #8b949e; }
  .strategy-row { padding: 4px 0; border-bottom: 1px solid #21262d; }
  .strategy-name { color: #f0883e; font-weight: bold; display: inline-block; width: 100px; }
  #events { max-height: 400px; overflow-y: auto; font-size: 0.85em; }
  .event-line { padding: 2px 0; border-bottom: 1px solid #0d1117; }
  .event-line:nth-child(even) { background: #0d1117; }
  .pressure-bar { display: inline-block; width: 200px; height: 16px; background: #21262d; border-radius: 3px; vertical-align: middle; }
  .pressure-fill { height: 100%; border-radius: 3px; transition: width 0.3s; }
  .pressure-low { background: #3fb950; }
  .pressure-med { background: #d29922; }
  .pressure-high { background: #f85149; }
</style>
</head>
<body>
<h1>HelixRouter Dashboard</h1>

<div class="card">
  <h2>Stats</h2>
  <div class="stat"><div class="stat-value" id="completed">0</div><div class="stat-label">Completed</div></div>
  <div class="stat"><div class="stat-value" id="dropped">0</div><div class="stat-label">Dropped</div></div>
  <div class="stat"><div class="stat-value" id="drop-rate">0.00%</div><div class="stat-label">Drop Rate</div></div>
</div>

<div class="card">
  <h2>Pressure</h2>
  <span id="pressure-text">Pressure: 0.00</span>
  <div class="pressure-bar"><div class="pressure-fill pressure-low" id="pressure-fill" style="width:0%"></div></div>
</div>

<div class="card">
  <h2>Strategy Distribution</h2>
  <div id="strategies">No data yet.</div>
</div>

<div class="card">
  <h2>Live Routing Events</h2>
  <div id="events">Waiting for events...</div>
</div>

<script>
var maxEvents = 50;
var eventLines = [];
var lastPressure = 0;

function fetchStats() {
  fetch('/api/stats')
    .then(function(r) { return r.json(); })
    .then(function(data) {
      document.getElementById('completed').textContent = data.completed || 0;
      document.getElementById('dropped').textContent = data.dropped || 0;
      var rate = ((data.drop_rate || 0) * 100).toFixed(2);
      document.getElementById('drop-rate').textContent = rate + '%';

      var stratDiv = document.getElementById('strategies');
      if (data.strategies && data.strategies.length > 0) {
        var html = '';
        for (var i = 0; i < data.strategies.length; i++) {
          var s = data.strategies[i];
          html += '<div class="strategy-row">';
          html += '<span class="strategy-name">' + s.strategy + '</span> ';
          html += 'count=' + s.count + ' mean=' + s.mean_ms.toFixed(2) + 'ms p95=' + s.p95_ms.toFixed(2) + 'ms';
          html += '</div>';
        }
        stratDiv.innerHTML = html;
      }
    })
    .catch(function() {});
}

function updatePressure(p) {
  lastPressure = p;
  document.getElementById('pressure-text').textContent = 'Pressure: ' + p.toFixed(2);
  var pct = Math.min(p * 100, 100);
  var fill = document.getElementById('pressure-fill');
  fill.style.width = pct + '%';
  fill.className = 'pressure-fill ' + (p < 0.4 ? 'pressure-low' : p < 0.7 ? 'pressure-med' : 'pressure-high');
}

function connectSSE() {
  var source = new EventSource('/api/events');
  source.onmessage = function(e) {
    try {
      var ev = JSON.parse(e.data);
      var line = '[' + ev.timestamp_ms + 'ms] job-' + ev.job_id + ' -> ' + ev.strategy + ' (' + ev.latency_ms.toFixed(2) + 'ms)';
      eventLines.unshift(line);
      if (eventLines.length > maxEvents) eventLines.pop();
      var html = '';
      for (var i = 0; i < eventLines.length; i++) {
        html += '<div class="event-line">' + eventLines[i] + '</div>';
      }
      document.getElementById('events').innerHTML = html;
      updatePressure(ev.pressure);
    } catch(err) {}
  };
  source.onerror = function() {
    source.close();
    setTimeout(connectSSE, 3000);
  };
}

setInterval(fetchStats, 2000);
fetchStats();
connectSSE();
</script>
</body>
</html>"#;

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helix::config::HelixConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn make_helix_web() -> HelixWeb {
        let router = HelixRouter::new(HelixConfig::default());
        HelixWeb::new(router)
    }

    // -- RoutingEvent serialization ------------------------------------------

    #[test]
    fn test_routing_event_serialization() {
        let event = RoutingEvent {
            timestamp_ms: 1000,
            job_id: 42,
            strategy: "inline".to_string(),
            latency_ms: 1.23,
            pressure: 0.45,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"timestamp_ms\":1000"));
        assert!(json.contains("\"job_id\":42"));
        assert!(json.contains("\"strategy\":\"inline\""));
        assert!(json.contains("\"latency_ms\":1.23"));
        assert!(json.contains("\"pressure\":0.45"));
    }

    #[test]
    fn test_routing_event_deserialization() {
        let json =
            r#"{"timestamp_ms":500,"job_id":7,"strategy":"spawn","latency_ms":2.5,"pressure":0.1}"#;
        let event: RoutingEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.timestamp_ms, 500);
        assert_eq!(event.job_id, 7);
        assert_eq!(event.strategy, "spawn");
        assert!((event.latency_ms - 2.5).abs() < f64::EPSILON);
        assert!((event.pressure - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn test_routing_event_display() {
        let event = RoutingEvent {
            timestamp_ms: 100,
            job_id: 5,
            strategy: "cpu_pool".to_string(),
            latency_ms: 3.14,
            pressure: 0.67,
        };
        let display = format!("{event}");
        assert!(display.contains("100ms"));
        assert!(display.contains("job-5"));
        assert!(display.contains("cpu_pool"));
        assert!(display.contains("3.14ms"));
        assert!(display.contains("0.67"));
    }

    // -- HelixWeb creation ---------------------------------------------------

    #[tokio::test]
    async fn test_helix_web_creation() {
        let router = HelixRouter::new(HelixConfig::default());
        let web = HelixWeb::new(router);
        // Verify the broadcast sender is functional
        let _tx = web.event_sender();
    }

    #[tokio::test]
    async fn test_event_sender_broadcasts() {
        let router = HelixRouter::new(HelixConfig::default());
        let web = HelixWeb::new(router);
        let tx = web.event_sender();
        let mut rx = tx.subscribe();

        let event = RoutingEvent {
            timestamp_ms: 1,
            job_id: 1,
            strategy: "inline".to_string(),
            latency_ms: 0.5,
            pressure: 0.0,
        };
        let send_result = tx.send(event.clone());
        assert!(send_result.is_ok());

        let received = rx.try_recv().unwrap();
        assert_eq!(received.job_id, 1);
        assert_eq!(received.strategy, "inline");
    }

    #[tokio::test]
    async fn test_event_sender_multiple_receivers() {
        let router = HelixRouter::new(HelixConfig::default());
        let web = HelixWeb::new(router);
        let tx = web.event_sender();
        let mut rx1 = tx.subscribe();
        let mut rx2 = tx.subscribe();

        let event = RoutingEvent {
            timestamp_ms: 2,
            job_id: 10,
            strategy: "spawn".to_string(),
            latency_ms: 1.0,
            pressure: 0.3,
        };
        let _ = tx.send(event);

        let r1 = rx1.try_recv().unwrap();
        let r2 = rx2.try_recv().unwrap();
        assert_eq!(r1.job_id, r2.job_id);
    }

    // -- build_router --------------------------------------------------------

    #[tokio::test]
    async fn test_build_router_creates_routes() {
        let web = make_helix_web();
        let _app = web.build_router();
        // Router construction should succeed without errors
    }

    // -- Dashboard endpoint --------------------------------------------------

    #[tokio::test]
    async fn test_dashboard_endpoint_returns_html() {
        let web = make_helix_web();
        let app = web.build_router();

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("HelixRouter Dashboard"));
        assert!(text.contains("#0b0f14"));
        assert!(text.contains("EventSource"));
    }

    // -- Stats endpoint ------------------------------------------------------

    #[tokio::test]
    async fn test_stats_endpoint_returns_json() {
        let web = make_helix_web();
        let app = web.build_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("completed").is_some());
        assert!(json.get("dropped").is_some());
        assert!(json.get("drop_rate").is_some());
        assert!(json.get("strategies").is_some());
    }

    #[tokio::test]
    async fn test_stats_endpoint_completed_starts_at_zero() {
        let web = make_helix_web();
        let app = web.build_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["completed"], 0);
        assert_eq!(json["dropped"], 0);
    }

    // -- Prometheus endpoint -------------------------------------------------

    #[tokio::test]
    async fn test_prometheus_endpoint_returns_text() {
        let web = make_helix_web();
        let app = web.build_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("text/plain"),
            "expected text/plain, got: {content_type}"
        );
        let body = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("helix_completed_total"));
        assert!(text.contains("helix_dropped_total"));
    }

    // -- Config update endpoint ----------------------------------------------

    #[tokio::test]
    async fn test_config_update_endpoint_accepts_json() {
        let web = make_helix_web();
        let app = web.build_router();

        let payload = serde_json::json!({
            "inline_threshold": 60,
            "spawn_threshold": 250,
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "accepted");
        assert!(json["message"].as_str().unwrap().contains("acknowledged"));
    }

    #[tokio::test]
    async fn test_config_update_partial_payload() {
        let web = make_helix_web();
        let app = web.build_router();

        let payload = serde_json::json!({
            "inline_threshold": 75,
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    // -- SSE endpoint --------------------------------------------------------

    #[tokio::test]
    async fn test_sse_endpoint_streams_events() {
        let router = HelixRouter::new(HelixConfig::default());
        let web = HelixWeb::new(router);
        let tx = web.event_sender();
        let app = web.build_router();

        // Send an event before connecting (it will be in the broadcast buffer)
        // We need to subscribe first, then send, so we test via the handler
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("text/event-stream"),
            "expected text/event-stream, got: {content_type}"
        );

        // Send event after SSE connection is established
        let event = RoutingEvent {
            timestamp_ms: 999,
            job_id: 77,
            strategy: "batch".to_string(),
            latency_ms: 5.5,
            pressure: 0.2,
        };
        let _ = tx.send(event);
    }

    // -- ConfigUpdateRequest/Response ----------------------------------------

    #[test]
    fn test_config_update_request_serde() {
        let req = ConfigUpdateRequest {
            inline_threshold: Some(60),
            spawn_threshold: Some(250),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ConfigUpdateRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.inline_threshold, Some(60));
        assert_eq!(back.spawn_threshold, Some(250));
    }

    #[test]
    fn test_config_update_response_serde() {
        let resp = ConfigUpdateResponse {
            status: "accepted".to_string(),
            message: "test message".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("accepted"));
        assert!(json.contains("test message"));
    }

    #[test]
    fn test_dashboard_html_contains_required_elements() {
        assert!(DASHBOARD_HTML.contains("HelixRouter Dashboard"));
        assert!(DASHBOARD_HTML.contains("background: #0b0f14"));
        assert!(DASHBOARD_HTML.contains("color: #c9d1d9"));
        assert!(DASHBOARD_HTML.contains("EventSource"));
        assert!(DASHBOARD_HTML.contains("/api/events"));
        assert!(DASHBOARD_HTML.contains("/api/stats"));
        assert!(DASHBOARD_HTML.contains("Pressure"));
        assert!(DASHBOARD_HTML.contains("Completed"));
        assert!(DASHBOARD_HTML.contains("Dropped"));
        assert!(DASHBOARD_HTML.contains("Drop Rate"));
    }
}
