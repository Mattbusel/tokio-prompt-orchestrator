//! Example: Server-Sent Events (SSE) Streaming
//!
//! Demonstrates real-time streaming via SSE.
//!
//! Run with:
//! ```bash
//! cargo run --example sse_stream --features web-api
//! ```
//!
//! Test with curl:
//! ```bash
//! # Submit request to get ID
//! ID=$(curl -s -X POST http://localhost:8080/api/v1/infer \
//!   -H "Content-Type: application/json" \
//!   -d '{"prompt": "Explain Rust"}' | jq -r '.request_id')
//!
//! # Stream results
//! curl http://localhost:8080/api/v1/stream/$ID
//! ```
//!
//! Or use JavaScript EventSource:
//! ```javascript
//! const eventSource = new EventSource(
//!   'http://localhost:8080/api/v1/stream/REQUEST_ID'
//! );
//!
//! eventSource.addEventListener('token', (e) => {
//!   console.log('Token:', JSON.parse(e.data));
//! });
//!
//! eventSource.addEventListener('complete', (e) => {
//!   console.log('Done!');
//!   eventSource.close();
//! });
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

    info!("🚀 Starting SSE streaming server");
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
    info!("   POST   http://localhost:8080/api/v1/infer (get request_id)");
    info!("   GET    http://localhost:8080/api/v1/stream/:request_id (SSE)");
    info!("");
    info!("💡 Two-step process:");
    info!("");
    info!("Step 1: Submit request");
    info!(r#"   curl -X POST http://localhost:8080/api/v1/infer \"#);
    info!(r#"     -H "Content-Type: application/json" \"#);
    info!(r#"     -d '{{"prompt": "Hello!"}}'\"#);
    info!("");
    info!("Step 2: Stream results (use request_id from step 1)");
    info!("   curl http://localhost:8080/api/v1/stream/<request_id>");
    info!("");
    info!("📝 Or use JavaScript EventSource:");
    info!("   const es = new EventSource('/api/v1/stream/<request_id>');");
    info!(r#"   es.addEventListener('token', e => console.log(e.data));"#);
    info!("");

    web_api::start_server(web_api::ServerConfig::default(), handles.input_tx).await?;

    Ok(())
}
