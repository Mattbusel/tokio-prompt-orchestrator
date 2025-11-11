//! Retry Logic
//!
//! Automatic retry with exponential backoff for transient failures.
//!
//! ## Usage
//!
//! ```rust
//! use tokio_prompt_orchestrator::enhanced::RetryPolicy;
//!
//! let policy = RetryPolicy::exponential(3, Duration::from_millis(100));
//!
//! let result = policy.retry(|| async {
//!     // Your fallible operation
//!     worker.infer(prompt).await
//! }).await?;
//! ```

use std::time::Duration;
use tracing::{warn, debug};

/// Retry policy configuration
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub strategy: RetryStrategy,
}

/// Retry backoff strategy
#[derive(Clone, Debug)]
pub enum RetryStrategy {
    /// Fixed delay between retries
    Fixed(Duration),
    /// Exponential backoff (delay doubles each time)
    Exponential {
        initial_delay: Duration,
        max_delay: Duration,
        multiplier: f64,
    },
    /// Linear backoff (delay increases linearly)
    Linear {
        initial_delay: Duration,
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
        last_error: E,
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
        let mut last_error = None;

        loop {
            attempt += 1;

            debug!(attempt = attempt, max = self.max_attempts, "retry: attempting operation");

            match f().await {
                Ok(result) => {
                    if attempt > 1 {
                        debug!(attempt = attempt, "retry: operation succeeded after retries");
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

                    last_error = Some(e);

                    if attempt >= self.max_attempts {
                        warn!(attempts = attempt, "retry: all attempts exhausted");
                        return Err(last_error.unwrap());
                    }

                    // Calculate delay
                    let delay = self.calculate_delay(attempt);
                    debug!(delay_ms = delay.as_millis(), "retry: waiting before next attempt");
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Execute with retries, returning detailed result
    pub async fn retry_with_details<F, Fut, T, E>(
        &self,
        f: F,
    ) -> RetryResult<T, E>
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
                let delay = initial_delay.as_millis() as f64
                    * multiplier.powi((attempt - 1) as i32);
                let delay = Duration::from_millis(delay as u64);
                delay.min(*max_delay)
            }
            RetryStrategy::Linear {
                initial_delay,
                increment,
            } => *initial_delay + *increment * (attempt as u32 - 1),
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
    let jitter = rand::thread_rng().gen_range(0..duration.as_millis() / 4);
    duration + Duration::from_millis(jitter as u64)
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
        let policy = RetryPolicy::linear(
            4,
            Duration::from_millis(100),
            Duration::from_millis(50),
        );

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
        let result = retry_if(
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
