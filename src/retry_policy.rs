//! # Retry Policy
//!
//! Configurable retry logic with multiple back-off strategies, jitter modes,
//! and async execution support.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::retry_policy::{
//!     JitterMode, RetryPolicy, RetryStrategy, retry_async,
//! };
//!
//! # tokio_test::block_on(async {
//! let policy = RetryPolicy {
//!     strategy: RetryStrategy::ExponentialBackoff {
//!         initial_ms: 10,
//!         multiplier: 2.0,
//!         max_ms: 1000,
//!     },
//!     jitter: JitterMode::Full,
//!     max_attempts: 3,
//!     retryable_errors: vec![],
//! };
//!
//! let mut call_count = 0u32;
//! let result: Result<(), String> = retry_async(&policy, || async {
//!     call_count += 1;
//!     if call_count < 3 { Err("transient".to_string()) } else { Ok(()) }
//! }).await;
//! assert!(result.is_ok());
//! # });
//! ```

use std::future::Future;

// ── RetryStrategy ─────────────────────────────────────────────────────────────

/// The back-off strategy that determines how long to wait between retry
/// attempts.
#[derive(Debug, Clone, PartialEq)]
pub enum RetryStrategy {
    /// Do not retry — fail immediately after the first attempt.
    NoRetry,
    /// Wait a constant `delay_ms` milliseconds between every attempt.
    FixedDelay {
        /// Constant delay in milliseconds.
        delay_ms: u64,
    },
    /// Multiply the previous delay by `multiplier` each attempt, capped at
    /// `max_ms`.
    ExponentialBackoff {
        /// Delay before the first retry in milliseconds.
        initial_ms: u64,
        /// Multiplicative factor applied after each attempt.
        multiplier: f64,
        /// Upper bound on the delay in milliseconds.
        max_ms: u64,
    },
    /// Increase the delay by `increment_ms` each attempt, capped at `max_ms`.
    LinearBackoff {
        /// Delay before the first retry in milliseconds.
        initial_ms: u64,
        /// Amount added to the previous delay each attempt.
        increment_ms: u64,
        /// Upper bound on the delay in milliseconds.
        max_ms: u64,
    },
    /// Use the Fibonacci sequence as delays (F(1)=initial, F(2)=initial, …),
    /// capped at `max_ms`.
    Fibonacci {
        /// Seed value for the first two terms of the sequence.
        initial_ms: u64,
        /// Upper bound on the delay in milliseconds.
        max_ms: u64,
    },
}

// ── JitterMode ────────────────────────────────────────────────────────────────

/// Jitter strategy to reduce correlated retry storms.
#[derive(Debug, Clone, PartialEq)]
pub enum JitterMode {
    /// No jitter — use the computed delay as-is.
    None,
    /// Full jitter: `random in [0, delay]`.
    Full,
    /// Equal jitter: `delay/2 + random in [0, delay/2]`.
    Equal,
    /// Decorrelated jitter: `random in [initial, 3 * prev_delay]`.
    Decorrelated,
}

// ── RetryPolicy ───────────────────────────────────────────────────────────────

/// A complete retry configuration.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Delay back-off strategy.
    pub strategy: RetryStrategy,
    /// Jitter mode applied to the computed delay.
    pub jitter: JitterMode,
    /// Maximum total number of attempts (including the first).
    pub max_attempts: u32,
    /// Error message substrings that are considered retryable.  An empty list
    /// means all errors are retryable.
    pub retryable_errors: Vec<String>,
}

impl RetryPolicy {
    /// Returns `true` if the given error message is eligible for a retry.
    ///
    /// If `retryable_errors` is empty every error is considered retryable.
    pub fn is_retryable(&self, error: &str) -> bool {
        if self.retryable_errors.is_empty() {
            return true;
        }
        self.retryable_errors.iter().any(|pat| error.contains(pat.as_str()))
    }
}

// ── LCG RNG ───────────────────────────────────────────────────────────────────

/// Minimal linear-congruential generator for deterministic, allocation-free
/// jitter without pulling in `rand`.
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        // Knuth's MMIX constants.
        Self {
            state: seed.wrapping_mul(6_364_136_223_846_793_005)
                       .wrapping_add(1_442_695_040_888_963_407),
        }
    }

    /// Returns a pseudo-random value in `[0, max]`.
    fn next_bounded(&mut self, max: u64) -> u64 {
        self.state = self.state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        if max == 0 {
            return 0;
        }
        self.state % (max + 1)
    }
}

// ── RetryState ────────────────────────────────────────────────────────────────

/// Mutable state tracked across retry attempts.
#[derive(Debug, Clone)]
pub struct RetryState {
    /// The attempt number that was most recently executed (1-based).
    pub attempt: u32,
    /// The delay that was used before the most recent attempt (0 for the first
    /// attempt).
    pub last_delay_ms: u64,
    /// Cumulative delay accumulated across all waits.
    pub total_delay_ms: u64,
    /// Previous-previous delay used by the Fibonacci strategy.
    fib_prev: u64,
    /// Previous delay used by the Fibonacci strategy.
    fib_curr: u64,
}

impl RetryState {
    /// Creates a fresh [`RetryState`] ready for the first attempt.
    pub fn new() -> Self {
        Self {
            attempt: 0,
            last_delay_ms: 0,
            total_delay_ms: 0,
            fib_prev: 0,
            fib_curr: 0,
        }
    }

    /// Advances the state and returns the delay in milliseconds that should
    /// be waited before the next attempt, or `None` if `max_attempts` has been
    /// reached.
    pub fn next_delay(&mut self, policy: &RetryPolicy) -> Option<u64> {
        self.attempt += 1;

        if self.attempt > policy.max_attempts {
            return None;
        }

        // First attempt: no delay.
        if self.attempt == 1 {
            return Some(0);
        }

        // Compute base delay from strategy.
        let retry_num = self.attempt - 1; // 1 on first retry
        let base_delay = match &policy.strategy {
            RetryStrategy::NoRetry => return None,

            RetryStrategy::FixedDelay { delay_ms } => *delay_ms,

            RetryStrategy::ExponentialBackoff {
                initial_ms,
                multiplier,
                max_ms,
            } => {
                let exp = (*multiplier).powi((retry_num - 1) as i32);
                let d = (*initial_ms as f64 * exp) as u64;
                d.min(*max_ms)
            }

            RetryStrategy::LinearBackoff {
                initial_ms,
                increment_ms,
                max_ms,
            } => {
                let d = initial_ms.saturating_add(increment_ms.saturating_mul((retry_num - 1) as u64));
                d.min(*max_ms)
            }

            RetryStrategy::Fibonacci { initial_ms, max_ms } => {
                if retry_num == 1 {
                    // Bootstrap: F(1) = initial, F(2) = initial
                    self.fib_prev = 0;
                    self.fib_curr = *initial_ms;
                    self.fib_curr.min(*max_ms)
                } else {
                    let next = self.fib_prev.saturating_add(self.fib_curr);
                    self.fib_prev = self.fib_curr;
                    self.fib_curr = next;
                    self.fib_curr.min(*max_ms)
                }
            }
        };

        // Apply jitter.
        let mut rng = Lcg::new(self.attempt as u64 ^ base_delay);
        let initial_ms = match &policy.strategy {
            RetryStrategy::ExponentialBackoff { initial_ms, .. } => *initial_ms,
            RetryStrategy::LinearBackoff { initial_ms, .. } => *initial_ms,
            RetryStrategy::Fibonacci { initial_ms, .. } => *initial_ms,
            RetryStrategy::FixedDelay { delay_ms } => *delay_ms,
            RetryStrategy::NoRetry => 0,
        };

        let jittered = match &policy.jitter {
            JitterMode::None => base_delay,

            JitterMode::Full => {
                if base_delay == 0 { 0 } else { rng.next_bounded(base_delay) }
            }

            JitterMode::Equal => {
                let half = base_delay / 2;
                let jitter_part = if half == 0 { 0 } else { rng.next_bounded(half) };
                half + jitter_part
            }

            JitterMode::Decorrelated => {
                let prev = self.last_delay_ms.max(initial_ms);
                let upper = (prev * 3).max(initial_ms);
                let lower = initial_ms.min(upper);
                lower + rng.next_bounded(upper.saturating_sub(lower))
            }
        };

        self.last_delay_ms = jittered;
        self.total_delay_ms = self.total_delay_ms.saturating_add(jittered);
        Some(jittered)
    }
}

impl Default for RetryState {
    fn default() -> Self {
        Self::new()
    }
}

// ── RetryMetrics ──────────────────────────────────────────────────────────────

/// Summary of a completed retry sequence.
#[derive(Debug, Clone, Default)]
pub struct RetryMetrics {
    /// Total number of calls made (including successes and failures).
    pub total_attempts: u32,
    /// The attempt number on which the call succeeded, or `None` if all failed.
    pub successful_on_attempt: Option<u32>,
    /// Total milliseconds slept across all waits.
    pub total_delay_ms: u64,
}

// ── retry_async ───────────────────────────────────────────────────────────────

/// Executes `f` up to `policy.max_attempts` times, sleeping between attempts
/// according to the policy, and returns the first `Ok` result or the last
/// error.
///
/// # Type Parameters
///
/// - `F`: An async closure/function that produces a `Future`.
/// - `Fut`: The `Future` type returned by `f`.
/// - `T`: The success type.
/// - `E`: The error type; must implement [`std::fmt::Display`] for retryability
///   checks.
pub async fn retry_async<F, Fut, T, E>(policy: &RetryPolicy, f: F) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut state = RetryState::new();
    let mut last_err: Option<E> = None;

    loop {
        let delay = match state.next_delay(policy) {
            Some(d) => d,
            None => {
                // Max attempts exhausted — return the last error.
                break;
            }
        };

        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }

        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let msg = e.to_string();
                if !policy.is_retryable(&msg) {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }

    // Safety: if max_attempts >= 1 we always set last_err before breaking.
    // If somehow we have 0 max_attempts, we never enter the loop.
    // The unreachable path is guarded by the Option contract.
    Err(last_err.expect("retry_async: max_attempts must be >= 1"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn no_retry_policy() -> RetryPolicy {
        RetryPolicy {
            strategy: RetryStrategy::NoRetry,
            jitter: JitterMode::None,
            max_attempts: 1,
            retryable_errors: vec![],
        }
    }

    fn fixed_policy(delay_ms: u64, max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            strategy: RetryStrategy::FixedDelay { delay_ms },
            jitter: JitterMode::None,
            max_attempts,
            retryable_errors: vec![],
        }
    }

    #[test]
    fn no_retry_gives_one_attempt() {
        let policy = no_retry_policy();
        let mut state = RetryState::new();

        // First attempt: delay = 0 (the initial call).
        assert_eq!(state.next_delay(&policy), Some(0));
        // Second attempt: max_attempts=1 already exhausted.
        assert_eq!(state.next_delay(&policy), None);
    }

    #[test]
    fn fixed_delay_sequence() {
        let policy = fixed_policy(100, 4);
        let mut state = RetryState::new();

        assert_eq!(state.next_delay(&policy), Some(0));   // attempt 1: no pre-wait
        assert_eq!(state.next_delay(&policy), Some(100)); // attempt 2
        assert_eq!(state.next_delay(&policy), Some(100)); // attempt 3
        assert_eq!(state.next_delay(&policy), Some(100)); // attempt 4
        assert_eq!(state.next_delay(&policy), None);       // exhausted
    }

    #[test]
    fn exponential_delay_sequence() {
        let policy = RetryPolicy {
            strategy: RetryStrategy::ExponentialBackoff {
                initial_ms: 100,
                multiplier: 2.0,
                max_ms: 10_000,
            },
            jitter: JitterMode::None,
            max_attempts: 4,
            retryable_errors: vec![],
        };
        let mut state = RetryState::new();

        assert_eq!(state.next_delay(&policy), Some(0));    // attempt 1
        assert_eq!(state.next_delay(&policy), Some(100));  // attempt 2: 100 * 2^0
        assert_eq!(state.next_delay(&policy), Some(200));  // attempt 3: 100 * 2^1
        assert_eq!(state.next_delay(&policy), Some(400));  // attempt 4: 100 * 2^2
        assert_eq!(state.next_delay(&policy), None);
    }

    #[test]
    fn exponential_capped_at_max() {
        let policy = RetryPolicy {
            strategy: RetryStrategy::ExponentialBackoff {
                initial_ms: 500,
                multiplier: 10.0,
                max_ms: 1_000,
            },
            jitter: JitterMode::None,
            max_attempts: 5,
            retryable_errors: vec![],
        };
        let mut state = RetryState::new();
        state.next_delay(&policy); // attempt 1

        let d2 = state.next_delay(&policy).unwrap_or(0); // 500
        let d3 = state.next_delay(&policy).unwrap_or(0); // 5000 -> capped 1000
        assert!(d2 <= 1_000);
        assert!(d3 <= 1_000);
    }

    #[test]
    fn fibonacci_sequence_correct() {
        let policy = RetryPolicy {
            strategy: RetryStrategy::Fibonacci {
                initial_ms: 100,
                max_ms: 10_000,
            },
            jitter: JitterMode::None,
            max_attempts: 6,
            retryable_errors: vec![],
        };
        let mut state = RetryState::new();

        assert_eq!(state.next_delay(&policy), Some(0));   // attempt 1
        assert_eq!(state.next_delay(&policy), Some(100)); // F(1) = 100
        assert_eq!(state.next_delay(&policy), Some(100)); // F(2) = 100
        assert_eq!(state.next_delay(&policy), Some(200)); // F(3) = 200
        assert_eq!(state.next_delay(&policy), Some(300)); // F(4) = 300
        assert_eq!(state.next_delay(&policy), Some(500)); // F(5) = 500
        assert_eq!(state.next_delay(&policy), None);
    }

    #[test]
    fn max_attempts_respected() {
        let policy = fixed_policy(50, 3);
        let mut state = RetryState::new();
        let mut count = 0;
        while state.next_delay(&policy).is_some() {
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn successful_on_third_attempt() {
        let policy = RetryPolicy {
            strategy: RetryStrategy::FixedDelay { delay_ms: 0 },
            jitter: JitterMode::None,
            max_attempts: 5,
            retryable_errors: vec![],
        };

        let call_count = Arc::new(Mutex::new(0u32));
        let cc = call_count.clone();

        let result: Result<u32, String> = retry_async(&policy, || {
            let cc = cc.clone();
            async move {
                let mut n = cc.lock().unwrap_or_else(|e| e.into_inner());
                *n += 1;
                let attempt = *n;
                drop(n);
                if attempt < 3 {
                    Err("transient error".to_string())
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 3);
        assert_eq!(*call_count.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn all_attempts_fail_returns_last_error() {
        let policy = RetryPolicy {
            strategy: RetryStrategy::FixedDelay { delay_ms: 0 },
            jitter: JitterMode::None,
            max_attempts: 3,
            retryable_errors: vec![],
        };

        let result: Result<(), String> = retry_async(&policy, || async {
            Err("permanent failure".to_string())
        })
        .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "permanent failure");
    }

    #[tokio::test]
    async fn non_retryable_error_stops_immediately() {
        let policy = RetryPolicy {
            strategy: RetryStrategy::FixedDelay { delay_ms: 0 },
            jitter: JitterMode::None,
            max_attempts: 10,
            retryable_errors: vec!["transient".to_string()],
        };

        let call_count = Arc::new(Mutex::new(0u32));
        let cc = call_count.clone();

        let result: Result<(), String> = retry_async(&policy, || {
            let cc = cc.clone();
            async move {
                *cc.lock().unwrap() += 1;
                Err("fatal: disk full".to_string())
            }
        })
        .await;

        assert!(result.is_err());
        // Should stop after first attempt because error doesn't match "transient".
        assert_eq!(*call_count.lock().unwrap(), 1);
    }

    #[test]
    fn linear_backoff_sequence() {
        let policy = RetryPolicy {
            strategy: RetryStrategy::LinearBackoff {
                initial_ms: 100,
                increment_ms: 50,
                max_ms: 10_000,
            },
            jitter: JitterMode::None,
            max_attempts: 5,
            retryable_errors: vec![],
        };
        let mut state = RetryState::new();

        assert_eq!(state.next_delay(&policy), Some(0));   // attempt 1
        assert_eq!(state.next_delay(&policy), Some(100)); // 100 + 50*0
        assert_eq!(state.next_delay(&policy), Some(150)); // 100 + 50*1
        assert_eq!(state.next_delay(&policy), Some(200)); // 100 + 50*2
        assert_eq!(state.next_delay(&policy), Some(250)); // 100 + 50*3
        assert_eq!(state.next_delay(&policy), None);
    }

    #[test]
    fn full_jitter_within_bounds() {
        let policy = RetryPolicy {
            strategy: RetryStrategy::FixedDelay { delay_ms: 1000 },
            jitter: JitterMode::Full,
            max_attempts: 10,
            retryable_errors: vec![],
        };
        let mut state = RetryState::new();
        state.next_delay(&policy); // skip attempt 1

        for _ in 0..9 {
            if let Some(d) = state.next_delay(&policy) {
                assert!(d <= 1000, "full jitter must be <= base delay");
            }
        }
    }
}
