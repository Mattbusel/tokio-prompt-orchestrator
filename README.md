# tokio-prompt-orchestrator

A production-leaning orchestrator for multi-stage LLM pipelines over Tokio with bounded channels, backpressure, and metrics hooks.

## Architecture

Five-stage pipeline with bounded channels:

```text
PromptRequest → RAG(512) → Assemble(512) → Inference(1024) → Post(512) → Stream(256)
                  ↓          ↓               ↓                ↓            ↓
              5ms delay   format          ModelWorker      join tokens   emit
```

### Stages

1. **RAG** - Retrieval-Augmented Generation (document retrieval, context injection)
2. **Assemble** - Prompt construction (templates, few-shot examples)
3. **Inference** - LLM execution via `ModelWorker` trait
4. **Post** - Post-processing (formatting, safety, PII redaction)
5. **Stream** - Output emission (SSE, WebSocket, gRPC)

### Key Features

- **Bounded Channels** - Configurable buffer sizes prevent memory exhaustion
- **Backpressure** - Graceful shedding on queue full (try_send + drop strategy)
- **Session Affinity** - Hash-based sharding for per-core pinning (roadmap)
- **Pluggable Workers** - `ModelWorker` trait for any inference backend
- **Metrics Hooks** - Tracing-based (swap to Prometheus/OTLP via feature flags)
- **Clean Shutdown** - Graceful drain on input channel close

## Usage

### Basic Example

```rust
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    EchoWorker, ModelWorker, PromptRequest, SessionId, spawn_pipeline,
};

#[tokio::main]
async fn main() {
    // Create a model worker
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    
    // Spawn the pipeline
    let handles = spawn_pipeline(worker);
    
    // Send requests
    let request = PromptRequest {
        session: SessionId::new("session-1"),
        input: "Hello, world!".to_string(),
        meta: Default::default(),
    };
    
    handles.input_tx.send(request).await.unwrap();
    
    // Close input and wait for drain
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
}
```

### Custom Model Worker

```rust
use async_trait::async_trait;
use tokio_prompt_orchestrator::{ModelWorker, OrchestratorError};

struct MyCustomWorker {
    // Your model state
}

#[async_trait]
impl ModelWorker for MyCustomWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        // Your inference logic
        Ok(vec!["token1".to_string(), "token2".to_string()])
    }
}
```

## Running the Demo

```bash
cargo run --bin orchestrator-demo
```

The demo sends 10 requests through the pipeline and displays tracing output.

## Testing

```bash
cargo test
```

## Roadmap

### Phase 1: Core Stability (MVP) ✅
- [x] 5-stage pipeline
- [x] Bounded channels with backpressure
- [x] ModelWorker trait
- [x] Basic metrics hooks
- [x] Session affinity sharding

### Phase 2: Production Hardening
- [ ] gRPC worker protocol (streaming tokens, cancellation, deadlines)
- [ ] Prometheus/OTLP metrics via feature flags
- [ ] Per-core runtime pinning with session affinity
- [ ] Dead-letter queue for failed requests
- [ ] Circuit breakers per worker
- [ ] Request tracing with OpenTelemetry

### Phase 3: Distributed Scale
- [ ] Distributed mesh (NATS/Kafka) for cross-node queues
- [ ] Declarative DAG config (.toml / .yaml)
- [ ] Dynamic worker pools with auto-scaling
- [ ] Multi-model routing (A/B testing, load shedding)
- [ ] Quota management per session/tenant

### Phase 4: Advanced Features
- [ ] Streaming inference support
- [ ] Batch processing mode
- [ ] Result caching layer
- [ ] Request deduplication
- [ ] Priority queues

## Performance Considerations

### Channel Sizing

Current sizes are optimized for moderate throughput:
- RAG → Assemble: 512 (fast stage, small buffer)
- Assemble → Inference: 512 (pre-inference buffer)
- Inference → Post: 1024 (inference is slowest, needs larger buffer)
- Post → Stream: 512 (post-processing buffer)

Tune based on your latency profile:
- Higher inference latency → increase inference input buffer
- Bursty traffic → increase all buffers
- Memory constrained → decrease all buffers

### Session Affinity

The `shard_session()` function enables per-core pinning:

```rust
let shard = shard_session(&request.session, num_cores);
// TODO: Pin task to runtime shard
```

This reduces cache misses and improves locality for stateful workers.

## License

MIT OR Apache-2.0

## Contributing

Contributions welcome! Please:
1. Add tests for new features
2. Update roadmap checkboxes
3. Follow existing code style
4. Add tracing spans for observability
