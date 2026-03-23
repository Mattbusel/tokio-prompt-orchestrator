//! # Token Bucket Rate Limiter
//!
//! Per-model token bucket rate limiter with configurable capacity and refill
//! rate.  Supports both non-blocking (`try_acquire`) and async waiting
//! (`acquire`) modes.
//!
//! ## Design
//!
//! Each model gets its own [`TokenBucket`].  Tokens are refilled continuously
//! based on wall-clock time elapsed since the last check.  The bucket is
//! represented by a `f64` count of available tokens, which is clamped to
//! `[0.0, burst_capacity]` on every access.
//!
//! The [`RateLimiter`] holds all per-model buckets in a `HashMap` behind an
//! `Arc<Mutex<...>>` so it can be shared across async tasks.
//!
//! ## Example
//!
//! ```
//! use tokio_prompt_orchestrator::rate_limiter::{BucketConfig, RateLimiter};
//!
//! let configs = vec![BucketConfig {
//!     model_id: "gpt-4o".to_string(),
//!     requests_per_second: 10.0,
//!     burst_capacity: 20,
//! }];
//! let limiter = RateLimiter::new(configs);
//! assert!(limiter.try_acquire("gpt-4o").is_ok());
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;

// ── Config ────────────────────────────────────────────────────────────────────

/// Per-model token bucket configuration.
#[derive(Debug, Clone)]
pub struct BucketConfig {
    /// Model identifier this bucket is associated with.
    pub model_id: String,
    /// Steady-state refill rate in tokens per second.
    pub requests_per_second: f64,
    /// Maximum number of tokens the bucket can hold (burst allowance).
    pub burst_capacity: u32,
}

// ── Error ─────────────────────────────────────────────────────────────────────

/// Errors returned by [`RateLimiter::try_acquire`].
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum RateLimitError {
    /// No bucket is configured for the requested model.
    #[error("unknown model: no rate-limit bucket configured for '{0}'")]
    UnknownModel(String),
    /// Bucket has no tokens available right now.
    #[error("rate limit exceeded for model (retry after {wait_ms} ms)")]
    BucketEmpty {
        /// Approximate milliseconds until at least one token is available.
        wait_ms: u64,
    },
}

// ── Per-model bucket ──────────────────────────────────────────────────────────

struct TokenBucket {
    /// Current number of available tokens.
    tokens: f64,
    /// Refill rate in tokens per second.
    refill_rate: f64,
    /// Maximum tokens the bucket can hold.
    capacity: f64,
    /// Last time tokens were refilled.
    last_refill: Instant,
    /// Cumulative requests that were allowed.
    requests_allowed: u64,
    /// Cumulative requests that were denied (bucket empty).
    requests_denied: u64,
}

impl TokenBucket {
    fn new(config: &BucketConfig) -> Self {
        let capacity = config.burst_capacity as f64;
        Self {
            tokens: capacity, // start full
            refill_rate: config.requests_per_second,
            capacity,
            last_refill: Instant::now(),
            requests_allowed: 0,
            requests_denied: 0,
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        self.last_refill = now;
    }

    /// Non-blocking try to consume one token.
    fn try_consume(&mut self) -> Result<(), RateLimitError> {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            self.requests_allowed += 1;
            Ok(())
        } else {
            // Calculate how long until a token becomes available.
            let wait_secs = (1.0 - self.tokens) / self.refill_rate;
            let wait_ms = (wait_secs * 1000.0).ceil() as u64;
            self.requests_denied += 1;
            Err(RateLimitError::BucketEmpty { wait_ms })
        }
    }

    /// How many milliseconds until a token is guaranteed to be available.
    fn wait_ms_for_token(&mut self) -> u64 {
        self.refill();
        if self.tokens >= 1.0 {
            0
        } else {
            let wait_secs = (1.0 - self.tokens) / self.refill_rate;
            (wait_secs * 1000.0).ceil() as u64
        }
    }
}

// ── Stats ─────────────────────────────────────────────────────────────────────

/// Per-model statistics snapshot.
#[derive(Debug, Clone)]
pub struct ModelRateLimiterStats {
    /// Model identifier.
    pub model_id: String,
    /// Requests that were allowed since the limiter was created.
    pub requests_allowed: u64,
    /// Requests that were denied (bucket empty) since the limiter was created.
    pub requests_denied: u64,
    /// Current token count (may be fractional).
    pub current_tokens: f64,
    /// Bucket capacity.
    pub capacity: f64,
}

/// Aggregate statistics for the whole [`RateLimiter`].
#[derive(Debug, Clone, Default)]
pub struct RateLimiterStats {
    /// Per-model statistics, one entry per configured model.
    pub per_model: Vec<ModelRateLimiterStats>,
}

// ── RateLimiter ───────────────────────────────────────────────────────────────

/// Shared, per-model token bucket rate limiter.
///
/// Clone-cheap: all clones share the same underlying buckets.
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, TokenBucket>>>,
}

impl RateLimiter {
    /// Create a new `RateLimiter` with one bucket per entry in `configs`.
    ///
    /// Each bucket starts full (at `burst_capacity` tokens).
    pub fn new(configs: Vec<BucketConfig>) -> Self {
        let mut map = HashMap::new();
        for cfg in configs {
            map.insert(cfg.model_id.clone(), TokenBucket::new(&cfg));
        }
        Self {
            buckets: Arc::new(Mutex::new(map)),
        }
    }

    /// Non-blocking: attempt to consume one token for `model_id`.
    ///
    /// # Errors
    ///
    /// - [`RateLimitError::UnknownModel`] — no bucket configured for this model.
    /// - [`RateLimitError::BucketEmpty`] — bucket has no tokens right now;
    ///   includes the estimated wait in milliseconds.
    pub fn try_acquire(&self, model_id: &str) -> Result<(), RateLimitError> {
        let mut buckets = self
            .buckets
            .lock()
            .map_err(|_| RateLimitError::UnknownModel(model_id.to_owned()))?;
        match buckets.get_mut(model_id) {
            None => Err(RateLimitError::UnknownModel(model_id.to_owned())),
            Some(bucket) => bucket.try_consume(),
        }
    }

    /// Async: wait until a token is available for `model_id`, then consume it.
    ///
    /// Uses `tokio::time::sleep` to avoid spinning.
    ///
    /// # Errors
    ///
    /// Returns [`RateLimitError::UnknownModel`] if no bucket is configured for
    /// `model_id`.  All other errors are retried internally until a token
    /// becomes available.
    pub async fn acquire(&self, model_id: &str) -> Result<(), RateLimitError> {
        loop {
            // Calculate the wait inside a short-lived lock.
            let wait_ms = {
                let mut buckets = self
                    .buckets
                    .lock()
                    .map_err(|_| RateLimitError::UnknownModel(model_id.to_owned()))?;
                match buckets.get_mut(model_id) {
                    None => return Err(RateLimitError::UnknownModel(model_id.to_owned())),
                    Some(bucket) => bucket.wait_ms_for_token(),
                }
            };

            if wait_ms > 0 {
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }

            // Try again after sleeping.
            match self.try_acquire(model_id) {
                Ok(()) => return Ok(()),
                Err(RateLimitError::BucketEmpty { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Return a statistics snapshot for all configured models.
    pub fn stats(&self) -> RateLimiterStats {
        let mut buckets = match self.buckets.lock() {
            Ok(g) => g,
            Err(_) => return RateLimiterStats::default(),
        };
        let per_model = buckets
            .iter_mut()
            .map(|(model_id, bucket)| {
                bucket.refill();
                ModelRateLimiterStats {
                    model_id: model_id.clone(),
                    requests_allowed: bucket.requests_allowed,
                    requests_denied: bucket.requests_denied,
                    current_tokens: bucket.tokens,
                    capacity: bucket.capacity,
                }
            })
            .collect();
        RateLimiterStats { per_model }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn make_limiter(rps: f64, burst: u32) -> RateLimiter {
        RateLimiter::new(vec![BucketConfig {
            model_id: "test-model".to_string(),
            requests_per_second: rps,
            burst_capacity: burst,
        }])
    }

    #[test]
    fn test_try_acquire_success() {
        let rl = make_limiter(10.0, 5);
        assert!(rl.try_acquire("test-model").is_ok());
    }

    #[test]
    fn test_try_acquire_unknown_model() {
        let rl = make_limiter(10.0, 5);
        let err = rl.try_acquire("unknown").unwrap_err();
        assert!(matches!(err, RateLimitError::UnknownModel(_)));
    }

    #[test]
    fn test_bucket_empties_after_burst() {
        let rl = make_limiter(1.0, 3);
        for _ in 0..3 {
            rl.try_acquire("test-model").unwrap();
        }
        let err = rl.try_acquire("test-model").unwrap_err();
        assert!(matches!(err, RateLimitError::BucketEmpty { .. }));
    }

    #[test]
    fn test_bucket_empty_returns_wait_ms() {
        let rl = make_limiter(1.0, 1);
        rl.try_acquire("test-model").unwrap();
        match rl.try_acquire("test-model").unwrap_err() {
            RateLimitError::BucketEmpty { wait_ms } => assert!(wait_ms > 0),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_multiple_models() {
        let rl = RateLimiter::new(vec![
            BucketConfig { model_id: "m1".to_string(), requests_per_second: 10.0, burst_capacity: 5 },
            BucketConfig { model_id: "m2".to_string(), requests_per_second: 1.0, burst_capacity: 1 },
        ]);
        assert!(rl.try_acquire("m1").is_ok());
        assert!(rl.try_acquire("m2").is_ok());
        // m2 bucket is now empty
        assert!(rl.try_acquire("m2").is_err());
        // m1 bucket still has tokens
        assert!(rl.try_acquire("m1").is_ok());
    }

    #[test]
    fn test_stats_allowed_increments() {
        let rl = make_limiter(10.0, 5);
        rl.try_acquire("test-model").unwrap();
        rl.try_acquire("test-model").unwrap();
        let stats = rl.stats();
        let model_stats = stats.per_model.iter().find(|s| s.model_id == "test-model").unwrap();
        assert_eq!(model_stats.requests_allowed, 2);
        assert_eq!(model_stats.requests_denied, 0);
    }

    #[test]
    fn test_stats_denied_increments() {
        let rl = make_limiter(1.0, 1);
        rl.try_acquire("test-model").unwrap();
        let _ = rl.try_acquire("test-model"); // denied
        let stats = rl.stats();
        let model_stats = stats.per_model.iter().find(|s| s.model_id == "test-model").unwrap();
        assert_eq!(model_stats.requests_denied, 1);
    }

    #[test]
    fn test_stats_current_tokens_depletes() {
        let rl = make_limiter(1.0, 5);
        rl.try_acquire("test-model").unwrap();
        let stats = rl.stats();
        let ms = stats.per_model.iter().find(|s| s.model_id == "test-model").unwrap();
        assert!(ms.current_tokens < 5.0);
    }

    #[test]
    fn test_refill_over_time() {
        let rl = make_limiter(1000.0, 1); // 1000 tokens/sec => 1 token/ms
        rl.try_acquire("test-model").unwrap(); // empty bucket
        std::thread::sleep(Duration::from_millis(10)); // wait ~10ms => ~10 tokens
        // Should succeed now.
        assert!(rl.try_acquire("test-model").is_ok());
    }

    #[test]
    fn test_burst_capacity_respected() {
        let rl = make_limiter(1000.0, 3);
        // Drain all 3 tokens.
        for _ in 0..3 {
            rl.try_acquire("test-model").unwrap();
        }
        let stats = rl.stats();
        let ms = stats.per_model.iter().find(|s| s.model_id == "test-model").unwrap();
        assert!(ms.current_tokens < 1.0);
    }

    #[test]
    fn test_clone_shares_buckets() {
        let rl = make_limiter(10.0, 5);
        let rl2 = rl.clone();
        rl.try_acquire("test-model").unwrap();
        let stats = rl2.stats();
        let ms = stats.per_model.iter().find(|s| s.model_id == "test-model").unwrap();
        assert_eq!(ms.requests_allowed, 1);
    }

    #[test]
    fn test_error_display_unknown_model() {
        let e = RateLimitError::UnknownModel("bad-model".to_string());
        assert!(e.to_string().contains("bad-model"));
    }

    #[test]
    fn test_error_display_bucket_empty() {
        let e = RateLimitError::BucketEmpty { wait_ms: 42 };
        assert!(e.to_string().contains("42"));
    }

    #[test]
    fn test_stats_capacity_field() {
        let rl = make_limiter(5.0, 10);
        let stats = rl.stats();
        let ms = stats.per_model.iter().find(|s| s.model_id == "test-model").unwrap();
        assert_eq!(ms.capacity, 10.0);
    }

    #[test]
    fn test_bucket_starts_full() {
        let rl = make_limiter(1.0, 5);
        let stats = rl.stats();
        let ms = stats.per_model.iter().find(|s| s.model_id == "test-model").unwrap();
        assert!((ms.current_tokens - 5.0).abs() < 0.1);
    }

    #[tokio::test]
    async fn test_acquire_async_succeeds() {
        let rl = make_limiter(100.0, 5);
        rl.acquire("test-model").await.unwrap();
    }

    #[tokio::test]
    async fn test_acquire_async_unknown_model() {
        let rl = make_limiter(1.0, 1);
        let err = rl.acquire("no-such-model").await.unwrap_err();
        assert!(matches!(err, RateLimitError::UnknownModel(_)));
    }

    #[tokio::test]
    async fn test_acquire_waits_for_refill() {
        // 1000 tokens/sec, burst=1 → after drain, 1ms to refill.
        let rl = make_limiter(1000.0, 1);
        rl.try_acquire("test-model").unwrap(); // drain
        // acquire should wait and then succeed within a short time.
        tokio::time::timeout(Duration::from_millis(100), rl.acquire("test-model"))
            .await
            .expect("timeout waiting for acquire")
            .expect("acquire should succeed");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Async DashMap-based rate limiter
// ═══════════════════════════════════════════════════════════════════════════════

use dashmap::DashMap;
use std::sync::Arc as StdArc;

// ── AsyncRateLimitError ───────────────────────────────────────────────────────

/// Errors returned by [`AsyncRateLimiter::check_and_consume`].
#[derive(thiserror::Error, Debug, Clone)]
pub enum AsyncRateLimitError {
    /// No bucket is registered for the requested model.
    #[error("model '{0}' is not registered in the rate limiter")]
    ModelNotRegistered(String),
    /// The bucket does not have enough tokens for the requested amount.
    #[error("rate limit exceeded for model '{model}': only {available:.2} tokens available")]
    RateLimitExceeded {
        /// The model whose limit was exceeded.
        model: String,
        /// How many tokens were available when the request was denied.
        available: f64,
    },
}

// ── TokenBucketAsync ──────────────────────────────────────────────────────────

/// A single token bucket used by [`AsyncRateLimiter`].
///
/// Tokens are refilled continuously based on elapsed wall-clock time.
pub struct TokenBucketAsync {
    /// Maximum number of tokens the bucket can hold.
    pub capacity: f64,
    /// Current available tokens.
    pub tokens: f64,
    /// Refill rate in tokens per second.
    pub refill_rate: f64,
    /// Last refill instant.
    pub last_refill: Instant,
}

impl TokenBucketAsync {
    /// Create a new bucket starting full.
    pub fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            capacity,
            tokens: capacity,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time, then attempt to consume `tokens`.
    ///
    /// Returns `true` if the tokens were consumed, `false` if the bucket has
    /// insufficient tokens.
    pub fn try_consume(&mut self, tokens: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        self.last_refill = now;

        if self.tokens >= tokens {
            self.tokens -= tokens;
            true
        } else {
            false
        }
    }

    /// Returns the number of tokens currently available (after refill).
    pub fn available(&mut self) -> f64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        self.last_refill = now;
        self.tokens
    }
}

// ── AsyncRateLimiter ──────────────────────────────────────────────────────────

/// Async, DashMap-backed per-model rate limiter.
///
/// Each model has its own [`TokenBucketAsync`] behind a `tokio::sync::Mutex`
/// so that individual model buckets can be locked without blocking other models.
///
/// Clone-cheap: the inner `Arc<DashMap<...>>` is reference-counted.
#[derive(Clone)]
pub struct AsyncRateLimiter {
    buckets: StdArc<DashMap<String, tokio::sync::Mutex<TokenBucketAsync>>>,
}

impl AsyncRateLimiter {
    /// Create a new `AsyncRateLimiter` with no models registered.
    pub fn new() -> Self {
        Self {
            buckets: StdArc::new(DashMap::new()),
        }
    }

    /// Register a model with the given capacity and refill rate.
    ///
    /// If the model is already registered its bucket is replaced.
    pub fn register_model(&self, model: &str, capacity: f64, refill_rate: f64) {
        let bucket = TokenBucketAsync::new(capacity, refill_rate);
        self.buckets
            .insert(model.to_string(), tokio::sync::Mutex::new(bucket));
    }

    /// Attempt to consume `tokens` from the model's bucket.
    ///
    /// # Errors
    ///
    /// - [`AsyncRateLimitError::ModelNotRegistered`] — model has no bucket.
    /// - [`AsyncRateLimitError::RateLimitExceeded`] — bucket has insufficient tokens.
    pub async fn check_and_consume(
        &self,
        model: &str,
        tokens: f64,
    ) -> Result<(), AsyncRateLimitError> {
        let entry = self
            .buckets
            .get(model)
            .ok_or_else(|| AsyncRateLimitError::ModelNotRegistered(model.to_string()))?;
        let mut bucket = entry.lock().await;
        let available = bucket.available();
        if bucket.try_consume(tokens) {
            Ok(())
        } else {
            Err(AsyncRateLimitError::RateLimitExceeded {
                model: model.to_string(),
                available,
            })
        }
    }
}

impl Default for AsyncRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Async limiter tests ───────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod async_limiter_tests {
    use super::*;

    #[tokio::test]
    async fn test_basic_consumption() {
        let limiter = AsyncRateLimiter::new();
        limiter.register_model("gpt-4", 10.0, 1.0);
        assert!(limiter.check_and_consume("gpt-4", 5.0).await.is_ok());
        assert!(limiter.check_and_consume("gpt-4", 5.0).await.is_ok());
        // Bucket should now be empty.
        let err = limiter.check_and_consume("gpt-4", 1.0).await.unwrap_err();
        assert!(
            matches!(err, AsyncRateLimitError::RateLimitExceeded { .. }),
            "Expected RateLimitExceeded, got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_unregistered_model() {
        let limiter = AsyncRateLimiter::new();
        let err = limiter
            .check_and_consume("unknown-model", 1.0)
            .await
            .unwrap_err();
        assert!(
            matches!(err, AsyncRateLimitError::ModelNotRegistered(_)),
            "Expected ModelNotRegistered"
        );
    }

    #[tokio::test]
    async fn test_exceeded_limit_returns_available() {
        let limiter = AsyncRateLimiter::new();
        limiter.register_model("claude", 3.0, 0.001);
        // Drain the bucket.
        limiter.check_and_consume("claude", 3.0).await.unwrap();
        match limiter.check_and_consume("claude", 1.0).await.unwrap_err() {
            AsyncRateLimitError::RateLimitExceeded { available, .. } => {
                assert!(available < 1.0, "available should be < 1 after drain, got {available}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_refill_over_time() {
        let limiter = AsyncRateLimiter::new();
        // 1000 tokens/sec → 1 token/ms.
        limiter.register_model("fast-model", 1.0, 1000.0);
        // Drain.
        limiter.check_and_consume("fast-model", 1.0).await.unwrap();
        // Wait for refill.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(limiter.check_and_consume("fast-model", 1.0).await.is_ok());
    }

    #[test]
    fn test_error_display_not_registered() {
        let e = AsyncRateLimitError::ModelNotRegistered("m".to_string());
        assert!(e.to_string().contains("'m'"));
    }

    #[test]
    fn test_error_display_exceeded() {
        let e = AsyncRateLimitError::RateLimitExceeded {
            model: "m".to_string(),
            available: 0.5,
        };
        assert!(e.to_string().contains("m"));
        assert!(e.to_string().contains("0.50"));
    }
}
