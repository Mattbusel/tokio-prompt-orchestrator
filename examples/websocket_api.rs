//! Example: WebSocket Streaming
//!
//! Demonstrates bidirectional WebSocket communication.
//!
//! Run with:
//! ```bash
//! cargo run --example websocket_api --features web-api
//! ```
//!
//! Test with websocat:
//! ```bash
//! # Install: cargo install websocat
//! websocat ws://localhost:8080/api/v1/ws
//!
//! # Then send JSON:
//! {"prompt": "Hello!", "stream": true}
//! ```
//!
//! Or use JavaScript:
//! ```javascript
//! const ws = new WebSocket('ws://localhost:8080/api/v1/ws');
//!
//! ws.onopen = () => {
//!   ws.send(JSON.stringify({
//!     prompt: "What is Rust?",
//!     metadata: { user: "alice" }
//!   }));
//! };
//!
//! ws.onmessage = (event) => {
//!   console.log('Received:', JSON.parse(event.data));
//! };
//! ```

use std::sync::Arc;
use tokio_prompt_orchestrator::{spawn_pipeline, web_api, EchoWorker, ModelWorker};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("🚀 Starting WebSocket server");
    info!("");

    // Create worker
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(50));

    // Spawn pipeline
    let handles = spawn_pipeline(worker);
    info!("✅ Pipeline spawned");

    // Start web API server
    info!("🌐 Starting web server on http://localhost:8080");
    info!("");
    info!("📡 WebSocket endpoint:");
    info!("   ws://localhost:8080/api/v1/ws");
    info!("");
    info!("💡 Test with websocat:");
    info!("   websocat ws://localhost:8080/api/v1/ws");
    info!("");
    info!("   Then send:");
    info!(r#"   {{"prompt": "Hello, world!", "stream": true}}"#);
    info!("");
    info!("📝 Or use JavaScript in browser console:");
    info!("   const ws = new WebSocket('ws://localhost:8080/api/v1/ws');");
    info!(r#"   ws.onmessage = e => console.log(JSON.parse(e.data));"#);
    info!(r#"   ws.send(JSON.stringify({{prompt: "Hi!"}}));"#);
    info!("");

    web_api::start_server("0.0.0.0:8080", handles.input_tx).await?;

    Ok(())
}
