//! Example: Using vLLM inference server
//!
//! Start vLLM server first:
//! ```bash
//! # Install vLLM
//! pip install vllm
//!
//! # Start server
//! python -m vllm.entrypoints.api_server \
//!     --model meta-llama/Llama-2-7b-chat-hf \
//!     --port 8000
//!
//! # Then run this example
//! cargo run --example vllm_example
//! ```
//!
//! Or set custom URL:
//! ```bash
//! export VLLM_URL="http://localhost:8000"
//! cargo run --example vllm_example
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    ModelWorker, PromptRequest, SessionId, VllmWorker, spawn_pipeline,
};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    println!("🤖 vLLM Worker Example\n");
    println!("📝 Make sure vLLM server is running on http://localhost:8000\n");

    // Create vLLM worker
    let worker: Arc<dyn ModelWorker> = Arc::new(
        VllmWorker::new()
            .with_max_tokens(512)
            .with_temperature(0.7)
            .with_top_p(0.95),
    );

    // Spawn pipeline
    let handles = spawn_pipeline(worker);

    // Send test requests
    let requests = vec![
        ("Write a function to calculate fibonacci", "session-1"),
        ("Explain async/await in Rust", "session-2"),
        ("What is the fastest sorting algorithm?", "session-3"),
    ];

    for (input, session_id) in requests {
        let request = PromptRequest {
            session: SessionId::new(session_id),
            input: input.to_string(),
            meta: HashMap::new(),
        };

        println!("📤 Sending: {}", input);
        handles.input_tx.send(request).await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // Close and drain
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    println!("\n✅ Example complete!");

    Ok(())
}
