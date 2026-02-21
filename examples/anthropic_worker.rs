//! Example: Using Anthropic Claude models
//!
//! Run with:
//! ```bash
//! export ANTHROPIC_API_KEY="sk-ant-..."
//! cargo run --example anthropic_worker
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    spawn_pipeline, AnthropicWorker, ModelWorker, PromptRequest, SessionId,
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

    info!("🤖 Anthropic Claude Worker Demo");

    // Create Anthropic worker
    // Available models:
    // - claude-3-5-sonnet-20241022 (latest, most capable)
    // - claude-3-opus-20240229
    // - claude-3-sonnet-20240229
    // - claude-3-haiku-20240307
    let worker: Arc<dyn ModelWorker> = Arc::new(
        AnthropicWorker::new("claude-3-5-sonnet-20241022")?
            .with_max_tokens(512)
            .with_temperature(1.0),
    );

    info!("✅ Created Anthropic Claude worker");

    // Spawn pipeline
    let handles = spawn_pipeline(worker);
    info!("✅ Pipeline spawned");

    // Send test requests
    let requests = vec![
        (
            "analyst-1",
            "Analyze the pros and cons of renewable energy.",
        ),
        (
            "writer-1",
            "Write a creative story about a robot learning to paint.",
        ),
        ("teacher-1", "Explain photosynthesis to a 10-year-old."),
    ];

    info!("📨 Sending {} requests to Claude", requests.len());

    for (session_id, prompt) in requests {
        let request = PromptRequest {
            session: SessionId::new(session_id),
            input: prompt.to_string(),
            meta: {
                let mut meta = HashMap::new();
                meta.insert("model".to_string(), "claude-3-5-sonnet".to_string());
                meta
            },
        };

        handles.input_tx.send(request).await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    info!("✅ All requests sent");

    // Close input and wait for drain
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    info!("🏁 Demo complete");

    Ok(())
}
