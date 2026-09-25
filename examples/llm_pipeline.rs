//! # Example: an LLM pipeline end to end
//!
//! Runs prompts through the five-stage pipeline (retrieve, assemble, infer,
//! post-process, stream) and shows the two things that save money and
//! keep you online:
//!
//! 1. **Deduplication**: 12 requests from 4 users asking 3 distinct questions
//!    reach the model only 3 times.
//! 2. **Circuit breaker + dead-letter queue**: during a simulated provider
//!    outage the breaker opens after 5 failures, later requests fail fast
//!    instead of piling up, and every dropped request lands in the DLQ.
//!
//! Run with no API key (uses a built-in mock model):
//!
//! ```bash
//! cargo run --example llm_pipeline
//! ```
//!
//! Run against a real provider (makes 3 small API calls):
//!
//! ```bash
//! PROVIDER=anthropic ANTHROPIC_API_KEY=sk-ant-... cargo run --example llm_pipeline
//! PROVIDER=openai    OPENAI_API_KEY=sk-...        cargo run --example llm_pipeline
//! ```
//!
//! Optional: `MODEL=<name>` overrides the default model for the provider.
//! Features needed: none.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_prompt_orchestrator::enhanced::{dedup_key, DeduplicationResult, Deduplicator};
use tokio_prompt_orchestrator::{
    spawn_pipeline, AnthropicWorker, ModelWorker, OpenAiWorker, OrchestratorError, PostOutput,
    PromptRequest, SessionId,
};

// ── Mock model: no network, no key ───────────────────────────────────────────

/// Answers a few known questions with canned text after a realistic delay.
struct MockLlm;

#[async_trait]
impl ModelWorker for MockLlm {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let answer = if prompt.contains("capital of France") {
            "The capital of France is Paris."
        } else if prompt.contains("backpressure") {
            "Backpressure means a slow consumer makes fast producers wait instead of letting queues grow without bound."
        } else if prompt.contains("haiku") {
            "Bounded channels fill / the sender waits its turn now / memory stays calm"
        } else {
            "I am a mock model. Set PROVIDER to use a real one."
        };
        Ok(answer.split_whitespace().map(str::to_string).collect())
    }
}

// ── Wrapper: dedup + call counting + outage switch ───────────────────────────

/// Sits between the pipeline and the real backend.
///
/// - Identical prompts share one backend call (`Deduplicator`).
/// - Counts how many calls actually reach the backend.
/// - `outage` makes every call fail without touching the backend, so the
///   circuit breaker demo costs nothing even with a real provider.
struct Frontline {
    backend: Arc<dyn ModelWorker>,
    dedup: Deduplicator,
    backend_calls: AtomicUsize,
    outage: AtomicBool,
}

#[async_trait]
impl ModelWorker for Frontline {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        if self.outage.load(Ordering::SeqCst) {
            return Err(OrchestratorError::Inference(
                "503 Service Unavailable (simulated outage)".into(),
            ));
        }

        let key = dedup_key(prompt, &HashMap::new(), None);
        match self.dedup.check_and_register(&key).await {
            DeduplicationResult::Cached(json) => decode(&json),
            DeduplicationResult::InProgress => match self.dedup.wait_for_result(&key).await {
                Some(json) => decode(&json),
                None => self.backend.infer(prompt).await,
            },
            DeduplicationResult::New(token) => {
                self.backend_calls.fetch_add(1, Ordering::SeqCst);
                match self.backend.infer(prompt).await {
                    Ok(tokens) => {
                        let json = serde_json::to_string(&tokens).unwrap_or_default();
                        self.dedup.complete(token, json).await;
                        Ok(tokens)
                    }
                    Err(e) => {
                        self.dedup.fail(token).await;
                        Err(e)
                    }
                }
            }
        }
    }
}

fn decode(json: &str) -> Result<Vec<String>, OrchestratorError> {
    serde_json::from_str(json).map_err(|e| OrchestratorError::Other(e.to_string()))
}

fn choose_backend() -> Result<(Arc<dyn ModelWorker>, String), OrchestratorError> {
    let provider = std::env::var("PROVIDER").unwrap_or_else(|_| "mock".into());
    let model = std::env::var("MODEL").ok();
    match provider.as_str() {
        "anthropic" => {
            let model = model.unwrap_or_else(|| "claude-sonnet-4-6".into());
            let w = AnthropicWorker::new(model.clone())?.with_max_tokens(120);
            Ok((Arc::new(w), format!("anthropic ({model})")))
        }
        "openai" => {
            let model = model.unwrap_or_else(|| "gpt-4o-mini".into());
            let w = OpenAiWorker::new(model.clone())?.with_max_tokens(120);
            Ok((Arc::new(w), format!("openai ({model})")))
        }
        "mock" => Ok((Arc::new(MockLlm), "mock model (no network)".into())),
        other => Err(OrchestratorError::ConfigError(format!(
            "unknown PROVIDER '{other}': use mock, anthropic or openai"
        ))),
    }
}

fn request(user: &str, id: usize, input: &str) -> PromptRequest {
    PromptRequest {
        session: SessionId::new(user),
        request_id: format!("req-{id:02}"),
        input: input.to_string(),
        meta: HashMap::new(),
        deadline: None,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (backend, label) = choose_backend()?;
    println!("Backend: {label}\n");

    let front = Arc::new(Frontline {
        backend,
        dedup: Deduplicator::new(Duration::from_secs(300)),
        backend_calls: AtomicUsize::new(0),
        outage: AtomicBool::new(false),
    });
    let handles = spawn_pipeline(front.clone() as Arc<dyn ModelWorker>);
    let mut output = handles
        .take_output_rx()
        .await
        .ok_or("pipeline output already taken")?;

    // ── 1. Deduplication ─────────────────────────────────────────────────────
    let questions = [
        "What is the capital of France? Answer in one sentence.",
        "Explain backpressure in one sentence.",
        "Write a haiku about bounded channels.",
    ];
    let users = ["alice", "bob", "carol", "dave"];

    println!("1) 12 requests: 4 users x 3 questions");
    let started = Instant::now();
    let mut n = 0;
    for user in users {
        for q in questions {
            n += 1;
            handles.input_tx.send(request(user, n, q)).await?;
        }
    }

    let mut answers: Vec<PostOutput> = Vec::new();
    while answers.len() < n {
        match tokio::time::timeout(Duration::from_secs(60), output.recv()).await {
            Ok(Some(out)) => answers.push(out),
            _ => break,
        }
    }
    answers.sort_by(|a, b| a.request_id.cmp(&b.request_id));
    for out in &answers {
        println!(
            "   {} {:<5}  {}",
            out.request_id,
            out.session.as_str(),
            out.text
        );
    }
    let calls = front.backend_calls.load(Ordering::SeqCst);
    println!(
        "   -> {} answers in {:.1}s, {} model calls ({} saved by dedup)\n",
        answers.len(),
        started.elapsed().as_secs_f64(),
        calls,
        answers.len().saturating_sub(calls)
    );

    // ── 2. Outage: circuit breaker + dead-letter queue ───────────────────────
    println!("2) Provider outage: 8 new requests while every call fails");
    front.outage.store(true, Ordering::SeqCst);
    for i in 0..8 {
        n += 1;
        handles
            .input_tx
            .send(request("erin", n, &format!("Outage request #{i}")))
            .await?;
    }

    // Failed requests produce no output; wait until all 8 are in the DLQ.
    let deadline = Instant::now() + Duration::from_secs(10);
    while handles.dlq.len() < 8 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for dropped in handles.dlq.drain() {
        let reason = if dropped.reason == "circuit_breaker_open" {
            "circuit open, failed fast (provider not called)".to_string()
        } else {
            dropped.reason
        };
        println!("   DLQ {}  {}", dropped.request_id, reason);
    }
    let cb = handles.circuit_breaker.stats().await;
    println!(
        "   -> breaker is {:?}; it lets one probe through after 60s to test recovery",
        cb.status
    );

    handles.shutdown();
    Ok(())
}
