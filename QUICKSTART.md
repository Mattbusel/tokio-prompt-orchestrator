# Quick Start Guide

## Installation

Add to `Cargo.toml`:

```toml
[dependencies]
tokio-prompt-orchestrator = "0.1"
```

## Basic Usage

### 1. Create a Worker

```rust
use tokio_prompt_orchestrator::{EchoWorker, ModelWorker};
use std::sync::Arc;

let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
```

### 2. Spawn Pipeline

```rust
use tokio_prompt_orchestrator::spawn_pipeline;

let handles = spawn_pipeline(worker);
```

### 3. Send Requests

```rust
use tokio_prompt_orchestrator::{PromptRequest, SessionId};
use std::collections::HashMap;

let request = PromptRequest {
    session: SessionId::new("user-123"),
    input: "What is Rust?".to_string(),
    meta: HashMap::new(),
};

handles.input_tx.send(request).await?;
```

### 4. Graceful Shutdown

```rust
// Close input channel
drop(handles.input_tx);

// Wait for drain
tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

// Wait for tasks
handles.rag.await?;
```

## Custom Model Worker

### Step 1: Implement Trait

```rust
use async_trait::async_trait;
use tokio_prompt_orchestrator::{ModelWorker, OrchestratorError};

struct MyLlamaWorker {
    client: LlamaClient,
}

#[async_trait]
impl ModelWorker for MyLlamaWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let response = self.client
            .complete(prompt)
            .await
            .map_err(|e| OrchestratorError::Inference(e.to_string()))?;
        
        Ok(response.tokens)
    }
}
```

### Step 2: Use in Pipeline

```rust
let worker = Arc::new(MyLlamaWorker {
    client: LlamaClient::new("http://localhost:8000"),
});

let handles = spawn_pipeline(worker);
```

## Session Affinity

```rust
use tokio_prompt_orchestrator::shard_session;

let session = SessionId::new("user-456");
let shard = shard_session(&session, 4); // 0-3

// Route to worker pool
let worker = worker_pool[shard].clone();
```

## Monitoring

### Enable Tracing

```rust
use tracing_subscriber;

tracing_subscriber::fmt()
    .with_max_level(tracing::Level::INFO)
    .init();
```

### Watch Metrics

```bash
cargo run --bin orchestrator-demo | grep metric=
```

Output:
```
INFO metric=stage_latency_ms stage=rag latency_ms=6
INFO metric=stage_latency_ms stage=inference latency_ms=152
INFO metric=queue_depth stage=inference depth=23
```

## Configuration

### Channel Sizes

```rust
// Modify in stages.rs:
let (inference_tx, inference_rx) = mpsc::channel::<InferenceOutput>(2048); // was 1024
```

### Worker Delay

```rust
let worker = Arc::new(EchoWorker::with_delay(20)); // 20ms
```

## Common Patterns

### Multiple Sessions

```rust
for user_id in user_ids {
    let request = PromptRequest {
        session: SessionId::new(format!("user-{}", user_id)),
        input: prompts.get(&user_id).unwrap().clone(),
        meta: HashMap::new(),
    };
    handles.input_tx.send(request).await?;
}
```

### Metadata Passing

```rust
let mut meta = HashMap::new();
meta.insert("user_id".to_string(), "123".to_string());
meta.insert("org_id".to_string(), "acme".to_string());
meta.insert("priority".to_string(), "high".to_string());

let request = PromptRequest {
    session: SessionId::new("sess-001"),
    input: "...".to_string(),
    meta,
};
```

### Backpressure Handling

```rust
match handles.input_tx.try_send(request) {
    Ok(_) => info!("sent"),
    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
        warn!("input queue full, backing off");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
        error!("pipeline closed");
        break;
    }
}
```

## Troubleshooting

### "Channel closed" errors

**Cause**: A stage panicked or shut down unexpectedly

**Fix**: Check logs for panics, add error handling in worker

### Requests timing out

**Cause**: Inference stage too slow, queue building up

**Fix**: Increase inference channel size, add more workers, or enable shedding

### High memory usage

**Cause**: Channels too large, requests accumulating

**Fix**: Reduce channel sizes, enable backpressure, add memory limits

### No output

**Cause**: Stream stage not running or output not configured

**Fix**: Check stream stage logs, ensure pipeline spawned correctly

## Next Steps

- Read [ARCHITECTURE.md](ARCHITECTURE.md) for deep dive
- Explore `pipeline.example.toml` for future config format
- Implement custom ModelWorker for your inference backend
- Add metrics backend (Prometheus/OTLP)
- Deploy with multi-node NATS mesh

## Examples

See `src/main.rs` for a complete working example.

Run with:
```bash
cargo run --bin orchestrator-demo
```
