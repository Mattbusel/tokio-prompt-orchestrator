# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.3.0] - 2026-03-22

### Added

- **Multi-provider cascade fallback** (`src/routing/cascade.rs`): new
  `ProviderCascade` type that chains an ordered list of `(WorkerKind,
  CircuitBreakerConfig)` pairs and tries primary → secondary → tertiary on
  failure. Skips providers whose circuit breaker is currently open. Records
  per-provider latency and success-rate atomics. Exposes Prometheus counters
  `cascade_attempts_total{provider}` and `cascade_failures_total{provider}`.
  Returns `CascadeResult { provider_used, attempts, total_latency_ms, value }`.
  Re-exported from `routing::cascade` and `routing`.

- **Deadline-aware priority queue** (`src/enhanced/priority.rs`): new
  `PriorityQueue::pop_with_deadline_check` method that skips requests whose
  `deadline` has already passed. Each expired request increments the new
  `expired_total: Arc<AtomicUsize>` field and emits a `tracing::warn!`.
  `QueueStats` gains a matching `expired_total: usize` field. The existing
  `pop` method is unchanged for backward compatibility.

- **DLQ replay scheduler** (`src/enhanced/dlq_replay.rs`): new
  `DlqReplayScheduler` type that wraps dead-letter queue entries and adds
  `replay_all(sender)`, `replay_by_session(session_id, sender)`, and
  `age_out(max_age)`. Uses exponential backoff (2^attempts seconds, capped at
  60 s) between retry sends. Exposes Prometheus counters `dlq_replayed_total`
  and `dlq_aged_out_total`. Re-exported from `enhanced`.

- **Provider health dashboard** (`src/routing/health.rs`): new
  `ProviderHealthBuilder` and `ProviderHealthSnapshot` types that aggregate
  per-provider health data — `last_success_at`, `last_failure_at`,
  `consecutive_failures`, `success_rate_1h`, `p50_latency_ms`,
  `p95_latency_ms` — and expose a `to_json()` method for REST endpoint
  integration. Re-exported from `routing`.

### Changed

- `Cargo.toml`: version bumped from `1.2.0` to `1.3.0`.
- `src/enhanced.rs`: added `pub mod dlq_replay` and re-exports for
  `DlqReplayScheduler`, `ReplayEntry`, and `QueueStats`.
- `src/routing/mod.rs`: added `pub mod cascade` and `pub mod health` with
  corresponding re-exports.

## [Unreleased]

### Added

- Production-readiness pass: doc comments added to all public API items in
  `lib.rs`, `worker.rs`, `stages.rs`, `config/mod.rs`, and `enhanced.rs`.
- CI workflow updated to run `cargo fmt --check`, `cargo clippy -- -D warnings`,
  `cargo test`, and `cargo doc --no-deps` on `ubuntu-latest` with the stable
  toolchain, plus cross-platform jobs on `windows-latest` and `macos-latest`.
- CI now triggers on both `master` and `main` branch pushes and pull requests.
- README rewritten with architecture ASCII diagram, quickstart async example,
  API overview table, full configuration reference, feature flags table, and
  contributing and license sections.
- Full `///` doc comments added to all five worker constructors (`OpenAiWorker::new`,
  `AnthropicWorker::new`, `LlamaCppWorker::new`, `VllmWorker::new`,
  `EchoWorker::new`), each with environment-variable table, `# Errors`, and
  `# Examples` sections.
- `## Timeout Semantics` section added to `src/stages.rs` module doc, covering
  `DEFAULT_INFERENCE_TIMEOUT_SECS`, per-request deadlines, circuit-breaker
  timeout independence, stage-level timeouts, and deadline-vs-stage-timeout
  interaction.
- Module-level `//!` doc comment in `src/config/validation.rs` now includes a
  complete validation-rule table. `validate()` gains a `# Errors` section
  listing every `ConfigError::InvalidField` variant that can be returned.
- `POST /api/v1/batch` and `GET /api/v1/batch/:job_id/progress` endpoints
  documented in `src/web_api.rs` with request/response JSON schemas and
  progress-polling instructions. `WEB_API.md` updated with curl examples.
- `## Configuration` subsections added to `README.md` covering timeout TOML
  snippets, circuit-breaker TOML, and rate-limit TOML.
- `## Troubleshooting` section added to `CONTRIBUTING.md` covering DLQ
  inspection, circuit-breaker state interpretation, replaying failed requests,
  and common configuration mistakes.
- Inline `// NOTE: …` comments added at every `parking_lot::Mutex` / sync
  `std::sync::Mutex` lock site inside async contexts, explaining why a sync
  lock is acceptable (short critical section, no `.await` inside the guard).
- `cargo doc --no-deps -D warnings` step added to CI to prevent documentation
  regressions.
- Module-level doc comment for `enhanced` with feature-table overview of all
  six sub-modules.
- `gather()` function in `metrics` now has a proper `///` doc comment.
- CI: added `--no-default-features` build and test steps to catch compilation
  breakage when all optional features are disabled.
- CI: `cargo doc` step now uses `--all-features` so feature-gated public API
  is always verified.
- CI: fixed bench step to reference the correct bench target name.
- Tests: `tier_integration_tests.rs` — independent integration tests for each
  of the four self-improvement tiers.
- Tests: four additional distributed tests covering quorum loss and leader
  election edge cases.
- Docstrings: `SessionId::new`, `SessionId::as_str`, `PromptRequest` fields,
  and `PipelineStage` variants now have `///` doc comments.

### Fixed

- `Deduplicator::check_and_register` now uses DashMap's atomic `entry()` API
  to eliminate the TOCTOU race condition that caused multiple concurrent callers
  to each receive `DeduplicationResult::New` for the same key. Under high
  concurrency (50 goroutines hitting the same key simultaneously), only one
  caller now receives `New`; all others correctly receive `InProgress`.
- Added `atomic_register_new` helper used after an expired entry is removed, so
  re-registration is also race-free.
- `test_concurrent_duplicate_requests_dedup_stress` in `tests/chaos_tests.rs`
  now reliably passes with the corrected deduplicator (was asserting `== 1` but
  getting 7 due to the pre-existing race).
- `LlamaCppWorker::infer` now returns an empty `Vec` for empty content instead
  of `vec![""]`, aligning with the test contract documented in `worker_extra_tests.rs`.
- `OpenAiWorker::infer` and `AnthropicWorker::infer` now return an empty `Vec`
  for whitespace-only responses instead of a single blank-string token.
- `try_build_otel_layer` replaced `eprintln!` with `tracing::warn!` so all
  diagnostic output goes through the structured logging layer.
- Five test assertions in `worker.rs` corrected: `vec!["hello", "world", "response"]`
  changed to `vec!["hello world response"]` to match the single-token return
  contract of OpenAI, Anthropic, LlamaCpp, and vLLM workers.
- Misplaced doc lines in `metrics.rs` and `stages.rs` now correctly attributed
  to their respective functions.
- `src/bin/self_improve.rs` and `src/main.rs` replaced `.expect()` calls with
  graceful error handlers that log via `tracing::error!` and exit with code 1.

## [1.0.0] - 2026-03-18

### Added

- `deny.toml` with an explicit license allow-list (MIT, Apache-2.0, ISC,
  Unicode-DFS-2016, BSD-2-Clause, BSD-3-Clause) and `cargo-deny` integrated
  into the CI `audit` job.
- Doc comment on `spawn_pipeline_with_config` in `stages.rs` (was absent,
  leaving a public API surface without documentation).
- `# Errors` section added to `metrics::init_metrics` doc comment.
- CI: `cargo deny check` step added to the `audit` job alongside the existing
  `rustsec/audit-check` action.

### Fixed

- Misplaced doc lines in `stages.rs`: the two doc sentences that belonged to
  `validated_channel_size` (a private helper) were incorrectly appended to the
  preceding public function's `# Panics` block. Both items are now correctly
  attributed.

### Changed

- Version bumped from `0.1.0` to `1.0.0` -- the public API is stable.

---

## [0.1.0] - 2025-01-01

### Added

- Five-stage async pipeline: RAG, Assemble, Inference, Post-process, Stream.
- Bounded `tokio::sync::mpsc` channels with configurable capacity and graceful load-shedding via `send_with_shed`.
- `DeadLetterQueue` ring-buffer for inspecting and replaying shed requests.
- `ModelWorker` trait with five production implementations: `EchoWorker`, `OpenAiWorker`, `AnthropicWorker`, `LlamaCppWorker`, `VllmWorker`.
- `LoadBalancedWorker` for round-robin distribution across multiple backends.
- `CircuitBreaker` (open/half-open/closed) with configurable failure threshold, timeout, and success-rate probe.
- `Deduplicator` — collapses identical in-flight prompts into a single inference call; unblocks all waiters on completion.
- `RetryPolicy` with exponential back-off, jitter, and per-attempt timeout.
- `CacheLayer` — in-process LRU cache with TTL for inference results.
- `RateLimiter` — token-bucket rate limiter configurable via `RateLimitConfig`.
- `PriorityQueue` with four-level priority scheduling.
- `ModelRouter` — complexity-scored routing between local llama.cpp and cloud APIs; adaptive threshold tuning.
- `PipelineConfig` — declarative TOML configuration with `deny_unknown_fields`, hot-reload support, and optional JSON Schema export.
- Prometheus metrics: 18 counters, histograms, and gauges covering every pipeline stage and resilience primitive.
- OpenTelemetry tracing integration with OTLP/Jaeger export and graceful fallback when no collector is present.
- `init_tracing()` with `RUST_LOG_FORMAT=json` support for structured log aggregation pipelines.
- `SessionId` with FNV-1a based `shard_session` for stable session affinity across process restarts.
- `PromptRequest::with_deadline` for per-request time-to-live enforcement.
- `PipelineStage` enum with `Display` for consistent metric/log labelling.
- Coordination module: TOML-driven agent fleet management with atomic filesystem-lock task claiming.
- `AgentSpawner`, `TaskQueue`, and `AgentMonitor` for zero-panic agent lifecycle management.
- Self-tuning stack (`self-tune` feature): PID controllers, telemetry bus, anomaly detection (Z-score + CUSUM), snapshot store.
- Self-modify stack (`self-modify` feature): task generation, validation gate (cargo test + clippy), agent memory.
- Intelligence layer (`intelligence` feature): `LearnedRouter` (epsilon-greedy bandit), `Autoscaler`, `FeedbackCollector`, `QualityEstimator`, `PromptOptimizer`, `SemanticDedup`.
- Evolution module (`evolution` feature): A/B experiments, snapshot rollback, transfer learning.
- Distributed mode (`distributed` feature): NATS pub/sub, Redis-based cross-node dedup, leader election with TTL renewal.
- TUI terminal dashboard (`tui` feature) built with ratatui: live stage latency, circuit-breaker status, dedup savings, sparklines.
- Web API (`web-api` feature): REST, WebSocket, and SSE streaming endpoints via axum.
- Metrics HTTP server (`metrics-server` feature): Prometheus scrape endpoint.
- MCP server (`mcp` feature): `infer`, `pipeline_status`, `batch_infer`, and `configure_pipeline` tools callable from Claude Desktop and Claude Code.
- CI: build, clippy `-D warnings`, rustfmt check, tests (default + all-features), MSRV 1.85, publish dry-run, benchmark regression check, `cargo audit` security scan.
- Clippy lints `unwrap_used` and `expect_used` set to `deny` in `Cargo.toml`.
- `#![forbid(unsafe_code)]` enforced across all production code paths.

[Unreleased]: https://github.com/Mattbusel/tokio-prompt-orchestrator/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Mattbusel/tokio-prompt-orchestrator/releases/tag/v0.1.0
