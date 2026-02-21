//! Rate Limiting
//!
//! Token bucket rate limiter per session/user.
//!
//! ## Usage
//!
//! ```no_run
//! use tokio_prompt_orchestrator::enhanced::RateLimiter;
//! # #[tokio::main]
//! # async fn main() {
//! let limiter = RateLimiter::new(100, 60); // 100 requests per 60 seconds
//!
//! if limiter.check("user-123").await {
//!     // Process request
//! } else {
//!     // Rate limit exceeded
//! }
//! # }
//! ```

#[cfg(feature = "rate-limiting")]
use crate::OrchestratorError;
#[cfg(feature = "rate-limiting")]
use governor::{
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter as GovernorRateLimiter,
};
#[cfg(feature = "rate-limiting")]
use std::num::NonZeroU32;

use dashmap::DashMap;
use std::sync::Arc;
use tracing::{debug, warn};

/// Rate limiter with per-session limits
#[derive(Clone)]
pub struct RateLimiter {
    backend: RateLimiterBackend,
}

#[derive(Clone)]
enum RateLimiterBackend {
    Simple(Arc<SimpleRateLimiter>),
    #[cfg(feature = "rate-limiting")]
    Governor(Arc<GovernorRateLimiterWrapper>),
}

/// Simple counter-based rate limiter
struct SimpleRateLimiter {
    limits: DashMap<String, SessionLimit>,
    max_requests: usize,
    window_secs: u64,
}

struct SessionLimit {
    count: usize,
    reset_at: std::time::SystemTime,
}

#[cfg(feature = "rate-limiting")]
struct GovernorRateLimiterWrapper {
    limiters: DashMap<String, GovernorRateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    quota: Quota,
}

impl RateLimiter {
    /// Create simple rate limiter
    ///
    /// - `max_requests`: Maximum requests allowed
    /// - `window_secs`: Time window in seconds
    pub fn new(max_requests: usize, window_secs: u64) -> Self {
        Self {
            backend: RateLimiterBackend::Simple(Arc::new(SimpleRateLimiter {
                limits: DashMap::new(),
                max_requests,
                window_secs,
            })),
        }
    }

    /// Create governor-based rate limiter (more accurate)
    ///
    /// # Errors
    ///
    /// Returns `Err(OrchestratorError::ConfigError)` if `max_requests` is zero.
    #[cfg(feature = "rate-limiting")]
    pub fn new_governor(max_requests: u32, _window_secs: u64) -> Result<Self, OrchestratorError> {
        let requests = NonZeroU32::new(max_requests)
            .ok_or_else(|| OrchestratorError::ConfigError("max_requests must be > 0".into()))?;
        // saturating_mul(2): since max_requests >= 1, burst_count >= 2 — never zero
        let burst_count = max_requests.saturating_mul(2);
        let burst = NonZeroU32::new(burst_count)
            .ok_or_else(|| OrchestratorError::ConfigError("burst limit overflow".into()))?;
        let quota = Quota::per_second(requests).allow_burst(burst);

        Ok(Self {
            backend: RateLimiterBackend::Governor(Arc::new(GovernorRateLimiterWrapper {
                limiters: DashMap::new(),
                quota,
            })),
        })
    }

    /// Check if request is allowed for session
    ///
    /// Returns `true` if allowed, `false` if rate limit exceeded
    pub async fn check(&self, session_id: &str) -> bool {
        match &self.backend {
            RateLimiterBackend::Simple(limiter) => limiter.check_simple(session_id),
            #[cfg(feature = "rate-limiting")]
            RateLimiterBackend::Governor(limiter) => limiter.check_governor(session_id),
        }
    }

    /// Reset rate limit for session
    pub async fn reset(&self, session_id: &str) {
        match &self.backend {
            RateLimiterBackend::Simple(limiter) => {
                limiter.limits.remove(session_id);
                debug!(session_id = session_id, "rate limit reset");
            }
            #[cfg(feature = "rate-limiting")]
            RateLimiterBackend::Governor(limiter) => {
                limiter.limiters.remove(session_id);
                debug!(session_id = session_id, "rate limit reset");
            }
        }
    }

    /// Get current usage for session
    pub fn get_usage(&self, session_id: &str) -> Option<RateLimitInfo> {
        match &self.backend {
            RateLimiterBackend::Simple(limiter) => {
                limiter.limits.get(session_id).map(|limit| RateLimitInfo {
                    used: limit.count,
                    remaining: limiter.max_requests.saturating_sub(limit.count),
                    reset_in_secs: limit
                        .reset_at
                        .duration_since(std::time::SystemTime::now())
                        .unwrap_or_default()
                        .as_secs(),
                })
            }
            #[cfg(feature = "rate-limiting")]
            RateLimiterBackend::Governor(_) => {
                // Governor doesn't expose usage stats easily
                None
            }
        }
    }
}

impl SimpleRateLimiter {
    fn check_simple(&self, session_id: &str) -> bool {
        let now = std::time::SystemTime::now();

        let mut entry = self
            .limits
            .entry(session_id.to_string())
            .or_insert(SessionLimit {
                count: 0,
                reset_at: now + std::time::Duration::from_secs(self.window_secs),
            });

        // Reset if window expired
        if entry.reset_at <= now {
            entry.count = 0;
            entry.reset_at = now + std::time::Duration::from_secs(self.window_secs);
        }

        // Check limit
        if entry.count >= self.max_requests {
            warn!(
                session_id = session_id,
                count = entry.count,
                limit = self.max_requests,
                "rate limit exceeded"
            );
            return false;
        }

        entry.count += 1;
        debug!(
            session_id = session_id,
            count = entry.count,
            limit = self.max_requests,
            "rate limit check passed"
        );
        true
    }
}

#[cfg(feature = "rate-limiting")]
impl GovernorRateLimiterWrapper {
    fn check_governor(&self, session_id: &str) -> bool {
        let limiter = self
            .limiters
            .entry(session_id.to_string())
            .or_insert_with(|| GovernorRateLimiter::direct(self.quota));

        match limiter.check() {
            Ok(_) => {
                debug!(session_id = session_id, "rate limit check passed");
                true
            }
            Err(_) => {
                warn!(session_id = session_id, "rate limit exceeded");
                false
            }
        }
    }
}

/// Rate limit information for a session
#[derive(Debug)]
pub struct RateLimitInfo {
    /// Number of requests consumed in the current window.
    pub used: usize,
    /// Number of requests still available in the current window.
    pub remaining: usize,
    /// Seconds until the current window resets.
    pub reset_in_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_simple_rate_limiter() {
        let limiter = RateLimiter::new(5, 10);

        // First 5 requests should pass
        for i in 0..5 {
            assert!(
                limiter.check("test-session").await,
                "request {} should pass",
                i
            );
        }

        // 6th request should fail
        assert!(
            !limiter.check("test-session").await,
            "request 6 should fail"
        );

        // Different session should have its own limit
        assert!(limiter.check("other-session").await);
    }

    #[tokio::test]
    async fn test_rate_limit_reset() {
        let limiter = RateLimiter::new(2, 1); // 2 requests per second

        // Use up limit
        assert!(limiter.check("test").await);
        assert!(limiter.check("test").await);
        assert!(!limiter.check("test").await);

        // Wait for window to expire
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Should be allowed again
        assert!(limiter.check("test").await);
    }

    #[tokio::test]
    async fn test_get_usage() {
        let limiter = RateLimiter::new(10, 60);

        limiter.check("test").await;
        limiter.check("test").await;

        let info = limiter.get_usage("test").unwrap();
        assert_eq!(info.used, 2);
        assert_eq!(info.remaining, 8);
    }
}
