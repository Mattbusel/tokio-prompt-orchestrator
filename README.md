# tokio-prompt-orchestrator

[![CI](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/Mattbusel/tokio-prompt-orchestrator/branch/main/graph/badge.svg)](https://codecov.io/gh/Mattbusel/tokio-prompt-orchestrator)
[![Crates.io](https://img.shields.io/crates/v/tokio-prompt-orchestrator.svg)](https://crates.io/crates/tokio-prompt-orchestrator)
[![docs.rs](https://docs.rs/tokio-prompt-orchestrator/badge.svg)](https://docs.rs/tokio-prompt-orchestrator)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Multi-core, Tokio-native orchestration for LLM inference pipelines. Built with bounded backpressure channels, circuit breakers, request deduplication, and an optional autonomous self-tuning control loop.

---

## Architecture

The pipeline is a five-stage directed acyclic graph of bounded async channels.
Each stage runs as an independent Tokio task. Backpressure propagates upstream
when a downstream channel fills, and excess requests are shed gracefully to a
dead-letter queue rather than blocking the pipeline.

```text
[Prompt Submissions] -> [Dedup Stage] -> [Circuit Breaker] -> [Rate Limiter]
                                                                     |
                                                           [Worker Pool (Tokio)]
                                                                     |
                                                     [LLM Providers (Anthropic/OpenAI)]
                                                                     |
                                                 [Self-Improving Control Loop]
                                                                     |
                       [Prometheus Metrics] <- [OpenTelemetry] <- [Results]
                       [Web API (HTTP/WS)]
```

```text
                        +------------------+
  PromptRequest ------> |  RAG Stage       | cap: 512
                        | (context fetch)  |
                        +--------+---------+
                                 |
                        +--------v---------+
                        |  Assemble Stage  | cap: 512
                        | (prompt builder) |
                        +--------+---------+
                                 |
                        +--------v---------+
                        |  Inference Stage | cap: 1024
                        | (model worker)   |
                        +--------+---------+
                                 |
                        +--------v---------+
                        |  Post Stage      | cap: 512
                        | (filter/format)  |
                        +--------+---------+
                                 |
                        +--------v---------+
                        |  Stream Stage    | cap: 256
                        | (output sink)    |
                        +------------------+
```

**Resilience layers applied at the inference stage:**

- Deduplication: in-flight requests with identical prompts are coalesced into a single inference call; all waiting callers receive the same result.
- Circuit breaker: opens on consecutive failures, enters half-open probe mode after a configurable timeout.
- Retry with exponential backoff and jitter.
- Rate limiter: token-bucket guard at the pipeline entry point.
- Dead-letter queue: shed requests are stored in a ring buffer for inspection and replay.

---

## Quickstart

### Prebuilt binary (no Rust required)

Download `orchestrator.exe` from the [releases page](https://github.com/Mattbusel/tokio-prompt-orchestrator/releases/tag/v0.1.0) and run it. The first launch runs an interactive setup wizard that saves your API key and provider preference to `orchestrator.env`.

```text
Which AI provider do you want to use?

1) Anthropic  (Claude)
2) OpenAI     (GPT-4o)
3) llama.cpp  (local, no key)
4) echo       (offline test mode)

Enter 1, 2, 3, or 4 [1]:
```

After setup the orchestrator starts a terminal REPL and a web API on `http://127.0.0.1:8080` simultaneously. You can type prompts in the terminal while agents and IDEs connect over HTTP in the background.

### Library usage

Add the dependency:

```toml
[dependencies]
tokio-prompt-orchestrator = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

Minimal async example using the echo worker (no API key required):

```rust,no_run
use std::collections::HashMap;
use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, PromptRequest, SessionId,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Spawn the five-stage pipeline with the echo worker.
    // Swap EchoWorker for OpenAiWorker, AnthropicWorker, LlamaCppWorker, or VllmWorker.
    let worker: Arc<dyn tokio_prompt_orchestrator::ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);

    // Send a prompt into the pipeline.
    handles
        .input_tx
        .send(PromptRequest {
            session: SessionId::new("demo"),
            request_id: "req-1".to_string(),
            input: "Hello, pipeline!".to_string(),
            meta: HashMap::new(),
            deadline: None,
        })
        .await?;

    // Collect output from the stream stage.
    let mut guard = handles.output_rx.lock().await;
    if let Some(rx) = guard.as_mut() {
        if let Some(output) = rx.recv().await {
            println!("Response: {}", output.text);
        }
    }

    Ok(())
}
```

See [`examples/`](examples/) for REST, WebSocket, SSE streaming, OpenAI, Anthropic, llama.cpp, and multi-worker round-robin setups.

---

## API Overview

| Type | Module | Description |
|------|--------|-------------|
| `PromptRequest` | `lib` | Input message sent into the pipeline |
| `SessionId` | `lib` | Session identifier for affinity sharding |
| `OrchestratorError` | `lib` | Crate-level error enum |
| `ModelWorker` | `worker` | Async trait implemented by all inference backends |
| `EchoWorker` | `worker` | Returns prompt words as tokens; for testing |
| `OpenAiWorker` | `worker` | OpenAI chat completions API |
| `AnthropicWorker` | `worker` | Anthropic Messages API |
| `LlamaCppWorker` | `worker` | Local llama.cpp HTTP server |
| `VllmWorker` | `worker` | vLLM inference server |
| `LoadBalancedWorker` | `worker` | Round-robin or least-loaded pool of workers |
| `spawn_pipeline` | `stages` | Launch the five-stage pipeline, return channel handles |
| `spawn_pipeline_with_config` | `stages` | Same, with a full `PipelineConfig` |
| `PipelineConfig` | `config` | TOML-deserialisable root configuration type |
| `CircuitBreaker` | `enhanced` | Failure-rate circuit breaker |
| `Deduplicator` | `enhanced` | In-flight request coalescer |
| `RetryPolicy` | `enhanced` | Exponential backoff with jitter |
| `CacheLayer` | `enhanced` | TTL LRU cache for inference results |
| `PriorityQueue` | `enhanced` | Four-level priority scheduler |
| `DeadLetterQueue` | `lib` | Ring buffer of shed requests |
| `send_with_shed` | `lib` | Non-blocking channel send with graceful shedding |
| `shard_session` | `lib` | FNV-1a session affinity shard helper |

---

## Configuration Reference

The orchestrator is configured via a TOML file. Pass it with `--config pipeline.toml`.

```toml
[pipeline]
name        = "production"
version     = "1.0"
description = "Optional description"

[stages.rag]
enabled           = true
timeout_ms        = 5000
max_context_tokens = 2048

[stages.assemble]
enabled          = true
channel_capacity = 512

[stages.inference]
worker      = "open_ai"   # open_ai | anthropic | llama_cpp | vllm | echo
model       = "gpt-4o"
max_tokens  = 1024
temperature = 0.7
timeout_ms  = 30000

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts               = 3
retry_base_ms                = 100
retry_max_ms                 = 5000
circuit_breaker_threshold    = 5
circuit_breaker_timeout_s    = 60
circuit_breaker_success_rate = 0.8

[rate_limits]
enabled             = false
requests_per_second = 100
burst_capacity      = 20

[deduplication]
enabled    = true
window_s   = 300
max_entries = 10000

[observability]
log_format        = "json"    # pretty | json
metrics_port      = 9090      # Prometheus scrape endpoint; omit to disable
tracing_endpoint  = "http://jaeger:4318"  # OTLP endpoint; omit to disable
```

**Environment variables:**

| Variable | Purpose |
|----------|---------|
| `OPENAI_API_KEY` | Required for `OpenAiWorker` |
| `ANTHROPIC_API_KEY` | Required for `AnthropicWorker` |
| `LLAMA_CPP_URL` | llama.cpp server URL (default: `http://localhost:8080`) |
| `VLLM_URL` | vLLM server URL (default: `http://localhost:8000`) |
| `RUST_LOG` | Log level filter (default: `info`) |
| `RUST_LOG_FORMAT` | Set to `json` for newline-delimited JSON logs |
| `JAEGER_ENDPOINT` | OTLP HTTP endpoint for distributed tracing |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Alternative OTLP endpoint variable |

---

## Live Dashboard

Start the TUI dashboard with the `tui` feature:

```bash
cargo run --bin tui --features tui
```

The dashboard shows per-stage queue depths, circuit breaker state, deduplication hit rate, latency sparklines, and a scrolling log panel.

---

## Web API

Enable with `--features web-api`. The REST and WebSocket API is documented in [`WEB_API.md`](WEB_API.md).

```bash
# Single prompt over REST
curl -X POST http://localhost:8080/v1/prompt \
  -H "Content-Type: application/json" \
  -d '{"input": "What is backpressure?"}'

# Streaming over WebSocket
wscat -c ws://localhost:8080/v1/stream
```

---

## MCP Integration

Enable the MCP server with `--features mcp` and point Claude Desktop at it:

```json
{
  "mcpServers": {
    "orchestrator": {
      "url": "http://127.0.0.1:8080"
    }
  }
}
```

---

## Feature Flags

All features are opt-in. The default build has no optional features.

| Flag | Enables | Required for |
|---|---|---|
| `web-api` | Axum HTTP server: REST, SSE, WebSocket endpoints | `mcp`, `dashboard` |
| `metrics-server` | Prometheus `/metrics` HTTP scrape endpoint | -- |
| `tui` | Ratatui terminal dashboard (requires `crossterm`) | `tui` binary |
| `mcp` | Model Context Protocol server (requires `web-api`) | Claude Desktop integration |
| `caching` | Redis-backed TTL result cache | -- |
| `rate-limiting` | Token-bucket rate limiter via `governor` crate | -- |
| `distributed` | Redis cross-node dedup + NATS pub/sub coordination | Multi-node deployments |
| `self-tune` | PID controllers, telemetry bus, anomaly detector, snapshot store | All self-* features |
| `self-modify` | MetaTaskGenerator, ValidationGate, AgentMemory (requires `self-tune`) | `self-improving` |
| `intelligence` | LearnedRouter (bandit), Autoscaler, PromptOptimizer, SemanticDedup (requires `self-tune`) | `evolution`, `self-improving` |
| `evolution` | A/B experiments, snapshot rollback, transfer learning (requires `self-tune` + `intelligence`) | -- |
| `self-improving` | Meta-feature: enables all of `self-tune`, `self-modify`, `intelligence`, `evolution` | `self-improve` binary |
| `full` | `web-api` + `metrics-server` + `caching` + `rate-limiting` | Full-featured single-node deployment |
| `schema` | JSON Schema export for `PipelineConfig` via `schemars` | `gen_schema` binary |
| `dashboard` | Web dashboard UI (requires `web-api`) | `dashboard` binary |

---

## Deployment Guide

### Standalone Binary

```bash
cargo build --release --features full
./target/release/orchestrator --config pipeline.toml
```

The orchestrator exposes:
- Web API on port 8080 (configurable)
- Prometheus metrics on port 9090 (configurable)
- TUI dashboard via `cargo run --bin tui --features tui`

### Docker

```bash
docker build -t tokio-prompt-orchestrator .
docker run -p 8080:8080 -p 9090:9090 \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  tokio-prompt-orchestrator
```

The `docker-compose.yml` in the repository starts the orchestrator alongside
Redis (for distributed dedup and caching) and a Prometheus/Grafana stack. A
pre-built Grafana dashboard is available in `grafana-dashboard.json`.

### Distributed Mode with Redis

Enable the `distributed` feature and configure Redis:

```toml
[distributed]
redis_url = "redis://redis:6379"
nats_url  = "nats://nats:4222"
node_id   = "node-1"
```

All nodes share a Redis cluster for cross-node deduplication and leader
election. Work is distributed via NATS subjects. Session affinity ensures
same-session requests land on the same node when possible.

---

## Tuning Guide

### Worker Count

Start with one worker per physical core. Increase if inference latency is low
and throughput is the bottleneck; decrease if memory pressure is high.

The Autoscaler (enabled with `self-tune` feature) adjusts this automatically
based on queue depth telemetry.

### Circuit Breaker Thresholds

```toml
[resilience]
circuit_breaker_threshold    = 5    # failures before opening
circuit_breaker_timeout_s    = 60   # seconds before half-open probe
circuit_breaker_success_rate = 0.8  # rate required to close from half-open
```

For unreliable providers (high timeout rate), lower `circuit_breaker_threshold`
to 3 and increase `circuit_breaker_timeout_s` to 120.

### Dedup Window

```toml
[deduplication]
window_s    = 300    # cache TTL in seconds
max_entries = 10000  # maximum cached entries
```

Increase `window_s` for workloads with repeated identical prompts (e.g. chatbot
FAQs). Decrease for workloads where freshness matters (e.g. real-time queries).

### Channel Buffer Sizes

The default channel sizes are optimised for a typical cloud LLM with 1-10 second
inference latency. For local models (< 100ms), reduce all buffers by 50% to
save memory. For very slow models (> 30s), double the Inference-to-Post buffer:

```rust,ignore
spawn_pipeline_with_config(worker, PipelineConfig {
    rag_channel_capacity: 512,
    assemble_channel_capacity: 512,
    inference_channel_capacity: 2048,   // doubled for slow models
    post_channel_capacity: 512,
    stream_channel_capacity: 256,
    ..Default::default()
})
```

---

## Contributing

1. Fork the repository and create a feature branch.
2. Run `cargo fmt --all` and `cargo clippy -- -D warnings` before pushing.
3. Add tests for any new public API surface.
4. Open a pull request against `master`. CI must pass before merge.

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full guide.

---

## License

MIT. See [`LICENSE`](LICENSE).
