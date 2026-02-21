//! Enhanced Features
//!
//! Provides caching, rate limiting, priority queues, deduplication,
//! circuit breakers, and retry logic.

pub mod cache;
pub mod circuit_breaker;
pub mod dedup;
pub mod priority;
pub mod rate_limit;
pub mod retry;

// Re-exports
pub use cache::{cache_key, CacheLayer};
pub use circuit_breaker::{CircuitBreaker, CircuitStatus};
pub use dedup::{DeduplicationResult, Deduplicator};
pub use priority::{Priority, PriorityQueue};
pub use rate_limit::RateLimiter;
pub use retry::RetryPolicy;
