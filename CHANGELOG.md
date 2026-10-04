# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0] - 2026-10-04

A leaner library that works with the Rust LLM clients people already use.
Upgrading from 1.x: see "Changed (breaking)" below; most code only needs the
`otel`, `hot-reload` or `cli` feature if it used those parts.

### Added

- **Anthropic-compatible endpoint, `POST /v1/messages`.** Anthropic clients
  (the official SDKs included) work against the orchestrator by changing the
  base URL: text conversations, system prompts and streaming with the full
  event sequence, with the same dedup, circuit breaker and spend cap as the
  OpenAI endpoint. The API key may be sent as `x-api-key`. Tested with the
  official Python SDK (`messages.create`, `messages.stream`).
- **Answers grounded in your documents.** New `Retriever` trait, and
  `TantivyRetriever` (feature `tantivy`): BM25 search with English stemming
  over a folder of Markdown/text files, in memory. `spawn_pipeline_with(worker,
  PipelineOptions::with_retriever(..))` in Rust, `orchestrator --docs <folder>`
  on the command line. Retrieval failures and timeouts (2 s default) send the
  prompt without context instead of dropping it.
- **Semantic dedup with real embeddings.** New `Embedder` trait with
  `FastEmbedder` (feature `fastembed`, local ONNX models, no API key),
  `OpenAiEmbedder` and `GenaiEmbedder`. `Deduplicator::with_embedder` and
  `check_and_register_semantic` answer a reworded question from the cache;
  `ServerConfig::embedder` and `orchestrator --semantic-dedup [threshold]`
  turn it on for the OpenAI and Anthropic endpoints (`x-orchestrator-dedup:
  semantic`). A match must also pass `same_specifics` (same numbers, shared
  words in the same order), because measured embedding similarity alone puts
  "convert 10 miles to km" and "convert 10 km to miles" at 0.99. Default
  threshold 0.93 from those measurements.
- **Integrations with other Rust LLM crates**, each its own feature and each
  a `ModelWorker`, so deduplication, the circuit breaker, retries and the
  dead-letter queue sit in front of the client you already use:
  - `async-openai`: `integrations::AsyncOpenAiWorker` wraps an
    `async_openai::Client` (OpenAI, Azure, or any OpenAI-compatible server).
  - `genai`: `integrations::GenaiWorker` wraps a `genai::Client`, so one
    worker covers OpenAI, Anthropic, Gemini, Ollama, Groq, DeepSeek, xAI,
    Cohere and the other providers genai supports.
  - `rig`: `integrations::RigWorker` wraps any rig-core completion model
    (rig-core needs Rust 1.95+).
  - `tower`: `integrations::ServiceWorker` runs any `tower::Service<String>`
    as a worker (with its tower layers), and `integrations::WorkerService`
    turns a worker into a service.
  All map provider errors the same way: 401/403 become `AuthFailed` (never
  retried), 429 becomes `RateLimited` (with `Retry-After` when the client
  exposes it), everything else a retryable `Inference` error. Tested end to
  end against a mock OpenAI server, including streaming.
- `web_api::serve_pipeline(config, &handles)`: serve the HTTP API for a
  pipeline in one call, with its output receiver, dead-letter queue and
  circuit breaker wired in.
- `DeadLetterQueue::snapshot()`: read the queue without draining it.
- `otel`, `hot-reload` and `cli` features (see below).

### Changed (breaking)

- **Lighter default build: 219 crates down to 167.** OpenTelemetry moved
  behind the new `otel` feature (on in `full`). `OtelLayer` and
  `try_build_otel_layer` need it; `init_tracing` works either way and warns
  if `OTEL_EXPORTER_OTLP_ENDPOINT` is set in a build without `otel`. The OTLP
  exporter no longer compiles the gRPC stack it never used, and the unused
  `opentelemetry-http` dependency is gone.
- `config::watcher` (the file watcher, via notify) moved behind
  `hot-reload`; `HotConfig` is unaffected. The `replay` binary needs `cli`.
- **Safer `web_api::ServerConfig::default()`**: it binds `127.0.0.1`
  instead of `0.0.0.0`, and `debug_mode` is off. The old default exposed an
  unauthenticated proxy to the provider (on your API key) to the whole
  network. Set `host` explicitly to serve other machines, and pair it with
  `api_key`. The `orchestrator` binary keeps its debug endpoints only while
  bound to a loopback address.
- `OrchestratorError` is `#[non_exhaustive]`, so new error kinds can be
  added without another major release. Add a `_` arm to exhaustive matches.
- axum 0.7 -> 0.8, tower 0.4 -> 0.5, tower-http 0.5 -> 0.6. One tower version
  in the tree instead of two.
- Minimum Rust is now 1.88. The declared 1.85 was no longer true: with
  current dependencies the default build already failed on 1.85
  (`yoke-derive` needs 1.87), and `full` needs 1.88. Checked on 1.88 for the
  default build and every feature except `tantivy` (1.90), `rig` (1.95) and
  `fastembed` (1.88, checked separately).

### Fixed

- **Prompts reach the model as written.** The Retrieve stage was a
  placeholder that wrapped every prompt from the CLI, `POST /api/v1/infer`,
  the WebSocket and the MCP server in `CONTEXT: Retrieved documents for
  '...' User Query: ... Assistant:`, so providers billed for, and answered
  around, invented context. It also slept 5 ms per request to "simulate"
  retrieval. Both are gone; context now comes only from a configured
  `Retriever`.
- Pipeline stages held a `tracing` span guard across `.await`, which
  attributes other tasks' events to the wrong request in traces. Spans are
  now attached to the futures with `Instrument`.
- Semantic dedup with a caller-supplied embedding returned an empty answer
  on a hit, and its embedding store grew without limit. It now returns the
  matched answer, and the store is bounded (10,000 entries) and expires with
  the cache.
- `ServerConfig` now loads from a partial JSON/TOML config: missing fields
  take their defaults instead of failing to deserialize.
- `GET /api/v1/debug/dlq` and the MCP `dump_dlq` tool drained the
  dead-letter queue and pushed it back, which could reorder or drop entries
  that arrived meanwhile and re-sent every entry to DLQ subscribers. Both now
  read a snapshot.
- The `rest_api`, `sse_stream`, `websocket_api` and `web_api_demo` examples
  had not compiled since `start_server` gained arguments, and they passed a
  dummy output channel, so results never reached clients. They now use
  `serve_pipeline`.
- Seven test suites had stopped compiling (web API, CORS, result tracker, MCP,
  TUI, tier integration, Redis dedup) because CI only ran `cargo test --lib`.
  They are fixed, and CI now builds and runs every test target and example.
  Eighteen `self_modify` tests that run `cargo test`/`clippy`/`bench` on the
  repository itself are now `#[ignore]` (run them with `--ignored`).

## [1.6.0] - 2026-10-02

This release swaps several hand-written parts for well-known open-source
crates and adds the features those crates make easy.

### Added

- **Retries for brief provider failures.** `orchestrator --retries N` (or
  `ORCHESTRATOR_RETRIES`) retries a model call up to N times (0 to 10) when the
  provider answers 429 or 5xx or the connection fails. The wait starts at
  200 ms and doubles each time, with random jitter, capped at 10 s. A
  provider's `Retry-After` is respected; if it asks for more than the cap, the
  request fails at once instead of stalling. A bad key or a budget error is
  never retried. Off by default.
- In the library: `spawn_pipeline_with_retry(worker, InferenceRetry)`, and
  `spawn_pipeline_with_config` now applies `[resilience] retry_attempts`,
  `retry_base_ms` and `retry_max_ms`. These fields used to be parsed and then
  ignored. All attempts of one request count as one call for the circuit
  breaker, and together they must fit in the inference timeout.
- New metric `orchestrator_inference_retries_total`.
- **Exact token counts for OpenAI models** with
  [tiktoken-rs](https://crates.io/crates/tiktoken-rs) (new `tiktoken`
  feature, turned on by `web-api`). The OpenAI proxy's `usage` is now counted
  with the model's own tokenizer, including message framing, so it matches
  the provider's bill, and the spend cap uses the same numbers.
  `CostEstimator::estimate_cost` counts exactly too. Other models keep the
  estimate. New functions: `token_counter::exact_token_count`,
  `exact_chat_prompt_tokens`, `count_for_model` and
  `TokenizerFamily::from_model`.
- `Deduplicator::with_max_entries`, `Deduplicator::cache_duration` and
  `enhanced::dedup::DEFAULT_DEDUP_MAX_ENTRIES`.

### Changed

- **The deduplicator now has a size limit.** It stores its entries in a
  [moka](https://crates.io/crates/moka) cache (100,000 keys by default)
  instead of an unbounded map swept by a background task every 60 s, so a
  flood of different prompts can no longer grow memory without limit.
  Expired answers are never served, the sweeper task is gone, and
  `Deduplicator::new` no longer has to be called inside a Tokio runtime.
  `shutdown()` and `signal_shutdown()` remain and do nothing.
- The deduplicator is safer under races: a finished or cancelled call only
  clears its own entry, never a newer one for the same key, and waiters are
  always woken even if the entry was evicted.
- `dedup_key` uses a 128-bit SHA-256 prefix instead of a 64-bit FNV hash, so
  two different prompts cannot share a cached answer by a hash collision.
  Keys change format (`dedup:g:` plus 32 hex characters), which only matters
  if you stored them.
- `RetryPolicy`, `retry_if` and `retry_inference` run on
  [backon](https://crates.io/crates/backon) instead of hand-written loops.
  Same API and same delays. `retry_inference` no longer retries
  `AuthFailed`, `ConfigError` or `Other` errors (it used to retry everything
  except `BudgetExceeded`).
- [prometheus](https://crates.io/crates/prometheus) 0.13 to 0.14, built
  without its protobuf support. `/metrics` output is unchanged, and the
  protobuf dependency with advisory RUSTSEC-2024-0437 is gone (so is the
  `deny.toml` ignore for it).
- `lazy_static` replaced by `std::sync::LazyLock`.

## [1.5.0] - 2026-09-30

### Added

- OpenAI-compatible API: `POST /v1/chat/completions` (JSON, or Server-Sent
  Events in `chat.completion.chunk` format ending with `data: [DONE]` when
  `"stream": true`) and `GET /v1/models`. Point any OpenAI client at
  `http://127.0.0.1:8080/v1` (or set `OPENAI_BASE_URL`) and its requests go
  through the pipeline: bounded queues, circuit breaker, timeouts and the
  dead-letter queue. Verified with the official `openai` Python package and
  `curl`; see "Drop-in OpenAI proxy" in the README and docs/REFERENCE.md.
- Deduplication on that endpoint: identical conversations share one upstream
  call while it is in flight, and the answer is reused for
  `ORCHESTRATOR_DEDUP_SECS` (default 300). Response header
  `x-orchestrator-dedup: miss | joined | cached`.
- Errors in OpenAI's JSON shape with matching status codes: 503 while the
  circuit breaker is open, 429 at the spend cap or when a queue is full, 502
  for a failed provider call, 504 on timeout, 401 for a bad key.
- `ORCHESTRATOR_API_KEY` turns on bearer auth for the HTTP API (`API_KEY`
  still works).
- `DeadLetterQueue::subscribe()` to be told about dropped requests as they
  happen, `PromptRequest::is_raw_prompt()` and `META_PROMPT_MODE` (send the
  input to the model without the placeholder context template), and
  `CostEstimator::cost_for_tokens()`.
- `ServerConfig` fields `provider`, `model`, `max_spend_usd`,
  `dedup_window_secs` and `api_key` (all with defaults).

### Changed

- `--max-spend` now works with the web API: the OpenAI endpoint records an
  estimated cost per upstream call and answers 429 `insufficient_quota` at the
  cap, and the server keeps running. Before, nothing recorded cost, so the cap
  never triggered. With `--no-web` it still exits at the cap.
- Requests the pipeline drops now show as `failed` with the reason on
  `GET /api/v1/status/<id>` and `/api/v1/result/<id>` right away, instead of
  staying `processing` until the timeout.
- The Linux release binary is built with the `web-api` feature. Earlier Linux
  builds (1.4.x) had no HTTP server; the Windows build already had it. The
  release job now also starts the binary and checks `/v1/models` and a chat
  completion before publishing.

### Fixed

- `cargo test --lib --features web-api` compiles again (a stale test helper
  in `web_api.rs`).

## [1.4.3] - 2026-09-28

### Changed

- README cut from 76 KB to about 10 KB: what it does, a direct Windows download,
  an animated "how it works" diagram drawn from `src/stages.rs`, three real
  examples, then links. Everything else moved, unchanged, to `docs/GUIDE.md`,
  `docs/REFERENCE.md` and `docs/MODULES.md`.
- `orchestrator --help` and the first-run welcome describe what the binary
  really does (bounded queue, circuit breaker, timeouts, dead-letter queue);
  they no longer claim deduplication and retries, which the server does not add.
- Terminal logs respect `NO_COLOR`; the log file never gets colour codes.
- Releases also carry `orchestrator-windows-x64.exe`, a stable name for the
  README's download link.
- Removed the stale `releases/orchestrator.exe` from the source tree.

### Added

- `examples/quickstart.rs`: the README's library example, runnable.

## [1.4.2] - 2026-09-25

### Added

- One-line installs: `install.sh` (Linux, macOS) and `install.ps1` (Windows)
  download the release archive, verify it against `SHA256SUMS.txt` and put
  `orchestrator` on your PATH. Also Homebrew (`mattbusel/tap`), Scoop
  (`mattbusel` bucket) and `cargo binstall` metadata.
- Releases now ship `SHA256SUMS.txt` and include the `replay`, `coordinator`
  and `validate` binaries next to `orchestrator`.
- `orchestrator --help` has an EXAMPLES section (echo mode, curl, a real model).

### Changed

- `--provider` with an unknown name now stops with the list of valid providers
  instead of silently falling back to echo.
- A missing API key error lists every way to fix it, including the
  `--provider echo` mode that needs no key.
- README opens with what it does, a recorded demo GIF, an install table and
  three steps; the docs.rs front page opens with a runnable example and links
  to the main types instead of repeating the whole README.

## [1.4.1] - 2026-09-25

### Fixed

- `orchestrator` built with `web-api` (the release binaries) exited right after
  printing its banner: the terminal prompt found the output channel already
  taken, returned, and ended the program. The pipeline output is now split by
  request id so the terminal prompt and the HTTP API both work; if stdin
  closes, the server keeps running.
- The startup banner and `--help` advertised `POST /v1/prompt`, which does not
  exist, and an HTTP URL for Claude Desktop. They now list the real routes
  (`/api/v1/infer`, `/api/v1/result/<id>`, `/v1/stream`, `/health`) and point
  Claude Desktop at the stdio `mcp` binary.
- The banner box is padded by character count, so its right border lines up,
  and it says when the web API is off because the `web-api` feature is missing.
- A port already in use now prints a clear message instead of exiting silently.

### Changed

- README and project site rebuilt around real output of `llm_pipeline`;
  the Pages workflow publishes the site with the API docs under `/api/` and
  keeps the benchmark history.

## [1.4.0] - 2026-09-25

Version 1.3.0 was never published to crates.io, so this release also carries
everything listed under 1.3.0 below.

### Added

- `examples/llm_pipeline.rs`: an end-to-end pipeline example that runs with no
  API key (mock model) or against Anthropic/OpenAI with `PROVIDER=`. Shows
  request deduplication (12 requests, 3 model calls) and the circuit breaker
  plus dead-letter queue during a simulated provider outage.
- `enhanced::retry_if` is re-exported (its doc example referenced it).
- Release workflow: pushing a `v*` tag builds `orchestrator` for Linux,
  macOS (arm64, x86_64) and Windows and attaches the archives to the release.
- CI `test` job: integration tests, doctests, example builds and a run of the
  `llm_pipeline` example.

### Fixed

- `web-api` feature did not compile (stale `rate_limiter::RateLimiter` type).
  `GET /api/v1/rate-limiter/stats` now returns per-model throttled counts from
  `RateLimiterRegistry`.
- `priority_queue`: aging thresholds were applied in reverse order
  (Background waited for the High threshold); promoted items now keep their
  enqueue order in the destination level.
- `context_mgr`: `SummarizeOldest` left the context over budget because the
  placeholder message was not counted.
- `hot_config`: `set`/`get` panicked when called inside a Tokio runtime.
- `config::watcher`: the reload loop blocked a Tokio worker thread (and hung
  current-thread runtimes); the debounce dropped the last write of a burst.
- `multi_modal`: serializing a `ContentPart::Text` always failed. Text parts
  now serialize as `{"type":"text","text":"..."}`.
- `prompt_template`: `{{ x | default:y }}` errored when `x` was unset;
  `truncate` could panic on multi-byte text.
- `prompt_versioning::blame` returned a version for out-of-range lines.
- `prompt_optimizer`: filler removal missed words before punctuation and
  mangled words that contain a filler (for example "displease").
- `job_scheduler`: intervals shorter than 100 ms fired late.
- `intent_classifier`: "Analyse X" was classified as a command.
- `retry_policy::retry_async` panicked with `max_attempts = 0`.
- `orchestrator` binary: `--provider` on the command line was ignored unless a
  saved config existed. `cargo run` now picks the `orchestrator` binary
  (`default-run`).
- README: the quick start used a nonexistent `--worker` flag; code fragments
  are marked so `cargo test --doc` passes; broken intra-doc links fixed.
- Clippy (`-D warnings`, Rust 1.98) and rustdoc (`-D warnings`) are clean;
  `Cargo.lock` updated for patched `h2`, `rustls`, `quinn-proto` and
  `crossbeam-epoch`.

### Changed

- Published package excludes `releases/` (a prebuilt binary), editor and hook
  files.

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
