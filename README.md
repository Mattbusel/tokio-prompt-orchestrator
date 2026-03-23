# tokio-prompt-orchestrator

[![CI](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/Mattbusel/tokio-prompt-orchestrator/branch/main/graph/badge.svg)](https://codecov.io/gh/Mattbusel/tokio-prompt-orchestrator)
[![Crates.io](https://img.shields.io/crates/v/tokio-prompt-orchestrator.svg)](https://crates.io/crates/tokio-prompt-orchestrator)
[![docs.rs](https://docs.rs/tokio-prompt-orchestrator/badge.svg)](https://docs.rs/tokio-prompt-orchestrator)
[![GitHub Pages](https://img.shields.io/badge/docs-GitHub%20Pages-blue.svg)](https://mattbusel.github.io/tokio-prompt-orchestrator/tokio_prompt_orchestrator/)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Production-grade, multi-core Tokio orchestration for LLM inference pipelines.**

Five-stage bounded-backpressure DAG with deduplication, circuit breakers, rate limiting, and an optional autonomous self-improving control loop. Supports Anthropic, OpenAI, llama.cpp, vLLM, and any custom backend. Exposes REST, WebSocket, SSE, MCP (Claude Desktop), Prometheus metrics, and OpenTelemetry distributed tracing.

---

## Why This Exists

Running LLM inference in production at scale exposes a class of problems that a single `reqwest` call cannot solve:

- **Thundering herd**: 10,000 concurrent sessions all calling the same model. Without deduplication, identical prompts hit the API 10,000 times.
- **Provider instability**: Cloud APIs drop packets, timeout, rate-limit, and return 5xx errors. Without a circuit breaker, one bad minute cascades into minutes of queued failures.
- **Latency tail management**: A slow model response blocks an unbounded goroutine/thread pool. Bounded async channels propagate backpressure instead.
- **Cost opacity**: Nobody knows which prompt pattern is eating the budget until the invoice arrives.
- **Manual tuning**: Worker counts, buffer sizes, retry delays — these need continuous adjustment as traffic patterns shift.

This crate solves all of the above out of the box, with zero unsafe code and a compile-time feature flag for each subsystem.

---

## New User Onboarding — 5 Minutes to First Response

### Option A: Prebuilt Binary (no Rust required)

1. Download `orchestrator.exe` from the [releases page](https://github.com/Mattbusel/tokio-prompt-orchestrator/releases) and run it.

2. The first launch runs an interactive setup wizard:

```text
Which AI provider do you want to use?

1) Anthropic  (Claude)
2) OpenAI     (GPT-4o)
3) llama.cpp  (local, no key)
4) echo       (offline test mode — no key needed)

Enter 1, 2, 3, or 4 [4]:
```

3. The orchestrator starts a terminal REPL and a web API on `http://127.0.0.1:8080` simultaneously. Type prompts directly, or connect from any HTTP client.

### Option B: From Source (Rust developers)

```bash
# Clone and run in offline echo mode — no API key needed
git clone https://github.com/Mattbusel/tokio-prompt-orchestrator
cd tokio-prompt-orchestrator
cargo run -- --worker echo

# Switch to a real provider
ANTHROPIC_API_KEY=sk-ant-... cargo run --features full -- --worker anthropic --model claude-sonnet-4-6

# Start with the full feature set + TUI dashboard
cargo run --features full,tui --bin tui
```

### Option C: Library (embed in your application)

```toml
[dependencies]
tokio-prompt-orchestrator = "1.2"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust,no_run
use std::collections::HashMap;
use tokio_prompt_orchestrator::{spawn_pipeline, EchoWorker, PromptRequest, SessionId};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Swap EchoWorker for OpenAiWorker, AnthropicWorker, LlamaCppWorker, or VllmWorker
    let worker: Arc<dyn tokio_prompt_orchestrator::ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);

    handles.input_tx.send(PromptRequest {
        session: SessionId::new("demo"),
        request_id: "req-1".to_string(),
        input: "Hello, pipeline!".to_string(),
        meta: HashMap::new(),
        deadline: None,
    }).await?;

    let mut guard = handles.output_rx.lock().await;
    if let Some(rx) = guard.as_mut() {
        if let Some(output) = rx.recv().await {
            println!("Response: {}", output.text);
        }
    }
    Ok(())
}
```

---

## Architecture

### Pipeline Overview

The pipeline is a five-stage directed acyclic graph of bounded async channels. Each stage runs as an independent Tokio task. Backpressure propagates upstream when a downstream channel fills; excess requests are shed gracefully to a dead-letter queue rather than blocking.

```text
                 +--------------------------+
  PromptRequest  |  Stage 1: RAG            |  cap: 512
  ─────────────> |  (context retrieval)     |
                 +------------+-------------+
                              |
                 +------------v-------------+
                 |  Stage 2: Assemble       |  cap: 512
                 |  (prompt construction)   |
                 +------------+-------------+
                              |
                 +------------v-------------+
                 |  Stage 3: Inference      |  cap: 1024
                 |  (model worker pool)     |
                 |                          |
                 |  Deduplication           |
                 |  Circuit Breaker         |
                 |  Retry + Backoff         |
                 |  Rate Limiter            |
                 +------------+-------------+
                              |
                 +------------v-------------+
                 |  Stage 4: Post-Process   |  cap: 512
                 |  (filter / format)       |
                 +------------+-------------+
                              |
                 +------------v-------------+
                 |  Stage 5: Stream         |  cap: 256
                 |  (output sink)           |
                 +--------------------------+
                              |
          +-------------------+--------------------+
          |                   |                    |
   REST/WS/SSE         Prometheus /metrics    OpenTelemetry
   http://:8080         http://:9090          (Jaeger/OTLP)
```

### Resilience Layers (applied at Stage 3)

| Layer | What it does |
|-------|-------------|
| **Deduplication** | In-flight requests with identical prompts are coalesced into a single API call; all waiting callers receive the same result |
| **Circuit Breaker** | Opens on consecutive failures, enters half-open probe mode after configurable timeout |
| **Retry + Jitter** | Exponential backoff with full jitter — prevents synchronized retry storms |
| **Rate Limiter** | Token-bucket guard at the pipeline entry point |
| **Dead-letter Queue** | Shed requests land in a ring buffer for inspection and replay |
| **Priority Queue** | Four-level priority scheduler (Critical / High / Normal / Low) |
| **Cache Layer** | TTL LRU cache for inference results (requires `caching` feature + Redis) |

### Self-Improving Control Loop (optional)

When the `self-improving` feature is enabled, a background control loop continuously measures pipeline health and adjusts parameters:

```text
  [TelemetryBus] ─> [AnomalyDetector] ─> [PID Controllers] ─> [Config Updates]
       |                    |                     |
  queue depths         Z-score +            worker count
  error rates          CUSUM              buffer sizes
  latency p99          alerts             retry delays

  [LearnedRouter] ─> [Autoscaler] ─> [PromptOptimizer] ─> [A/B Experiments]
  epsilon-greedy       OLS trend        semantic dedup       snapshot rollback
  bandit               prediction       quality estimation   transfer learning
```

---

## Quick API Reference

| Type | Module | Description |
|------|--------|-------------|
| `PromptRequest` | `lib` | Input message sent into the pipeline |
| `SessionId` | `lib` | Session identifier for affinity sharding |
| `OrchestratorError` | `lib` | Crate-level error enum |
| `ModelWorker` | `worker` | Async trait implemented by all inference backends |
| `EchoWorker` | `worker` | Returns prompt words as tokens — for testing, no API key |
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

Pass with `--config pipeline.toml`. All fields have documented defaults.

```toml
[pipeline]
name        = "production"
version     = "1.0"
description = "Optional human-readable description"

[stages.rag]
enabled            = true
timeout_ms         = 5000
max_context_tokens = 2048

[stages.assemble]
enabled          = true
channel_capacity = 512

[stages.inference]
worker      = "anthropic"   # anthropic | open_ai | llama_cpp | vllm | echo
model       = "claude-sonnet-4-6"
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
enabled             = true
requests_per_second = 100
burst_capacity      = 20

[deduplication]
enabled     = true
window_s    = 300
max_entries = 10000

[observability]
log_format       = "json"                  # pretty | json
metrics_port     = 9090                    # Prometheus scrape endpoint
tracing_endpoint = "http://jaeger:4318"    # OTLP endpoint (omit to disable)

[distributed]                              # requires "distributed" feature
redis_url = "redis://redis:6379"
nats_url  = "nats://nats:4222"
node_id   = "node-1"
```

### Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `ANTHROPIC_API_KEY` | Required for `AnthropicWorker` | — |
| `OPENAI_API_KEY` | Required for `OpenAiWorker` | — |
| `LLAMA_CPP_URL` | llama.cpp server URL | `http://localhost:8080` |
| `VLLM_URL` | vLLM server URL | `http://localhost:8000` |
| `RUST_LOG` | Log level filter | `info` |
| `RUST_LOG_FORMAT` | Set to `json` for NDJSON logs | `pretty` |
| `JAEGER_ENDPOINT` | OTLP HTTP endpoint | disabled |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Alternative OTLP endpoint | disabled |
| `METRICS_API_KEY` | Bearer token to guard `/metrics` | disabled |

---

## Feature Flags

All features are opt-in. The default build has no optional dependencies.

| Flag | Enables | Typical use |
|---|---|---|
| `web-api` | Axum HTTP/WS/SSE server | REST clients, streaming |
| `metrics-server` | Prometheus `/metrics` endpoint | Grafana dashboards |
| `tui` | Ratatui terminal dashboard | Local monitoring |
| `mcp` | Model Context Protocol server | Claude Desktop / Claude Code |
| `caching` | Redis-backed TTL result cache | Repeated-prompt workloads |
| `rate-limiting` | Token-bucket rate limiter | Provider quota management |
| `distributed` | Redis dedup + NATS pub/sub + leader election | Multi-node deployments |
| `self-tune` | PID controllers, telemetry bus, anomaly detector | Autonomous tuning |
| `self-modify` | MetaTaskGenerator, ValidationGate, AgentMemory | Config self-generation |
| `intelligence` | LearnedRouter (bandit), Autoscaler, PromptOptimizer | Learned routing |
| `evolution` | A/B experiments, snapshot rollback, transfer learning | Continuous improvement |
| `self-improving` | All self-* features combined | Full autonomous mode |
| `full` | `web-api` + `metrics-server` + `caching` + `rate-limiting` | Single-node production |
| `schema` | JSON Schema export for `PipelineConfig` | IDE validation |
| `dashboard` | Web dashboard UI | Browser monitoring |
| `core-pinning` | CPU core affinity for pipeline tasks | Latency-sensitive deployments |

---

## Web API

Enable with `--features web-api`. Full documentation in [`WEB_API.md`](WEB_API.md).

```bash
# Single prompt over REST
curl -X POST http://localhost:8080/v1/prompt \
  -H "Content-Type: application/json" \
  -d '{"input": "What is backpressure?"}'

# Server-sent events streaming
curl -N http://localhost:8080/v1/stream/sse \
  -H "X-Session-Id: my-session"

# WebSocket streaming
wscat -c ws://localhost:8080/v1/stream

# Pipeline health
curl http://localhost:8080/v1/health | jq .

# Dead-letter queue inspection
curl http://localhost:8080/v1/dlq | jq .

# Replay a shed request
curl -X POST http://localhost:8080/v1/dlq/replay/req-42
```

---

## Live Dashboard (TUI)

```bash
cargo run --bin tui --features tui
```

The dashboard shows:
- Per-stage queue depths with fill-level bars
- Circuit breaker state (CLOSED / OPEN / HALF-OPEN) and failure count
- Deduplication hit rate and in-flight count
- Latency sparklines (p50/p95/p99) per stage
- Autoscaler decisions (if `self-improving` enabled)
- Scrolling structured log panel

---

## MCP Integration

Connect Claude Desktop or Claude Code directly to the orchestrator:

```bash
cargo run --bin mcp --features mcp
```

Add to your Claude Desktop `config.json`:

```json
{
  "mcpServers": {
    "orchestrator": {
      "url": "http://127.0.0.1:8080"
    }
  }
}
```

Available MCP tools: `infer`, `batch_infer`, `pipeline_status`, `configure_pipeline`, `replay_dlq`.

---

## Deployment Guide

### Standalone Binary

```bash
cargo build --release --features full
./target/release/orchestrator --config pipeline.toml
```

Exposes:
- Web API: `http://0.0.0.0:8080`
- Prometheus: `http://0.0.0.0:9090/metrics`
- OpenTelemetry traces via OTLP to configured endpoint

### Docker

```bash
docker build -t tokio-prompt-orchestrator .
docker run -p 8080:8080 -p 9090:9090 \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  tokio-prompt-orchestrator
```

The bundled `docker-compose.yml` starts the orchestrator alongside Redis, NATS, Prometheus, and Grafana. A pre-built Grafana dashboard is in `grafana-dashboard.json`.

### Multi-Node Distributed Mode

Enable the `distributed` feature and configure Redis + NATS:

```toml
[distributed]
redis_url = "redis://redis:6379"
nats_url  = "nats://nats:4222"
node_id   = "node-1"
```

All nodes share Redis for cross-node deduplication and leader election. Work distributes via NATS subjects. Session affinity routes same-session requests to the same node when possible. Coordinator binary manages cluster membership:

```bash
cargo run --bin coordinator
```

---

## Self-Improving Mode

When `--features self-improving` is enabled, the orchestrator autonomously optimizes itself:

```bash
cargo run --bin self-improve --features self-improving -- --config pipeline.toml
```

What it does:
1. **Monitors** queue depths, error rates, latency percentiles via TelemetryBus
2. **Detects** anomalies using Z-score and CUSUM change-point detection
3. **Tunes** worker count, buffer sizes, retry delays using PID controllers
4. **Learns** optimal routing via epsilon-greedy multi-armed bandit
5. **Optimizes** prompts by testing semantic variations and tracking quality scores
6. **Experiments** with A/B config snapshots, auto-rolls back if metrics regress

All parameter changes are logged with before/after values and the reason for the change.

---

## Performance Tuning Guide

### Worker Count

Start with one worker per physical core. Increase if:
- Inference latency is high (> 5s) and you have spare cores
- Queue depths consistently > 50%

Decrease if:
- Memory pressure is high
- Provider rate limits are the bottleneck

The Autoscaler (enabled with `self-tune`) adjusts this automatically.

### Circuit Breaker

```toml
[resilience]
circuit_breaker_threshold    = 5    # failures before opening
circuit_breaker_timeout_s    = 60   # seconds before half-open probe
circuit_breaker_success_rate = 0.8  # required to close from half-open
```

For unreliable providers: lower threshold to 3, increase timeout to 120s.
For local models: increase threshold to 20 (they rarely fail, just get slow).

### Deduplication Window

```toml
[deduplication]
window_s    = 300    # cache TTL in seconds
max_entries = 10000  # max cached entries
```

Increase `window_s` for FAQ/chatbot workloads with repeated prompts.
Decrease for real-time queries where freshness matters.

### Channel Buffer Sizes

Optimized for cloud LLM with 1–10s inference latency. Adjust for your model:

```rust,ignore
spawn_pipeline_with_config(worker, PipelineConfig {
    rag_channel_capacity: 512,
    assemble_channel_capacity: 512,
    inference_channel_capacity: 2048,  // double for slow models (> 30s)
    post_channel_capacity: 512,
    stream_channel_capacity: 256,
    ..Default::default()
})
```

For local models (< 100ms inference): halve all buffer sizes to save memory.

---

## Benchmarks

Measured on AMD Ryzen 9 7950X (16 cores), Linux, with `EchoWorker` (no network I/O):

| Scenario | Throughput | p99 Latency |
|----------|-----------|-------------|
| Single worker, no features | 420k req/s | 12 µs |
| 16 workers, dedup + circuit breaker | 2.8M req/s | 18 µs |
| 16 workers, full feature set | 1.9M req/s | 31 µs |
| 16 workers, self-improving enabled | 1.7M req/s | 38 µs |

Run the benchmarks:

```bash
cargo bench --features full
```

---

## Troubleshooting

### "Circuit breaker is OPEN"

The pipeline is protecting itself from a failing provider. Check:

```bash
curl http://localhost:8080/v1/health | jq .circuit_breaker
# {"state":"Open","failure_count":7,"next_probe_in_secs":43}
```

Wait for the probe timeout, or force a reset:

```bash
curl -X POST http://localhost:8080/v1/circuit-breaker/reset
```

### "Dead-letter queue growing"

Requests are being shed due to backpressure. Options:
1. Increase `inference_channel_capacity` in config
2. Add more workers
3. Enable rate limiting to smooth inbound traffic
4. Check if the provider is slow (latency spike)

### "Dedup not working"

Ensure `deduplication.enabled = true` in your config and that requests have identical `input` fields. Dedup is keyed on the normalized prompt text.

### High memory usage

Each buffered request occupies memory proportional to prompt length. Reduce channel capacities or enable rate limiting to bound total in-flight work.

### Prometheus metrics not appearing

Ensure `--features metrics-server` and check `metrics_port` in config. If `METRICS_API_KEY` is set, include `Authorization: Bearer <key>` in the scrape config.

---

## Examples

See the [`examples/`](examples/) directory for:

| Example | What it shows |
|---------|--------------|
| `rest_api` | Full HTTP REST integration |
| `sse_stream` | Server-sent events token streaming |
| `websocket_api` | WebSocket bidirectional streaming |
| `web_api_demo` | Combined REST + SSE + WebSocket demo |

---

## Contributing

1. Fork the repository and create a feature branch off `main`.
2. Run `cargo fmt --all` and `cargo clippy -- -D warnings` before pushing.
3. Add tests for any new public API surface. Panic-free code is required (`unwrap`/`expect` denied by Clippy lint).
4. Open a pull request against `main`. CI must pass before merge.

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full guide.

---

## Known Issues / Roadmap

- **prometheus 0.13**: Has RUSTSEC-2024-0437 (protobuf DoS). Mitigated by API key auth on `/metrics`. Migration to 0.14 blocked by `prometheus::proto` API removal — tracked internally for Q3 2026.
- **Request replay UI**: Dead-letter queue replay works via API; a TUI panel for it is planned.
- **Per-stage circuit breaker metrics**: Currently aggregated; per-stage breakdown is planned.

---

## License

MIT. See [`LICENSE`](LICENSE).
