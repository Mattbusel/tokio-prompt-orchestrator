//! Enhanced Features
//!
//! Provides caching, rate limiting, priority queues, deduplication,
//! circuit breakers, and retry logic.

pub mod cache;
pub mod rate_limit;
pub mod priority;
pub mod dedup;
pub mod circuit_breaker;
pub mod retry;

// Re-exports
pub use cache::CacheLayer;
pub use rate_limit::RateLimiter;
pub use priority::{PriorityQueue, Priority};
pub use dedup::{Deduplicator, DeduplicationResult};
pub use circuit_breaker::{CircuitBreaker, CircuitStatus};
pub use retry::RetryPolicy;
