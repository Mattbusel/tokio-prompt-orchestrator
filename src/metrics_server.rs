//! Metrics HTTP server
//!
//! Exposes Prometheus metrics via HTTP endpoint on port 9090.
//!
//! ## Usage
//!
//! ```no_run
//! use tokio_prompt_orchestrator::metrics_server;
//!
//! #[tokio::main]
//! async fn main() {
//!     // Start metrics server (non-blocking)
//!     let metrics_handle = tokio::spawn(metrics_server::start_server("0.0.0.0:9090"));
//!     
//!     // Your application code...
//!     
//!     // Graceful shutdown
//!     metrics_handle.abort();
//! }
//! ```
//!
//! ## Endpoints
//!
//! - `GET /metrics` - Prometheus metrics in text format
//! - `GET /health` - Health check endpoint
//!
//! ## Scraping with Prometheus
//!
//! Add to your `prometheus.yml`:
//! ```yaml
//! scrape_configs:
//!   - job_name: 'orchestrator'
//!     static_configs:
//!       - targets: ['localhost:9090']
//! ```

#[cfg(feature = "metrics-server")]
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
#[cfg(feature = "metrics-server")]
use std::net::SocketAddr;
#[cfg(feature = "metrics-server")]
use tower_http::trace::TraceLayer;
#[cfg(feature = "metrics-server")]
use tracing::info;

#[cfg(feature = "metrics-server")]
/// Start the metrics HTTP server
///
/// Exposes Prometheus metrics on the specified address.
/// Returns a future that runs indefinitely.
///
/// ## Example
///
/// ```no_run
/// #[tokio::main]
/// async fn main() {
///     let handle = tokio::spawn(
///         tokio_prompt_orchestrator::metrics_server::start_server("0.0.0.0:9090")
///     );
///     
///     // Your app runs here...
///     
///     handle.abort(); // Shutdown metrics server
/// }
/// ```
pub async fn start_server(addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = addr.parse()?;

    info!("🔧 Starting metrics server on http://{}", addr);

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(health_handler))
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("✅ Metrics server ready at http://{}/metrics", addr);
    info!("✅ Health check at http://{}/health", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(feature = "metrics-server")]
/// Handler for /metrics endpoint
async fn metrics_handler() -> Response {
    let metrics = crate::metrics::gather_metrics();

    (
        StatusCode::OK,
        [("Content-Type", "text/plain; version=0.0.4")],
        metrics,
    )
        .into_response()
}

#[cfg(feature = "metrics-server")]
/// Handler for /health endpoint
async fn health_handler() -> Response {
    let summary = crate::metrics::get_metrics_summary();

    let health_status = serde_json::json!({
        "status": "healthy",
        "requests_total": summary.requests_total.len(),
        "shed_total": summary.requests_shed.values().sum::<u64>(),
        "errors_total": summary.errors_total.len(),
    });

    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        serde_json::to_string_pretty(&health_status)
            .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()),
    )
        .into_response()
}

#[cfg(not(feature = "metrics-server"))]
/// Metrics server is not available without the `metrics-server` feature
pub async fn start_server(_addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("Metrics server requires 'metrics-server' feature. Build with: cargo build --features metrics-server".into())
}

#[cfg(test)]
#[cfg(feature = "metrics-server")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_endpoint() {
        let _response = health_handler().await;
        // Should return 200 OK
        // In real test, would check response body
    }
}
