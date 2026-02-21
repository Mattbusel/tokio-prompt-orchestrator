//! Example: Using local llama.cpp server
//!
//! Prerequisites:
//! 1. Download and build llama.cpp: https://github.com/ggerganov/llama.cpp
//! 2. Download a model (e.g., Llama-2-7B-GGUF)
//! 3. Start server:
//!    ```bash
//!    ./llama-cpp-server -m models/llama-2-7b.gguf --port 8080
//!    ```
//!
//! Run example:
//! ```bash
//! cargo run --example llamacpp_worker
//! ```
//!
//! Or with custom URL:
//! ```bash
//! export LLAMA_CPP_URL="http://localhost:8080"
//! cargo run --example llamacpp_worker
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    spawn_pipeline, LlamaCppWorker, ModelWorker, PromptRequest, SessionId,
};
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

    info!("🦙 llama.cpp Worker Demo");

    // Create llama.cpp worker
    // Connects to local llama.cpp server
    let worker: Arc<dyn ModelWorker> = Arc::new(
        LlamaCppWorker::new()
            .with_url("http://localhost:8080")
            .with_max_tokens(256)
            .with_temperature(0.8),
    );

    info!("✅ Created llama.cpp worker (connecting to localhost:8080)");

    // Spawn pipeline
    let handles = spawn_pipeline(worker);
    info!("✅ Pipeline spawned");

    // Send test requests
    let requests = vec![
        ("session-1", "Once upon a time in a land far away"),
        ("session-2", "The meaning of life is"),
        ("session-3", "In the future, AI will"),
        ("session-4", "The best programming language is"),
    ];

    info!("📨 Sending {} requests to llama.cpp", requests.len());

    for (session_id, prompt) in requests {
        let request = PromptRequest {
            session: SessionId::new(session_id),
            input: prompt.to_string(),
            request_id: "example-req-0".to_string(),
            meta: {
                let mut meta = HashMap::new();
                meta.insert("backend".to_string(), "llama.cpp".to_string());
                meta
            },
        };

        match handles.input_tx.send(request).await {
            Ok(_) => {}
            Err(e) => {
                info!("Failed to send request: {}", e);
                break;
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    info!("✅ All requests sent");

    // Close input and wait for drain
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    info!("🏁 Demo complete");
    info!(
        "💡 Tip: If you see connection errors, make sure llama.cpp server is running on port 8080"
    );

    Ok(())
}
