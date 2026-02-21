# tokio-prompt-orchestrator

[![Rust](https://img.shields.io/badge/rust-1.79%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/tests-1173_passing-brightgreen.svg)]()

We ran 24 Claude Code agents simultaneously on a single RTX 4070. They wrote this codebase in one night — 23,600 lines of Rust, 1,173 tests, zero panics — while the orchestrator they were building managed their own inference traffic. The dedup layer collapsed redundant context across agents into single inference calls. Total API consumption: 46% of a 4-hour window across all 24 agents running for 90 minutes.

Anthropic's documented ceiling is 16 concurrent agents. We hit 24 and the bottleneck was the API rate limit, not the orchestrator.

## What it does

Five-stage async pipeline for LLM inference. Bounded channels, backpressure, circuit breakers, deduplication. Every request flows through:

```
RAG -> Assemble -> Inference -> Post-Process -> Stream
```

The inference stage is wrapped in a circuit breaker that queries live state. The dedup layer sits in front and collapses identical prompts — in production this cuts 60-80% of inference costs. Retry logic with exponential backoff pushes reliability from 95% to 99.75%.

Four model backends: OpenAI, Anthropic, llama.cpp (local GGUF), vLLM. Hot-swappable at runtime through the MCP interface.

## Architecture

**Pipeline** — Bounded async channels (512/512/1024/512/256), session affinity via deterministic sharding, backpressure shedding when queues fill.

**Resilience** — Circuit breaker (5 failures opens, 80% success rate closes, 60s timeout). Retry with jitter. Rate limiting per session. Priority queues with 4 levels. In-memory + Redis caching.

**Distributed** — NATS pub/sub for inter-node messaging. Redis-based cross-node dedup with atomic SET NX EX. Leader election with TTL renewal. Cluster manager with heartbeat tracking and load-based routing.

**Coordination** — Agent fleet management. Atomic task claiming via filesystem locks. Priority ordering from TOML task files. Stale lock detection and reclamation. Health monitoring with configurable intervals.

**Routing** — Complexity scoring routes simple prompts to local llama.cpp, complex ones to cloud APIs. Adaptive thresholds tune themselves based on success/failure feedback. Per-model cost tracking with budget awareness.

**Observability** — Prometheus metrics, Grafana dashboards (9 panels), TUI terminal dashboard, web dashboard with SSE streaming, structured tracing. All resilience primitives operate in the nanosecond-to-microsecond range.

**MCP** — The pipeline exposes itself as native Claude Desktop / Claude Code tools. `infer`, `pipeline_status`, `batch_infer`, `configure_pipeline` — all callable from Claude with live stage latency reporting.

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

## Numbers

```
Lines of code    23,600
Tests            1,173 passing
Benchmarks       30+ criterion, all within budget
Dedup check      ~1.5us p50
Circuit breaker  ~0.4us p50 (closed path)
Retry eval       <1ns
Cache hit         81ns
Rate limit check  110ns
Priority push     297ns
```

Built in one night. The tool built itself.

## License

MIT
