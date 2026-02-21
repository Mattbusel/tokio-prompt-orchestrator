# ROADMAP.md — tokio-prompt-orchestrator
## Mission-Critical Feature Development Plan

> Every phase gate requires: tests passing, ratio ≥1.5:1, zero panics, audit clean.
> No phase begins until the previous phase gate is fully closed.

---

## Current State Assessment

```
Status:        Phase 1 Complete (foundation)
Ratio:         ~1.57:1 (maintain or improve)
Panic surface: Audit required before Phase 2 begins
Missing:       Distributed mode, declarative config, streaming inference, 
               structured logging, distributed tracing
```

---

## Phase 0 — Hardening (Do This First)
**Goal:** Bring existing code to aerospace standards before building anything new.
**Duration:** Before any new features. Non-negotiable.

### 0.1 — Panic Audit & Elimination
- [ ] Run `./scripts/panic_scan.sh` — catalog every `unwrap()` and `expect()` in `src/`
- [ ] Convert each to explicit `Result` propagation
- [ ] Add `#[deny(clippy::unwrap_used, clippy::expect_used)]` to all modules
- [ ] Verify zero panic surface via CI

### 0.2 — Error Enum Consolidation
- [ ] Audit all error types — eliminate `Box<dyn Error>` from library-facing APIs
- [ ] Create `OrchestratorError` as the single top-level error enum
- [ ] Implement `From<T>` chains for all downstream errors
- [ ] Add a trigger test for every error variant

### 0.3 — Ratio Audit
- [ ] Run `./scripts/ratio_check.sh` on current codebase
- [ ] Identify undertested modules
- [ ] Write missing tests until all modules are ≥1.5:1 individually
- [ ] Add property tests for: dedup hashing, retry backoff, channel sizing

### 0.4 — Script Infrastructure
- [ ] Create `scripts/ratio_check.sh`
- [ ] Create `scripts/panic_scan.sh`
- [ ] Create `scripts/pre_commit.sh` (orchestrates all checks)
- [ ] Create `scripts/bench_all.sh`
- [ ] Create `scripts/coverage.sh`
- [ ] Install scripts as git pre-commit hook

### 0.5 — Benchmark Baseline
- [ ] Establish P50/P99 benchmarks for all existing stages
- [ ] Document results in `BENCHMARKS.md`
- [ ] Add CI step that fails if benchmarks regress >10%

**Phase 0 Gate:**
```
✓ cargo clippy -D warnings: clean
✓ cargo audit: clean  
✓ panic_scan.sh: 0 found
✓ ratio_check.sh: ≥1.5:1 globally AND per module
✓ All benchmarks documented
✓ pre_commit.sh installed as hook
```

---

## Phase 1 — Structured Logging & Observability Hardening
**Goal:** Every event in the system is observable, traceable, and queryable.
**Blocking on:** Phase 0 gate

### 1.1 — Structured JSON Logging
- [ ] Replace all `println!` and ad-hoc `log::` calls with `tracing::` spans and events
- [ ] Define log schema: every event has `stage`, `session_id`, `request_id`, `duration_us`, `outcome`
- [ ] Add `tracing-subscriber` with JSON format for production, pretty format for dev
- [ ] Test: log output is valid JSON under all code paths (parse log output in test)
- [ ] Test: sensitive fields (prompts, responses) are redacted in logs

```rust
// Required span pattern for every pipeline stage
#[tracing::instrument(
    name = "pipeline.dedup",
    skip(self, request),
    fields(
        session_id = %request.session,
        request_id = %request.id,
        dedup_key = tracing::field::Empty,
        cache_hit = tracing::field::Empty,
    )
)]
pub async fn process(&self, request: PromptRequest) -> Result<DeduplicationResult, OrchestratorError> {
```

**Tests Required:**
- Unit: span is created for every request
- Unit: sensitive fields are not present in log output
- Integration: correlated spans appear across all 5 stages for a single request

### 1.2 — Distributed Tracing (OpenTelemetry)
- [ ] Add `opentelemetry` + `tracing-opentelemetry` integration
- [ ] Propagate trace context across async stage boundaries
- [ ] Export to Jaeger (docker-compose service)
- [ ] Test: trace_id is consistent across all stages for a single request
- [ ] Test: span relationships correctly model the pipeline DAG

### 1.3 — Enhanced Prometheus Metrics
- [ ] Add per-worker latency histograms (not just per-stage)
- [ ] Add deduplication hit rate as a gauge
- [ ] Add circuit breaker state as a gauge (0=closed, 1=half-open, 2=open)
- [ ] Add retry count histogram per worker
- [ ] Test: every metric is exercised by at least one test
- [ ] Update Grafana dashboard with new panels

**Phase 1 Gate:**
```
✓ Every pipeline request produces correlated structured logs
✓ Trace IDs propagate correctly end-to-end (verified by integration test)
✓ All new metrics have tests
✓ Ratio maintained ≥1.5:1
✓ Jaeger integration documented in docker-compose
```

---

## Phase 2 — Declarative Pipeline Configuration
**Goal:** Define entire pipeline topology in TOML/YAML. No code changes for common configurations.
**Blocking on:** Phase 1 gate

### 2.1 — Configuration Schema Design
- [ ] Design `pipeline.toml` schema supporting: worker selection, stage parameters, channel sizes, retry policy, circuit breaker config, rate limits
- [ ] Use `serde` + `schemars` for typed deserialization with validation
- [ ] Write JSON Schema output for editor autocompletion
- [ ] Document every config field with examples

```toml
# Target schema example
[pipeline]
name = "production-gpt4"
version = "1.0"

[pipeline.stages.rag]
enabled = true
timeout_ms = 5000
max_context_tokens = 2048

[pipeline.stages.inference]
worker = "openai"
model = "gpt-4"
max_tokens = 1024
temperature = 0.7

[pipeline.resilience]
retry_attempts = 3
retry_base_ms = 100
retry_max_ms = 5000
circuit_breaker_threshold = 5
circuit_breaker_timeout_s = 60

[pipeline.deduplication]
enabled = true
window_s = 300
max_entries = 10000
```

### 2.2 — Config Validation Engine
- [ ] Validate all config values at load time (not at runtime)
- [ ] Return structured validation errors with field paths and suggestions
- [ ] Test: invalid configs produce clear, actionable error messages
- [ ] Test: every valid config produces a correctly-structured pipeline
- [ ] Property test: any valid config can be serialized and deserialized deterministically

### 2.3 — Hot Reload Support
- [ ] Watch config file for changes using `notify` crate
- [ ] Apply non-breaking config changes without restart (rate limits, timeouts, log levels)
- [ ] Reject breaking changes with clear error (worker changes require restart)
- [ ] Test: hot reload applies changes within 1 second
- [ ] Test: invalid hot reload config is rejected without disrupting active pipeline

### 2.4 — Config CLI
- [ ] `orchestrator validate pipeline.toml` — validate without running
- [ ] `orchestrator schema` — output JSON Schema for IDE integration
- [ ] `orchestrator explain pipeline.toml` — human-readable pipeline description

**Phase 2 Gate:**
```
✓ pipeline.example.toml fully exercises all config options
✓ Invalid configs produce clear errors (tested for 20+ invalid cases)
✓ Hot reload tested with active traffic
✓ JSON Schema generated and valid
✓ Ratio maintained ≥1.5:1
```

---

## Phase 3 — Streaming Inference Support
**Goal:** First-token-to-user latency matters. Pipeline must stream tokens, not buffer responses.
**Blocking on:** Phase 2 gate

### 3.1 — Streaming Worker Trait
- [ ] Define `StreamingModelWorker` trait returning `Stream<Item = Result<TokenChunk, WorkerError>>`
- [ ] Implement for OpenAI (SSE streaming)
- [ ] Implement for Anthropic (SSE streaming)
- [ ] Implement for llama.cpp (streaming inference)
- [ ] Test: streaming and non-streaming workers are interchangeable at the pipeline level
- [ ] Test: stream is correctly terminated on worker error (no hanging streams)

```rust
pub trait StreamingModelWorker: Send + Sync {
    fn infer_stream(
        &self,
        prompt: AssembledPrompt,
    ) -> BoxStream<'_, Result<TokenChunk, WorkerError>>;
    
    fn supports_streaming(&self) -> bool;
}
```

### 3.2 — Streaming Pipeline Stages
- [ ] Post-process stage must handle streaming (filter tokens, not full responses)
- [ ] Stream stage passes through token chunks to WebSocket clients
- [ ] Backpressure applies to streaming: slow clients do not block the pipeline
- [ ] Test: 10 concurrent streaming requests do not degrade each other
- [ ] Test: client disconnect mid-stream does not leak resources

### 3.3 — Streaming Deduplication
- [ ] Duplicate streaming requests share a single upstream stream (fan-out)
- [ ] Fan-out must handle clients at different read speeds
- [ ] Test: fan-out delivers identical token sequences to all subscribers
- [ ] Test: slow subscriber does not delay fast subscriber

### 3.4 — Time-to-First-Token Metrics
- [ ] Add `orchestrator_ttft_seconds` histogram
- [ ] Add `orchestrator_stream_duration_seconds` histogram  
- [ ] Add `orchestrator_stream_tokens_total` counter
- [ ] Benchmark: TTFT overhead of orchestrator vs direct API call is <10ms

**Phase 3 Gate:**
```
✓ TTFT benchmark: overhead <10ms at P99
✓ Fan-out streaming: no token loss at any subscriber speed
✓ Resource leak test: 1000 mid-stream disconnects, no goroutine/task leak
✓ All streaming workers tested end-to-end
✓ Ratio maintained ≥1.5:1
```

---

## Phase 4 — Distributed Mode (Multi-Node)
**Goal:** Single orchestrator is a single point of failure. Distribute across nodes.
**Blocking on:** Phase 3 gate

### 4.1 — Message Bus Integration
- [ ] Abstract message transport behind `MessageBus` trait
- [ ] Implement NATS backend
- [ ] Implement Kafka backend (feature-flagged)
- [ ] Test: pipeline works identically with in-process channels vs NATS
- [ ] Test: message loss during transport is detected and triggers retry

### 4.2 — Distributed Deduplication
- [ ] Dedup key checks must be cluster-wide (not per-node)
- [ ] Implement Redis-backed distributed dedup
- [ ] Handle Redis failure: fall back to per-node dedup with warning
- [ ] Test: two nodes do not process the same deduplicated request simultaneously
- [ ] Test: Redis failure does not cause service outage (graceful degradation)

### 4.3 — Leader Election & Coordination
- [ ] Implement leader election for singleton operations (e.g., config updates)
- [ ] Use NATS JetStream or Redis for distributed locks
- [ ] Test: leader failure causes new election within 5 seconds
- [ ] Test: split-brain scenario does not cause duplicate processing

### 4.4 — Cluster Health & Discovery
- [ ] Nodes register themselves in a shared registry
- [ ] Health checks propagate across cluster
- [ ] Grafana dashboard shows per-node metrics and cluster topology
- [ ] Test: node addition and removal are handled without dropped requests

**Phase 4 Gate:**
```
✓ 3-node cluster test: all nodes handle requests correctly
✓ Node failure test: traffic redistributes within 5 seconds
✓ Distributed dedup test: zero duplicate processing across 2 nodes
✓ Split-brain test: system remains consistent
✓ Ratio maintained ≥1.5:1
```

---

## Phase 5 — Batch Processing Mode
**Goal:** High-throughput offline processing without the latency constraints of real-time mode.
**Blocking on:** Phase 3 gate (can run in parallel with Phase 4)

### 5.1 — Batch Job API
- [ ] Define `BatchJob` with: job_id, prompt_list, output_destination, priority, deadline
- [ ] REST endpoint: `POST /api/v1/batch` — submit job
- [ ] REST endpoint: `GET /api/v1/batch/{job_id}` — poll status
- [ ] REST endpoint: `GET /api/v1/batch/{job_id}/results` — stream results
- [ ] Test: batch job processes 10,000 prompts correctly with progress reporting

### 5.2 — Batch Scheduler
- [ ] Priority-aware scheduling: deadline-sensitive jobs preempt low-priority batches
- [ ] Cost optimization: batch requests to reduce per-token pricing where supported
- [ ] Test: high-priority batch does not starve real-time traffic
- [ ] Test: batch job is resumable after node restart

### 5.3 — Output Destinations
- [ ] File output: write results to S3/local disk as JSONL
- [ ] Streaming output: real-time result delivery as job completes
- [ ] Webhook output: POST results to configured URL
- [ ] Test: all output destinations handle partial failures correctly

**Phase 5 Gate:**
```
✓ 10,000 prompt batch job completes correctly
✓ Real-time traffic unaffected by concurrent batch job
✓ Job resumability tested (kill node mid-job, restart, verify continuation)
✓ All output destinations tested for partial failure handling
✓ Ratio maintained ≥1.5:1
```

---

## Phase 6 — Auto-Scaling & Adaptive Routing
**Goal:** The system scales itself and routes intelligently based on real-time conditions.
**Blocking on:** Phase 4 gate

### 6.1 — Queue-Based Auto-Scaling
- [ ] Define scaling policy: scale out when queue depth >80% for >30s, scale in when <20% for >2m
- [ ] Implement scaling signal emission (external autoscaler reads Prometheus metrics)
- [ ] Test: scaling signal is emitted at correct thresholds
- [ ] Test: no oscillation (scale out/in flapping) under sustained moderate load

### 6.2 — Adaptive Model Routing
- [ ] Route by complexity: short prompts → cheap model, long/complex → powerful model
- [ ] Route by latency budget: SLA-sensitive requests get fastest available worker
- [ ] Route by availability: if primary worker circuit opens, route to fallback
- [ ] Test: routing decisions are correct under 10+ simulated scenarios
- [ ] Benchmark: routing decision overhead <100μs

### 6.3 — Cost-Aware Routing
- [ ] Track per-worker cost per token
- [ ] Implement cost budget per session/tenant
- [ ] Route to cheaper model when approaching budget
- [ ] Test: budget enforcement is accurate within 1% at 1000 req/session

**Phase 6 Gate:**
```
✓ Auto-scaling signals emit at correct thresholds (tested)
✓ Adaptive routing: correct model selected for 10 defined scenarios
✓ Cost enforcement: within 1% accuracy at tested volumes
✓ No routing oscillation under sustained load
✓ Ratio maintained ≥1.5:1
```

---

## Milestone Summary

| Phase | Focus | Key Deliverable | Gate Criteria |
|-------|-------|-----------------|---------------|
| 0 | Hardening | Zero panics, ratio audit | Scripts installed, 0 panics |
| 1 | Observability | Structured logs + tracing | Correlated traces end-to-end |
| 2 | Config | Declarative TOML pipeline | Hot reload + schema output |
| 3 | Streaming | Token-level streaming | TTFT overhead <10ms |
| 4 | Distributed | Multi-node cluster | 3-node chaos test passes |
| 5 | Batch | Offline processing mode | 10k prompt job + resumability |
| 6 | Adaptive | Auto-scale + smart routing | Cost accuracy + no oscillation |

---

## What Never Gets Compromised

Across all phases:
- **Ratio ≥1.5:1** — always
- **Zero panics** — always
- **Zero failing tests at merge** — always
- **Performance budgets** — regression blocks the phase gate
- **Error coverage** — every error variant has a trigger test

The phases can shift. The non-negotiables cannot.
