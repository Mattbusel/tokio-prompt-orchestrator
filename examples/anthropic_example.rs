//! Example: Using Anthropic Claude API
//!
//! Set ANTHROPIC_API_KEY environment variable before running:
//! ```bash
//! export ANTHROPIC_API_KEY="sk-ant-..."
//! cargo run --example anthropic_example
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    AnthropicWorker, ModelWorker, PromptRequest, SessionId, spawn_pipeline,
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

    println!("🤖 Anthropic Claude Worker Example\n");

    // Create Anthropic worker
    let worker: Arc<dyn ModelWorker> = Arc::new(
        AnthropicWorker::new("claude-3-5-sonnet-20241022")
            .with_max_tokens(200)
            .with_temperature(1.0),
    );

    // Spawn pipeline
    let handles = spawn_pipeline(worker);

    // Send test requests
    let requests = vec![
        ("Explain quantum entanglement simply", "session-1"),
        ("What makes Rust memory-safe?", "session-2"),
        ("Describe a sunset in one sentence", "session-3"),
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
