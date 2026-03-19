# Contributing to tokio-prompt-orchestrator

Thank you for your interest in contributing. This is a research-grade, production-quality system. Contributions that improve reliability, expand test coverage, or extend the resilience pipeline are most welcome.

---

## Development setup

### Prerequisites

1. **Rust toolchain** — install via [rustup](https://rustup.rs/):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   rustup update stable
   ```
   **Minimum Rust version: 1.85** (`rust-version = "1.85"` in `Cargo.toml`). The MSRV is gated in CI.

2. **Clippy and rustfmt**:
   ```bash
   rustup component add clippy rustfmt
   ```

3. **Clone the repo**:
   ```bash
   git clone https://github.com/Mattbusel/tokio-prompt-orchestrator
   cd tokio-prompt-orchestrator
   ```

4. **Install the pre-commit gate** (one-time setup):
   ```bash
   git config core.hooksPath .githooks
   ```
   This runs `scripts/pre_commit.sh` automatically before every commit — covering `cargo fmt`, `cargo clippy`, tests, panic-scan, and the test-ratio check.

### Environment variables

| Variable | Purpose | Required |
|---|---|---|
| `OPENAI_API_KEY` | OpenAI API key for `OpenAiWorker` | Only for OpenAI workers |
| `ANTHROPIC_API_KEY` | Anthropic API key for `AnthropicWorker` | Only for Anthropic workers |
| `LLAMA_CPP_URL` | llama.cpp server URL | Default: `http://localhost:8080` |
| `VLLM_URL` | vLLM inference server URL | Default: `http://localhost:8000` |
| `REDIS_URL` | Redis connection string | Required for `caching` / `distributed` features |
| `NATS_URL` | NATS broker URL | Required for `distributed` feature |
| `RUST_LOG` | `tracing` log filter | Default: `info` |
| `RUST_LOG_FORMAT` | Set to `json` for structured JSON logs (Datadog / Loki) | Optional |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OpenTelemetry OTLP exporter endpoint | Optional — no collector required |
| `JAEGER_ENDPOINT` | Jaeger OTLP endpoint (e.g. `http://localhost:4318`) | Optional |
| `PORT` | HTTP port for the web API server | Default: `8080` |

Example minimal setup (echo worker, no API keys):
```bash
RUST_LOG=info cargo run --bin orchestrator
```

---

## Running tests

```bash
# All unit and integration tests (default features)
cargo test

# With all features
cargo test --all-features

# With verbose output
cargo test -- --nocapture

# A specific test module
cargo test stages::

# With the self-improving stack
cargo test --features self-improving

# Test ratio check (must stay >= 1.5:1)
./scripts/ratio_check.sh
```

All tests must pass before a PR is merged. The project enforces a minimum **1.5:1 test-to-production LOC ratio**. New production code must ship with tests.

---

## Mutation testing

Mutation testing helps verify that your tests actually detect defects rather than just achieving line coverage. We use [`cargo-mutants`](https://mutants.rs/) for this.

### Install

```bash
cargo install cargo-mutants
```

### Run

```bash
# Run mutation testing on the core enhanced and stages modules:
cargo mutants --package tokio-prompt-orchestrator

# Focus on a specific module:
cargo mutants --file src/enhanced/circuit_breaker.rs

# Use the project config (excludes generated code, limits to core modules):
cargo mutants  # picks up mutants.toml automatically
```

Aim for a **caught-mutant rate of ≥ 80%**. Surviving mutants indicate logic that is not covered by any assertion. Review them and either add a test or document why the mutant is equivalent.

The full mutation test run is computationally expensive and is not part of the default CI pipeline. Enable it manually by un-commenting the `mutation-test` job in `.github/workflows/mutation.yml` when you want a deep quality check on a branch.

---

## Running benchmarks

```bash
cargo bench
```

Benchmarks live in `benches/` and use [Criterion](https://bheisler.github.io/criterion.rs/book/):
- `benches/pipeline.rs` — end-to-end pipeline throughput
- `benches/workers.rs` — inference worker latency
- `benches/enhanced.rs` — circuit breaker and dedup hot paths
- `benches/helix.rs` — HelixRouter integration benchmarks

Key latency targets (from production measurements):
- Dedup check: ~1.5 µs p50
- Circuit breaker (closed path): ~0.4 µs p50
- Cache hit: ~81 ns
- Rate limit check: ~110 ns
- Channel send: <1 µs p99

To compare against a baseline:
```bash
cargo bench -- --save-baseline before
# make your change
cargo bench -- --baseline before
```

---

## Code style

All code must pass before committing:

```bash
# Format
cargo fmt --all

# Lint (zero warnings — enforced in Cargo.toml)
cargo clippy --all-features -- -D warnings

# Tests
cargo test --all-features
```

The `Cargo.toml` sets `clippy::unwrap_used` and `clippy::expect_used` to `deny`. The project also forbids unsafe code (`#![forbid(unsafe_code)]`).

Do not use `unwrap()`, `expect()`, or `panic!()` in any production code path. Use `Result`, `Option`, or explicit matching instead.

---

## What we want

- **New resilience primitives** — bulkheads, hedged requests, adaptive timeouts
- **Provider adapters** — Mistral, Gemini, or other LLM providers
- **Test coverage** — all new code must ship with tests; ratio must stay >= 1.5:1
- **Benchmarks** — latency measurements for any new hot-path code
- **Bug reports** — especially anything that causes a panic or a channel deadlock

## What we do not want

- Removing error handling or replacing `Result` returns with `unwrap()`
- Adding dependencies without justification (see `Cargo.toml` policy in `CLAUDE.md`)
- PRs with no tests for new production code

---

## PR checklist

Before opening a pull request, confirm all of the following:

- [ ] `cargo fmt --all` — no formatting diff
- [ ] `cargo clippy --all-features -- -D warnings` — zero warnings
- [ ] `cargo test --all-features` — all tests pass
- [ ] `./scripts/ratio_check.sh` — test-to-production LOC ratio >= 1.5:1
- [ ] No `unwrap()`, `expect()`, or `panic!()` in production paths
- [ ] No `unsafe` blocks
- [ ] Public functions, structs, and enums have `///` doc comments
- [ ] PR description explains the *why*, not just the *what*
- [ ] If a new pipeline stage is added: `PipelineHandles` is updated and `spawn_pipeline_with_config` handles the new stage
- [ ] If a new config field is added: `PipelineConfig` validation is updated and a TOML example is included

### Commit format

```
[feat|fix|test|refactor|perf|docs] short description

Body explaining the motivation and any tradeoffs.
```

---

## Troubleshooting

### Inspecting the dead-letter queue (DLQ)

Requests that are shed under backpressure are written to the `DeadLetterQueue`
ring buffer. Use the debug endpoint (requires `debug_mode = true` in
`ServerConfig`) to inspect its current contents:

```bash
# List all shed requests currently in the DLQ
curl http://localhost:8080/api/v1/debug/dlq | jq .
```

Or inspect it programmatically in Rust:

```rust,no_run
use tokio_prompt_orchestrator::DeadLetterQueue;
use std::sync::Arc;

// Obtain the DLQ handle from PipelineHandles or shared state.
fn inspect(dlq: &Arc<DeadLetterQueue>) {
    for entry in dlq.drain() {
        eprintln!("shed: session={} request_id={} reason={:?}",
            entry.session.as_str(), entry.request_id, entry.reason);
    }
}
```

### Replaying failed requests from the DLQ

After fixing the root cause (e.g. a downstream service outage), drain and
resubmit the shed requests:

```bash
# Replay via the debug replay endpoint
curl -X POST http://localhost:8080/api/v1/debug/dlq/replay
```

The replay endpoint resubmits all items currently in the DLQ back through the
pipeline entry channel. Items that are shed again remain in the DLQ for the
next replay attempt.

### Interpreting circuit-breaker state

The circuit breaker has three states:

| State | Meaning | Action |
|---|---|---|
| `CLOSED` | Normal operation. All requests flow through. | No action needed. |
| `OPEN` | Failure threshold breached. All requests are rejected immediately without reaching the backend. | Wait for `circuit_breaker_timeout_s` before the breaker automatically probes. Check backend health. |
| `HALF-OPEN` | Probe mode. One request is allowed through; if it succeeds the breaker closes; if it fails the breaker re-opens. | Monitor the next request outcome. |

View current state from the TUI dashboard or via:

```bash
curl http://localhost:8080/api/v1/debug/circuit_breaker | jq .state
```

### Common configuration mistakes

| Error message | Likely cause | Fix |
|---|---|---|
| `Field 'resilience.retry_base_ms' has invalid value … must be ≤ retry_max_ms` | `retry_base_ms > retry_max_ms` in TOML | Swap the values or increase `retry_max_ms`. |
| `Field 'resilience.retry_attempts' has invalid value 0` | `retry_attempts = 0` | Set `retry_attempts` to at least 1. |
| `Field 'stages.inference.model' has invalid value` | Empty string for model name | Set `model = "gpt-4o"` (or your chosen model). |
| `Field 'stages.rag.timeout_ms' has invalid value 0` | `timeout_ms = 0` with `enabled = true` | Set a positive timeout, e.g. `timeout_ms = 5000`. |
| `OPENAI_API_KEY environment variable not set` | Missing env var | Export `OPENAI_API_KEY=sk-…` before running. |
| `ANTHROPIC_API_KEY environment variable not set` | Missing env var | Export `ANTHROPIC_API_KEY=sk-ant-…` before running. |
| `channel capacity must be at least 1` | A channel capacity is set to 0 | Set all `channel_capacity` fields to ≥ 1 (defaults: 512). |

---

## Questions

Open a [Discussion](https://github.com/Mattbusel/tokio-prompt-orchestrator/discussions) for design questions, research ideas, or "would you accept a PR for X?" conversations.
