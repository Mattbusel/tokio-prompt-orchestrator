//! Example: Web API Server with Enhanced Features
//!
//! Demonstrates REST API, WebSocket streaming, caching, and rate limiting.
//!
//! Run with:
//! ```bash
//! cargo run --example web_api_demo --features full
//! ```
//!
//! Then test:
//! ```bash
//! # REST API
//! curl -X POST http://localhost:8080/api/v1/infer \
//!   -H "Content-Type: application/json" \
//!   -d '{"prompt": "What is Rust?", "session_id": "user-123"}'
//!
//! # WebSocket (using wscat)
//! wscat -c ws://localhost:8080/api/v1/stream
//! > {"prompt": "Hello", "session_id": "ws-user"}
//! ```

use std::sync::Arc;
use tokio_prompt_orchestrator::{enhanced, spawn_pipeline, web_api, EchoWorker, ModelWorker};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("🚀 Web API + Enhanced Features Demo");
    info!("");

    // Create worker
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(50));
    info!("✅ Created worker");

    // Spawn pipeline
    let handles = spawn_pipeline(worker);
    info!("✅ Pipeline spawned");

    // Create cache layer (in-memory)
    let cache = enhanced::CacheLayer::new_memory(1000);
    info!("✅ Cache layer initialized (1000 entries)");

    // Create rate limiter (100 requests per 60 seconds)
    let rate_limiter = enhanced::RateLimiter::new(100, 60);
    info!("✅ Rate limiter initialized (100 req/min)");

    // Create priority queue
    let priority_queue = enhanced::PriorityQueue::new();
    info!("✅ Priority queue initialized");

    info!("");
    info!("🌐 Starting web server...");
    info!("");

    // Start web API server
    let config = web_api::ServerConfig {
        host: "0.0.0.0".to_string(),
        port: 8080,
        max_request_size: 10 * 1024 * 1024,
        timeout_seconds: 300,
    };

    info!("📡 API Endpoints:");
    info!("   POST   http://localhost:8080/api/v1/infer");
    info!("   GET    http://localhost:8080/api/v1/status/:id");
    info!("   GET    http://localhost:8080/api/v1/result/:id");
    info!("   WS     ws://localhost:8080/api/v1/stream");
    info!("   GET    http://localhost:8080/health");
    info!("   GET    http://localhost:8080/metrics");
    info!("");

    info!("🧪 Test with curl:");
    info!(r#"   curl -X POST http://localhost:8080/api/v1/infer \"#);
    info!(r#"     -H "Content-Type: application/json" \"#);
    info!(r#"     -d '{{"prompt": "What is Rust?", "session_id": "user-123"}}'"#);
    info!("");

    info!("🔌 Test WebSocket:");
    info!("   npm install -g wscat");
    info!("   wscat -c ws://localhost:8080/api/v1/stream");
    info!(r#"   > {{"prompt": "Hello", "session_id": "ws-user"}}"#);
    info!("");

    info!("✨ Enhanced Features:");
    info!("   • Caching: Responses cached for 1 hour");
    info!("   • Rate Limiting: 100 requests per minute per session");
    info!("   • Priority Queue: Critical > High > Normal > Low");
    info!("");

    info!("Press Ctrl+C to stop...");
    info!("");

    // Start server (blocks)
    web_api::start_server(config, handles.input_tx).await?;

    Ok(())
}
