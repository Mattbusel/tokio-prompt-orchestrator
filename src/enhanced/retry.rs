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

/// Retry policy configuration
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Maximum number of attempts (including the first try).
    pub max_attempts: usize,
    /// Backoff strategy applied between consecutive attempts.
    pub strategy: RetryStrategy,
}

/// Retry backoff strategy
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
    /// Create policy with fixed delay
    pub fn fixed(max_attempts: usize, delay: Duration) -> Self {
        Self {
            max_attempts,
            strategy: RetryStrategy::Fixed(delay),
        }
    }

    /// Create policy with exponential backoff
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

    /// Execute operation with retries
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
                let delay_ms = (initial_delay.as_millis() as f64
                    * multiplier.powi((attempt - 1) as i32))
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

/// Conditional retry - only retry if predicate returns true
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

/// Retry with jitter to prevent thundering herd
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
                let delay_ms = (base_delay.as_millis() as f64 * 2_f64.powi(attempt as i32 - 1))
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
