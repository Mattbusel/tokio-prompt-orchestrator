//! Enhanced resilience and performance features for the pipeline.
//!
//! This module groups six production-grade primitives that operate at the
//! nanosecond-to-microsecond range and are safe to use concurrently across
//! multiple pipeline stages:
//!
//! | Sub-module | Purpose |
//! |------------|---------|
//! | [`cache`] | In-process LRU cache with TTL for inference results |
//! | [`circuit_breaker`] | Prevents cascading failures; opens on threshold, probes recovery |
//! | [`dedup`] | Request coalescing — collapses identical in-flight prompts |
//! | [`priority`] | Four-level priority queue for request scheduling |
//! | [`rate_limit`] | Token-bucket rate limiter with configurable burst |
//! | [`retry`] | Exponential back-off with jitter and configurable attempt cap |
//!
//! All types are `Clone` and `Send + Sync`, suitable for sharing across Tokio tasks.

pub mod adaptive_timeout;
pub mod bulkhead;
pub mod cache;
pub mod circuit_breaker;
pub mod dedup;
pub mod priority;
pub mod rate_limit;
pub mod retry;

// Re-exports
pub use adaptive_timeout::AdaptiveTimeout;
pub use bulkhead::{Bulkhead, BulkheadPermit};
pub use cache::{cache_key, CacheLayer};
pub use circuit_breaker::{CircuitBreaker, CircuitStatus};
pub use dedup::{dedup_key, DeduplicationResult, Deduplicator};
pub use priority::{Priority, PriorityQueue};
pub use rate_limit::RateLimiter;
pub use retry::RetryPolicy;
