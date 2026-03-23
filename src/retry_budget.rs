//! Retry budgeting with exponential backoff tracking.
//!
//! Provides per-key retry budgets with configurable backoff strategies,
//! time-based budget enforcement, and per-attempt history recording.

use dashmap::DashMap;
use std::sync::{
    atomic::{AtomicU32, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// BackoffStrategy
// ---------------------------------------------------------------------------

/// Backoff strategy used to compute the delay before each retry attempt.
#[derive(Debug, Clone)]
pub enum BackoffStrategy {
    /// Wait the same fixed duration before every attempt.
    Fixed(Duration),
    /// Increase the delay linearly: `base + attempt * increment`.
    Linear {
        /// Starting delay.
        base: Duration,
        /// Amount added per attempt.
        increment: Duration,
    },
    /// Exponential backoff: `base * multiplier^attempt`, capped at `max`.
    Exponential {
        /// Initial delay.
        base: Duration,
        /// Maximum delay.
        max: Duration,
        /// Growth multiplier (e.g. `2.0` for doubling).
        multiplier: f64,
    },
    /// Decorrelated jitter: `min(max, random_between(base, prev * 3))`.
    /// Uses a simple LCG seeded with `seed`.
    DecorrelatedJitter {
        /// Minimum delay.
        base: Duration,
        /// Maximum delay.
        max: Duration,
        /// Initial LCG seed.
        seed: u64,
    },
}

// ---------------------------------------------------------------------------
// RetryError
// ---------------------------------------------------------------------------

/// Errors returned when the retry machinery refuses to issue another attempt.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    /// No more attempts remain (either the count or time budget was exceeded).
    #[error("retry budget exhausted after {attempts} attempts")]
    BudgetExhausted {
        /// Number of attempts that were made.
        attempts: u32,
    },
    /// The error is not retryable (e.g. a 4xx client error).
    #[error("non-retryable error: {0}")]
    NonRetryableError(String),
    /// The computed delay would exceed the remaining time budget.
    #[error("next delay would exceed remaining time budget")]
    MaxDelayExceeded,
}

// ---------------------------------------------------------------------------
// AttemptRecord
// ---------------------------------------------------------------------------

/// Record of a single attempt.
#[derive(Debug, Clone)]
pub struct AttemptRecord {
    /// Attempt number (1-based).
    pub attempt_num: u32,
    /// Instant at which the attempt started.
    pub started_at: Instant,
    /// How long the attempt took in milliseconds.
    pub duration_ms: u64,
    /// Whether the attempt succeeded.
    pub success: bool,
    /// Optional error message if the attempt failed.
    pub error_message: Option<String>,
}

// ---------------------------------------------------------------------------
// BudgetReport
// ---------------------------------------------------------------------------

/// Summary report of a [`RetryBudget`]'s current state.
#[derive(Debug, Clone)]
pub struct BudgetReport {
    /// Number of attempts consumed so far.
    pub attempts_used: u32,
    /// Number of attempts still available.
    pub attempts_remaining: u32,
    /// Total milliseconds consumed across all attempts.
    pub time_used_ms: u64,
    /// Fraction of attempts that succeeded.
    pub success_rate: f64,
    /// Average duration per attempt in milliseconds.
    pub avg_attempt_ms: f64,
}

// ---------------------------------------------------------------------------
// RetryBudget
// ---------------------------------------------------------------------------

/// A budget that limits how many times an operation may be retried, and for
/// how long in total.
pub struct RetryBudget {
    max_attempts: u32,
    total_time_budget: Duration,
    strategy: BackoffStrategy,
    current_attempts: AtomicU32,
    total_elapsed_ms: AtomicU64,
    history: Mutex<Vec<AttemptRecord>>,
}

impl RetryBudget {
    /// Create a new `RetryBudget`.
    pub fn new(max_attempts: u32, time_budget: Duration, strategy: BackoffStrategy) -> Self {
        Self {
            max_attempts,
            total_time_budget: time_budget,
            strategy,
            current_attempts: AtomicU32::new(0),
            total_elapsed_ms: AtomicU64::new(0),
            history: Mutex::new(Vec::new()),
        }
    }

    /// Compute the delay before attempt number `attempt` (0-indexed).
    ///
    /// Returns `None` if the budget is exhausted.
    pub fn next_delay(&self, attempt: u32) -> Option<Duration> {
        if !self.can_retry(Duration::from_millis(self.total_elapsed_ms.load(Ordering::Relaxed))) {
            return None;
        }

        let delay = match &self.strategy {
            BackoffStrategy::Fixed(d) => *d,

            BackoffStrategy::Linear { base, increment } => {
                *base + *increment * attempt
            }

            BackoffStrategy::Exponential { base, max, multiplier } => {
                let factor = multiplier.powi(attempt as i32);
                let ms = (base.as_millis() as f64 * factor) as u64;
                let computed = Duration::from_millis(ms);
                computed.min(*max)
            }

            BackoffStrategy::DecorrelatedJitter { base, max, seed } => {
                // LCG: next = (a * prev + c) % m
                let mut state = seed.wrapping_add(attempt as u64 * 6364136223846793005);
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let prev_ms = if attempt == 0 {
                    base.as_millis() as u64
                } else {
                    // approximate prev as base * 3^(attempt-1), capped
                    let prev_factor = 3u64.saturating_pow(attempt - 1);
                    (base.as_millis() as u64).saturating_mul(prev_factor)
                };
                let range = (prev_ms * 3).max(base.as_millis() as u64) - base.as_millis() as u64;
                let jitter_ms = if range == 0 {
                    0
                } else {
                    base.as_millis() as u64 + state % range
                };
                Duration::from_millis(jitter_ms).min(*max)
            }
        };

        Some(delay)
    }

    /// Returns `true` if another attempt is permitted given that `elapsed`
    /// time has already been spent.
    pub fn can_retry(&self, elapsed: Duration) -> bool {
        let attempts = self.current_attempts.load(Ordering::Relaxed);
        attempts < self.max_attempts && elapsed < self.total_time_budget
    }

    /// Record the outcome of an attempt.
    pub fn record_attempt(&self, success: bool, duration_ms: u64, error: Option<String>) {
        let attempt_num = self.current_attempts.fetch_add(1, Ordering::Relaxed) + 1;
        self.total_elapsed_ms
            .fetch_add(duration_ms, Ordering::Relaxed);

        let record = AttemptRecord {
            attempt_num,
            started_at: Instant::now(),
            duration_ms,
            success,
            error_message: error,
        };

        if let Ok(mut guard) = self.history.lock() {
            guard.push(record);
        }
    }

    /// Reset all counters and clear attempt history.
    pub fn reset(&self) {
        self.current_attempts.store(0, Ordering::Relaxed);
        self.total_elapsed_ms.store(0, Ordering::Relaxed);
        if let Ok(mut guard) = self.history.lock() {
            guard.clear();
        }
    }

    /// Return the number of attempts still available.
    pub fn remaining_attempts(&self) -> u32 {
        let used = self.current_attempts.load(Ordering::Relaxed);
        self.max_attempts.saturating_sub(used)
    }

    /// Return the remaining time budget given that `elapsed` has already been
    /// consumed.  Returns `None` if the budget is already exhausted.
    pub fn remaining_time(&self, elapsed: Duration) -> Option<Duration> {
        if elapsed >= self.total_time_budget {
            None
        } else {
            Some(self.total_time_budget - elapsed)
        }
    }

    /// Produce a summary report of the current budget state.
    pub fn budget_report(&self) -> BudgetReport {
        let attempts_used = self.current_attempts.load(Ordering::Relaxed);
        let attempts_remaining = self.max_attempts.saturating_sub(attempts_used);
        let time_used_ms = self.total_elapsed_ms.load(Ordering::Relaxed);

        let (success_rate, avg_attempt_ms) = if let Ok(guard) = self.history.lock() {
            if guard.is_empty() {
                (0.0, 0.0)
            } else {
                let successes = guard.iter().filter(|r| r.success).count() as f64;
                let total = guard.len() as f64;
                let avg = guard.iter().map(|r| r.duration_ms as f64).sum::<f64>() / total;
                (successes / total, avg)
            }
        } else {
            (0.0, 0.0)
        };

        BudgetReport {
            attempts_used,
            attempts_remaining,
            time_used_ms,
            success_rate,
            avg_attempt_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// RetryPolicy
// ---------------------------------------------------------------------------

/// A collection of named [`RetryBudget`]s, keyed by an arbitrary string
/// (e.g. provider name, endpoint, or request type).
pub struct RetryPolicy {
    budgets: DashMap<String, Arc<RetryBudget>>,
}

impl RetryPolicy {
    /// Create a new, empty `RetryPolicy`.
    pub fn new() -> Self {
        Self {
            budgets: DashMap::new(),
        }
    }

    /// Retrieve the budget for `key`, creating it with `config` if it does
    /// not yet exist.
    ///
    /// `config` is `(max_attempts, time_budget, strategy)`.
    pub fn get_or_create(
        &self,
        key: &str,
        config: (u32, Duration, BackoffStrategy),
    ) -> Arc<RetryBudget> {
        if let Some(budget) = self.budgets.get(key) {
            return Arc::clone(&*budget);
        }

        let (max_attempts, time_budget, strategy) = config;
        let budget = Arc::new(RetryBudget::new(max_attempts, time_budget, strategy));
        self.budgets.insert(key.to_string(), Arc::clone(&budget));
        budget
    }

    /// Return a summary report for every tracked budget.
    pub fn policy_summary(&self) -> Vec<(String, BudgetReport)> {
        self.budgets
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().budget_report()))
            .collect()
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixed_backoff() {
        let budget = RetryBudget::new(
            3,
            Duration::from_secs(60),
            BackoffStrategy::Fixed(Duration::from_millis(100)),
        );
        assert_eq!(budget.next_delay(0), Some(Duration::from_millis(100)));
        assert_eq!(budget.next_delay(2), Some(Duration::from_millis(100)));
    }

    #[test]
    fn test_exponential_backoff() {
        let budget = RetryBudget::new(
            5,
            Duration::from_secs(60),
            BackoffStrategy::Exponential {
                base: Duration::from_millis(100),
                max: Duration::from_secs(10),
                multiplier: 2.0,
            },
        );
        let d0 = budget.next_delay(0).expect("should have delay");
        let d1 = budget.next_delay(1).expect("should have delay");
        assert!(d1 > d0);
    }

    #[test]
    fn test_budget_exhausted() {
        let budget = RetryBudget::new(
            2,
            Duration::from_secs(60),
            BackoffStrategy::Fixed(Duration::from_millis(10)),
        );
        budget.record_attempt(false, 10, Some("err".to_string()));
        budget.record_attempt(false, 10, Some("err".to_string()));
        assert!(!budget.can_retry(Duration::from_millis(20)));
        assert_eq!(budget.next_delay(2), None);
    }

    #[test]
    fn test_reset() {
        let budget = RetryBudget::new(
            3,
            Duration::from_secs(60),
            BackoffStrategy::Fixed(Duration::from_millis(50)),
        );
        budget.record_attempt(true, 50, None);
        assert_eq!(budget.remaining_attempts(), 2);
        budget.reset();
        assert_eq!(budget.remaining_attempts(), 3);
    }

    #[test]
    fn test_policy_get_or_create() {
        let policy = RetryPolicy::new();
        let b1 = policy.get_or_create(
            "api",
            (
                3,
                Duration::from_secs(30),
                BackoffStrategy::Fixed(Duration::from_millis(100)),
            ),
        );
        let b2 = policy.get_or_create(
            "api",
            (
                5,
                Duration::from_secs(60),
                BackoffStrategy::Fixed(Duration::from_millis(200)),
            ),
        );
        // Should return the same budget
        assert_eq!(Arc::ptr_eq(&b1, &b2), true);
    }

    #[test]
    fn test_budget_report() {
        let budget = RetryBudget::new(
            5,
            Duration::from_secs(60),
            BackoffStrategy::Fixed(Duration::from_millis(100)),
        );
        budget.record_attempt(true, 50, None);
        budget.record_attempt(false, 80, Some("timeout".to_string()));
        let report = budget.budget_report();
        assert_eq!(report.attempts_used, 2);
        assert_eq!(report.attempts_remaining, 3);
        assert!((report.success_rate - 0.5).abs() < 1e-9);
    }
}
