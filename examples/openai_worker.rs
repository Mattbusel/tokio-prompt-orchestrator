//! Example: Using OpenAI GPT models
//!
//! Run with:
//! ```bash
//! export OPENAI_API_KEY="sk-..."
//! cargo run --example openai_worker
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    spawn_pipeline, ModelWorker, OpenAiWorker, PromptRequest, SessionId,
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

    info!("🤖 OpenAI Worker Demo");

    // Create OpenAI worker
    // Uses GPT-3.5-turbo-instruct (completion model)
    // For chat models, use the Anthropic-style messages API
    let worker: Arc<dyn ModelWorker> = Arc::new(
        OpenAiWorker::new("gpt-3.5-turbo-instruct")?
            .with_max_tokens(256)
            .with_temperature(0.7),
    );

    info!("✅ Created OpenAI worker");

    // Spawn pipeline
    let handles = spawn_pipeline(worker);
    info!("✅ Pipeline spawned");

    // Send test requests
    let requests = vec![
        ("user-1", "Explain quantum entanglement in one sentence."),
        ("user-2", "What is the capital of France?"),
        ("user-3", "Write a haiku about AI."),
    ];

    info!("📨 Sending {} requests to OpenAI", requests.len());

    for (session_id, prompt) in requests {
        let request = PromptRequest {
            session: SessionId::new(session_id),
            input: prompt.to_string(),
            request_id: "example-req-0".to_string(),
            meta: HashMap::new(),
        };

        handles.input_tx.send(request).await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    info!("✅ All requests sent");

    // Close input and wait for drain
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    info!("🏁 Demo complete");

    Ok(())
}
