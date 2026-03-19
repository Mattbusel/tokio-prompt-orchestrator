//! # Example: Custom Worker
//!
//! Demonstrates: How to implement the `ModelWorker` trait for your own backend.
//!
//! Run with: `cargo run --example custom_worker`
//! Features needed: none
//!
//! This example implements `ReverseWorker` — a toy worker that reverses every
//! word in the input prompt and returns the result as tokens.  It shows every
//! method you may need to override when building a real custom backend
//! (e.g. a local model server, a proprietary API, or a mock for testing).

use async_trait::async_trait;
use futures::stream;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_prompt_orchestrator::{
    spawn_pipeline, worker::TokenStream, EchoWorker, ModelWorker, OrchestratorError, PromptRequest,
    SessionId,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

// ── Custom worker implementation ─────────────────────────────────────────────

/// A toy worker that reverses every word in the input prompt.
///
/// In a real implementation you would replace the body of `infer` with an HTTP
/// request to your model server, a call into a Rust-native model library, or
/// any other async operation.
pub struct ReverseWorker {
    /// Simulated inference latency in milliseconds.
    latency_ms: u64,
    /// Optional prefix prepended to every response.
    response_prefix: String,
}

impl ReverseWorker {
    /// Create a new `ReverseWorker` with the given latency and prefix.
    pub fn new(latency_ms: u64, response_prefix: impl Into<String>) -> Self {
        Self {
            latency_ms,
            response_prefix: response_prefix.into(),
        }
    }
}

#[async_trait]
impl ModelWorker for ReverseWorker {
    /// The core inference method.
    ///
    /// # Contract
    /// - Must return `Ok(Vec<String>)` where each element is a token.
    /// - Must return `Err(OrchestratorError)` on failure (never panic).
    /// - Must be `Send + Sync` — it will be called from multiple Tokio tasks
    ///   concurrently (one per pipeline instance or parallel inference worker).
    ///
    /// # Token granularity
    /// You can return one token per word, one per character, or one for the
    /// entire response — the post-processing stage joins them with spaces.
    /// Word-level granularity (one element per word) is the most common choice.
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        // Validate input — return a typed error instead of panicking.
        if prompt.is_empty() {
            return Err(OrchestratorError::Inference(
                "empty prompt".to_string(),
            ));
        }

        // Simulate network / model latency.
        if self.latency_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.latency_ms)).await;
        }

        // Core logic: reverse each word, then return as a token vector.
        let reversed_words: Vec<String> = prompt
            .split_whitespace()
            .map(|w| w.chars().rev().collect())
            .collect();

        let mut tokens = Vec::new();

        // Prepend the optional response prefix as its own token group.
        if !self.response_prefix.is_empty() {
            tokens.push(self.response_prefix.clone());
        }

        tokens.extend(reversed_words);
        Ok(tokens)
    }

    /// Optional: override `infer_stream` for true token-by-token streaming.
    ///
    /// The default implementation calls `infer` and streams the resulting
    /// tokens one by one.  Override this method only if your backend natively
    /// supports SSE or chunked streaming (e.g. via an HTTP/2 stream).
    ///
    /// Here we show a manual override that yields tokens with a small inter-
    /// token delay to simulate a streaming model.
    async fn infer_stream(&self, prompt: &str) -> Result<TokenStream, OrchestratorError> {
        let tokens = self.infer(prompt).await?;

        // Yield one token every 20 ms to simulate incremental delivery.
        let delay = Duration::from_millis(20);
        let stream = stream::unfold(
            (tokens.into_iter(), delay),
            |(mut iter, d)| async move {
                let token = iter.next()?;
                tokio::time::sleep(d).await;
                Some((Ok(token), (iter, d)))
            },
        );

        Ok(Box::pin(stream))
    }
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Custom Worker Demo");
    info!("==================");
    info!("");
    info!("ReverseWorker reverses every word in the prompt.");
    info!("It is plugged into a real pipeline — identical to using EchoWorker.");
    info!("");

    // ── Direct usage (without the full pipeline) ──────────────────────────
    info!("Direct worker test (no pipeline):");
    let worker = ReverseWorker::new(0, "[reversed]");

    let result = worker.infer("Hello World from Rust").await?;
    info!("  Input:  \"Hello World from Rust\"");
    info!("  Tokens: {:?}", result);
    info!("  Joined: \"{}\"", result.join(" "));
    info!("");

    // ── Full pipeline usage ───────────────────────────────────────────────
    info!("Pipeline usage:");
    info!("  Plugging ReverseWorker into spawn_pipeline() — same API as EchoWorker.");

    let worker_arc: Arc<dyn ModelWorker> = Arc::new(ReverseWorker::new(50, ""));
    let handles = spawn_pipeline(worker_arc);

    // Subscribe to output before sending requests.
    let mut output_guard = handles.output_rx.lock().await;
    let output_rx = output_guard.as_mut().expect("output receiver missing");

    let prompts = [
        "The quick brown fox",
        "Pipeline orchestration in Rust",
        "Custom workers are easy",
    ];

    for (i, prompt) in prompts.iter().enumerate() {
        let req = PromptRequest {
            session: SessionId::new(format!("demo-session-{i}")),
            request_id: format!("demo-req-{i}"),
            input: prompt.to_string(),
            meta: HashMap::new(),
            deadline: None,
        };

        handles.input_tx.send(req).await?;
        info!("  Sent:     \"{}\"", prompt);

        if let Some(output) = output_rx.recv().await {
            info!("  Received: \"{}\"", output.text);
            info!("");
        }
    }

    drop(output_guard);
    drop(handles.input_tx);

    // ── Side-by-side comparison with EchoWorker ───────────────────────────
    info!("Comparison: EchoWorker vs ReverseWorker for the same prompt");
    info!("-------------------------------------------------------------");

    let echo: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let reverse: Arc<dyn ModelWorker> = Arc::new(ReverseWorker::new(0, ""));

    let test_prompt = "Tokio async runtime";
    let echo_tokens = echo.infer(test_prompt).await?;
    let reverse_tokens = reverse.infer(test_prompt).await?;

    info!("  Prompt:         \"{}\"", test_prompt);
    info!("  EchoWorker:     {:?}", echo_tokens);
    info!("  ReverseWorker:  {:?}", reverse_tokens);

    info!("");
    info!("Key implementation points:");
    info!("  1. Implement the `ModelWorker` trait from tokio_prompt_orchestrator");
    info!("  2. Add `#[async_trait]` — the trait uses async methods");
    info!("  3. Return `OrchestratorError` variants, never panic");
    info!("  4. Make your struct `Send + Sync` (required for Arc<dyn ModelWorker>)");
    info!("  5. Override `infer_stream` only if your backend supports native streaming");

    Ok(())
}
