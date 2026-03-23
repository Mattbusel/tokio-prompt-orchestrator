//! Token-bucket and sliding-window rate limiter per model.
//!
//! Provides [`RateLimiterRegistry`] which holds a [`ModelRateLimiter`] for
//! each registered model.  Each model limiter combines a [`TokenBucket`]
//! (burst-capable refilling bucket) with a [`SlidingWindow`] (rolling
//! request-count check) so that both instantaneous bursts and sustained
//! throughput are controlled.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use thiserror::Error;

// ── RateLimitError ────────────────────────────────────────────────────────────

/// Errors returned when a rate-limit check fails.
#[derive(Debug, Error)]
pub enum RateLimitError {
    /// The token bucket has been exhausted; caller should retry after the
    /// indicated delay.
    #[error("token bucket exhausted; retry after {retry_after_ms} ms")]
    TokenBucketExhausted {
        /// Approximate milliseconds until enough tokens are available.
        retry_after_ms: u64,
    },
    /// The sliding-window request count has been exceeded; caller should wait
    /// until the window resets.
    #[error("sliding window limit exceeded; window resets in {reset_at_ms} ms")]
    WindowLimitExceeded {
        /// Milliseconds until the oldest slot falls out of the window.
        reset_at_ms: u64,
    },
    /// No limiter has been registered for the given model.
    #[error("no rate limiter registered for model '{0}'")]
    UnknownModel(String),
}

// ── RateLimiterConfig ─────────────────────────────────────────────────────────

/// Configuration used when registering a model with the [`RateLimiterRegistry`].
#[derive(Debug, Clone)]
pub struct RateLimiterConfig {
    /// Maximum requests allowed per minute in the sliding window.
    pub requests_per_minute: u32,
    /// Maximum tokens allowed per minute in the token bucket.
    pub tokens_per_minute: u64,
    /// Burst multiplier applied to `tokens_per_minute` to derive bucket
    /// capacity (e.g. `2.0` allows a burst up to 2× the per-minute quota).
    pub burst_multiplier: f64,
}

// ── TokenBucket ───────────────────────────────────────────────────────────────

/// Refilling token bucket.
///
/// Tokens are stored as a signed 64-bit integer (scaled by 1000 to allow
/// sub-token precision) and refilled lazily on every call to
/// [`TokenBucket::try_consume`].
pub struct TokenBucket {
    /// Maximum tokens the bucket can hold (scaled ×1000).
    capacity: u64,
    /// Current token count (scaled ×1000); stored as `i64` for atomic CAS.
    tokens: AtomicI64,
    /// Tokens added per millisecond (scaled ×1000).
    refill_rate: u64,
    /// Wall-clock time of the last refill.
    last_refill: Mutex<Instant>,
}

impl TokenBucket {
    /// Create a new bucket with the given capacity (in whole tokens) and a
    /// per-minute refill rate.  The bucket starts full.
    pub fn new(capacity_tokens: u64, tokens_per_minute: u64) -> Self {
        let capacity_scaled = capacity_tokens.saturating_mul(1_000);
        // refill_rate in (tokens * 1000) per ms
        let refill_rate = tokens_per_minute.saturating_mul(1_000) / 60_000;
        Self {
            capacity: capacity_scaled,
            tokens: AtomicI64::new(capacity_scaled as i64),
            refill_rate,
            last_refill: Mutex::new(Instant::now()),
        }
    }

    /// Attempt to consume `n` tokens.
    ///
    /// Refills the bucket based on elapsed time first, then deducts `n`.
    /// Returns `true` if the tokens were available and consumed, `false`
    /// otherwise (bucket remains unchanged on failure).
    pub fn try_consume(&self, n: u64) -> bool {
        // Refill
        let elapsed_ms = {
            let mut guard = self.last_refill.lock().unwrap();
            let now = Instant::now();
            let ms = now.duration_since(*guard).as_millis() as u64;
            if ms > 0 {
                *guard = now;
            }
            ms
        };

        let add = (elapsed_ms.saturating_mul(self.refill_rate)) as i64;
        if add > 0 {
            let cap = self.capacity as i64;
            // Clamp to capacity
            let _ = self.tokens.fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some((cur + add).min(cap))
            });
        }

        // Try to consume
        let needed = (n.saturating_mul(1_000)) as i64;
        self.tokens
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                if cur >= needed {
                    Some(cur - needed)
                } else {
                    None
                }
            })
            .is_ok()
    }

    /// Estimated milliseconds until `n` tokens are available.
    pub fn retry_after_ms(&self, n: u64) -> u64 {
        let cur = self.tokens.load(Ordering::Acquire);
        let needed = (n.saturating_mul(1_000)) as i64;
        if cur >= needed {
            return 0;
        }
        let deficit = (needed - cur) as u64;
        if self.refill_rate == 0 {
            return u64::MAX;
        }
        deficit / self.refill_rate + 1
    }
}

// ── SlidingWindow ─────────────────────────────────────────────────────────────

/// Rolling-window request counter.
///
/// Tracks `(timestamp, token_count)` slots and enforces a maximum request
/// count over the configured window.
pub struct SlidingWindow {
    /// Window width in milliseconds.
    window_ms: u64,
    /// Slots of `(arrival_instant, token_count)`.
    slots: VecDeque<(Instant, u64)>,
    /// Maximum number of requests allowed within the window.
    max_count: u32,
}

impl SlidingWindow {
    /// Create a new sliding window.
    pub fn new(window_ms: u64, max_count: u32) -> Self {
        Self {
            window_ms,
            slots: VecDeque::new(),
            max_count,
        }
    }

    /// Evict slots that have fallen outside the current window.
    fn evict(&mut self) {
        let now = Instant::now();
        let cutoff_ms = self.window_ms;
        while let Some(&(ts, _)) = self.slots.front() {
            if now.duration_since(ts).as_millis() as u64 >= cutoff_ms {
                self.slots.pop_front();
            } else {
                break;
            }
        }
    }

    /// Record a new request with the given token count.
    pub fn record(&mut self, tokens: u64) {
        self.evict();
        self.slots.push_back((Instant::now(), tokens));
    }

    /// Returns `true` if a new request can be admitted (count would not
    /// exceed `max_count` after adding it).
    pub fn check(&mut self) -> bool {
        self.evict();
        (self.slots.len() as u32) < self.max_count
    }

    /// Milliseconds until the oldest slot expires (0 if window is empty).
    pub fn reset_at_ms(&mut self) -> u64 {
        self.evict();
        match self.slots.front() {
            None => 0,
            Some(&(ts, _)) => {
                let elapsed = Instant::now().duration_since(ts).as_millis() as u64;
                self.window_ms.saturating_sub(elapsed) + 1
            }
        }
    }
}

// ── ModelRateLimiter ──────────────────────────────────────────────────────────

/// Combined rate limiter for a single model.
///
/// Checks both the token bucket and the sliding window on every call to
/// [`ModelRateLimiter::check_and_consume`].
pub struct ModelRateLimiter {
    pub(crate) token_bucket: TokenBucket,
    pub(crate) sliding_window: Mutex<SlidingWindow>,
    /// Model identifier.
    pub model_id: String,
    /// Cumulative count of requests that were throttled (either limiter).
    pub total_throttled: AtomicU64,
}

impl ModelRateLimiter {
    /// Create a limiter from a [`RateLimiterConfig`].
    pub fn new(model_id: String, config: &RateLimiterConfig) -> Self {
        let capacity = (config.tokens_per_minute as f64 * config.burst_multiplier) as u64;
        Self {
            token_bucket: TokenBucket::new(capacity, config.tokens_per_minute),
            sliding_window: Mutex::new(SlidingWindow::new(60_000, config.requests_per_minute)),
            model_id,
            total_throttled: AtomicU64::new(0),
        }
    }

    /// Check whether a request consuming `tokens` may proceed.
    ///
    /// On success, the tokens are deducted from the bucket and the request is
    /// recorded in the sliding window.  On failure, `total_throttled` is
    /// incremented and the appropriate [`RateLimitError`] is returned.
    pub fn check_and_consume(&self, tokens: u64) -> Result<(), RateLimitError> {
        // Check sliding window first (cheaper)
        {
            let mut win = self.sliding_window.lock().unwrap();
            if !win.check() {
                let reset = win.reset_at_ms();
                self.total_throttled.fetch_add(1, Ordering::Relaxed);
                return Err(RateLimitError::WindowLimitExceeded { reset_at_ms: reset });
            }
        }

        // Check token bucket
        if !self.token_bucket.try_consume(tokens) {
            let retry = self.token_bucket.retry_after_ms(tokens);
            self.total_throttled.fetch_add(1, Ordering::Relaxed);
            return Err(RateLimitError::TokenBucketExhausted {
                retry_after_ms: retry,
            });
        }

        // Record in sliding window
        {
            let mut win = self.sliding_window.lock().unwrap();
            win.record(tokens);
        }

        Ok(())
    }
}

// ── RateLimiterRegistry ───────────────────────────────────────────────────────

/// Registry that holds one [`ModelRateLimiter`] per model.
#[derive(Default)]
pub struct RateLimiterRegistry {
    limiters: RwLock<HashMap<String, ModelRateLimiter>>,
}

impl RateLimiterRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a model with the given configuration.
    ///
    /// Overwrites any previous limiter for the same `model_id`.
    pub fn register(&self, model_id: String, config: RateLimiterConfig) {
        let limiter = ModelRateLimiter::new(model_id.clone(), &config);
        self.limiters.write().unwrap().insert(model_id, limiter);
    }

    /// Check and consume `tokens` for the given model.
    ///
    /// Returns [`RateLimitError::UnknownModel`] if the model has not been
    /// registered.
    pub fn check(&self, model_id: &str, tokens: u64) -> Result<(), RateLimitError> {
        let guard = self.limiters.read().unwrap();
        match guard.get(model_id) {
            Some(lim) => lim.check_and_consume(tokens),
            None => Err(RateLimitError::UnknownModel(model_id.to_owned())),
        }
    }

    /// Return `(model_id, total_throttled)` pairs for all registered models.
    pub fn stats(&self) -> Vec<(String, u64)> {
        self.limiters
            .read()
            .unwrap()
            .iter()
            .map(|(id, lim)| (id.clone(), lim.total_throttled.load(Ordering::Relaxed)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_consume_and_exhaust() {
        let bucket = TokenBucket::new(10, 600); // 10 tokens capacity, 600/min
        assert!(bucket.try_consume(5));
        assert!(bucket.try_consume(5));
        assert!(!bucket.try_consume(1)); // exhausted
    }

    #[test]
    fn sliding_window_count_limit() {
        let mut win = SlidingWindow::new(60_000, 3);
        assert!(win.check());
        win.record(1);
        win.record(1);
        win.record(1);
        assert!(!win.check()); // 3 requests recorded, limit reached
    }

    #[test]
    fn registry_unknown_model() {
        let reg = RateLimiterRegistry::new();
        assert!(matches!(
            reg.check("unknown", 1),
            Err(RateLimitError::UnknownModel(_))
        ));
    }

    #[test]
    fn registry_allows_then_throttles() {
        let reg = RateLimiterRegistry::new();
        reg.register(
            "gpt-4o".to_string(),
            RateLimiterConfig {
                requests_per_minute: 2,
                tokens_per_minute: 1_000,
                burst_multiplier: 1.0,
            },
        );
        assert!(reg.check("gpt-4o", 1).is_ok());
        assert!(reg.check("gpt-4o", 1).is_ok());
        // Third request exceeds sliding window limit of 2
        assert!(reg.check("gpt-4o", 1).is_err());
    }
}
