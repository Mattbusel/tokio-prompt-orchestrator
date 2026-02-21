//! Example: Using vLLM inference server
//!
//! Prerequisites:
//! 1. Install vLLM: pip install vllm
//! 2. Start vLLM server:
//!    ```bash
//!    python -m vllm.entrypoints.api_server \
//!      --model meta-llama/Llama-2-7b-chat-hf \
//!      --port 8000
//!    ```
//!
//! Run example:
//! ```bash
//! cargo run --example vllm_worker
//! ```
//!
//! Or with custom URL:
//! ```bash
//! export VLLM_URL="http://localhost:8000"
//! cargo run --example vllm_worker
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    spawn_pipeline, ModelWorker, PromptRequest, SessionId, VllmWorker,
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

    info!("⚡ vLLM Worker Demo");

    // Create vLLM worker
    // vLLM provides high-throughput inference with PagedAttention
    let worker: Arc<dyn ModelWorker> = Arc::new(
        VllmWorker::new()
            .with_url("http://localhost:8000")
            .with_max_tokens(512)
            .with_temperature(0.7)
            .with_top_p(0.95),
    );

    info!("✅ Created vLLM worker (connecting to localhost:8000)");

    // Spawn pipeline
    let handles = spawn_pipeline(worker);
    info!("✅ Pipeline spawned");

    // Send test requests
    // vLLM can handle high concurrency efficiently
    let requests = vec![
        ("user-1", "Explain machine learning in simple terms"),
        (
            "user-2",
            "What are the benefits of Rust programming language?",
        ),
        ("user-3", "Describe how neural networks work"),
        ("user-4", "What is the difference between AI and ML?"),
        ("user-5", "How does transformer architecture work?"),
    ];

    info!("📨 Sending {} requests to vLLM", requests.len());

    for (session_id, prompt) in requests {
        let request = PromptRequest {
            session: SessionId::new(session_id),
            input: prompt.to_string(),
            meta: {
                let mut meta = HashMap::new();
                meta.insert("backend".to_string(), "vllm".to_string());
                meta.insert("batch_size".to_string(), "5".to_string());
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

        // Shorter delay - vLLM handles concurrency well
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    info!("✅ All requests sent");

    // Close input and wait for drain
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    info!("🏁 Demo complete");
    info!("💡 Tip: vLLM excels at batched inference with high throughput");

    Ok(())
}
