# tokio-prompt-orchestrator

[![Rust](https://img.shields.io/badge/rust-1.79%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/tests-893_passing-brightgreen.svg)]()

We ran 24 Claude Code agents simultaneously on a single RTX 4070. They wrote this codebase in one night — 23,600 lines of Rust, 893 tests, zero panics — while the orchestrator they were building managed their own inference traffic. The dedup layer collapsed redundant context across agents into single inference calls. Total API consumption: 26% of a 4-hour window across all 24 agents running for 90 minutes.

Anthropic's documented ceiling is 16 concurrent agents. We hit 24 and the bottleneck was the API rate limit, not the orchestrator.

## Live Dashboard

![TUI Dashboard](assets/tui-dashboard.png)

Real-time terminal dashboard: 985 requests in → 323 actual inferences (67.2% dedup collapse, **$7.94 saved in 3 minutes**). Circuit breakers across OpenAI (453 ok), Anthropic (729 ok), and llama.cpp (253 ok) all CLOSED. Log shows the full self-healing loop — latency spike detected at INFER p99=483ms, circuit opened to llama.cpp, probe requests sent, recovery confirmed in 15 seconds, circuit closed. Throughput steady at 15 req/s throughout. Built with ratatui, runs in any terminal.

## What it does

Five-stage async pipeline for LLM inference with a self-improving control loop. Bounded channels enforce backpressure end-to-end. Every request flows through:

```
RAG → Assemble → Inference → Post-Process → Stream
```

The inference stage is wrapped in a circuit breaker that queries live state. The dedup layer sits in front and collapses identical prompts — in the screenshot above, 81 requests became 27 inference calls (66.7% savings). Retry logic with exponential backoff pushes reliability from 95% to 99.75%.

Four model backends: OpenAI, Anthropic, llama.cpp (local GGUF), vLLM. Hot-swappable at runtime through the MCP interface. The routing layer scores prompt complexity and sends simple requests to local models — cutting cloud API spend by up to 80% in cost-sensitive deployments.

## Architecture

**Pipeline** — Bounded async channels (RAG→ASM: 512, ASM→INF: 512, INF→PST: 1024, PST→STR: 256). Session affinity via deterministic sharding, backpressure shedding when queues fill.

**Resilience** — Circuit breaker (5 failures opens, 80% success rate closes, 60s timeout). Retry with exponential backoff + jitter. Rate limiting per session via token bucket. Priority queues with 4 levels. In-memory + Redis caching with TTL.

**Self-Improving Core** — PID controllers tune 12 pipeline parameters in real-time based on telemetry. Anomaly detection (Z-score + CUSUM) flags degradation before it hits users. A/B experiment engine runs controlled rollouts with Welch's t-test significance gating. Snapshot store captures best-performing configs for rollback.

**Distributed** — NATS pub/sub for inter-node messaging. Redis-based cross-node dedup with atomic `SET NX EX`. Leader election with TTL renewal. Cluster manager with heartbeat tracking and load-based routing.

**Coordination** — Agent fleet management. Atomic task claiming via filesystem locks. Priority ordering from TOML task files. Stale lock detection and reclamation. Health monitoring with configurable intervals.

**Routing** — Complexity scoring routes simple prompts to local llama.cpp, complex ones to cloud APIs. Adaptive thresholds tune themselves based on success/failure feedback. Per-model cost tracking with budget awareness and savings reporting.

**Observability** — Prometheus metrics, Grafana-compatible dashboards, TUI terminal dashboard, web dashboard with SSE streaming, structured tracing with JSON + pretty output modes. All resilience primitives operate in the nanosecond-to-microsecond range.

**MCP** — The pipeline exposes itself as native Claude Desktop / Claude Code tools. `infer`, `pipeline_status`, `batch_infer`, `configure_pipeline` — all callable from Claude with live stage latency reporting.

## Quick Start

```bash
git clone https://github.com/Mattbusel/tokio-prompt-orchestrator.git
cd tokio-prompt-orchestrator

# Demo (no API keys needed)
cargo run --bin orchestrator-demo

# TUI dashboard (ratatui, runs in any terminal)
cargo run --bin tui --features tui

# TUI with self-improving control loop active
cargo run --bin tui --features self-improving,tui

# Web dashboard at http://localhost:3000
cargo run --bin dashboard --features dashboard

# MCP server for Claude Desktop / Claude Code
cargo run --bin mcp --features mcp -- --worker echo

# Agent coordinator
cargo run --bin coordinator
```

## Feature Flags

```toml
web-api          # REST API + WebSocket streaming
metrics-server   # Prometheus /metrics endpoint
caching          # Redis-backed response caching
rate-limiting    # Token bucket rate limiter
tui              # Terminal dashboard (ratatui)
mcp              # Claude MCP server (rmcp 0.16)
dashboard        # Web dashboard with SSE streaming
distributed      # NATS pub/sub + Redis clustering
self-tune        # PID controllers + telemetry bus
self-modify      # Config rewrite from telemetry signals
intelligence     # Anomaly detection + experiment engine
evolution        # Snapshot store + rollback + distributed
self-improving   # Full autonomous optimization stack
full             # web-api + metrics-server + caching + rate-limiting + tui + dashboard
```

## Environment Variables

```bash
OPENAI_API_KEY="sk-..."               # OpenAI worker
ANTHROPIC_API_KEY="sk-ant-..."        # Anthropic worker
LLAMA_CPP_URL="http://localhost:8080" # llama.cpp server
VLLM_URL="http://localhost:8000"      # vLLM server
REDIS_URL="redis://localhost:6379"    # Redis (caching + distributed)
NATS_URL="nats://localhost:4222"      # NATS (distributed clustering)
RUST_LOG="info"                       # Log level (trace/debug/info/warn/error)
LOG_FORMAT="json"                     # Structured logs for Datadog/Loki (default: pretty)
```

## Numbers

```
Lines of code       23,600
Tests               893 passing, 0 failing
Benchmarks          30+ criterion, all within budget
Dedup savings       66.7% collapse rate in live demo
Dedup check         ~1.5μs p50
Circuit breaker     ~0.4μs p50 (closed path)
Retry eval          <1ns
Cache hit           81ns
Rate limit check    110ns
Priority push       297ns
Channel send        <1μs p99
```

## Investment Thesis

The inference cost problem is not solved at the model layer — it's solved at the orchestration layer. This system demonstrates that a production-grade Rust orchestrator can collapse 60-80% of LLM API spend through deduplication alone, before any model optimization. The self-improving stack adds a control loop that tunes pipeline parameters continuously — no human intervention required.

The infrastructure built here — bounded pipelines, adaptive routing, multi-model failover, MCP-native tooling, agent fleet coordination — is the substrate every serious LLM deployment will need. We built it in one night with 24 agents. The orchestrator managed its own construction.

Built in one night. The tool built itself.

## License

MIT
