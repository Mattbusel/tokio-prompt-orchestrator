# tokio-prompt-orchestrator

[![CI](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/tokio-prompt-orchestrator.svg)](https://crates.io/crates/tokio-prompt-orchestrator)
[![docs.rs](https://docs.rs/tokio-prompt-orchestrator/badge.svg)](https://docs.rs/tokio-prompt-orchestrator)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/Mattbusel/tokio-prompt-orchestrator)

Multi-core, Tokio-native orchestration for LLM inference pipelines. Built with backpressure, circuit breakers, deduplication, and an autonomous self-improving control loop.

We ran 24 Claude Code agents simultaneously on a single RTX 4070. They wrote this codebase in one night (129,000+ lines of Rust, 1,491 tests, zero panics) while the orchestrator they were building managed their own inference traffic. The dedup layer collapsed redundant context across agents into single inference calls, cutting total API consumption to 26% of expected usage across all 24 agents over 90 minutes.

Anthropic's documented ceiling is 16 concurrent agents. We hit 24. The bottleneck was the API rate limit, not the orchestrator.

[![Star History Chart](https://api.star-history.com/svg?repos=Mattbusel/tokio-prompt-orchestrator&type=Date)](https://star-history.com/#Mattbusel/tokio-prompt-orchestrator)

---

## Windows .exe — No Install Required

Download **[orchestrator.exe](https://github.com/Mattbusel/tokio-prompt-orchestrator/releases/tag/v0.1.0)** from the releases page and double-click it. No Rust, no dependencies, nothing to install. The binary is **7.6 MB** — the entire orchestrator, all features included. It runs on any Windows machine, including low-end hardware, VMs, and air-gapped environments.

### Step 1 — Get an API key (one-time, ~2 minutes)

You need a key from whichever AI provider you want to use:

| Provider | Where to get a key | Starting cost |
|---|---|---|
| **Anthropic** (recommended) | https://console.anthropic.com/settings/keys | ~$5 credit |
| **OpenAI** | https://platform.openai.com/api-keys | ~$5 credit |
| **llama.cpp** | No key needed — runs locally | Free |
| **echo** | No key needed — offline test mode | Free |

### Step 2 — Run the wizard

The first time you open `orchestrator.exe` it walks you through setup:

```text
  ┌──────────────────────────────────────────────────┐
  │       Welcome to tokio-prompt-orchestrator        │
  │  Setup takes about 60 seconds.                    │
  └──────────────────────────────────────────────────┘

  Which AI provider do you want to use?

  1) Anthropic  (Claude — recommended for most users)
     Get a key: https://console.anthropic.com/settings/keys

  2) OpenAI     (GPT-4o and friends)
     Get a key: https://platform.openai.com/api-keys

  3) llama      (run models locally — no key needed)
  4) echo       (test mode — no key, no internet)

  Enter 1, 2, 3, or 4 [1]: 1

  Paste your key here (sk-ant-…): sk-ant-xxxxx
  ✓ Key saved.

  Model name (press Enter to use default) [claude-sonnet-4-6]: ↵
  Port for the web API [8080]: ↵

  ✓ All done! Settings saved to orchestrator.env next to this .exe.
    Run with --reset any time to change your key or provider.
```

Your key is stored only in `orchestrator.env` on your machine — never transmitted anywhere except to the provider you chose.

### Step 3 — Start using it

After setup the banner appears and you can type prompts straight into the terminal:

```text
╔══════════════════════════════════════════════════╗
║   tokio-prompt-orchestrator v0.1.0               ║
║  Provider : anthropic (claude-sonnet-4-6)        ║
║  Web API  : http://127.0.0.1:8080                ║
╠══════════════════════════════════════════════════╣
║  HOW TO USE                                      ║
║                                                  ║
║  Terminal  ─ type any question below             ║
║                                                  ║
║  Agents / IDEs ─ HTTP while this window is open: ║
║   POST http://127.0.0.1:8080/v1/prompt           ║
║   WS   ws://127.0.0.1:8080/v1/stream             ║
║                                                  ║
║  Claude Desktop ─ claude_desktop_config.json:    ║
║   mcpServers > orchestrator > url:               ║
║     "http://127.0.0.1:8080"                      ║
╚══════════════════════════════════════════════════╝

Ask me anything — try: What can you help me with?
Type 'exit' to quit.

> What can you help me with?

I can answer questions, write and review code, summarise documents,
help plan projects, and more — routed through a resilience pipeline
with automatic retries and deduplication.

>
```

The terminal REPL and the web API **run at the same time** — you type in the window while agents and IDEs connect to `http://127.0.0.1:8080` in the background.

### Connecting agents and IDEs

**Claude Desktop** — add to `claude_desktop_config.json` and restart Claude Desktop:
```json
{
  "mcpServers": {
    "orchestrator": {
      "url": "http://127.0.0.1:8080"
    }
  }
}
```

**VS Code / Cursor / Windsurf** — point your AI extension's custom endpoint at `http://127.0.0.1:8080/v1/prompt`

**curl:**
```bash
curl -X POST http://127.0.0.1:8080/v1/prompt \
  -H "Content-Type: application/json" \
  -d '{"input": "What is the capital of France?"}'
```

**Python / any script:**
```python
import requests
r = requests.post("http://127.0.0.1:8080/v1/prompt",
                  json={"input": "Summarise this codebase"})
print(r.json()["text"])
```

### Changing your key or provider

Run `orchestrator.exe --reset` — it wipes the saved settings and runs the wizard again. Or edit `orchestrator.env` directly (it's a plain text file next to the `.exe`).

---

## Library Usage

```toml
# Cargo.toml
[dependencies]
tokio-prompt-orchestrator = { git = "https://github.com/Mattbusel/tokio-prompt-orchestrator" }
tokio = { version = "1", features = ["full"] }
```

```rust,ignore
use tokio_prompt_orchestrator::{Pipeline, PipelineConfig, EchoWorker, InferenceRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build a pipeline with an echo worker.
    // Swap in OpenAIWorker, AnthropicWorker, or LlamaCppWorker for real inference.
    let config = PipelineConfig::default();
    let worker = EchoWorker::new();
    let pipeline = Pipeline::new(config, worker).await?;

    // Dedup, circuit breaker, and retry all happen transparently.
    let req = InferenceRequest::new("Explain backpressure in one sentence.");
    let response = pipeline.infer(req).await?;

    println!("{}", response.text);
    Ok(())
}
```

See [`examples/`](examples/) for full working examples: OpenAI, Anthropic, llama.cpp, vLLM, REST, WebSocket, SSE streaming, and multi-worker setups.

---

## Live Dashboard

![TUI Dashboard](assets/tui-dashboard.png)

Real-time terminal dashboard: 985 requests in, 323 actual inferences (67.2% dedup collapse, **$7.94 saved in 3 minutes**). Circuit breakers across OpenAI, Anthropic, and llama.cpp all CLOSED. A latency spike at INFER p99=483ms caused the circuit to open; a probe was sent, and recovery was confirmed in 15 seconds. Built with ratatui.

---

## What It Does

A five-stage async pipeline for LLM inference with a closed-loop self-improving control system. Bounded channels enforce backpressure end-to-end. Every request flows through:

```text
RAG -> Assemble -> Inference -> Post-Process -> Stream
```

The self-improving stack runs alongside the pipeline as a live background service. It observes telemetry, detects anomalies, tunes parameters, and records outcomes without human intervention.

---

## Architecture

**Pipeline** -- Bounded async channels (RAG->ASM: 512, ASM->INF: 512, INF->PST: 1024, PST->STR: 256). Session affinity via deterministic sharding. Requests are shed rather than blocked when queues fill.

**Resilience** -- Circuit breaker (opens at 5 failures, closes at 80% success rate, 60s timeout). Retry with exponential backoff and jitter. Token-bucket rate limiting. Priority queues with 4 levels. In-memory and Redis caching with TTL.

**Self-Improving Loop** -- `SelfImprovementLoop` runs as a single background Tokio task:

```text
TelemetryBus
    |  broadcast snapshot every 5s
    v
AnomalyDetector  ---- Z-score + CUSUM ---> TuningController
                                                |  PID adjusts 12 params
                                                v
MetaTaskGenerator <------------------- SnapshotStore
    |  triggered on Warning/Critical       records best configs
    v
ValidationGate  (cargo test + clippy)
    v
AgentMemory  (outcome recorded, dead ends flagged)
```

Start it: `cargo run --bin coordinator --features self-improving -- --self-improve`

**Intelligence Layer** -- `IntelligenceBridge` wires six learned systems into the pipeline:

- `LearnedRouter` -- epsilon-greedy multi-armed bandit, routes by observed quality
- `Autoscaler` -- predictive, scales worker capacity before demand arrives
- `FeedbackCollector` -- aggregates quality signals from post-processing
- `QualityEstimator` -- scores response quality at inference time
- `PromptOptimizer` -- rewrites prompts to minimize token spend
- `SemanticDedup` -- embedding-based dedup that collapses semantically equivalent prompts before they reach the provider

These run as a closed loop: feedback scores update the router, the router changes load distribution, and the autoscaler reacts.

**HelixRouter Integration** -- Three `self_tune` modules bridge the orchestrator to HelixRouter in real time. `helix_probe` polls `/api/stats` and converts queue depth, drop rate, and latency into a pressure signal. `helix_config_pusher` writes PID-derived adjustments back via `PATCH /api/config`. `helix_feedback` forwards quality scores from the intelligence layer as routing hints.

**Capability Discovery** -- `CapabilityDiscovery` runs `cargo check --message-format=json` to detect dead code and `cargo audit --json` to surface CVEs. Findings become tasks for the agent fleet.

**Distributed** -- NATS pub/sub for inter-node messaging. Redis-based cross-node dedup via atomic `SET NX EX`. Leader election with TTL renewal. Cluster manager with heartbeat tracking.

**Coordination** -- Agent fleet management. Atomic task claiming via filesystem locks. Priority ordering from TOML task files. Configurable health monitoring.

**Routing** -- Complexity scoring routes simple prompts to local llama.cpp and complex ones to cloud APIs. Adaptive thresholds tune themselves on success/failure feedback. Per-model cost tracking with budget awareness.

**Observability** -- Prometheus metrics, TUI terminal dashboard, web dashboard with SSE streaming, structured JSON tracing. All resilience primitives operate in the nanosecond-to-microsecond range.

**MCP** -- `infer`, `pipeline_status`, `batch_infer`, and `configure_pipeline` are all callable from Claude Desktop and Claude Code, with live stage latency reporting.

---

## Quick Start

```bash
git clone https://github.com/Mattbusel/tokio-prompt-orchestrator.git
cd tokio-prompt-orchestrator

# Demo (no API keys needed)
cargo run --bin orchestrator-demo

# TUI dashboard
cargo run --bin tui --features tui

# Full self-improving stack
cargo run --bin tui --features self-improving,tui

# Coordinator with self-improvement loop
cargo run --bin coordinator --features self-improving -- --self-improve

# Web dashboard at http://localhost:3000
cargo run --bin dashboard --features dashboard

# MCP server for Claude Desktop / Claude Code
cargo run --bin mcp --features mcp -- --worker echo
```

---

## Feature Flags

```toml
self-tune        # PID controllers, telemetry bus, anomaly detection
self-modify      # Agent loop: task generation, validation gate, memory
intelligence     # LearnedRouter, Autoscaler, FeedbackCollector, QualityEstimator
evolution        # Snapshot store, A/B experiments, rollback
self-improving   # Full autonomous optimization stack (all of the above)
tui              # Terminal dashboard (ratatui)
mcp              # Claude MCP server (rmcp 0.16)
dashboard        # Web dashboard with SSE streaming
distributed      # NATS pub/sub + Redis clustering
full             # web-api, metrics-server, caching, rate-limiting, tui, dashboard
```

---

## Environment Variables

Complete reference for all environment variables recognised by the orchestrator binary and library.

| Variable | Default | Required | Description |
|---|---|---|---|
| `OPENAI_API_KEY` | — | For OpenAI workers | API key for `OpenAiWorker`. Must start with `sk-`. |
| `ANTHROPIC_API_KEY` | — | For Anthropic workers | API key for `AnthropicWorker`. Must start with `sk-ant-`. |
| `LLAMA_CPP_URL` | `http://localhost:8080` | No | llama.cpp server base URL. |
| `VLLM_URL` | `http://localhost:8000` | No | vLLM inference server base URL. |
| `REDIS_URL` | `redis://localhost:6379` | For `caching` / `distributed` features | Redis connection string. |
| `NATS_URL` | `nats://localhost:4222` | For `distributed` feature | NATS broker URL for inter-node messaging. |
| `RUST_LOG` | `info` | No | `tracing` log filter. E.g. `tokio_prompt_orchestrator=debug,info`. |
| `RUST_LOG_FORMAT` | *(pretty)* | No | Set to `json` for newline-delimited JSON logs (Datadog / Loki / Grafana Loki). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | — | No | OTLP exporter endpoint. Activates the OTel tracing layer when set. |
| `JAEGER_ENDPOINT` | — | No | Jaeger OTLP HTTP endpoint (e.g. `http://localhost:4318`). Checked before `OTEL_EXPORTER_OTLP_ENDPOINT`. |
| `API_KEY` | — | No | Bearer token for HTTP API authentication. Omit to run unauthenticated. |
| `PORT` | `8080` | No | HTTP port for the web API / metrics server. |

```bash
# Minimal setup (echo worker, no external services)
RUST_LOG=info cargo run --bin orchestrator

# Anthropic with structured logging and Jaeger tracing
ANTHROPIC_API_KEY="sk-ant-..." \
RUST_LOG_FORMAT=json \
JAEGER_ENDPOINT="http://localhost:4318" \
cargo run --bin orchestrator --features full
```

---

## Prometheus Metrics

Start the metrics server with the `metrics-server` feature:

```bash
cargo run --bin orchestrator --features metrics-server
```

Prometheus scrape endpoint: `http://127.0.0.1:9090/metrics`

Example metrics exposed:

| Metric | Type | Description |
|---|---|---|
| `orchestrator_requests_total{stage}` | Counter | Requests entering each pipeline stage |
| `orchestrator_requests_shed_total{stage}` | Counter | Requests dropped due to backpressure |
| `orchestrator_errors_total{stage}` | Counter | Errors per pipeline stage |
| `orchestrator_stage_latency_seconds{stage}` | Histogram | Per-stage latency distribution |
| `orchestrator_dedup_hits_total` | Counter | Requests collapsed by deduplication |
| `orchestrator_circuit_breaker_state{backend}` | Gauge | Circuit state (0=closed, 1=open, 2=half-open) |
| `orchestrator_inference_cost_usd_total` | Counter | Cumulative inference cost |
| `orchestrator_dlq_dropped_total` | Counter | Requests stored in dead-letter queue |

Add to `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: orchestrator
    static_configs:
      - targets: ['localhost:9090']
```

---

## OpenTelemetry

Distributed tracing is enabled automatically when `JAEGER_ENDPOINT` or
`OTEL_EXPORTER_OTLP_ENDPOINT` is set. The exporter uses HTTP/protobuf (OTLP).

```bash
# Jaeger all-in-one (local dev)
docker run -p 4318:4318 -p 16686:16686 jaegertracing/all-in-one

JAEGER_ENDPOINT="http://localhost:4318" cargo run --bin orchestrator
# View traces at http://localhost:16686
```

```bash
# Any OTLP-compatible backend (Grafana Tempo, Honeycomb, Datadog, etc.)
OTEL_EXPORTER_OTLP_ENDPOINT="http://otel-collector:4318" cargo run --bin orchestrator
```

When neither variable is set the OTel layer is omitted and the binary starts
without blocking on a collector. Service name in traces: `tokio-prompt-orchestrator`.

---

## Numbers

```text
Lines of code       129,442
Tests               1,491 passing, 0 failing
Benchmarks          30+ criterion, all within budget
Dedup savings       66.7% collapse rate in live demo
Dedup check         ~1.5us p50
Circuit breaker     ~0.4us p50 (closed path)
Cache hit           81ns
Rate limit check    110ns
Channel send        <1us p99
```

---

## Why This vs LangChain / LlamaIndex / CrewAI

| | tokio-prompt-orchestrator | LangChain / LlamaIndex | CrewAI |
|---|---|---|---|
| Language | Rust | Python | Python |
| Latency overhead | Nanoseconds | Milliseconds | Milliseconds |
| Memory safety | Compile-time | Runtime | Runtime |
| Backpressure | Bounded channels, shed on full | Not built-in | Not built-in |
| Circuit breaker | Built-in | Plugin/manual | Not built-in |
| Dedup collapse | 67% in production | Not built-in | Not built-in |
| Self-improving loop | Live, autonomous | Not built-in | Not built-in |
| Concurrency | Tokio async, zero-copy | Python GIL | Python GIL |
| Binary size | **7.6 MB** (single .exe, no runtime) | 100s of MB deps | 100s of MB deps |

Use LangChain if you need to prototype quickly in Python. Use this if you need predictable latency, zero panics, and autonomous optimization at scale.

---

## Observability

The orchestrator exposes three complementary observability surfaces:

### Prometheus metrics (`/metrics`)

Enable with `--features metrics-server`. Scraped at `http://127.0.0.1:9090/metrics`.

| Metric | Type | Description |
|---|---|---|
| `orchestrator_requests_total{stage}` | Counter | Requests entering each pipeline stage |
| `orchestrator_requests_shed_total{stage}` | Counter | Requests dropped (backpressure) per stage |
| `orchestrator_errors_total{stage}` | Counter | Errors per pipeline stage |
| `orchestrator_stage_latency_seconds{stage}` | Histogram | Per-stage latency distribution |
| `orchestrator_dedup_hits_total` | Counter | Requests collapsed by the deduplication layer |
| `orchestrator_circuit_breaker_state{backend}` | Gauge | Circuit state: 0=closed, 1=open, 2=half-open |
| `orchestrator_inference_cost_usd_total` | Counter | Cumulative inference cost in USD |
| `orchestrator_dlq_dropped_total` | Counter | Requests stored in the dead-letter queue |
| `orchestrator_dlq_lock_poisoned_total` | Counter | DLQ mutex poison recovery events (should stay 0) |

A pre-built Grafana dashboard JSON is provided at `grafana-dashboard.json`. Import it into any Grafana instance pointed at your Prometheus datasource.

### Distributed tracing (OpenTelemetry)

Activated automatically when `JAEGER_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Uses OTLP HTTP/protobuf. Compatible with Jaeger, Grafana Tempo, Honeycomb, Datadog, and any W3C Trace Context-compliant backend.

Service name in traces: `tokio-prompt-orchestrator`. Each pipeline stage creates a child span, so the full RAG → Assemble → Inference → Post → Stream latency breakdown is visible in any trace view.

When neither variable is set, the OTel layer is omitted entirely — the binary starts without blocking on a collector.

### TUI terminal dashboard

Enable with `--features tui`. Displays real-time throughput, dedup savings, per-stage latency (p50/p95/p99), circuit breaker states, and the self-improvement loop status in a ratatui terminal UI.

```bash
cargo run --bin tui --features tui
```

---

## Architecture Decision Records

The three most consequential design decisions and the reasoning behind them:

| # | Decision | Alternatives considered | Rationale |
|---|---|---|---|
| ADR-001 | **Bounded `mpsc` channels with load-shedding instead of unbounded queues** | Unbounded queues; backpressure via `await` on `send` | Unbounded queues allow memory growth unbounded by arrival rate. Blocking `send` propagates latency upstream and can deadlock pipeline stages. Bounded channels with `try_send` + shed give predictable memory and latency at the cost of some requests being dropped — an explicit and measurable tradeoff. |
| ADR-002 | **Single-pass deduplication via FNV-1a hash with configurable TTL** | SHA-256 content hashing; embedding-based semantic dedup; no dedup | SHA-256 is 10-100x slower on the hot path. Embedding-based dedup requires a model call to dedup, which defeats the purpose. FNV-1a at ~1.5 µs p50 gives 67% collapse rate in production without adding a model dependency. Semantic dedup is available as an opt-in (`SemanticDedup`) for cases where FNV collisions are insufficient. |
| ADR-003 | **Circuit breaker opens on 5 consecutive failures, closes at 80% success rate** | Fixed timeout with automatic reset; no circuit breaker | Fixed-timeout reset re-routes traffic to a degraded provider at a fixed interval, causing thundering-herd recovery. A success-rate gate (80%) closes the circuit only after confirming the provider is healthy, reducing retry storms. The 5-failure threshold avoids tripping on transient single-request errors while catching genuine provider outages within one request cycle. |

---

## Why This Works

The inference cost problem is not solved at the model layer. It is solved at the orchestration layer. A production-grade Rust orchestrator can collapse 60-80% of LLM API spend through deduplication before any model optimization. The self-improving stack adds a control loop that tunes pipeline parameters continuously.

`SelfImprovementLoop` is a live background service. `IntelligenceBridge` is a closed feedback loop between quality estimation and routing. `CapabilityDiscovery` executes `cargo check` and `cargo audit` against the running codebase. The system improves itself while serving inference.

The infrastructure here -- bounded pipelines, adaptive routing, multi-model failover, MCP-native tooling, agent fleet coordination, autonomous optimization -- is the foundation every serious LLM deployment will need.

---

## Safety

This crate uses `#![forbid(unsafe_code)]`. No unsafe blocks exist in production code paths. Lints `clippy::unwrap_used`, `clippy::expect_used`, and `clippy::panic` are set to `deny`. This policy held across 24 concurrent agents building the codebase in one session without a single runtime panic.

## Minimum Supported Rust Version

Rust **1.85 or later** is required. The MSRV is tested in CI on every push. Changes to the MSRV are treated as semver minor bumps.

## License

MIT

---

## Ecosystem

This orchestrator is built on a set of standalone Rust primitives. Each is independently usable:

| Crate | Role |
|---|---|
| [tokio-agent-memory](https://github.com/Mattbusel/tokio-agent-memory) | Agent memory bus -- episodic recall for the self-improving loop |
| [llm-budget](https://github.com/Mattbusel/llm-budget) | Hard cost enforcement across the agent fleet |
| [llm-sync](https://github.com/Mattbusel/llm-sync) | CRDT state sync for distributed multi-node deployments |
| [fin-stream](https://github.com/Mattbusel/fin-stream) | Lock-free ingestion pipeline -- same pattern as the inference queue |

**Related tools:**
- [Every-Other-Token](https://github.com/Mattbusel/Every-Other-Token) -- token-level LLM research tool whose telemetry feeds HelixRouter
- [agent-runtime](https://github.com/Mattbusel/agent-runtime) -- unified Tokio agent runtime: orchestration, memory, knowledge graph, and ReAct loop
- [llm-cost-dashboard](https://github.com/Mattbusel/llm-cost-dashboard) -- ratatui terminal dashboard for real-time LLM token spend

---

## Related Projects

- [rust-crates](https://github.com/Mattbusel/rust-crates) -- Production Rust crates for LLM agent infrastructure
- [tokio-agent-memory](https://github.com/Mattbusel/tokio-agent-memory) -- Episodic and semantic memory for async Tokio agents
- [Every-Other-Token](https://github.com/Mattbusel/Every-Other-Token) -- LLM token interpretability and streaming perplexity visualization
