//! Example: REST API Server
//!
//! Demonstrates HTTP REST API for inference requests.
//!
//! Run with:
//! ```bash
//! cargo run --example rest_api --features web-api
//! ```
//!
//! Then test with curl:
//! ```bash
//! # Submit inference request
//! curl -X POST http://localhost:8080/api/v1/infer \
//!   -H "Content-Type: application/json" \
//!   -d '{"prompt": "What is Rust?", "metadata": {"user": "alice"}}'
//!
//! # Check status
//! curl http://localhost:8080/api/v1/status/{request_id}
//!
//! # Get result
//! curl http://localhost:8080/api/v1/result/{request_id}
//! ```

use std::sync::Arc;
use tokio_prompt_orchestrator::{spawn_pipeline, web_api, EchoWorker, ModelWorker};
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

    info!("🚀 Starting REST API server");
    info!("");

    // Create worker
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(50));

    // Spawn pipeline
    let handles = spawn_pipeline(worker);
    info!("✅ Pipeline spawned");

    // Start web API server
    info!("🌐 Starting web server on http://localhost:8080");
    info!("");
    info!("📡 Available endpoints:");
    info!("   POST   http://localhost:8080/api/v1/infer");
    info!("   GET    http://localhost:8080/api/v1/status/:id");
    info!("   GET    http://localhost:8080/api/v1/result/:id");
    info!("   GET    http://localhost:8080/health");
    info!("");
    info!("💡 Test with curl:");
    info!(r#"   curl -X POST http://localhost:8080/api/v1/infer \"#);
    info!(r#"     -H "Content-Type: application/json" \"#);
    info!(r#"     -d '{{"prompt": "Hello, world!"}}'\"#);
    info!("");

    web_api::start_server(web_api::ServerConfig::default(), handles.input_tx).await?;

    Ok(())
}
