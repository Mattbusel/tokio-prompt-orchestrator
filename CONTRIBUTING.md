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

## Questions

Open a [Discussion](https://github.com/Mattbusel/tokio-prompt-orchestrator/discussions) for design questions, research ideas, or "would you accept a PR for X?" conversations.
