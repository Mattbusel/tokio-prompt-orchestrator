# CLAUDE.md — tokio-prompt-orchestrator
## Aerospace-Grade Development Protocol

> **Governing Principle:** This codebase operates at the reliability tier of flight-critical software.
> Every line written assumes it will run at scale, under load, with no human in the loop to catch failures.
> If it isn't tested, it doesn't exist. If it can panic, it isn't done.

---

## 1. The Immutable Law: 1.5:1 Test-to-Production Ratio

**This is non-negotiable and enforced at every stage.**

```
For every 1 line of production code → 1.5 lines of test code minimum
```

### How to Count

- **Production lines**: Any code in `src/` excluding comments and blank lines
- **Test lines**: Any code in `#[cfg(test)]` blocks, `tests/` directory, or `benches/` directory
- Run `cargo count` or the provided `scripts/ratio_check.sh` before every commit

### Enforcement

```bash
# This script must pass before ANY commit is accepted
./scripts/ratio_check.sh

# Expected output:
# Production LOC:  4,200
# Test LOC:        6,450
# Ratio:           1.535:1  ✓ PASS (minimum 1.5:1)
```

If the ratio drops below 1.5:1, **stop all feature work** and write tests until compliant.

---

## 2. Zero-Panic Production Policy

Production code paths **must never panic**. This means:

### Forbidden in `src/` (outside test blocks)
```rust
// NEVER use these in production paths:
unwrap()          // Use `?` or explicit match
expect("msg")     // Use proper error propagation
panic!()          // Return Result::Err instead
unreachable!()    // Use an exhaustive error variant
todo!()           // Do not merge unfinished production code
```

### Required Pattern
```rust
// BAD — will cause agent to reject this code
fn get_session(&self, id: &SessionId) -> Session {
    self.sessions.get(id).unwrap()
}

// GOOD — explicit error surface, auditable failure
fn get_session(&self, id: &SessionId) -> Result<&Session, OrchestratorError> {
    self.sessions.get(id).ok_or(OrchestratorError::SessionNotFound(id.clone()))
}
```

**Linting enforcement:**
```toml
# Cargo.toml — these lints must be active
[lints.rust]
clippy::unwrap_used = "deny"
clippy::expect_used = "deny"
clippy::panic = "deny"
clippy::todo = "deny"
```

---

## 3. Error Architecture

Every error in this system must be:

1. **Named** — in a domain-specific `OrchestratorError` enum
2. **Typed** — no `Box<dyn Error>` in library-facing code
3. **Propagatable** — implement `std::error::Error` + `From<T>` chains
4. **Testable** — every error variant must have at least one test that triggers it

```rust
// Error structure pattern
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("Pipeline stage '{stage}' failed after {attempts} attempts: {source}")]
    StageFailure {
        stage: StageName,
        attempts: u32,
        #[source] source: Box<dyn std::error::Error + Send + Sync>,
    },
    
    #[error("Circuit breaker open for service '{service}' since {since:?}")]
    CircuitOpen { service: ServiceId, since: Instant },

    #[error("Request deduplication key collision: {key}")]
    DeduplicationConflict { key: DeduplicationKey },

    #[error("Backpressure threshold exceeded: queue depth {depth}/{capacity}")]
    BackpressureShed { depth: usize, capacity: usize },

    #[error("Session '{id}' not found")]
    SessionNotFound(SessionId),

    #[error("Worker inference failed: {0}")]
    WorkerError(#[from] WorkerError),
}
```

---

## 4. Test Architecture Requirements

Every module must contain tests at four levels:

### Level 1: Unit Tests (in-module, `#[cfg(test)]`)
Test every function in complete isolation. Mock all dependencies.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::mocks::*;

    // Name tests: test_{what}_{condition}_{expected_outcome}
    #[test]
    fn test_deduplicator_check_cached_key_returns_cached_result() { ... }
    
    #[test]
    fn test_circuit_breaker_open_after_threshold_failures_fast_fails() { ... }
    
    #[tokio::test]
    async fn test_pipeline_stage_backpressure_sheds_gracefully_under_load() { ... }
}
```

### Level 2: Integration Tests (`tests/` directory)
Test module interactions across stage boundaries.

```
tests/
  integration/
    pipeline_end_to_end.rs       # Full request lifecycle
    backpressure_cascade.rs      # Backpressure propagates correctly
    circuit_breaker_recovery.rs  # CB opens, waits, closes
    dedup_concurrent.rs          # Concurrent dedup races
    retry_exponential_backoff.rs # Retry timing is correct
```

### Level 3: Property-Based Tests (`proptest` or `quickcheck`)
For all algorithmic components: deduplication hashing, retry backoff, channel sizing, priority ordering.

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_dedup_key_is_deterministic(prompt in any::<String>()) {
        let key1 = DeduplicationKey::from_prompt(&prompt);
        let key2 = DeduplicationKey::from_prompt(&prompt);
        prop_assert_eq!(key1, key2);
    }
    
    #[test]
    fn test_retry_backoff_never_exceeds_max_duration(
        base_ms in 1u64..=1000,
        attempts in 1u32..=10
    ) {
        let policy = RetryPolicy::exponential(attempts, Duration::from_millis(base_ms));
        let delay = policy.delay_for_attempt(attempts);
        prop_assert!(delay <= MAX_RETRY_DELAY);
    }
}
```

### Level 4: Chaos / Fault Injection Tests
Every resilience mechanism must be tested by making it fail:

```rust
#[tokio::test]
async fn test_worker_failure_triggers_circuit_breaker_open() {
    let worker = Arc::new(FailingWorker::after_n_calls(5));
    let breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
    
    // Drive 5 failures
    for _ in 0..5 {
        let _ = breaker.call(|| worker.infer("test")).await;
    }
    
    // 6th call must fast-fail (circuit is open)
    let result = breaker.call(|| worker.infer("test")).await;
    assert!(matches!(result, Err(CircuitBreakerError::Open)));
}
```

---

## 5. Commit Protocol

Every commit must pass this checklist **in order**:

```
[ ] cargo fmt --all                          # Zero formatting violations
[ ] cargo clippy --all-features -- -D warnings  # Zero warnings
[ ] cargo test --all-features                # 100% test pass rate
[ ] ./scripts/ratio_check.sh                 # ≥1.5:1 ratio confirmed
[ ] cargo audit                              # Zero known CVEs
[ ] ./scripts/panic_scan.sh                 # Zero unwrap/expect in src/
```

Commit message format:
```
[stage] short description (max 72 chars)

Body explaining WHY, not what (the diff shows what).
Reference any design decisions or tradeoffs made.

Test coverage: +N lines production, +M lines tests (ratio: X.XX:1)
```

Valid stages: `[feat]`, `[fix]`, `[test]`, `[refactor]`, `[perf]`, `[docs]`, `[infra]`

---

## 6. Module Ownership & Boundaries

Each module must have a documented contract in its `mod.rs`:

```rust
//! # Stage: Deduplicator
//!
//! ## Responsibility
//! Detect and short-circuit duplicate inference requests within a configurable
//! time window. This stage must add <1ms overhead on the hot path.
//!
//! ## Guarantees
//! - Deterministic: same prompt + metadata always produces the same dedup key
//! - Thread-safe: all operations are safe under concurrent access
//! - Bounded: memory usage is capped at `max_entries * avg_key_size`
//! - Non-blocking: `check_and_register` never blocks the calling task
//!
//! ## NOT Responsible For
//! - Cross-node deduplication (see: distributed/ module)
//! - Semantic similarity (exact match only)
//! - Persistence (in-memory only, Redis integration is a separate layer)
```

---

## 7. Performance Contracts

Each stage has a defined latency budget. Tests must verify these:

| Stage | P50 Budget | P99 Budget | Measurement |
|-------|-----------|-----------|-------------|
| Deduplication | <0.5ms | <1ms | `#[bench]` test |
| Circuit Breaker check | <10μs | <50μs | `#[bench]` test |
| RAG retrieval | <5ms | <20ms | Integration test |
| Retry policy evaluation | <1μs | <5μs | `#[bench]` test |
| Channel send (bounded) | <1μs | <10μs | `#[bench]` test |

```rust
// Required benchmark format
#[bench]
fn bench_dedup_check_hot_path(b: &mut Bencher) {
    let dedup = Deduplicator::new(Duration::from_secs(300));
    let key = DeduplicationKey::from_prompt("benchmark prompt");
    
    b.iter(|| {
        black_box(dedup.check(&key))
    });
}
```

---

## 8. Documentation Requirements

Every public item requires:

```rust
/// Brief one-line summary of what this does.
///
/// # Arguments
/// * `key` — The deduplication key derived from prompt + metadata hash
/// * `timeout` — Maximum time to wait for an in-progress request to complete
///
/// # Returns
/// - `Ok(DeduplicationResult::Cached(response))` — if a cached result exists
/// - `Ok(DeduplicationResult::New(token))` — if this is a novel request
/// - `Ok(DeduplicationResult::InProgress)` — if a matching request is in-flight
/// - `Err(OrchestratorError::DeduplicationConflict)` — on key collision
///
/// # Panics
/// This function never panics.
///
/// # Example
/// ```rust
/// let result = dedup.check_and_register(&key).await?;
/// ```
pub async fn check_and_register(&self, key: &DeduplicationKey) -> Result<DeduplicationResult, OrchestratorError> {
```

---

## 9. Dependency Policy

Before adding any dependency:

1. **Audit it**: `cargo audit` must return clean
2. **Justify it**: Comment in `Cargo.toml` explaining why it's needed
3. **Scope it**: Feature-flag it if it's not needed in the core path
4. **Test it**: At least one test must exercise the dependency directly

```toml
[dependencies]
# tokio: async runtime — core dependency, never remove
tokio = { version = "1.40", features = ["full"] }

# thiserror: zero-cost error derive — replaces manual Display impls
thiserror = "1.0"

# proptest: property-based testing — required for algorithmic correctness guarantees
# [dev-dependency only, never in production binary]
```

---

## 10. Agent Coordination Rules

When multiple agents work on this codebase simultaneously:

- **One agent owns one module at a time** — no concurrent edits to the same file
- **All inter-module changes require a contract update** — update the module's doc comment before changing its interface
- **No agent merges without ratio check** — the merging agent runs `ratio_check.sh` before finalizing
- **Test agents run in parallel** — writing tests for existing code can happen concurrently with new feature development in different modules
- **Breaking changes require a deprecation cycle** — mark old API `#[deprecated]`, add new API, migrate call sites, remove old in next phase

### Build Isolation (Windows / parallel agents)

Each agent session **must** isolate its build artifacts to avoid LNK1104 file-lock contention:

```bash
# Set at the start of every agent session (use a unique suffix per agent):
export CARGO_TARGET_DIR=target/agent-$(date +%s)
```

`.cargo/config.toml` sets a shared fallback of `target/claude`. Agents that set
`CARGO_TARGET_DIR` explicitly will override this and get full isolation.

**LNK1104 / file lock retry policy:**
- If a `cargo build` or `cargo test` command fails with LNK1104 (file in use):
  1. Wait **30 seconds** (do not retry immediately)
  2. Retry the **same command once**
  3. If it fails again, report the error — do **not** spawn parallel build attempts

### Commit & Push Workflow

The pre-commit git hook is **disabled** (`.git/hooks/pre-commit.disabled`).
`scripts/pre_commit.sh` is a **manual-only** script — it does NOT run automatically.

**Required workflow before every `git push`:**

```bash
# Run ONCE manually before pushing (not before every commit):
cargo test --features "full,mcp"

# Only push if the above passes:
git push
```

Do **not** run `cargo test` before every individual commit. Run it once before the final push.

---

## 11. The Definition of Done

A feature is **done** when:

```
✓ Implementation passes cargo test --all-features
✓ Unit tests cover every branch (including error paths)
✓ Integration test covers the feature end-to-end
✓ Property tests cover algorithmic invariants (if applicable)
✓ Chaos test covers the failure mode (if applicable)
✓ Benchmark establishes and verifies performance budget
✓ Public API is fully documented with examples
✓ CLAUDE.md ratio check passes (≥1.5:1)
✓ cargo clippy -- -D warnings is clean
✓ cargo audit is clean
✓ Module contract doc is updated
✓ ROADMAP.md phase status updated
```

If any item is missing, the feature is **not done**.

---

## 12. Scripts Reference

```bash
./scripts/ratio_check.sh      # Verify 1.5:1 test ratio
./scripts/panic_scan.sh       # Detect unwrap/expect in src/
./scripts/coverage.sh         # Generate HTML coverage report
./scripts/bench_all.sh        # Run all benchmarks, fail if budget exceeded
./scripts/audit_all.sh        # cargo audit + clippy + fmt check
./scripts/pre_commit.sh       # Full pre-commit gate (runs all above)
```

These scripts must exist and pass before any feature is considered merged.
