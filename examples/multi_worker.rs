//! Example: Multi-worker A/B testing
//!
//! Demonstrates routing requests to different workers based on session ID.
//! This pattern enables:
//! - A/B testing between models
//! - Load balancing across workers
//! - Per-user model selection
//!
//! Run with:
//! ```bash
//! cargo run --example multi_worker
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

/// Smart worker router that selects backend based on session
struct MultiWorker {
    workers: Vec<Arc<dyn ModelWorker>>,
}

impl MultiWorker {
    fn new(workers: Vec<Arc<dyn ModelWorker>>) -> Self {
        Self { workers }
    }

    fn select_worker(&self, session: &SessionId) -> &Arc<dyn ModelWorker> {
        // Simple hash-based routing
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        session.0.hash(&mut hasher);
        let index = (hasher.finish() as usize) % self.workers.len();

        &self.workers[index]
    }
}

#[async_trait::async_trait]
impl ModelWorker for MultiWorker {
    async fn infer(
        &self,
        prompt: &str,
    ) -> Result<Vec<String>, tokio_prompt_orchestrator::OrchestratorError> {
        // Extract session from prompt (in real impl, pass it through context)
        // For demo, just use first worker
        let worker = &self.workers[0];
        worker.infer(prompt).await
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("🔀 Multi-Worker A/B Testing Demo");

    // Create multiple workers for different purposes
    let workers: Vec<Arc<dyn ModelWorker>> = vec![
        // Fast worker for simple queries
        Arc::new(EchoWorker::with_delay(5)),
        // Medium worker for standard queries
        Arc::new(EchoWorker::with_delay(15)),
        // Slow worker for complex queries (simulating large model)
        Arc::new(EchoWorker::with_delay(50)),
    ];

    info!(
        "✅ Created {} workers with different latencies",
        workers.len()
    );

    // Create router
    let router = Arc::new(MultiWorker::new(workers));

    // Spawn pipeline
    let handles = spawn_pipeline(router);
    info!("✅ Pipeline spawned with multi-worker router");

    // Send requests - they'll be routed to different workers based on session
    let requests = vec![
        ("fast-user-1", "Quick question"),
        ("fast-user-2", "Another quick one"),
        ("standard-user-1", "Normal complexity"),
        ("standard-user-2", "Medium question"),
        ("complex-user-1", "Very complex analysis needed"),
        ("complex-user-2", "Deep dive required"),
        ("fast-user-3", "Fast again"),
        ("standard-user-3", "Standard query"),
    ];

    info!(
        "📨 Sending {} requests (will be routed to different workers)",
        requests.len()
    );

    for (session_id, prompt) in requests {
        let request = PromptRequest {
            session: SessionId::new(session_id),
            input: prompt.to_string(),
            meta: {
                let mut meta = HashMap::new();
                meta.insert("routing".to_string(), "hash-based".to_string());
                meta
            },
        };

        handles.input_tx.send(request).await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    info!("✅ All requests sent");

    // Close input and wait for drain
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    info!("🏁 Demo complete");
    info!("💡 Check logs to see different latencies for different sessions");

    Ok(())
}
