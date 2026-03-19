# Enhanced Primitives Guide

The `enhanced` module provides six resilience and flow-control primitives that
can be used independently or composed together in a pipeline.

All primitives are `Clone + Send + Sync` — clone freely and share across Tokio
tasks.

---

## 1. Deduplicator

**Module:** `tokio_prompt_orchestrator::enhanced` \
**Type:** `Deduplicator` / `DeduplicationResult` / `DeduplicationToken`

Prevents identical concurrent requests from being processed more than once
and caches recently-completed results for a configurable window.

### When to use

- Users repeatedly submitting the same prompt (double-click, network retry).
- Idempotent background jobs that may be enqueued multiple times.

### How it works

```
caller A  ─► check_and_register("key")  ──►  New(token)   ─► process ─► complete(token, result)
caller B  ─► check_and_register("key")  ──►  InProgress   ─► wait_for_result("key")  ─► "result"
caller C  ─► check_and_register("key")  ──►  Cached("result")  (within TTL)
```

### Example

```rust
use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::{Deduplicator, DeduplicationResult, dedup_key};

let dedup = Deduplicator::new(Duration::from_secs(300));
let meta  = std::collections::HashMap::new();
let key   = dedup_key("hello world", &meta, Some("session-1"));

match dedup.check_and_register(&key).await {
    DeduplicationResult::New(token) => {
        let result = do_inference().await;
        dedup.complete(token, result).await;
    }
    DeduplicationResult::InProgress => {
        if let Some(result) = dedup.wait_for_result(&key).await {
            deliver(result);
        }
    }
    DeduplicationResult::Cached(result) => deliver(result),
}
```

### Key generation

Use `dedup_key(prompt, metadata, session_id)` to produce a stable FNV-1a hash.
Pass `Some(session_id)` to scope keys per session; pass `None` for global
deduplication across all sessions.

---

## 2. CircuitBreaker

**Module:** `tokio_prompt_orchestrator::enhanced` \
**Type:** `CircuitBreaker` / `CircuitStatus` / `CircuitBreakerError`

Stops requests to a failing downstream service to prevent cascading failures.

### States

```
Closed ──(N failures)──► Open
  ▲                        │
  │                        │ (timeout)
  │                        ▼
  └──(success rate ok)── HalfOpen
```

### When to use

- Wrapping any network call to an LLM provider or external API.
- Protecting a database connection or upstream service.

### Example

```rust
use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::CircuitBreaker;
use tokio_prompt_orchestrator::enhanced::circuit_breaker::CircuitBreakerError;

let cb = CircuitBreaker::new(
    5,                        // open after 5 consecutive failures
    0.8,                      // close when 80 % success over window
    Duration::from_secs(30),  // probe after 30 s open
);

match cb.call(|| async { call_openai().await }).await {
    Ok(tokens)                         => handle(tokens),
    Err(CircuitBreakerError::Open)     => { /* fail fast */ }
    Err(CircuitBreakerError::Failed(e))=> { /* log e */ }
}
```

### Manual control

```rust
cb.trip().await;   // force open for maintenance
cb.reset().await;  // force closed to restore traffic
let stats = cb.stats().await;
println!("{:?}", stats.status);
```

---

## 3. CacheLayer

**Module:** `tokio_prompt_orchestrator::enhanced` \
**Type:** `CacheLayer` / `CacheStats`

TTL-based response cache with pluggable backends.

### Backends

| Backend | Feature flag | Notes |
|---------|-------------|-------|
| In-memory | (always) | Per-process; evicts on capacity |
| Redis | `caching` | Shared across instances |

### When to use

- Caching deterministic or near-deterministic model responses.
- Reducing API costs for repeated prompts (complement `Deduplicator`).

### Example

```rust
use tokio_prompt_orchestrator::enhanced::{CacheLayer, cache_key};

let cache = CacheLayer::new_memory(5_000);

let key = cache_key("What is 2+2?");
if let Some(resp) = cache.get(&key).await {
    return resp;
}

let resp = infer("What is 2+2?").await;
cache.set(key, resp.clone(), 3600).await; // 1-hour TTL
resp
```

### Composition with Deduplicator

Check the deduplicator first (in-flight coalescing), then the cache (prior
completions), then do the actual inference.

---

## 4. PriorityQueue

**Module:** `tokio_prompt_orchestrator::enhanced` \
**Type:** `PriorityQueue` / `Priority` / `QueueError` / `QueueStats`

A bounded async priority queue with four tiers and FIFO ordering within each
tier.

### Priority tiers

| Variant | Value | Use case |
|---------|-------|----------|
| `Critical` | 3 | SLA-critical real-time requests |
| `High` | 2 | Interactive user requests |
| `Normal` | 1 | Standard API calls (default) |
| `Low` | 0 | Background batch jobs |

### Example

```rust
use tokio_prompt_orchestrator::enhanced::{PriorityQueue, Priority};

let queue = PriorityQueue::with_capacity(1_000);
queue.push(Priority::High, user_request).await?;
queue.push(Priority::Low,  batch_job).await?;

while let Some((priority, req)) = queue.pop().await {
    process(priority, req).await;
}
```

---

## 5. RateLimiter

**Module:** `tokio_prompt_orchestrator::enhanced` \
**Type:** `RateLimiter` / `RateLimitInfo`

Per-session token-bucket rate limiter.

### Backends

| Backend | Feature flag | Algorithm |
|---------|-------------|-----------|
| Simple | (always) | Sliding window counter |
| Governor | `rate-limiting` | True token bucket |

### Example

```rust
use tokio_prompt_orchestrator::enhanced::RateLimiter;

let limiter = RateLimiter::new(100, 60); // 100 req / 60 s per session

if !limiter.check(&session_id).await {
    return Err("rate limit exceeded");
}
```

---

## 6. RetryPolicy

**Module:** `tokio_prompt_orchestrator::enhanced` \
**Type:** `RetryPolicy` / `RetryStrategy` / `RetryResult`

**Functions:** `retry_if`, `with_jitter`, `retry_inference`

Automatic retry with configurable back-off.

### Strategies

| Strategy | Factory | When to use |
|----------|---------|-------------|
| Fixed | `RetryPolicy::fixed` | Predictable wait times |
| Exponential | `RetryPolicy::exponential` | Transient network errors |
| Linear | `RetryPolicy::linear` | Gradual cool-down |

### Example

```rust
use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::RetryPolicy;

let policy = RetryPolicy::exponential(4, Duration::from_millis(100));
// delays: 100 ms, 200 ms, 400 ms

let result = policy.retry(|| async {
    call_api().await
}).await?;
```

### Inference-aware retry

`retry_inference` handles `OrchestratorError::RateLimited` by honouring the
provider's `Retry-After` hint and stops immediately on
`OrchestratorError::BudgetExceeded`:

```rust
use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::retry::retry_inference;

let tokens = retry_inference(3, Duration::from_millis(200), || async {
    worker.infer("hello").await
}).await?;
```

---

## Composing primitives

A production inference path typically layers all six primitives:

```rust
// 1. Rate gate
if !rate_limiter.check(&session_id).await { return Err(RateLimitExceeded) }

// 2. Dedup / cache
let key = dedup_key(&prompt, &meta, Some(&session_id));
match dedup.check_and_register(&key).await {
    DeduplicationResult::Cached(r) => return Ok(r),
    DeduplicationResult::InProgress => return dedup.wait_for_result(&key).await,
    DeduplicationResult::New(token) => {
        // 3. Check persistent cache
        if let Some(r) = cache.get(&cache_key(&prompt)).await {
            dedup.complete(token, r.clone()).await;
            return Ok(r);
        }

        // 4. Priority queue
        queue.push(Priority::Normal, request).await?;

        // 5. Circuit breaker + retry
        let result = circuit_breaker.call(|| async {
            retry_policy.retry(|| async {
                worker.infer(&prompt).await
            }).await
        }).await?;

        // 6. Cache the result
        cache.set(cache_key(&prompt), result.clone(), 3600).await;
        dedup.complete(token, result.clone()).await;
        Ok(result)
    }
}
```
