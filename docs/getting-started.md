# Getting Started

## Prerequisites

- Rust 1.85 or later
- A running Tokio runtime (the crate is async-only)

## Add the dependency

```toml
[dependencies]
tokio-prompt-orchestrator = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

## Your first pipeline

The simplest pipeline uses `EchoWorker`, which echoes the prompt back as
tokens — no API key required.

```rust
use std::collections::HashMap;
use std::sync::Arc;

use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create a worker.  Replace EchoWorker with OpenAiWorker for production.
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());

    // 2. Spawn the five-stage pipeline.
    let handles = spawn_pipeline(worker);

    // 3. Submit a request.
    handles.input_tx.send(PromptRequest {
        session:    SessionId::new("demo-session"),
        request_id: "req-001".to_string(),
        input:      "Hello, pipeline!".to_string(),
        meta:       HashMap::new(),
        deadline:   None,
    }).await?;

    // 4. Read the first response.
    let mut guard = handles.output_rx.lock().await;
    if let Some(rx) = guard.as_mut() {
        if let Some(output) = rx.recv().await {
            println!("Response: {}", output.text);
        }
    }

    Ok(())
}
```

Expected output:

```
Response: Hello, pipeline!
```

## Using a real LLM provider

### OpenAI

```rust
use tokio_prompt_orchestrator::{OpenAiWorker, ModelWorker};

std::env::set_var("OPENAI_API_KEY", "sk-...");
let worker = OpenAiWorker::new("gpt-4o")?;
```

### Anthropic

```rust
use tokio_prompt_orchestrator::AnthropicWorker;

std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-...");
let worker = AnthropicWorker::new("claude-3-5-sonnet-20241022")?;
```

### Local llama.cpp

```rust
use tokio_prompt_orchestrator::LlamaCppWorker;

// defaults to http://localhost:8080
let worker = LlamaCppWorker::new("llama3");
```

## Pipeline with config file

```rust
use std::sync::Arc;
use tokio_prompt_orchestrator::{spawn_pipeline_with_config, EchoWorker, ModelWorker};
use tokio_prompt_orchestrator::config::loader::load_config;

let config = load_config("pipeline.toml")?;
let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
let handles = spawn_pipeline_with_config(worker, config);
```

Minimal `pipeline.toml`:

```toml
[pipeline]
name    = "dev"
version = "1.0"

[stages.rag]
enabled = true

[stages.assemble]
enabled = true

[stages.inference]
worker = "echo"
model  = "echo"

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts             = 3
circuit_breaker_threshold  = 5
circuit_breaker_timeout_s  = 60
circuit_breaker_success_rate = 0.8

[deduplication]
enabled = false

[observability]
log_format = "pretty"
```

## Adding resilience primitives

```rust
use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::{
    Deduplicator, CacheLayer, CircuitBreaker, RetryPolicy,
    dedup_key, cache_key,
};

let dedup   = Deduplicator::new(Duration::from_secs(300));
let cache   = CacheLayer::new_memory(1_000);
let cb      = CircuitBreaker::new(5, 0.8, Duration::from_secs(30));
let policy  = RetryPolicy::exponential(3, Duration::from_millis(100));
```

See [primitives.md](primitives.md) for a full composition example.

## Initialising tracing

```rust
tokio_prompt_orchestrator::init_tracing();
```

Set `RUST_LOG=debug` for verbose output, `RUST_LOG_FORMAT=json` for structured
JSON logs suitable for log aggregation.

## Next steps

- [primitives.md](primitives.md) — detailed guide to all enhanced primitives
- [configuration.md](configuration.md) — all config fields with defaults
- [architecture.md](architecture.md) — pipeline design and module map
- [API docs](https://docs.rs/tokio-prompt-orchestrator) — full rustdoc
