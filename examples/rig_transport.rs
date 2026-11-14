//! # Example: dedup, circuit breaker and retry under a rig 0.46 model
//!
//! Wraps the transport of a rig OpenAI chat model in
//! `OrchestratedTransport`, sends five identical requests at once, and
//! counts how many reached the provider (one).
//!
//! Works with OpenAI or any OpenAI-compatible server (llama.cpp, vLLM,
//! Ollama, LM Studio):
//!
//! ```text
//! OPENAI_API_KEY=sk-... cargo run --example rig_transport --features rig
//! OPENAI_BASE_URL=http://localhost:8080/v1 OPENAI_API_KEY=none MODEL=local \
//!     cargo run --example rig_transport --features rig
//! ```
//!
//! Features needed: `rig` (Rust 1.97.1 or newer).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use rig_core::completion::CompletionRequest;
use rig_core::driver::{Exchange, Model, Opening, Transport};
use rig_core::http_client::framing::WireFrame;
use rig_core::providers::openai::OpenAIConfig;
use rig_core::wire::{Encoded, Wire};
use tokio_prompt_orchestrator::integrations::OrchestratedTransport;

/// Counts the requests that actually leave the process.
#[derive(Clone)]
struct Counted<T> {
    inner: T,
    sent: Arc<AtomicUsize>,
}

impl<W, T> Transport<W> for Counted<T>
where
    W: Wire<Payload = Encoded, Frame = WireFrame>,
    T: Transport<W>,
{
    fn send(&self, payload: Encoded, exchange: Exchange) -> Opening<WireFrame> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        self.inner.send(payload, exchange)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(key) = std::env::var("OPENAI_API_KEY") else {
        println!(
            "Set OPENAI_API_KEY (and optionally OPENAI_BASE_URL and MODEL) to run this example."
        );
        return Ok(());
    };
    let mut config = OpenAIConfig::new(key);
    if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
        config = config.with_base_url(base_url);
    }
    let model_name = std::env::var("MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    // The model you already have...
    let plain = config.client().chat(model_name);

    // ...with its transport wrapped. Nothing else about the model changes:
    // messages, tools, temperature and max_tokens go out as rig encodes them.
    let sent = Arc::new(AtomicUsize::new(0));
    let counted = Counted {
        inner: plain.transport,
        sent: Arc::clone(&sent),
    };
    let model = Model::new(plain.wire, OrchestratedTransport::new(counted));

    let request = || {
        CompletionRequest::new("In one sentence: why do circuit breakers help LLM apps?")
            .max_tokens(Some(80))
    };

    let started = Instant::now();
    let replies = futures::future::join_all((0..5).map(|_| model.call(request()))).await;
    let elapsed = started.elapsed();

    for (i, reply) in replies.into_iter().enumerate() {
        match reply {
            Ok(reply) => println!("caller {i}: {}", reply.text().trim()),
            Err(e) => println!("caller {i}: error: {e}"),
        }
    }
    println!(
        "\n5 concurrent identical requests, {} sent upstream, {:.0?} total.",
        sent.load(Ordering::SeqCst),
        elapsed
    );
    Ok(())
}
