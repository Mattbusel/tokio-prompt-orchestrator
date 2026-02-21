//! Demo binary for tokio-prompt-orchestrator
//!
//! Spawns the 5-stage pipeline and sends test requests.

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    metrics, spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing subscriber
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .with_thread_ids(true)
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;

    // Initialize Prometheus metrics registry before any pipeline stage runs.
    metrics::init_metrics()?;

    info!("🚀 Starting tokio-prompt-orchestrator demo");

    // Create model worker (using echo worker for demo)
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(10));

    // Spawn the pipeline
    let handles = spawn_pipeline(worker);

    info!("✅ Pipeline stages spawned");

    // Send demo requests
    let demo_requests = vec![
        ("session-1", "What is the capital of France?"),
        ("session-2", "Explain quantum computing in simple terms"),
        ("session-3", "Write a haiku about programming"),
        ("session-1", "Follow-up: What about Germany?"),
        ("session-4", "How does photosynthesis work?"),
        ("session-5", "What are the benefits of exercise?"),
        ("session-6", "Describe the water cycle"),
        ("session-2", "Follow-up: Can you give an example?"),
        ("session-7", "What is machine learning?"),
        ("session-8", "Explain the theory of relativity"),
    ];

    info!("📨 Sending {} demo requests", demo_requests.len());

    for (session_id, input) in demo_requests {
        let request = PromptRequest {
            session: SessionId::new(session_id),
            input: input.to_string(),
            meta: {
                let mut meta = HashMap::new();
                meta.insert("timestamp".to_string(), chrono::Utc::now().to_rfc3339());
                meta.insert("client".to_string(), "demo-client".to_string());
                meta
            },
        };

        // Send with error handling
        if let Err(e) = handles.input_tx.send(request).await {
            tracing::error!(error = ?e, "Failed to send request");
            break;
        }

        // Small delay between requests to simulate realistic load
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    info!("✅ All requests sent");

    // Drop the sender to signal completion
    drop(handles.input_tx);

    // Wait for pipeline to drain
    info!("⏳ Waiting for pipeline to drain...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    info!("🏁 Demo complete - shutting down");

    // Graceful shutdown: wait for all stages
    // In production, you'd want timeout + force-kill logic
    tokio::select! {
        _ = handles.rag => info!("RAG stage finished"),
        _ = handles.assemble => info!("Assemble stage finished"),
        _ = handles.inference => info!("Inference stage finished"),
        _ = handles.post => info!("Post stage finished"),
        _ = handles.stream => info!("Stream stage finished"),
    }

    Ok(())
}
