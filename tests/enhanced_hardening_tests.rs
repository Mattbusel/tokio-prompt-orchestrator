//! Enhanced modules — integration / hardening tests
//!
//! Tests in this module exercise `rate_limit`, `priority`, and `cache`
//! from the public API surface (integration-level).

use std::collections::HashMap;
use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::{CacheLayer, Priority, PriorityQueue, RateLimiter};
use tokio_prompt_orchestrator::{PromptRequest, SessionId};

// ── Helpers ──────────────────────────────────────────────────────────

fn make_request(id: &str) -> PromptRequest {
    PromptRequest {
        session: SessionId::new(id),
        request_id: format!("req-{id}"),
        input: id.to_string(),
        meta: HashMap::new(),
    }
}

// ── Rate Limiter integration ─────────────────────────────────────────

#[tokio::test]
async fn test_rate_limiter_full_lifecycle() {
    let limiter = RateLimiter::new(3, 600);

    // Consume quota
    assert!(limiter.check("user-1").await);
    assert!(limiter.check("user-1").await);
    assert!(limiter.check("user-1").await);
    assert!(!limiter.check("user-1").await);

    // Check usage reflects consumed quota
    let info = limiter.get_usage("user-1").unwrap();
    assert_eq!(info.used, 3);
    assert_eq!(info.remaining, 0);

    // Reset and verify restored
    limiter.reset("user-1").await;
    assert!(limiter.check("user-1").await);
}

#[tokio::test]
async fn test_rate_limiter_zero_max_blocks_everything() {
    let limiter = RateLimiter::new(0, 60);
    for _ in 0..5 {
        assert!(!limiter.check("any").await);
    }
}

#[tokio::test]
async fn test_rate_limiter_window_expiry() {
    let limiter = RateLimiter::new(1, 1);

    assert!(limiter.check("s").await);
    assert!(!limiter.check("s").await);

    tokio::time::sleep(Duration::from_millis(1100)).await;

    assert!(limiter.check("s").await, "must allow after window expires");
}

// ── Priority Queue integration ───────────────────────────────────────

#[tokio::test]
async fn test_priority_queue_ordering_end_to_end() {
    let queue = PriorityQueue::new();

    queue.push(Priority::Low, make_request("low")).await.ok();
    queue
        .push(Priority::Critical, make_request("critical"))
        .await
        .ok();
    queue
        .push(Priority::Normal, make_request("normal"))
        .await
        .ok();
    queue.push(Priority::High, make_request("high")).await.ok();

    let (p1, _) = queue.pop().await.unwrap();
    let (p2, _) = queue.pop().await.unwrap();
    let (p3, _) = queue.pop().await.unwrap();
    let (p4, _) = queue.pop().await.unwrap();

    assert_eq!(p1, Priority::Critical);
    assert_eq!(p2, Priority::High);
    assert_eq!(p3, Priority::Normal);
    assert_eq!(p4, Priority::Low);
    assert!(queue.pop().await.is_none());
}

#[tokio::test]
async fn test_priority_queue_fifo_within_same_level() {
    let queue = PriorityQueue::new();

    for i in 0..5 {
        queue
            .push(Priority::Normal, make_request(&format!("req-{i}")))
            .await
            .ok();
    }

    for i in 0..5 {
        let (_, req) = queue.pop().await.unwrap();
        assert_eq!(req.input, format!("req-{i}"), "FIFO violated at index {i}");
    }
}

#[tokio::test]
async fn test_priority_queue_capacity_enforcement() {
    let queue = PriorityQueue::with_capacity(3);

    queue.push(Priority::Normal, make_request("1")).await.ok();
    queue.push(Priority::Normal, make_request("2")).await.ok();
    queue.push(Priority::Normal, make_request("3")).await.ok();

    let result = queue.push(Priority::Normal, make_request("4")).await;
    assert!(result.is_err(), "must reject when at capacity");
}

#[tokio::test]
async fn test_priority_queue_stats_accuracy() {
    let queue = PriorityQueue::new();

    queue.push(Priority::High, make_request("h")).await.ok();
    queue.push(Priority::High, make_request("h2")).await.ok();
    queue.push(Priority::Low, make_request("l")).await.ok();

    let stats = queue.stats().await;
    assert_eq!(stats.total, 3);
    assert_eq!(*stats.by_priority.get(&Priority::High).unwrap(), 2);
    assert_eq!(*stats.by_priority.get(&Priority::Low).unwrap(), 1);
}

// ── Cache Layer integration ──────────────────────────────────────────

#[tokio::test]
async fn test_cache_set_get_delete_lifecycle() {
    let cache = CacheLayer::new_memory(100);

    cache.set("k", "v", 3600).await;
    assert_eq!(cache.get("k").await, Some("v".to_string()));

    cache.delete("k").await;
    assert_eq!(cache.get("k").await, None);
}

#[tokio::test]
async fn test_cache_ttl_expiration() {
    let cache = CacheLayer::new_memory(100);

    cache.set("exp", "data", 1).await;
    assert_eq!(cache.get("exp").await, Some("data".to_string()));

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(cache.get("exp").await, None);
}

#[tokio::test]
async fn test_cache_eviction_at_capacity() {
    let cache = CacheLayer::new_memory(2);

    cache.set("a", "1", 3600).await;
    cache.set("b", "2", 3600).await;
    cache.set("c", "3", 3600).await; // evicts one

    assert_eq!(cache.stats().entries, 2);
    assert_eq!(cache.get("c").await, Some("3".to_string()));
}

#[tokio::test]
async fn test_cache_concurrent_writes_no_panic() {
    let cache = CacheLayer::new_memory(500);

    let mut handles = Vec::new();
    for i in 0..10 {
        let c = cache.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..20 {
                c.set(format!("t{i}-k{j}"), format!("v{j}"), 3600).await;
            }
        }));
    }

    for h in handles {
        h.await.unwrap_or(());
    }

    assert!(cache.stats().entries <= 500);
}

#[tokio::test]
async fn test_cache_clear_and_stats() {
    let cache = CacheLayer::new_memory(100);

    for i in 0..5 {
        cache.set(format!("key-{i}"), "val", 3600).await;
    }
    assert_eq!(cache.stats().entries, 5);

    cache.clear().await;
    assert_eq!(cache.stats().entries, 0);
}

#[tokio::test]
async fn test_cache_key_generation_deterministic() {
    use tokio_prompt_orchestrator::enhanced::cache_key;

    let k1 = cache_key("prompt text");
    let k2 = cache_key("prompt text");
    let k3 = cache_key("different");

    assert_eq!(k1, k2);
    assert_ne!(k1, k3);
    assert!(k1.starts_with("prompt:"));
}
