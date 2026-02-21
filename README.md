# tokio-prompt-orchestrator

Production-ready orchestrator for multi-stage LLM inference pipelines in Rust.

[![Rust](https://img.shields.io/badge/rust-1.79%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/tests-1173_passing-brightgreen.svg)]()

## Overview

A 5-stage async pipeline that routes LLM requests through RAG, assembly, inference, post-processing, and streaming — with built-in cost optimization, resilience, and observability.

```text
Request -> RAG -> Assemble -> Inference -> Post-Process -> Stream -> Response
```

## Features

**Pipeline & Workers** — Bounded async channels with backpressure, session affinity, 4 model backends (OpenAI, Anthropic, llama.cpp, vLLM)

**Cost Optimization** — Request deduplication (60-80% savings), intelligent model routing (local vs cloud), adaptive thresholds, cost tracking

**Resilience** — Circuit breaker, retry with exponential backoff, rate limiting, priority queues, caching (in-memory + Redis)

**Distributed Clustering** — NATS pub/sub, Redis cross-node dedup, leader election, cluster topology with heartbeat tracking

**Agent Coordination** — Fleet task management with atomic filesystem-lock claiming, priority ordering, health monitoring

**Observability** — Prometheus metrics, Grafana dashboards, TUI terminal dashboard, web dashboard with SSE, structured tracing

**Claude Integration (MCP)** — Expose the pipeline as native Claude Desktop / Claude Code tools via Model Context Protocol

## Quick Start

```bash
git clone https://github.com/Mattbusel/tokio-prompt-orchestrator.git
cd tokio-prompt-orchestrator

# Demo (no API keys needed)
cargo run --bin orchestrator-demo

# TUI dashboard
cargo run --bin tui --features tui

# Web dashboard at http://localhost:3000
cargo run --bin dashboard --features dashboard

# MCP server for Claude
cargo run --bin mcp --features mcp -- --worker echo

# Agent coordinator
cargo run --bin coordinator
```

## Usage

```rust
use tokio_prompt_orchestrator::{spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);

    let request = PromptRequest {
        session: SessionId::new("user-123"),
        input: "Hello, world!".to_string(),
        meta: Default::default(),
    };

    handles.input_tx.send(request).await.unwrap();
}
```

## Feature Flags

```toml
web-api        # REST API + WebSocket streaming
metrics-server # Prometheus metrics endpoint
caching        # Redis-backed caching
rate-limiting  # Token bucket rate limiter
tui            # Terminal dashboard (ratatui)
mcp            # Claude MCP server (rmcp)
dashboard      # Web dashboard with SSE
distributed    # NATS + Redis clustering
full           # web-api + metrics-server + caching + rate-limiting + tui + dashboard
```

## Environment Variables

```bash
OPENAI_API_KEY="sk-..."            # OpenAI worker
ANTHROPIC_API_KEY="sk-ant-..."     # Anthropic worker
LLAMA_CPP_URL="http://localhost:8080"  # llama.cpp server
VLLM_URL="http://localhost:8000"   # vLLM server
REDIS_URL="redis://localhost:6379" # Redis (caching/distributed)
NATS_URL="nats://localhost:4222"   # NATS (distributed clustering)
```

## Testing

```bash
cargo test --all-features    # 1,173 tests
cargo bench                  # Criterion benchmarks
```

## Docker

```bash
docker-compose up -d   # Orchestrator + Prometheus + Grafana + Redis
```

## Docs

- **[WORKERS.md](WORKERS.md)** — Model integration guide
- **[METRICS.md](METRICS.md)** — Observability setup
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — Design deep-dive
- **[BENCHMARKS.md](BENCHMARKS.md)** — Performance benchmarks
- **[HIGH_IMPACT.md](HIGH_IMPACT.md)** — Cost optimization guide

Full API docs: `cargo doc --open`

## Project Stats

- **Lines of Code**: ~23,600
- **Tests**: 1,173 passing
- **Modules**: Pipeline, workers, config, enhanced (dedup/CB/retry/cache/rate-limit/priority), routing, distributed, coordination, MCP, TUI, web dashboard
- **Benchmarks**: 30+ criterion benchmarks, all within latency budgets

## License

MIT — see [LICENSE](LICENSE)
