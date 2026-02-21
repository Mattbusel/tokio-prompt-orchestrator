//! Example: Using OpenAI GPT models
//!
//! Set OPENAI_API_KEY environment variable before running:
//! ```bash
//! export OPENAI_API_KEY="sk-..."
//! cargo run --example openai_example
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    spawn_pipeline, ModelWorker, OpenAiWorker, PromptRequest, SessionId,
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

    println!("🤖 OpenAI Worker Example\n");

    // Create OpenAI worker
    let worker: Arc<dyn ModelWorker> = Arc::new(
        OpenAiWorker::new("gpt-3.5-turbo-instruct")?
            .with_max_tokens(100)
            .with_temperature(0.7),
    );

    // Spawn pipeline
    let handles = spawn_pipeline(worker);

    // Send test requests
    let requests = vec![
        ("What is the capital of France?", "session-1"),
        ("Explain photosynthesis briefly", "session-2"),
        ("Write a short poem about coding", "session-3"),
    ];

    for (input, session_id) in requests {
        let request = PromptRequest {
            session: SessionId::new(session_id),
            input: input.to_string(),
            request_id: "example-req-0".to_string(),
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
