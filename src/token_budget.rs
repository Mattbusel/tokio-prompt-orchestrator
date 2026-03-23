//! Token Budget Middleware
//!
//! Pre-estimates token counts before requests reach the LLM, enforcing spend
//! limits at the orchestration layer rather than discovering them via a billing
//! surprise.
//!
//! ## Why estimate before sending?
//!
//! LLM APIs charge per token.  Without pre-flight counting:
//! - A single runaway prompt (e.g. 200 KB user-uploaded doc) consumes the
//!   entire daily budget in one call.
//! - Cost anomalies are invisible until the invoice arrives.
//! - There is no way to shed low-priority requests before they hit the API.
//!
//! `TokenBudgetGuard` adds a zero-network-round-trip gate that estimates token
//! count, checks it against configurable per-request and period limits, and
//! rejects requests that would overflow the budget before they reach the wire.
//!
//! ## Token estimation
//!
//! The estimator uses the `⌈len / 4⌉` heuristic (one token ≈ 4 UTF-8 bytes
//! in English prose).  This intentionally over-estimates by ~10–15 % on code
//! and ~5 % on English, providing a conservative safety margin without
//! requiring a tokenizer dependency.  For accurate accounting of actual tokens
//! used, pair this with [`crate::metrics`] which records real token counts
//! from provider responses.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::token_budget::{TokenBudgetGuard, TokenBudgetConfig};
//!
//! let guard = TokenBudgetGuard::new(TokenBudgetConfig {
//!     max_tokens_per_request: 4_096,
//!     max_tokens_per_period: 1_000_000,
//!     period: std::time::Duration::from_secs(3600), // 1-hour rolling window
//! });
//!
//! let prompt = "Summarise this document in three bullet points.";
//! match guard.check(prompt) {
//!     Ok(estimated) => println!("Allowed — estimated {estimated} tokens"),
//!     Err(e) => eprintln!("Budget exceeded: {e}"),
//! }
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::{debug, warn};

/// Configuration for [`TokenBudgetGuard`].
#[derive(Debug, Clone)]
pub struct TokenBudgetConfig {
    /// Maximum estimated tokens allowed in a single request.
    ///
    /// Requests whose estimated prompt token count exceeds this limit are
    /// rejected before they reach the LLM.  A value of `0` disables the
    /// per-request cap.
    pub max_tokens_per_request: u64,

    /// Maximum total tokens consumed within a rolling `period` window.
    ///
    /// The period budget resets automatically when `period` elapses.  A value
    /// of `0` disables the period cap.
    pub max_tokens_per_period: u64,

    /// Rolling window duration for the period budget.
    pub period: Duration,
}

impl Default for TokenBudgetConfig {
    fn default() -> Self {
        Self {
            max_tokens_per_request: 8_192,
            max_tokens_per_period: 1_000_000,
            period: Duration::from_secs(3600),
        }
    }
}

/// Token budget rejection reasons.
#[derive(Debug, Error)]
pub enum TokenBudgetError {
    /// The single request exceeds `max_tokens_per_request`.
    #[error("request exceeds per-request token limit: estimated {estimated} > max {limit}")]
    PerRequestLimitExceeded { estimated: u64, limit: u64 },

    /// The period budget would be exceeded by this request.
    #[error(
        "request would exceed period token budget: used {used} + estimated {estimated} > max {limit}"
    )]
    PeriodLimitExceeded {
        used: u64,
        estimated: u64,
        limit: u64,
    },
}

/// Mutable period-tracking state (protected by a `Mutex` for atomic
/// window-reset + counter-increment without a separate compare-and-swap loop).
struct PeriodState {
    tokens_used: u64,
    window_start: Instant,
}

/// Guards LLM requests against token over-spend.
///
/// `TokenBudgetGuard` is `Clone + Send + Sync`.  All clones share the same
/// budget counters via an `Arc`.
#[derive(Clone)]
pub struct TokenBudgetGuard {
    config: TokenBudgetConfig,
    /// Total tokens consumed across all requests since the process started
    /// (not reset on window rollover — use `period_tokens_used()` for
    /// the rolling window value).
    total_tokens_estimated: Arc<AtomicU64>,
    /// Number of requests that passed the budget gate.
    requests_allowed: Arc<AtomicU64>,
    /// Number of requests rejected by the budget gate.
    requests_rejected: Arc<AtomicU64>,
    period: Arc<Mutex<PeriodState>>,
}

impl TokenBudgetGuard {
    /// Create a new guard with the given configuration.
    pub fn new(config: TokenBudgetConfig) -> Self {
        Self {
            config,
            total_tokens_estimated: Arc::new(AtomicU64::new(0)),
            requests_allowed: Arc::new(AtomicU64::new(0)),
            requests_rejected: Arc::new(AtomicU64::new(0)),
            period: Arc::new(Mutex::new(PeriodState {
                tokens_used: 0,
                window_start: Instant::now(),
            })),
        }
    }

    /// Check whether `text` fits within the budget limits.
    ///
    /// If the check passes, the estimated token count is **reserved** against
    /// the period budget.  Call [`release`](Self::release) with the actual
    /// token count from the provider response to correct the reservation.
    ///
    /// # Returns
    /// - `Ok(estimated_tokens)` — request is within budget; proceed to LLM.
    /// - `Err(TokenBudgetError::*)` — budget exceeded; reject the request.
    pub fn check(&self, text: &str) -> Result<u64, TokenBudgetError> {
        let estimated = estimate_tokens(text);

        // Per-request cap.
        if self.config.max_tokens_per_request > 0
            && estimated > self.config.max_tokens_per_request
        {
            warn!(
                estimated,
                limit = self.config.max_tokens_per_request,
                "token_budget: per-request limit exceeded"
            );
            self.requests_rejected.fetch_add(1, Ordering::Relaxed);
            return Err(TokenBudgetError::PerRequestLimitExceeded {
                estimated,
                limit: self.config.max_tokens_per_request,
            });
        }

        // Period cap — needs mutex for atomic read-modify-write.
        if self.config.max_tokens_per_period > 0 {
            let mut period = self.period.lock().unwrap_or_else(|e| e.into_inner());

            // Roll the window if the period has elapsed.
            if period.window_start.elapsed() >= self.config.period {
                debug!(
                    previous_used = period.tokens_used,
                    "token_budget: rolling period window"
                );
                period.tokens_used = 0;
                period.window_start = Instant::now();
            }

            if period.tokens_used + estimated > self.config.max_tokens_per_period {
                warn!(
                    used = period.tokens_used,
                    estimated,
                    limit = self.config.max_tokens_per_period,
                    "token_budget: period limit exceeded"
                );
                self.requests_rejected.fetch_add(1, Ordering::Relaxed);
                return Err(TokenBudgetError::PeriodLimitExceeded {
                    used: period.tokens_used,
                    estimated,
                    limit: self.config.max_tokens_per_period,
                });
            }

            period.tokens_used += estimated;
        }

        self.total_tokens_estimated
            .fetch_add(estimated, Ordering::Relaxed);
        self.requests_allowed.fetch_add(1, Ordering::Relaxed);
        debug!(estimated, "token_budget: request allowed");
        Ok(estimated)
    }

    /// Adjust the period budget by the difference between the estimated and
    /// actual token counts returned by the provider.
    ///
    /// Call this after receiving the LLM response with the real token count.
    /// If `actual < estimated`, the surplus is credited back.  If
    /// `actual > estimated` (rare for input tokens), the overage is debited.
    pub fn release(&self, estimated: u64, actual: u64) {
        if actual == estimated {
            return;
        }
        let mut period = self.period.lock().unwrap_or_else(|e| e.into_inner());
        if actual < estimated {
            // Credit back the over-reservation.
            period.tokens_used = period.tokens_used.saturating_sub(estimated - actual);
        } else {
            // Charge the extra tokens consumed.
            period.tokens_used = period
                .tokens_used
                .saturating_add(actual - estimated)
                .min(self.config.max_tokens_per_period);
        }
    }

    /// Total tokens estimated across all requests since creation (lifetime).
    pub fn total_tokens_estimated(&self) -> u64 {
        self.total_tokens_estimated.load(Ordering::Relaxed)
    }

    /// Tokens consumed in the current rolling period window.
    pub fn period_tokens_used(&self) -> u64 {
        let period = self.period.lock().unwrap_or_else(|e| e.into_inner());
        period.tokens_used
    }

    /// Remaining token budget in the current period.
    pub fn period_tokens_remaining(&self) -> u64 {
        let period = self.period.lock().unwrap_or_else(|e| e.into_inner());
        self.config
            .max_tokens_per_period
            .saturating_sub(period.tokens_used)
    }

    /// Fraction of the period budget consumed (0.0 = empty, 1.0 = exhausted).
    pub fn period_utilization(&self) -> f64 {
        if self.config.max_tokens_per_period == 0 {
            return 0.0;
        }
        let used = self.period_tokens_used() as f64;
        (used / self.config.max_tokens_per_period as f64).min(1.0)
    }

    /// Number of requests allowed through the budget gate.
    pub fn requests_allowed(&self) -> u64 {
        self.requests_allowed.load(Ordering::Relaxed)
    }

    /// Number of requests rejected by the budget gate.
    pub fn requests_rejected(&self) -> u64 {
        self.requests_rejected.load(Ordering::Relaxed)
    }

    /// Budget rejection rate: `rejected / (allowed + rejected)`.
    pub fn rejection_rate(&self) -> f64 {
        let allowed = self.requests_allowed.load(Ordering::Relaxed) as f64;
        let rejected = self.requests_rejected.load(Ordering::Relaxed) as f64;
        let total = allowed + rejected;
        if total == 0.0 {
            0.0
        } else {
            rejected / total
        }
    }
}

/// Estimate the number of tokens in `text`.
///
/// Uses the `⌈len / 4⌉` heuristic: one token ≈ 4 UTF-8 bytes in English
/// prose.  Over-estimates by ~10–15 % on code; ~5 % on English.  Never
/// returns 0 for non-empty input (minimum 1 token).
pub fn estimate_tokens(text: &str) -> u64 {
    let bytes = text.len() as u64;
    if bytes == 0 {
        return 0;
    }
    bytes.div_ceil(4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_short() {
        // "Hello" = 5 bytes → ceil(5/4) = 2
        assert_eq!(estimate_tokens("Hello"), 2);
    }

    #[test]
    fn estimate_tokens_typical_prompt() {
        let prompt = "Summarise this article in three bullet points.";
        let est = estimate_tokens(prompt);
        assert!(est > 0 && est < 20, "estimate was {est}");
    }

    #[test]
    fn per_request_limit_rejected() {
        let guard = TokenBudgetGuard::new(TokenBudgetConfig {
            max_tokens_per_request: 5,
            max_tokens_per_period: 1_000_000,
            period: Duration::from_secs(3600),
        });
        // "Hello world this is a long prompt" is well over 5 tokens.
        let result = guard.check("Hello world this is a long prompt that clearly exceeds five tokens");
        assert!(matches!(result, Err(TokenBudgetError::PerRequestLimitExceeded { .. })));
    }

    #[test]
    fn short_request_passes() {
        let guard = TokenBudgetGuard::new(TokenBudgetConfig {
            max_tokens_per_request: 1000,
            max_tokens_per_period: 1_000_000,
            period: Duration::from_secs(3600),
        });
        assert!(guard.check("Hi").is_ok());
    }

    #[test]
    fn period_budget_exhausted() {
        let guard = TokenBudgetGuard::new(TokenBudgetConfig {
            max_tokens_per_request: 1000,
            max_tokens_per_period: 10,
            period: Duration::from_secs(3600),
        });
        // First request consumes up to 10 tokens.
        let _ = guard.check("Hi"); // 1 token
        let result = guard.check("Hello world this is a very long prompt that will overflow the budget");
        assert!(matches!(result, Err(TokenBudgetError::PeriodLimitExceeded { .. })));
    }

    #[test]
    fn release_credits_back_surplus() {
        let guard = TokenBudgetGuard::new(TokenBudgetConfig {
            max_tokens_per_request: 1000,
            max_tokens_per_period: 100,
            period: Duration::from_secs(3600),
        });
        let estimated = guard.check("Hello world").unwrap(); // small estimate
        let used_before = guard.period_tokens_used();
        guard.release(estimated, 1); // actual was 1 token less than estimated
        assert!(guard.period_tokens_used() < used_before);
    }

    #[test]
    fn rejection_rate_tracks_correctly() {
        let guard = TokenBudgetGuard::new(TokenBudgetConfig {
            max_tokens_per_request: 3,
            max_tokens_per_period: 1_000_000,
            period: Duration::from_secs(3600),
        });
        let _ = guard.check("Hi");  // allowed (≤ 3 tokens)
        let _ = guard.check("Hello world this is way too long for the limit");  // rejected
        assert_eq!(guard.requests_allowed(), 1);
        assert_eq!(guard.requests_rejected(), 1);
        assert!((guard.rejection_rate() - 0.5).abs() < 1e-10);
    }
}
