//! Example: Using local llama.cpp server
//!
//! Start llama.cpp server first:
//! ```bash
//! # Download and run llama.cpp server
//! ./server -m models/llama-2-7b.gguf --port 8080
//!
//! # Then run this example
//! cargo run --example llama_cpp_example
//! ```
//!
//! Or set custom URL:
//! ```bash
//! export LLAMA_CPP_URL="http://localhost:8080"
//! cargo run --example llama_cpp_example
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    LlamaCppWorker, ModelWorker, PromptRequest, SessionId, spawn_pipeline,
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

    println!("🤖 llama.cpp Worker Example\n");
    println!("📝 Make sure llama.cpp server is running on http://localhost:8080\n");

    // Create llama.cpp worker
    let worker: Arc<dyn ModelWorker> = Arc::new(
        LlamaCppWorker::new()
            .with_max_tokens(256)
            .with_temperature(0.8),
    );

    // Spawn pipeline
    let handles = spawn_pipeline(worker);

    // Send test requests
    let requests = vec![
        ("Once upon a time", "session-1"),
        ("The best programming language is", "session-2"),
        ("Artificial intelligence will", "session-3"),
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
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    println!("\n✅ Example complete!");

    Ok(())
}
