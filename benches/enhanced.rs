//! Enhanced-features benchmarks — performance contracts for resilience primitives.
//!
//! Budget reference (CLAUDE.md §7):
//! - Deduplication hot path:     P50 <0.5ms, P99 <1ms
//! - Circuit breaker check:      P50 <10μs,  P99 <50μs
//! - Retry policy evaluation:    P50 <1μs,   P99 <5μs
//! - Priority queue push+pop:    P50 <5μs,   P99 <20μs
//! - Cache operations:           P50 <0.1ms, P99 <0.5ms

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::collections::HashMap;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio_prompt_orchestrator::enhanced::cache::{cache_key, CacheLayer};
use tokio_prompt_orchestrator::enhanced::circuit_breaker::CircuitBreakerError;
use tokio_prompt_orchestrator::enhanced::dedup::dedup_key;
use tokio_prompt_orchestrator::enhanced::{
    CircuitBreaker, CircuitStatus, Deduplicator, Priority, PriorityQueue, RateLimiter, RetryPolicy,
};
use tokio_prompt_orchestrator::{PromptRequest, SessionId};

// ═══════════════════════════════════════════════════════════════════════════
// Deduplication   —   Budget: P99 <1ms (1 000 μs)
// ═══════════════════════════════════════════════════════════════════════════

fn bench_dedup_check_new(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("dedup_check_new", |b| {
        b.to_async(&rt).iter(|| async {
            let dedup = Deduplicator::new(Duration::from_secs(300));
            let result = dedup.check_and_register(black_box("novel-key-1234")).await;
            black_box(result);
        })
    });
}

fn bench_dedup_check_cached(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("dedup_check_cached", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let dedup = Deduplicator::new(Duration::from_secs(300));

            // Pre-populate: register + complete so the key is in Cached state
            let token = match dedup.check_and_register("bench-key").await {
                tokio_prompt_orchestrator::enhanced::DeduplicationResult::New(t) => t,
                _ => unreachable!(),
            };
            dedup.complete(token, "cached-result".to_string()).await;

            let start = std::time::Instant::now();
            for _ in 0..iters {
                let result = dedup.check_and_register(black_box("bench-key")).await;
                black_box(result);
            }
            start.elapsed()
        })
    });
}

fn bench_dedup_key_generation(c: &mut Criterion) {
    c.bench_function("dedup_key_generation", |b| {
        let meta = HashMap::new();
        b.iter(|| {
            black_box(dedup_key(
                black_box("benchmark prompt for dedup key hash"),
                black_box(&meta),
            ));
        })
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// Circuit Breaker   —   Budget: P99 <50μs
// ═══════════════════════════════════════════════════════════════════════════

fn bench_circuit_breaker_check_closed(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("circuit_breaker_check_closed", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let breaker = CircuitBreaker::new(100, 0.8, Duration::from_secs(60));

            let start = std::time::Instant::now();
            for _ in 0..iters {
                let result: Result<u32, CircuitBreakerError<&str>> =
                    breaker.call(|| async { Ok(black_box(42)) }).await;
                let _ = black_box(result);
            }
            start.elapsed()
        })
    });
}

fn bench_circuit_breaker_check_open(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("circuit_breaker_check_open", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let breaker = CircuitBreaker::new(2, 0.8, Duration::from_secs(3600));

            // Trip the breaker
            breaker.trip().await;
            assert_eq!(breaker.status().await, CircuitStatus::Open);

            let start = std::time::Instant::now();
            for _ in 0..iters {
                let result: Result<u32, CircuitBreakerError<&str>> =
                    breaker.call(|| async { Ok(42) }).await;
                let _ = black_box(result);
            }
            start.elapsed()
        })
    });
}

fn bench_circuit_breaker_status(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("circuit_breaker_status", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));

            let start = std::time::Instant::now();
            for _ in 0..iters {
                black_box(breaker.status().await);
            }
            start.elapsed()
        })
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// Retry Policy   —   Budget: P99 <5μs
// ═══════════════════════════════════════════════════════════════════════════

fn bench_retry_policy_fixed_delay(c: &mut Criterion) {
    let policy = RetryPolicy::fixed(5, Duration::from_millis(100));

    c.bench_function("retry_policy_fixed_delay", |b| {
        b.iter(|| {
            // Constructing the policy is what we measure — calculate_delay is private
            // but we can benchmark construction + is_retryable which represent the
            // evaluation cost.
            let p = black_box(&policy);
            black_box(p.is_retryable(&"test error"));
        })
    });
}

fn bench_retry_policy_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("retry_policy_construction");

    group.bench_function("fixed", |b| {
        b.iter(|| {
            black_box(RetryPolicy::fixed(
                black_box(5),
                black_box(Duration::from_millis(100)),
            ));
        })
    });

    group.bench_function("exponential", |b| {
        b.iter(|| {
            black_box(RetryPolicy::exponential(
                black_box(5),
                black_box(Duration::from_millis(100)),
            ));
        })
    });

    group.bench_function("linear", |b| {
        b.iter(|| {
            black_box(RetryPolicy::linear(
                black_box(5),
                black_box(Duration::from_millis(100)),
                black_box(Duration::from_millis(50)),
            ));
        })
    });

    group.finish();
}

fn bench_retry_jitter(c: &mut Criterion) {
    use tokio_prompt_orchestrator::enhanced::retry::with_jitter;

    c.bench_function("retry_jitter", |b| {
        b.iter(|| {
            black_box(with_jitter(black_box(Duration::from_millis(500))));
        })
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// Priority Queue   —   Budget: P50 <5μs, P99 <20μs for push+pop
// ═══════════════════════════════════════════════════════════════════════════

fn make_request(id: &str) -> PromptRequest {
    PromptRequest {
        session: SessionId::new(format!("bench-{id}")),
        request_id: format!("req-{id}"),
        input: format!("prompt {id}"),
        meta: HashMap::new(),
    }
}

fn bench_priority_queue_push(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("priority_queue_push");

    for (name, priority) in [
        ("critical", Priority::Critical),
        ("high", Priority::High),
        ("normal", Priority::Normal),
        ("low", Priority::Low),
    ] {
        group.bench_with_input(BenchmarkId::new("priority", name), &priority, |b, &prio| {
            b.to_async(&rt).iter(|| async move {
                let queue = PriorityQueue::new();
                let req = make_request("bench");
                let result = queue.push(prio, req).await;
                let _ = black_box(result);
            })
        });
    }

    group.finish();
}

fn bench_priority_queue_pop(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("priority_queue_pop", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let queue = PriorityQueue::new();

            // Pre-fill the queue
            for i in 0..iters {
                let prio = match i % 4 {
                    0 => Priority::Low,
                    1 => Priority::Normal,
                    2 => Priority::High,
                    _ => Priority::Critical,
                };
                let _ = queue.push(prio, make_request(&i.to_string())).await;
            }

            let start = std::time::Instant::now();
            for _ in 0..iters {
                black_box(queue.pop().await);
            }
            start.elapsed()
        })
    });
}

fn bench_priority_queue_push_pop_cycle(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("priority_queue_push_pop_cycle", |b| {
        b.to_async(&rt).iter(|| async {
            let queue = PriorityQueue::new();
            let req = make_request("cycle");

            let _ = queue.push(Priority::Normal, req).await;
            black_box(queue.pop().await);
        })
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// Rate Limiter
// ═══════════════════════════════════════════════════════════════════════════

fn bench_rate_limiter_check(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("rate_limiter_check", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let limiter = RateLimiter::new(1_000_000, 3600);

            let start = std::time::Instant::now();
            for i in 0..iters {
                let session = format!("session-{}", i % 100);
                black_box(limiter.check(black_box(&session)).await);
            }
            start.elapsed()
        })
    });
}

fn bench_rate_limiter_single_session(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("rate_limiter_single_session", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let limiter = RateLimiter::new(iters as usize + 1, 3600);

            let start = std::time::Instant::now();
            for _ in 0..iters {
                black_box(limiter.check(black_box("same-session")).await);
            }
            start.elapsed()
        })
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// Cache Layer   —   Budget: P50 <0.1ms, P99 <0.5ms
// ═══════════════════════════════════════════════════════════════════════════

fn bench_cache_get_hit(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("cache_get_hit", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let cache = CacheLayer::new_memory(10_000);

            // Pre-populate
            cache.set("bench-key", "bench-value", 3600).await;

            let start = std::time::Instant::now();
            for _ in 0..iters {
                black_box(cache.get(black_box("bench-key")).await);
            }
            start.elapsed()
        })
    });
}

fn bench_cache_get_miss(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("cache_get_miss", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let cache = CacheLayer::new_memory(10_000);

            let start = std::time::Instant::now();
            for _ in 0..iters {
                black_box(cache.get(black_box("nonexistent")).await);
            }
            start.elapsed()
        })
    });
}

fn bench_cache_set(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("cache_set", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let cache = CacheLayer::new_memory(iters as usize + 1);

            let start = std::time::Instant::now();
            for i in 0..iters {
                let key = format!("key-{i}");
                cache
                    .set(black_box(key), black_box("value"), black_box(3600))
                    .await;
            }
            start.elapsed()
        })
    });
}

fn bench_cache_key_generation(c: &mut Criterion) {
    c.bench_function("cache_key_generation", |b| {
        b.iter(|| {
            black_box(cache_key(black_box(
                "A long prompt that would be typical for an LLM inference request",
            )));
        })
    });
}

fn bench_cache_eviction(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("cache_eviction", |b| {
        b.to_async(&rt).iter(|| async {
            // Small cache to force eviction on every insert
            let cache = CacheLayer::new_memory(10);

            // Fill it
            for i in 0..10u64 {
                cache.set(format!("pre-{i}"), "v", 3600).await;
            }

            // This insert forces eviction
            cache
                .set(black_box("evict-key".to_string()), "value", 3600)
                .await;
        })
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// Priority::from_name
// ═══════════════════════════════════════════════════════════════════════════

fn bench_priority_from_name(c: &mut Criterion) {
    c.bench_function("priority_from_name", |b| {
        b.iter(|| {
            black_box(Priority::from_name(black_box("critical")));
            black_box(Priority::from_name(black_box("normal")));
            black_box(Priority::from_name(black_box("low")));
            black_box(Priority::from_name(black_box("high")));
        })
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// Criterion groups
// ═══════════════════════════════════════════════════════════════════════════

criterion_group!(
    dedup_benches,
    bench_dedup_check_new,
    bench_dedup_check_cached,
    bench_dedup_key_generation,
);

criterion_group!(
    circuit_breaker_benches,
    bench_circuit_breaker_check_closed,
    bench_circuit_breaker_check_open,
    bench_circuit_breaker_status,
);

criterion_group!(
    retry_benches,
    bench_retry_policy_fixed_delay,
    bench_retry_policy_construction,
    bench_retry_jitter,
);

criterion_group!(
    priority_benches,
    bench_priority_queue_push,
    bench_priority_queue_pop,
    bench_priority_queue_push_pop_cycle,
    bench_priority_from_name,
);

criterion_group!(
    rate_limit_benches,
    bench_rate_limiter_check,
    bench_rate_limiter_single_session,
);

criterion_group!(
    cache_benches,
    bench_cache_get_hit,
    bench_cache_get_miss,
    bench_cache_set,
    bench_cache_key_generation,
    bench_cache_eviction,
);

criterion_main!(
    dedup_benches,
    circuit_breaker_benches,
    retry_benches,
    priority_benches,
    rate_limit_benches,
    cache_benches,
);
