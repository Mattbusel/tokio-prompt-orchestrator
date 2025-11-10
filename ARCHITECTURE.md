# Architecture Documentation

## Overview

`tokio-prompt-orchestrator` implements a staged event-driven architecture (SEDA) for LLM inference pipelines. Each stage is an independent async task communicating via bounded MPSC channels.

## Pipeline Flow

```text
┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Client    │────▶│     RAG     │────▶│  Assemble   │────▶│  Inference  │────▶│    Post     │
└─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘
                           │                    │                    │                    │
                           │                    │                    │                    │
                           ▼                    ▼                    ▼                    ▼
                      Vector DB           Template            Model Worker          Filtering
                      Retrieval           Engine              (vLLM/llama.cpp)      PII Redaction
                                                                                    
                                                                                    ▼
                                                                            ┌─────────────┐
                                                                            │   Stream    │────▶ Client
                                                                            └─────────────┘
                                                                                    │
                                                                                    ▼
                                                                            SSE/WebSocket
```

## Channel Sizing Strategy

### Current Configuration

| Channel | Size | Rationale |
|---------|------|-----------|
| RAG → Assemble | 512 | Fast stage, minimal buffering needed |
| Assemble → Inference | 512 | Pre-inference buffer, moderate size |
| Inference → Post | 1024 | **Largest buffer** - inference is slowest stage |
| Post → Stream | 512 | Post-processing buffer |
| Stream output | 256 | Small output buffer, fast emission |

### Sizing Principles

1. **Bottleneck Buffering**: Largest buffer before the slowest stage (inference)
2. **Fast Stage Economy**: Small buffers for fast stages to save memory
3. **Backpressure Control**: All buffers bounded to prevent OOM under surge

### Tuning Guidelines

```rust
// High-latency inference? Increase input buffer
let (inference_tx, inference_rx) = mpsc::channel::<AssembleOutput>(2048);

// Bursty traffic? Increase all buffers proportionally
let scale_factor = 2;
let (rag_tx, rag_rx) = mpsc::channel::<RagOutput>(512 * scale_factor);

// Memory constrained? Decrease uniformly
let scale_factor = 0.5;
let (post_tx, post_rx) = mpsc::channel::<PostOutput>((512 as f32 * scale_factor) as usize);
```

## Backpressure Mechanism

### Graceful Shedding

When a channel is full, we use `try_send` instead of blocking:

```rust
match tx.try_send(item) {
    Ok(_) => Ok(()),
    Err(TrySendError::Full(_)) => {
        // Log and drop the NEW item (oldest stays in queue)
        warn!("queue full, shedding request");
        Ok(())
    }
    Err(TrySendError::Closed(_)) => Err(ChannelClosed),
}
```

### Why Drop Instead of Block?

1. **Prevents Cascading Delays**: Blocking would slow down all upstream stages
2. **Explicit Failure Mode**: Dropped requests are logged and can be monitored
3. **Maintains Throughput**: System continues processing existing requests
4. **Predictable Behavior**: No unbounded queueing leading to OOM

### Alternative Strategies (Roadmap)

```rust
// Dead-letter queue
Err(TrySendError::Full(item)) => {
    dlq_tx.send(item).await?;
    metrics::record_dlq_enqueue("inference");
}

// Exponential backoff
Err(TrySendError::Full(item)) => {
    for attempt in 0..3 {
        tokio::time::sleep(Duration::from_millis(10 * 2_u64.pow(attempt))).await;
        if tx.try_send(item.clone()).is_ok() {
            return Ok(());
        }
    }
    warn!("shed after retries");
}

// Priority queue (high-priority requests bypass)
if request.priority == Priority::High {
    tx.send(request).await?; // Block for high-pri
} else {
    send_with_shed(&tx, request, "stage")?;
}
```

## Session Affinity

### Current Implementation

```rust
pub fn shard_session(session: &SessionId, shards: usize) -> usize {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    
    let mut hasher = DefaultHasher::new();
    session.0.hash(&mut hasher);
    (hasher.finish() as usize) % shards
}
```

### Future: Per-Core Pinning

```rust
// Create per-core runtimes
let cores = num_cpus::get();
let runtimes: Vec<_> = (0..cores)
    .map(|i| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name(format!("worker-core-{}", i))
            .build()
            .unwrap()
    })
    .collect();

// Spawn stage on affinity-matched runtime
let shard = shard_session(&request.session, cores);
runtimes[shard].spawn(async move {
    // Process request on dedicated core
    // Improves cache locality for stateful workers
});
```

### Benefits of Affinity

1. **Cache Locality**: Session data stays in L1/L2/L3 cache
2. **NUMA Optimization**: Pin to memory near CPU socket
3. **State Locality**: Stateful workers avoid coordination overhead
4. **Reduced Contention**: Per-core locks/data structures

## ModelWorker Trait

### Design

```rust
#[async_trait]
pub trait ModelWorker: Send + Sync {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError>;
}
```

### Why Object-Safe?

- **Dynamic Dispatch**: Swap workers at runtime via `Arc<dyn ModelWorker>`
- **Plugin System**: Load workers from shared libraries
- **A/B Testing**: Route requests to different models
- **Graceful Upgrades**: Hot-swap model versions without restart

### Future Extensions

```rust
#[async_trait]
pub trait ModelWorker: Send + Sync {
    // Streaming inference
    async fn infer_stream(&self, prompt: &str) 
        -> Result<impl Stream<Item = String>, OrchestratorError>;
    
    // Batch inference (higher throughput)
    async fn infer_batch(&self, prompts: Vec<&str>) 
        -> Result<Vec<Vec<String>>, OrchestratorError>;
    
    // Cancellation support
    async fn infer_cancellable(&self, prompt: &str, cancel: CancellationToken) 
        -> Result<Vec<String>, OrchestratorError>;
    
    // Model metadata
    fn model_info(&self) -> ModelInfo;
    
    // Health check
    async fn health(&self) -> HealthStatus;
}
```

## Error Handling

### Current Strategy

1. **Inference Errors**: Log and drop request (no retry)
2. **Channel Closed**: Propagate error, trigger shutdown
3. **Backpressure**: Log and drop (graceful shed)

### Future: Dead-Letter Queue

```rust
struct DLQEntry {
    request: PromptRequest,
    error: OrchestratorError,
    timestamp: SystemTime,
    retries: u32,
}

async fn retry_worker(dlq_rx: mpsc::Receiver<DLQEntry>, input_tx: mpsc::Sender<PromptRequest>) {
    while let Some(entry) = dlq_rx.recv().await {
        if entry.retries < MAX_RETRIES {
            tokio::time::sleep(Duration::from_secs(entry.retries as u64)).await;
            input_tx.send(entry.request).await.ok();
        } else {
            // Permanent failure - write to persistent DLQ (DB, S3, etc.)
            write_to_permanent_dlq(entry).await;
        }
    }
}
```

## Metrics & Observability

### Current: Tracing

```rust
metrics::record_stage_latency("inference", duration);
// Emits: INFO metric=stage_latency_ms stage=inference latency_ms=152
```

### Future: Prometheus

```rust
lazy_static! {
    static ref STAGE_LATENCY: HistogramVec = register_histogram_vec!(
        "orchestrator_stage_latency_seconds",
        "Stage processing latency",
        &["stage"],
        vec![0.001, 0.01, 0.1, 1.0, 5.0, 10.0]
    ).unwrap();
}

pub fn record_stage_latency(stage: &str, dur: Duration) {
    STAGE_LATENCY
        .with_label_values(&[stage])
        .observe(dur.as_secs_f64());
}
```

### Key Metrics

| Metric | Type | Purpose |
|--------|------|---------|
| `stage_latency` | Histogram | Processing time per stage |
| `queue_depth` | Gauge | Current backlog per channel |
| `requests_shed` | Counter | Backpressure drops |
| `requests_processed` | Counter | Throughput |
| `inference_tokens_total` | Counter | Token generation rate |
| `error_rate` | Counter | Failures per stage |

## Distributed Architecture (Roadmap)

### NATS Mesh

```text
┌─────────┐     ┌─────────┐     ┌─────────┐
│  Node 1 │────▶│  NATS   │◀────│  Node 2 │
│  RAG    │     │ Cluster │     │ Inference│
└─────────┘     └─────────┘     └─────────┘
                      ▲
                      │
                      ▼
                ┌─────────┐
                │  Node 3 │
                │  Post   │
                └─────────┘
```

### Cross-Node Channels

```rust
// Replace local MPSC with NATS subjects
let js = nats_client.jetstream();
let stream = js.create_stream("orchestrator.rag_to_assemble").await?;

// Producer
stream.publish("rag_out", serde_json::to_vec(&rag_output)?).await?;

// Consumer
let consumer = stream.create_consumer(ConsumerConfig {
    durable_name: Some("assemble-worker-1".to_string()),
    ack_policy: AckPolicy::Explicit,
    ..Default::default()
}).await?;
```

## Testing Strategy

### Unit Tests

- ✅ Session sharding determinism
- ✅ Session distribution
- ✅ Echo worker behavior

### Integration Tests

```rust
#[tokio::test]
async fn test_end_to_end_pipeline() {
    let worker = Arc::new(EchoWorker::with_delay(1));
    let handles = spawn_pipeline(worker);
    
    let request = PromptRequest { /* ... */ };
    handles.input_tx.send(request).await.unwrap();
    
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Assert on output / metrics
}
```

### Load Tests (Roadmap)

```rust
#[tokio::test]
async fn test_backpressure_shed() {
    let worker = Arc::new(SlowWorker::with_delay(1000)); // Slow inference
    let handles = spawn_pipeline(worker);
    
    // Flood with requests
    for i in 0..10000 {
        handles.input_tx.send(create_request(i)).await.unwrap();
    }
    
    // Assert: some requests shed, no OOM, pipeline stable
}
```

## Performance Targets (MVP)

| Metric | Target |
|--------|--------|
| RAG latency | <10ms p99 |
| Assemble latency | <5ms p99 |
| Inference latency | <500ms p99 (model-dependent) |
| Post latency | <10ms p99 |
| Stream latency | <5ms p99 |
| End-to-end | <600ms p99 |
| Throughput | >100 req/s (with fast model) |
| Memory | <500MB baseline + model weights |

## Security Considerations (Roadmap)

1. **Input Validation**: Prompt injection detection
2. **Rate Limiting**: Per-session quotas
3. **Authentication**: JWT/OAuth2 integration
4. **Encryption**: TLS for all network I/O
5. **Audit Logging**: Tamper-proof request logs
6. **PII Protection**: Automatic redaction in post stage
