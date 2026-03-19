//! Retry Logic
//!
//! Automatic retry with exponential backoff for transient failures.
//!
//! ## Usage
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_prompt_orchestrator::enhanced::RetryPolicy;
//! # #[tokio::main]
//! # async fn main() {
//! let policy = RetryPolicy::exponential(3, Duration::from_millis(100));
//!
//! let result = policy.retry(|| async {
//!     // Your fallible operation — returns Ok on success, Err on transient failure
//!     Ok::<String, std::io::Error>("inference result".to_string())
//! }).await;
//!
//! match result {
//!     Ok(output) => println!("{output}"),
//!     Err(e) => eprintln!("All retries exhausted: {e}"),
//! }
//! # }
//! ```

use crate::OrchestratorError;
use std::time::Duration;
use tracing::{debug, warn};

/// Configuration for automatic retry behaviour with backoff.
///
/// Construct via the factory helpers [`RetryPolicy::fixed`],
/// [`RetryPolicy::exponential`], or [`RetryPolicy::linear`], then execute an
/// async closure with [`RetryPolicy::retry`].
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::RetryPolicy;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), String> {
/// let policy = RetryPolicy::exponential(3, Duration::from_millis(100));
///
/// let result: Result<String, String> = policy.retry(|| async {
///     // replace with a real async call
///     Err("transient".to_string())
/// }).await;
///
/// // All 3 attempts failed
/// assert!(result.is_err());
/// # Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Maximum number of attempts (including the first try).
    pub max_attempts: usize,
    /// Backoff strategy applied between consecutive attempts.
    pub strategy: RetryStrategy,
}

/// Backoff algorithm applied between successive [`RetryPolicy`] attempts.
///
/// Select the strategy that best matches your workload:
///
/// - [`RetryStrategy::Fixed`] — constant pause; good for idempotent operations
///   where timing is well understood.
/// - [`RetryStrategy::Exponential`] — classic exponential back-off; good for
///   transient network errors where you want to back off quickly under load.
/// - [`RetryStrategy::Linear`] — linear growth; a middle ground useful when a
///   fixed number of timed steps is preferable to unbounded growth.
#[derive(Clone, Debug)]
pub enum RetryStrategy {
    /// Fixed delay between retries
    Fixed(Duration),
    /// Exponential backoff (delay doubles each time)
    Exponential {
        /// Delay applied before the second attempt.
        initial_delay: Duration,
        /// Upper bound; the delay will never grow beyond this value.
        max_delay: Duration,
        /// Multiplier applied to the delay on each successive failure.
        multiplier: f64,
    },
    /// Linear backoff (delay increases linearly)
    Linear {
        /// Delay applied before the second attempt.
        initial_delay: Duration,
        /// Amount added to the delay on each successive failure.
        increment: Duration,
    },
}

/// Retry result
#[derive(Debug)]
pub enum RetryResult<T, E> {
    /// Operation succeeded
    Success(T),
    /// All retries exhausted
    Failed {
        /// The error returned by the final attempt.
        last_error: E,
        /// Total number of attempts made before giving up.
        attempts: usize,
    },
}

impl RetryPolicy {
    /// Create a policy that waits a constant `delay` between attempts.
    ///
    /// # Arguments
    ///
    /// * `max_attempts` — Total attempts including the first try.  `1` means
    ///   no retries.
    /// * `delay` — Fixed pause between consecutive attempts.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::RetryPolicy;
    ///
    /// let policy = RetryPolicy::fixed(3, Duration::from_millis(50));
    /// assert_eq!(policy.max_attempts, 3);
    /// ```
    pub fn fixed(max_attempts: usize, delay: Duration) -> Self {
        Self {
            max_attempts,
            strategy: RetryStrategy::Fixed(delay),
        }
    }

    /// Create a policy with exponential back-off (multiplier 2×, cap 60 s).
    ///
    /// The delay before attempt `n` is:
    /// `min(initial_delay × 2^(n-1), 60 s)`
    ///
    /// # Arguments
    ///
    /// * `max_attempts` — Total attempts including the first try.
    /// * `initial_delay` — Delay before the second attempt.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::RetryPolicy;
    ///
    /// // Delays: 100 ms, 200 ms, 400 ms
    /// let policy = RetryPolicy::exponential(4, Duration::from_millis(100));
    /// ```
    pub fn exponential(max_attempts: usize, initial_delay: Duration) -> Self {
        Self {
            max_attempts,
            strategy: RetryStrategy::Exponential {
                initial_delay,
                max_delay: Duration::from_secs(60),
                multiplier: 2.0,
            },
        }
    }

    /// Create policy with linear backoff
    pub fn linear(max_attempts: usize, initial_delay: Duration, increment: Duration) -> Self {
        Self {
            max_attempts,
            strategy: RetryStrategy::Linear {
                initial_delay,
                increment,
            },
        }
    }

    /// Execute a fallible async closure, retrying on error according to this
    /// policy.
    ///
    /// The closure `f` is called up to `max_attempts` times.  Between
    /// consecutive failures the task sleeps for the duration computed by the
    /// configured [`RetryStrategy`].
    ///
    /// # Arguments
    ///
    /// * `f` — A `FnMut` that returns a `Future<Output = Result<T, E>>`.
    ///
    /// # Returns
    ///
    /// `Ok(value)` from the first successful attempt, or `Err(last_error)` if
    /// all attempts fail.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::RetryPolicy;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let policy = RetryPolicy::fixed(3, Duration::from_millis(10));
    /// let result: Result<&str, &str> = policy.retry(|| async { Ok("done") }).await;
    /// assert_eq!(result, Ok("done"));
    /// # }
    /// ```
    pub async fn retry<F, Fut, T, E>(&self, mut f: F) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let mut attempt = 0;

        loop {
            attempt += 1;

            debug!(
                attempt = attempt,
                max = self.max_attempts,
                "retry: attempting operation"
            );

            match f().await {
                Ok(result) => {
                    if attempt > 1 {
                        debug!(
                            attempt = attempt,
                            "retry: operation succeeded after retries"
                        );
                    }
                    return Ok(result);
                }
                Err(e) => {
                    warn!(
                        attempt = attempt,
                        max = self.max_attempts,
                        error = %e,
                        "retry: operation failed"
                    );

                    if attempt >= self.max_attempts {
                        warn!(attempts = attempt, "retry: all attempts exhausted");
                        return Err(e);
                    }

                    // Calculate delay
                    let delay = self.calculate_delay(attempt);
                    debug!(
                        delay_ms = delay.as_millis(),
                        "retry: waiting before next attempt"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Execute with retries, returning detailed result
    pub async fn retry_with_details<F, Fut, T, E>(&self, f: F) -> RetryResult<T, E>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        match self.retry(f).await {
            Ok(value) => RetryResult::Success(value),
            Err(error) => RetryResult::Failed {
                last_error: error,
                attempts: self.max_attempts,
            },
        }
    }

    fn calculate_delay(&self, attempt: usize) -> Duration {
        match &self.strategy {
            RetryStrategy::Fixed(delay) => *delay,
            RetryStrategy::Exponential {
                initial_delay,
                max_delay,
                multiplier,
            } => {
                let max_ms = max_delay.as_millis() as f64;
                // saturating_sub prevents underflow when attempt == 0 (would give powi(-1) = decay).
                // The cap at 62 prevents f64 overflow for very large attempt counts.
                let exp = attempt.saturating_sub(1).min(62) as i32;
                let delay_ms = (initial_delay.as_millis() as f64 * multiplier.powi(exp))
                    .min(max_ms);
                Duration::from_millis(delay_ms as u64)
            }
            RetryStrategy::Linear {
                initial_delay,
                increment,
            } => {
                let steps = attempt.saturating_sub(1) as u32;
                let added = increment.as_millis().saturating_mul(steps as u128);
                let total_ms = initial_delay.as_millis().saturating_add(added);
                Duration::from_millis(total_ms.min(u64::MAX as u128) as u64)
            }
        }
    }

    /// Check if error is retryable (can be customized)
    pub fn is_retryable<E>(&self, _error: &E) -> bool {
        // Default: retry all errors
        // Override this for specific error types
        true
    }
}

/// Retry a fallible async closure only when a predicate approves the error.
///
/// Unlike [`RetryPolicy::retry`], errors for which `should_retry` returns
/// `false` are propagated immediately without consuming further attempts.
/// This is useful for distinguishing transient errors (e.g. network timeout)
/// from permanent ones (e.g. invalid request body).
///
/// # Arguments
///
/// * `policy` — The [`RetryPolicy`] controlling attempt counts and delays.
/// * `f` — The fallible async closure to invoke.
/// * `should_retry` — A predicate called on each error; return `true` to retry
///   or `false` to propagate immediately.
///
/// # Returns
///
/// `Ok(value)` on success, or `Err(error)` when the predicate returns `false`
/// or `max_attempts` are exhausted.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::{RetryPolicy, retry_if};
///
/// # #[tokio::main]
/// # async fn main() {
/// let policy = RetryPolicy::fixed(5, Duration::from_millis(10));
/// let result: Result<(), &str> = retry_if(
///     &policy,
///     || async { Err("permanent") },
///     |e| *e == "transient",
/// ).await;
/// assert_eq!(result.unwrap_err(), "permanent"); // stopped immediately
/// # }
/// ```
pub async fn retry_if<F, Fut, T, E, P>(
    policy: &RetryPolicy,
    mut f: F,
    mut should_retry: P,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    P: FnMut(&E) -> bool,
    E: std::fmt::Display,
{
    let mut attempt = 0;

    loop {
        attempt += 1;

        match f().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if !should_retry(&e) {
                    warn!(error = %e, "retry: error is not retryable");
                    return Err(e);
                }

                if attempt >= policy.max_attempts {
                    return Err(e);
                }

                let delay = policy.calculate_delay(attempt);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Add random jitter (up to 25 %) to a delay to prevent thundering-herd
/// scenarios when many clients retry at the same time.
///
/// # Arguments
///
/// * `duration` — Base delay to which jitter is added.
///
/// # Returns
///
/// A `Duration` in the range `[duration, duration + duration/4)`.
/// When `duration` is very small (< 4 ms), the jitter is zero.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::retry::with_jitter;
///
/// let base = Duration::from_millis(200);
/// let jittered = with_jitter(base);
/// assert!(jittered >= base);
/// assert!(jittered <= base + Duration::from_millis(50));
/// ```
pub fn with_jitter(duration: Duration) -> Duration {
    use rand::Rng;
    let max_jitter = duration.as_millis() / 4;
    if max_jitter == 0 {
        return duration;
    }
    let jitter = rand::thread_rng().gen_range(0..max_jitter);
    duration + Duration::from_millis(jitter as u64)
}

/// Execute an inference operation with retries, honouring `RateLimited` back-off.
///
/// Unlike `RetryPolicy::retry`, this variant:
/// - Sleeps for exactly `retry_after_secs` when the provider says to back off.
/// - Stops retrying on `BudgetExceeded` (non-transient).
/// - Falls back to exponential backoff for all other errors.
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::retry::retry_inference;
/// use tokio_prompt_orchestrator::{OrchestratorError, OpenAiWorker, ModelWorker};
/// # #[tokio::main]
/// # async fn main() -> Result<(), OrchestratorError> {
/// # let worker = OpenAiWorker::new("gpt-4o")?;
/// let tokens = retry_inference(3, Duration::from_millis(200), || async {
///     worker.infer("hello").await
/// }).await?;
/// # Ok(()) }
/// ```
pub async fn retry_inference<F, Fut>(
    max_attempts: usize,
    base_delay: Duration,
    mut f: F,
) -> Result<Vec<String>, OrchestratorError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, OrchestratorError>>,
{
    let mut attempt = 0;

    loop {
        attempt += 1;
        match f().await {
            Ok(v) => return Ok(v),
            Err(OrchestratorError::BudgetExceeded { spent, limit }) => {
                // Non-transient — propagate immediately.
                return Err(OrchestratorError::BudgetExceeded { spent, limit });
            }
            Err(OrchestratorError::RateLimited { retry_after_secs }) => {
                if attempt >= max_attempts {
                    return Err(OrchestratorError::RateLimited { retry_after_secs });
                }
                let wait = Duration::from_secs(retry_after_secs);
                warn!(
                    attempt,
                    wait_secs = retry_after_secs,
                    "rate limited — sleeping for Retry-After duration"
                );
                tokio::time::sleep(wait).await;
            }
            Err(e) => {
                if attempt >= max_attempts {
                    return Err(e);
                }
                // Exponential backoff for all other transient errors.
                // saturating_sub prevents underflow when attempt == 0 (would give powi(-1) = decay).
                // The cap at 62 prevents f64 overflow for very large attempt counts.
                let exp = attempt.saturating_sub(1).min(62) as i32;
                let delay_ms = (base_delay.as_millis() as f64 * 2_f64.powi(exp))
                    .min(60_000.0) as u64;
                let delay = with_jitter(Duration::from_millis(delay_ms));
                warn!(attempt, delay_ms = delay.as_millis(), error = %e, "transient error — retrying");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_retry_succeeds_eventually() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = attempts.clone();

        let policy = RetryPolicy::fixed(5, Duration::from_millis(10));

        let result = policy
            .retry(|| {
                let attempts = attempts_clone.clone();
                async move {
                    let count = attempts.fetch_add(1, Ordering::SeqCst);
                    if count < 2 {
                        Err("failing")
                    } else {
                        Ok("success")
                    }
                }
            })
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_exhausts_attempts() {
        let policy = RetryPolicy::fixed(3, Duration::from_millis(10));

        let result = policy
            .retry(|| async { Err::<(), _>("always fails") })
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_exponential_backoff() {
        let policy = RetryPolicy::exponential(4, Duration::from_millis(10));

        let delay1 = policy.calculate_delay(1);
        let delay2 = policy.calculate_delay(2);
        let delay3 = policy.calculate_delay(3);

        assert_eq!(delay1, Duration::from_millis(10));
        assert_eq!(delay2, Duration::from_millis(20));
        assert_eq!(delay3, Duration::from_millis(40));
    }

    #[tokio::test]
    async fn test_linear_backoff() {
        let policy = RetryPolicy::linear(4, Duration::from_millis(100), Duration::from_millis(50));

        assert_eq!(policy.calculate_delay(1), Duration::from_millis(100));
        assert_eq!(policy.calculate_delay(2), Duration::from_millis(150));
        assert_eq!(policy.calculate_delay(3), Duration::from_millis(200));
    }

    #[tokio::test]
    async fn test_retry_if() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = attempts.clone();

        let policy = RetryPolicy::fixed(5, Duration::from_millis(10));

        // Only retry on "transient" errors
        let result: Result<(), &str> = retry_if(
            &policy,
            || {
                let attempts = attempts_clone.clone();
                async move {
                    let count = attempts.fetch_add(1, Ordering::SeqCst);
                    if count == 0 {
                        Err("transient")
                    } else {
                        Err("permanent")
                    }
                }
            },
            |e| *e == "transient",
        )
        .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "permanent");
        assert_eq!(attempts.load(Ordering::SeqCst), 2); // First attempt + one retry
    }

    #[test]
    fn test_jitter() {
        let base = Duration::from_secs(1);
        let jittered = with_jitter(base);

        // Should be within range
        assert!(jittered >= base);
        assert!(jittered <= base + Duration::from_millis(250));
    }
}
