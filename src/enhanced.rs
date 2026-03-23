//! Enhanced resilience and performance features for the pipeline.
//!
//! This module groups production-grade primitives that operate at the
//! nanosecond-to-microsecond range and are safe to use concurrently across
//! multiple pipeline stages:
//!
//! | Sub-module | Purpose |
//! |------------|---------|
//! | [`cache`] | In-process LRU cache with TTL for inference results |
//! | [`circuit_breaker`] | Prevents cascading failures; opens on threshold, probes recovery with exponential backoff |
//! | [`dedup`] | Request coalescing — collapses byte-identical in-flight prompts |
//! | [`semantic_dedup`] | Near-duplicate detection using SimHash LSH — catches paraphrases with configurable Hamming-distance threshold; when a near-duplicate is found, logs the similarity score and optionally returns the cached result |
//! | [`priority`] | Four-level priority queue for request scheduling |
//! | [`rate_limit`] | Token-bucket rate limiter with configurable burst |
//! | [`retry`] | Exponential back-off with jitter and configurable attempt cap |
//! | [`smart_batch`] | Adaptive micro-batching with prefix-based similarity grouping |
//! | [`tournament`] | Multi-provider tournament: fan-out and return the best response |
//!
//! All types are `Clone` and `Send + Sync`, suitable for sharing across Tokio tasks.

pub mod adaptive_timeout;
pub mod bulkhead;
pub mod cache;
pub mod circuit_breaker;
pub mod dedup;
pub mod dlq_replay;
pub mod priority;
pub mod rate_limit;
pub mod retry;
pub mod semantic_dedup;
pub mod smart_batch;
pub mod tournament;

// Re-exports
pub use adaptive_timeout::AdaptiveTimeout;
pub use bulkhead::{Bulkhead, BulkheadPermit};
pub use cache::{cache_key, CacheLayer};
pub use circuit_breaker::{CircuitBreaker, CircuitStatus};
pub use dedup::{dedup_key, DeduplicationResult, DeduplicationToken, Deduplicator, DEDUP_CANCELLED_SENTINEL};
pub use dlq_replay::{DlqReplayScheduler, ReplayEntry};
pub use priority::{Priority, PriorityQueue, QueueStats};
pub use rate_limit::RateLimiter;
pub use retry::RetryPolicy;
pub use semantic_dedup::{SemanticDeduplicator, SimHashFingerprint};
pub use smart_batch::{BatchConfig, BatcherStats, SmartBatcher};
pub use tournament::{
    FastestResponseScorer, KeywordDensityScorer, LongestResponseScorer, ResponseScorer,
    TournamentConfig, TournamentResult, TournamentRunner, TournamentStats,
};
