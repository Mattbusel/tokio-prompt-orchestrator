# tokio-prompt-orchestrator

A **production-ready**, **cost-optimized** orchestrator for multi-stage LLM pipelines with comprehensive reliability features, observability, and enterprise-grade capabilities.

[![Rust](https://img.shields.io/badge/rust-1.79%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/tests-959_passing-brightgreen.svg)]()
[![Lines of Code](https://img.shields.io/badge/lines-23.6k-brightgreen.svg)]()

##  What Is This?

An intelligent orchestrator that manages the complete lifecycle of LLM inference requests through a 5-stage pipeline:

```text
Request → RAG → Assemble → Inference → Post-Process → Stream → Response
            ↓       ↓          ↓            ↓            ↓
         Context  Format    AI Model     Filter       Output
```

**Built for production** with features that save costs, improve reliability, and provide enterprise-grade observability.

##  Key Features

###  High-Impact Features

- **Request Deduplication** - Save 60-80% on inference costs by caching duplicate requests
- ** Circuit Breaker** - Prevent cascading failures, fast-fail when services are down
- ** Retry Logic** - Automatic retry with exponential backoff, 99%+ reliability

### Real Model Integrations

- **OpenAI** - GPT-4, GPT-3.5-turbo, and all OpenAI models
- **Anthropic** - Claude 3.5 Sonnet, Claude 3 Opus, Haiku
- **llama.cpp** - Local inference with any GGUF model
- **vLLM** - High-throughput GPU inference with PagedAttention

###  Web API Layer

- **REST API** - Full HTTP API for inference requests
- **WebSocket** - Real-time bidirectional streaming
- **CORS Support** - Cross-origin request handling
- **Request Tracking** - UUID-based request tracking

### Advanced Metrics & Observability

- **Prometheus Integration** - Production-grade metrics
- **Grafana Dashboard** - Pre-built visualization with 9 panels
- **Alert Rules** - 5 critical alerts included
- **Tracing** - Comprehensive structured logging

### Claude Desktop & Claude Code Integration (MCP)

- **Model Context Protocol** - Expose the pipeline as native Claude tools
- **`infer`** - Run prompts through the local pipeline with stage latency reporting
- **`pipeline_status`** - Real-time circuit breaker, channel depth, and dedup stats
- **`batch_infer`** - Submit multiple prompts in one call
- **`configure_pipeline`** - Hot-swap workers and settings at runtime

### TUI Dashboard

- **Real-time terminal UI** - Monitor pipeline flow, channel depths, circuit breakers
- **Story mode** - 2-minute scripted demo that loops
- **Live mode** - Connect to a running Prometheus endpoint
- **Scrollable log** - Keybindings for quit, pause, reset, scroll

### Distributed Clustering (Phase 5)

- **NATS Pub/Sub** - Inter-node messaging for cluster coordination
- **Redis Cross-Node Dedup** - Distributed deduplication with atomic SET NX EX
- **Leader Election** - Redis-based SETNX + TTL leader election with renewal
- **Cluster Manager** - Node registry, heartbeat tracking, stale-node eviction, load-based routing

### Intelligent Model Routing

- **Complexity Scoring** - Heuristic-based prompt analysis for routing decisions
- **Local vs Cloud** - Route simple prompts to local llama.cpp, complex to cloud APIs
- **Adaptive Thresholds** - Auto-tune routing thresholds based on success/failure feedback
- **Cost Tracking** - Per-model cost accounting with budget awareness

### Agent Fleet Coordination

- **Task Claiming** - Atomic task claiming via filesystem locks
- **Priority Queues** - TOML-based task files with priority ordering
- **Fleet Health** - Configurable health intervals, stale lock detection

### Web Dashboard

- **Real-time SSE** - Server-sent events for live metric streaming
- **Pipeline Visualization** - Stage latencies, circuit breaker status, dedup savings
- **Cost Savings Display** - Live cost tracking with model routing breakdown
- **Dark Theme** - Responsive single-page HTML dashboard

### Declarative Configuration

- **TOML config files** - Full pipeline configuration with validation
- **JSON Schema export** - IDE autocomplete for config files
- **Hot-reload** - Filesystem watcher for live config updates
- **Validation** - Multi-error reporting with field-level diagnostics

### Enhanced Features

- **Caching** - In-memory + Redis support, configurable TTL
- **Rate Limiting** - Per-session limits, token bucket algorithm
- **Priority Queues** - 4-level priority (Critical, High, Normal, Low)
- **Backpressure** - Graceful degradation under load

### Production Ready

- **Bounded Channels** - Prevent memory exhaustion
- **Session Affinity** - Hash-based sharding for optimization
- **Health Checks** - Detailed component monitoring
- **Docker Support** - Complete docker-compose stack
- **Criterion Benchmarks** - Performance contracts enforced by CI

##  ROI & Impact

### Cost Savings

**Typical Production Scenario (1M requests/month):**

| Feature | Without | With | Savings |
|---------|---------|------|---------|
| Deduplication | $30,000/mo | $9,000/mo | **$21,000/mo** |
| Caching | - | Additional 10-20% | **$1,800-3,600/mo** |
| **Total** | **$30,000/mo** | **$7,200/mo** | **$22,800/mo** |

**Annual savings: $273,600**

### Reliability Improvements

- **Without retries**: 95% success rate
- **With retries**: 99.75% success rate
- **Improvement**: 4.75% fewer user-visible errors

### Performance

- **Request deduplication**: <1ms overhead, 60-80% cost savings
- **Circuit breaker**: <10μs overhead, prevents cascading failures
- **Retry logic**: Automatic, 4-5% reliability improvement

##  Quick Start

### Installation

```bash
git clone https://github.com/Mattbusel/tokio-prompt-orchestrator.git
cd tokio-prompt-orchestrator
```

### Basic Demo (No API Keys)

```bash
cargo run --bin orchestrator-demo
```

Uses `EchoWorker` for testing the pipeline without API keys.

### With OpenAI

```bash
export OPENAI_API_KEY="sk-..."
cargo run --example openai_worker
```

### With Anthropic Claude

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
cargo run --example anthropic_worker
```

### Web API Server (All Features)

```bash
cargo run --example web_api_demo --features full
```

Starts REST API on http://localhost:8080

### High-Impact Features Demo

```bash
cargo run --example high_impact_demo --features full
```

See deduplication, circuit breaker, and retry logic in action.

### TUI Dashboard

```bash
# Mock story mode (no external dependencies)
cargo run --bin tui --features tui

# Live mode (connect to Prometheus)
cargo run --bin tui --features tui -- --live
```

### MCP Server (Claude Desktop / Claude Code)

```bash
# Build and run with echo worker
cargo build --bin mcp --features mcp --release
./target/release/mcp --worker echo

# With local llama.cpp
./target/release/mcp --worker llama_cpp
```

### Web Dashboard

```bash
# Start with echo worker on port 3030
cargo run --bin dashboard --features dashboard

# With llama.cpp worker on custom port
cargo run --bin dashboard --features dashboard -- --worker llama_cpp --port 8081
```

Open http://localhost:3030 for live pipeline metrics via SSE.

##  Usage Examples

### Basic Usage

```rust
use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Create worker
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    
    // Spawn pipeline
    let handles = spawn_pipeline(worker);
    
    // Send request
    let request = PromptRequest {
        session: SessionId::new("user-123"),
        input: "Hello, world!".to_string(),
        meta: Default::default(),
    };
    
    handles.input_tx.send(request).await.unwrap();
}
```

### Using OpenAI

```rust
use tokio_prompt_orchestrator::{spawn_pipeline, OpenAiWorker};

let worker = Arc::new(
    OpenAiWorker::new("gpt-4")
        .with_max_tokens(512)
        .with_temperature(0.7)
);

let handles = spawn_pipeline(worker);
```

### Using Anthropic Claude

```rust
use tokio_prompt_orchestrator::{spawn_pipeline, AnthropicWorker};

let worker = Arc::new(
    AnthropicWorker::new("claude-3-5-sonnet-20241022")
        .with_max_tokens(1024)
        .with_temperature(1.0)
);

let handles = spawn_pipeline(worker);
```

### With Deduplication (Save 60-80% Costs!)

```rust
use tokio_prompt_orchestrator::enhanced::{Deduplicator, dedup};

let dedup = Deduplicator::new(Duration::from_secs(300)); // 5 min cache

let key = dedup::dedup_key(prompt, &metadata);

match dedup.check_and_register(&key).await {
    DeduplicationResult::Cached(result) => {
        // Return cached result - FREE!
        return Ok(result);
    }
    DeduplicationResult::New(token) => {
        // Process new request
        let result = process_request(prompt).await?;
        dedup.complete(token, result.clone()).await;
        Ok(result)
    }
    DeduplicationResult::InProgress => {
        // Wait for in-progress request
        dedup.wait_for_result(&key).await
    }
}
```

### With Circuit Breaker

```rust
use tokio_prompt_orchestrator::enhanced::CircuitBreaker;

let breaker = CircuitBreaker::new(
    5,                          // 5 failures opens circuit
    0.8,                        // 80% success rate closes it
    Duration::from_secs(60),    // 60 second timeout
);

match breaker.call(|| async {
    worker.infer(prompt).await
}).await {
    Ok(result) => Ok(result),
    Err(CircuitBreakerError::Open) => {
        // Service is down, fail fast
        Err("Service temporarily unavailable")
    }
    Err(CircuitBreakerError::Failed(e)) => Err(e),
}
```

### With Retry Logic

```rust
use tokio_prompt_orchestrator::enhanced::RetryPolicy;

let policy = RetryPolicy::exponential(3, Duration::from_millis(100));

let result = policy.retry(|| async {
    worker.infer(prompt).await
}).await?;
```

### REST API Usage

**Submit request:**
```bash
curl -X POST http://localhost:8080/api/v1/infer \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "What is Rust?",
    "session_id": "user-123",
    "metadata": {"priority": "high"}
  }'
```

**Response:**
```json
{
  "request_id": "550e8400-e29b-41d4-a716-446655440000",
  "status": "processing"
}
```

**Get result:**
```bash
curl http://localhost:8080/api/v1/result/550e8400-e29b-41d4-a716-446655440000
```

### WebSocket Streaming

```javascript
const ws = new WebSocket('ws://localhost:8080/api/v1/stream');

ws.send(JSON.stringify({
  prompt: "Tell me a story",
  session_id: "user-123"
}));

ws.onmessage = (event) => {
  const data = JSON.parse(event.data);
  console.log(data.result);
};
```

##  Architecture

### Pipeline Stages

```text
┌─────────────┐  512  ┌──────────────┐  512  ┌──────────────┐
│  RAG Stage  │──────→│ Assemble     │──────→│  Inference   │
│  (5ms)      │       │ (format)     │       │  (Model)     │
└─────────────┘       └──────────────┘       └──────────────┘
                                                     │ 1024
                                                     ↓
                    ┌──────────────┐       ┌──────────────┐
                    │   Stream     │←──────│    Post      │
                    │   (output)   │  512  │  (filter)    │
                    └──────────────┘       └──────────────┘
```

**Channel sizes:** 512 → 512 → 1024 → 512 → 256

### Key Design Decisions

- **Bounded Channels** - Prevent memory exhaustion under load
- **Backpressure** - Graceful shedding when queues are full
- **Object-Safe Workers** - Hot-swappable inference backends
- **Session Affinity** - Deterministic sharding for optimization

## TUI Dashboard

Real-time terminal dashboard for monitoring the orchestrator pipeline.

```bash
# Mock mode (default) — 2-minute scripted story that loops
cargo run --bin tui --features tui

# Live mode — connect to Prometheus endpoint
cargo run --bin tui --features tui -- --live

# Custom metrics URL
cargo run --bin tui --features tui -- --live --metrics-url http://host:9090/metrics
```

```text
┌─ tokio-prompt-orchestrator ──────────────────────────── 2026-02-21 14:32:01 ─┐
│ ┌─ Pipeline Flow ─────────────────────┐ ┌─ System Health ──────────────────┐  │
│ │  [RAG]──→[ASM]──→[INF]──→[PST]──→  │ │  CPU  ████████░░  62%            │  │
│ │  3.1ms   1.7ms  270ms   2.0ms      │ │  MEM  ██████░░░░  38%            │  │
│ │                  ▲ active           │ │  Tasks: 210    Uptime: 1h 32m    │  │
│ ├─ Channel Depths ────────────────────┤ ├─ Circuit Breakers ───────────────┤  │
│ │  RAG→ASM  ████░░░░  15%  77/512    │ │  ● openai    CLOSED  (3247 ok)   │  │
│ │  ASM→INF  ██░░░░░░   8%  41/512    │ │  ● anthropic CLOSED  (4120 ok)   │  │
│ │  INF→PST  █████░░░  25% 256/1024   │ │  ○ llama.cpp OPEN    (opened 3s) │  │
│ │  PST→STR  █░░░░░░░   6%  31/512    │ ├─ Dedup Savings ──────────────────┤  │
│ ├─ Throughput (req/s) ────────────────┤ │  Requests:  12.4K  Infer: 2.8K   │  │
│ │  ▂▃▅▇█▇▅▃▂▁▂▃▅▇█▇▅▃▂▁▂▃▅▇        │ │  Savings:   77.4%  $145.20       │  │
│ ├─ Log ───────────────────────────────┤ └──────────────────────────────────┘  │
│ │  14:32:01 INFO  Request deduped     key=0a3f2c session=user-42             │
│ │  14:32:01 WARN  Circuit OPEN        worker=llama.cpp failures=5            │
│ │  14:32:02 ERROR Worker timeout      worker=llama.cpp attempt=2/3           │
│ └──────────────────────────────────── [q]uit [p]ause [r]eset [h]elp ─────────┘
```

Keybindings: `q` quit, `p` pause, `r` reset, `h` help, `↑↓` scroll log.

## Claude Desktop & Claude Code Integration (MCP)

Use tokio-prompt-orchestrator as a native Claude tool via the Model Context Protocol (MCP).

### Setup

1. Build the MCP server:
   ```bash
   cargo build --bin mcp --features mcp --release
   ```

2. **Claude Desktop** — add to `~/Library/Application Support/Claude/claude_desktop_config.json`:
   ```json
   {
     "mcpServers": {
       "tokio-prompt-orchestrator": {
         "command": "/path/to/target/release/mcp",
         "args": ["--worker", "llama_cpp"]
       }
     }
   }
   ```

3. **Claude Code** — add to `.claude/mcp.json` in your project:
   ```json
   {
     "mcpServers": {
       "tokio-prompt-orchestrator": {
         "command": "./target/release/mcp",
         "args": ["--worker", "echo"]
       }
     }
   }
   ```

### Available Tools

| Tool | Description |
|------|-------------|
| `infer` | Run a prompt through the 5-stage pipeline. Returns output, session ID, request ID, latency, and per-stage timing. |
| `pipeline_status` | Real-time health: circuit breaker states, channel depths, dedup savings, throughput. |
| `batch_infer` | Submit multiple prompts in one call. Returns a job ID for tracking. |
| `configure_pipeline` | Hot-swap worker, retry attempts, circuit breaker threshold, and rate limits at runtime. |

### Workers

Pass `--worker <name>` when starting the MCP server:

- `echo` (default) — testing/demo, no external dependencies
- `llama_cpp` — local inference via llama.cpp server (`LLAMA_CPP_URL`, default `http://localhost:8080`)

### Example

```
You: "Run 'explain quicksort' through my local pipeline"
Claude: calls infer(prompt="explain quicksort") → returns output + stage latencies
```

##  Monitoring & Observability

### Prometheus Metrics

Start metrics server:
```bash
cargo run --example metrics_demo --features metrics-server
```

Access metrics: http://localhost:9090/metrics

**Available metrics:**
- `orchestrator_requests_total` - Request throughput
- `orchestrator_stage_duration_seconds` - Latency per stage
- `orchestrator_queue_depth` - Current queue depth
- `orchestrator_requests_shed_total` - Backpressure events
- `orchestrator_errors_total` - Error rates

### Grafana Dashboard

```bash
# Start full monitoring stack
docker-compose up -d
```

Access Grafana: http://localhost:3000 (admin/admin)

**Dashboard includes:**
- Request rate graphs
- Latency percentiles (p50, p95, p99)
- Queue depth monitoring
- Error rate tracking
- 9 pre-configured panels

### Alerts

5 critical alerts included:
- High error rate (>5% for 2 min)
- High latency (p95 >5s for 5 min)
- High backpressure (>10% shed for 5 min)
- Queue depth high (>900 for 2 min)
- Service down (>1 min)

##  Configuration

### Feature Flags

```toml
[features]
default = []
web-api = ["axum", "tower", "tower-http", "tokio-stream", "futures"]
metrics-server = ["axum", "tower", "tower-http"]
caching = ["dep:redis"]
rate-limiting = ["governor", "nonzero_ext"]
tui = ["ratatui", "crossterm"]
mcp = ["dep:rmcp"]
dashboard = ["axum", "tower", "tower-http", "tokio-stream", "dep:async-stream"]
distributed = ["dep:async-nats", "dep:redis"]
full = ["web-api", "metrics-server", "caching", "rate-limiting", "tui", "dashboard"]
```

**Build with all features:**
```bash
cargo build --features full
```

### Environment Variables

```bash
# Model API keys
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."

# Local model servers
export LLAMA_CPP_URL="http://localhost:8080"
export VLLM_URL="http://localhost:8000"

# Redis (optional)
export REDIS_URL="redis://localhost:6379"

# Distributed clustering (Phase 5)
export NATS_URL="nats://localhost:4222"

# Server config
export SERVER_PORT="8080"
export METRICS_PORT="9090"
export DASHBOARD_PORT="3030"
```

## Testing

**959 tests passing** across unit, integration, property-based, and doc tests.

```bash
# Run all tests
cargo test --all-features

# Run specific module tests
cargo test enhanced::dedup
cargo test enhanced::circuit_breaker
cargo test enhanced::retry
cargo test config::validation
cargo test routing::
cargo test distributed::
cargo test coordination::

# Run integration tests only
cargo test --test mcp_tests
cargo test --test enhanced_hardening_tests
cargo test --test worker_extra_tests
cargo test --test distributed_integration

# Run benchmarks (criterion)
cargo bench
cargo bench --bench pipeline
cargo bench --bench enhanced
cargo bench --bench workers

# Run with output
cargo test -- --nocapture
```

## Docker Deployment

### Quick Start

```bash
docker-compose up -d
```

**Includes:**
- Orchestrator (ports 8080, 9090)
- Prometheus (port 9091)
- Grafana (port 3000)
- Redis (port 6379)
- Alertmanager (port 9093)

### Custom Deployment

```dockerfile
FROM rust:1.79 as builder
WORKDIR /app
COPY . .
RUN cargo build --release --features full

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/orchestrator /app/
EXPOSE 8080 9090
CMD ["/app/orchestrator"]
```

##  Documentation

### Comprehensive Guides

- **[WORKERS.md](WORKERS.md)** - Model integration guide (400+ lines)
- **[METRICS.md](METRICS.md)** - Observability setup (600+ lines)
- **[ARCHITECTURE.md](ARCHITECTURE.md)** - Design deep-dive
- **[HIGH_IMPACT.md](HIGH_IMPACT.md)** - Cost optimization guide
- **[BENCHMARKS.md](BENCHMARKS.md)** - Performance benchmarks and contracts
- **[QUICKSTART.md](QUICKSTART.md)** - Getting started

### API Documentation

```bash
cargo doc --open
```

Navigate to `tokio_prompt_orchestrator` for full API docs.

##  Use Cases

### Cost-Sensitive Applications

**Scenario:** Chat application with 1M requests/month

- Without optimization: $30,000/month
- With deduplication: $9,000/month (70% hit rate)
- **Savings: $21,000/month**

### High-Reliability Services

**Scenario:** Customer-facing API requiring 99.9% uptime

- Retry logic: 95% → 99.75% success rate
- Circuit breaker: Fast-fail during outages
- **Result: 4.75% fewer user-visible errors**

### Multi-Model Routing

**Scenario:** Route requests to different models based on complexity

```rust
let worker = if prompt.len() < 100 {
    Arc::new(OpenAiWorker::new("gpt-3.5-turbo-instruct")) // Cheap
} else {
    Arc::new(OpenAiWorker::new("gpt-4")) // Powerful
};
```

### A/B Testing

**Scenario:** Compare model performance

```rust
let worker = if rand::random::<f32>() < 0.5 {
    Arc::new(OpenAiWorker::new("gpt-4"))
} else {
    Arc::new(AnthropicWorker::new("claude-3-5-sonnet-20241022"))
};
```

##  Roadmap

### Completed

- [x] 5-stage pipeline with backpressure
- [x] 4 production model workers (OpenAI, Anthropic, llama.cpp, vLLM)
- [x] Prometheus metrics + Grafana dashboards
- [x] REST API + WebSocket streaming
- [x] Caching, rate limiting, priority queues
- [x] Request deduplication (60-80% cost savings)
- [x] Circuit breaker (failure protection)
- [x] Retry logic with exponential backoff
- [x] Declarative TOML configuration with validation and hot-reload
- [x] TUI dashboard (story mode + live Prometheus mode)
- [x] MCP server for Claude Desktop and Claude Code integration
- [x] Criterion benchmark harness with performance contracts
- [x] Distributed clustering (NATS pub/sub, Redis dedup, leader election)
- [x] Intelligent model routing (complexity scoring, adaptive thresholds, cost tracking)
- [x] Agent fleet coordination (task claiming, priority queues, health monitoring)
- [x] Web dashboard with real-time SSE metrics
- [x] 959 tests (unit, integration, property-based, doc)

### Coming Soon

- [ ] OpenAPI/Swagger documentation
- [ ] Distributed tracing (Jaeger/Zipkin)

### Future

- [ ] Streaming inference support
- [ ] Auto-scaling based on queue depth
- [ ] Model fine-tuning integration

##  Contributing

Contributions welcome! Please:

1. Fork the repository
2. Create a feature branch
3. Add tests for new features
4. Ensure `cargo test` passes
5. Run `cargo fmt` and `cargo clippy`
6. Submit a pull request

##  License

MIT License - see [LICENSE](LICENSE) file for details.

##  Acknowledgments

Built with:
- [Tokio](https://tokio.rs/) - Async runtime
- [Axum](https://github.com/tokio-rs/axum) - Web framework
- [NATS](https://nats.io/) - Distributed messaging
- [Prometheus](https://prometheus.io/) - Metrics
- [Tracing](https://tracing.rs/) - Structured logging

## Project Stats

- **Source Files**: 45+ Rust modules across `src/`, `tests/`, `benches/`
- **Lines of Code**: ~23,600 (src/) + tests and benchmarks
- **Tests**: 959 passing (unit, integration, property-based, doc)
- **Benchmarks**: 30+ criterion benchmarks with enforced budgets
- **New Modules**: distributed clustering, model routing, agent coordination, web dashboard
- **Dependencies**: Minimal, production-grade, feature-gated
- **Performance**: <1ms overhead for all resilience primitives
- **Reliability**: 99%+ with default retry configuration

##  Star History

If this project helps you, please consider giving it a star! 

---

**Built for production. Optimized for cost. Ready for scale.**
