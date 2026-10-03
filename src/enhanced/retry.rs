//! Retry Logic
//!
//! Automatic retry with backoff for transient failures, built on the
//! [`backon`](https://docs.rs/backon) crate. This module keeps the
//! orchestrator's own policy types ([`RetryPolicy`], [`InferenceRetry`]) and
//! lets `backon` do the attempt counting, delay schedule, jitter and sleeping.
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
//!     // Your fallible operation: returns Ok on success, Err on transient failure
//!     Ok::<String, std::io::Error>("inference result".to_string())
//! }).await;
//!
//! match result {
//!     Ok(output) => println!("{output}"),
//!     Err(e) => eprintln!("All retries exhausted: {e}"),
//! }
//! # }
//! ```

use crate::config::ResilienceConfig;
use crate::OrchestratorError;
use backon::{
    BackoffBuilder, ConstantBackoff, ConstantBuilder, ExponentialBackoff, ExponentialBuilder,
    Retryable,
};
use std::time::Duration;
use tracing::warn;

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
/// - [`RetryStrategy::Fixed`]: constant pause.
/// - [`RetryStrategy::Exponential`]: the delay is multiplied after each
///   failure, up to a cap.
/// - [`RetryStrategy::Linear`]: the delay grows by a fixed increment.
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

/// The delay schedule of a [`RetryPolicy`], as a `backon` backoff.
///
/// Fixed and exponential schedules are `backon`'s own; linear has no
/// `backon` builder, so it is a plain iterator.
enum PolicyBackoff {
    Fixed(ConstantBackoff),
    Exponential(ExponentialBackoff),
    Linear {
        next: Duration,
        increment: Duration,
        remaining: usize,
    },
}

impl Iterator for PolicyBackoff {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        match self {
            Self::Fixed(b) => b.next(),
            // backon multiplies in f32 (40 ms comes out as 39.999999 ms);
            // round to whole milliseconds as the policy always has.
            Self::Exponential(b) => b
                .next()
                .map(|d| Duration::from_millis((d.as_secs_f64() * 1000.0).round() as u64)),
            Self::Linear {
                next,
                increment,
                remaining,
            } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining -= 1;
                let current = *next;
                *next = next.saturating_add(*increment);
                Some(current)
            }
        }
    }
}

impl RetryPolicy {
    /// Create a policy that waits a constant `delay` between attempts.
    ///
    /// # Arguments
    ///
    /// * `max_attempts`: total attempts including the first try. `1` means
    ///   no retries.
    /// * `delay`: fixed pause between consecutive attempts.
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

    /// Create a policy with exponential back-off (multiplier 2x, cap 60 s).
    ///
    /// The delay before attempt `n + 1` is `min(initial_delay * 2^(n-1), 60 s)`.
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

    /// The delays between attempts, `retries` of them at most.
    fn backoff(&self, retries: usize) -> PolicyBackoff {
        match &self.strategy {
            RetryStrategy::Fixed(delay) => PolicyBackoff::Fixed(
                ConstantBuilder::new()
                    .with_delay(*delay)
                    .with_max_times(retries)
                    .build(),
            ),
            RetryStrategy::Exponential {
                initial_delay,
                max_delay,
                multiplier,
            } => PolicyBackoff::Exponential(
                ExponentialBuilder::new()
                    .with_min_delay(*initial_delay)
                    .with_max_delay(*max_delay)
                    .with_factor(*multiplier as f32)
                    .with_max_times(retries)
                    .build(),
            ),
            RetryStrategy::Linear {
                initial_delay,
                increment,
            } => PolicyBackoff::Linear {
                next: *initial_delay,
                increment: *increment,
                remaining: retries,
            },
        }
    }

    /// Delay slept after failed attempt number `attempt` (1-based).
    #[cfg(test)]
    fn calculate_delay(&self, attempt: usize) -> Duration {
        self.backoff(attempt)
            .nth(attempt.saturating_sub(1))
            .unwrap_or_default()
    }

    /// Execute a fallible async closure, retrying on error according to this
    /// policy.
    ///
    /// The closure `f` is called up to `max_attempts` times. Between
    /// consecutive failures the task sleeps for the delay given by the
    /// configured [`RetryStrategy`].
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
    pub async fn retry<F, Fut, T, E>(&self, f: F) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        retry_if(self, f, |_| true).await
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

    /// Check if error is retryable (can be customized)
    pub fn is_retryable<E>(&self, _error: &E) -> bool {
        // Default: retry all errors. Use `retry_if` to filter.
        true
    }
}

/// Retry a fallible async closure only when a predicate approves the error.
///
/// Unlike [`RetryPolicy::retry`], errors for which `should_retry` returns
/// `false` are propagated immediately without consuming further attempts.
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
pub async fn retry_if<F, Fut, T, E, P>(policy: &RetryPolicy, f: F, should_retry: P) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    P: FnMut(&E) -> bool,
    E: std::fmt::Display,
{
    let retries = policy.max_attempts.saturating_sub(1);
    f.retry(policy.backoff(retries))
        .when(should_retry)
        .notify(|e: &E, delay: Duration| {
            warn!(error = %e, delay_ms = delay.as_millis() as u64, "retry: operation failed, retrying");
        })
        .await
}

/// Add random jitter (up to 25 %) to a delay to prevent thundering-herd
/// scenarios when many clients retry at the same time.
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
/// - Stops at once on errors that cannot succeed on a retry
///   ([`OrchestratorError::is_retryable`] is `false`: bad key, budget, config).
/// - Uses jittered exponential backoff (capped at 60 s) for everything else.
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
    f: F,
) -> Result<Vec<String>, OrchestratorError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, OrchestratorError>>,
{
    let backoff = ExponentialBuilder::new()
        .with_min_delay(base_delay)
        .with_max_delay(Duration::from_secs(60))
        .with_max_times(max_attempts.saturating_sub(1))
        .with_jitter();
    f.retry(backoff)
        .when(OrchestratorError::is_retryable)
        .adjust(|e, delay| match e {
            OrchestratorError::RateLimited { retry_after_secs } => {
                delay.map(|_| Duration::from_secs(*retry_after_secs))
            }
            _ => delay,
        })
        .notify(|e, delay| {
            warn!(error = %e, delay_ms = delay.as_millis() as u64, "transient error, retrying");
        })
        .await
}

/// Retry settings for the pipeline's inference stage.
///
/// Every model call made by the pipeline goes through
/// [`InferenceRetry::run`]. With `max_retries == 0` (the default for
/// [`spawn_pipeline`](crate::spawn_pipeline)) the call is made exactly once.
/// Otherwise a call that fails with a transient error (429, 5xx, network)
/// is retried with jittered exponential backoff, and a provider's
/// `Retry-After` is respected when it is no longer than `max_delay`.
/// Errors that cannot succeed on a retry (bad key, budget exceeded, bad
/// config) are returned at once.
///
/// All attempts of one request count as one call for the circuit breaker,
/// and all of them together must fit in the inference timeout.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::InferenceRetry;
///
/// let retry = InferenceRetry::new(2, Duration::from_millis(200), Duration::from_secs(5));
/// assert!(retry.is_enabled());
/// assert!(!InferenceRetry::disabled().is_enabled());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceRetry {
    /// Retries after the first attempt. `0` disables retrying.
    pub max_retries: u32,
    /// Delay before the first retry; it doubles on each further retry.
    pub base_delay: Duration,
    /// Cap on the backoff delay, and on how long a `Retry-After` may ask
    /// the stage to wait. A longer `Retry-After` fails the request instead.
    pub max_delay: Duration,
}

impl Default for InferenceRetry {
    fn default() -> Self {
        Self::disabled()
    }
}

impl InferenceRetry {
    /// No retries: every request makes exactly one model call.
    pub const fn disabled() -> Self {
        Self {
            max_retries: 0,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
        }
    }

    /// Retry up to `max_retries` times, starting at `base_delay` and capping
    /// each wait at `max_delay`.
    pub const fn new(max_retries: u32, base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_retries,
            base_delay,
            max_delay,
        }
    }

    /// Read `retry_attempts`, `retry_base_ms` and `retry_max_ms` from the
    /// `[resilience]` section of a pipeline config.
    pub fn from_resilience(r: &ResilienceConfig) -> Self {
        Self::new(
            r.retry_attempts,
            Duration::from_millis(r.retry_base_ms),
            Duration::from_millis(r.retry_max_ms),
        )
    }

    /// `true` when at least one retry is allowed.
    pub const fn is_enabled(&self) -> bool {
        self.max_retries > 0
    }

    /// Run `f`, retrying transient failures according to these settings.
    pub async fn run<F, Fut, T>(&self, mut f: F) -> Result<T, OrchestratorError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, OrchestratorError>>,
    {
        if !self.is_enabled() {
            return f().await;
        }
        let max_delay = self.max_delay;
        let backoff = ExponentialBuilder::new()
            .with_min_delay(self.base_delay.min(max_delay))
            .with_max_delay(max_delay)
            .with_max_times(self.max_retries as usize)
            .with_jitter();
        f.retry(backoff)
            .when(OrchestratorError::is_retryable)
            .adjust(move |e, delay| match e {
                // Wait as long as the provider asked, unless that is longer
                // than we are allowed to wait: then give up and fail fast.
                OrchestratorError::RateLimited { retry_after_secs } => {
                    let asked = Duration::from_secs(*retry_after_secs);
                    if asked > max_delay {
                        None
                    } else {
                        delay.map(|d| d.max(asked))
                    }
                }
                _ => delay,
            })
            .notify(|e, delay| {
                crate::metrics::inc_inference_retry();
                warn!(
                    target: "orchestrator::pipeline",
                    error_kind = e.error_kind(),
                    delay_ms = delay.as_millis() as u64,
                    "Inference failed with a transient error, retrying"
                );
            })
            .await
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

        assert!(result.is_ok(), "retry should succeed on the third attempt");
        assert_eq!(result.unwrap(), "success", "expected result value 'success'");
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "expected exactly 3 total attempts (2 failures + 1 success)");
    }

    #[tokio::test]
    async fn test_retry_exhausts_attempts() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let a = attempts.clone();
        let policy = RetryPolicy::fixed(3, Duration::from_millis(10));

        let result = policy
            .retry(|| {
                a.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>("always fails") }
            })
            .await;

        assert!(result.is_err(), "all 3 attempts exhausted, result must be Err");
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "max_attempts counts the first try");
    }

    #[tokio::test]
    async fn test_exponential_backoff() {
        let policy = RetryPolicy::exponential(4, Duration::from_millis(10));

        let delay1 = policy.calculate_delay(1);
        let delay2 = policy.calculate_delay(2);
        let delay3 = policy.calculate_delay(3);

        assert_eq!(delay1, Duration::from_millis(10), "attempt 1 delay should equal initial_delay");
        assert_eq!(delay2, Duration::from_millis(20), "attempt 2 delay should be initial_delay * 2");
        assert_eq!(delay3, Duration::from_millis(40), "attempt 3 delay should be initial_delay * 4");
    }

    #[test]
    fn test_exponential_backoff_respects_cap() {
        let policy = RetryPolicy {
            max_attempts: 10,
            strategy: RetryStrategy::Exponential {
                initial_delay: Duration::from_millis(100),
                max_delay: Duration::from_millis(300),
                multiplier: 2.0,
            },
        };
        let delays: Vec<_> = policy.backoff(5).collect();
        assert_eq!(
            delays,
            [100, 200, 300, 300, 300].map(Duration::from_millis).to_vec()
        );
    }

    #[tokio::test]
    async fn test_linear_backoff() {
        let policy = RetryPolicy::linear(4, Duration::from_millis(100), Duration::from_millis(50));

        assert_eq!(policy.calculate_delay(1), Duration::from_millis(100), "attempt 1 delay should equal initial_delay");
        assert_eq!(policy.calculate_delay(2), Duration::from_millis(150), "attempt 2 delay should be initial + 1*increment");
        assert_eq!(policy.calculate_delay(3), Duration::from_millis(200), "attempt 3 delay should be initial + 2*increment");
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

        assert!(result.is_err(), "retry_if with non-retryable error must return Err");
        assert_eq!(result.unwrap_err(), "permanent", "expected the non-retryable 'permanent' error to propagate");
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "expected 2 attempts: first (transient) + second (permanent, not retried)");
    }

    #[test]
    fn test_jitter() {
        let base = Duration::from_secs(1);
        let jittered = with_jitter(base);

        assert!(jittered >= base, "jittered delay must not be less than the base delay");
        assert!(jittered <= base + Duration::from_millis(250), "jitter must not exceed 25% of base (250ms for a 1s base)");
    }

    #[tokio::test]
    async fn test_retry_inference_stops_on_auth_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result = retry_inference(5, Duration::from_millis(1), || {
            c.fetch_add(1, Ordering::SeqCst);
            async { Err(OrchestratorError::AuthFailed("HTTP 401".into())) }
        })
        .await;
        assert!(matches!(result, Err(OrchestratorError::AuthFailed(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a bad key is never retried");
    }

    #[tokio::test]
    async fn test_inference_retry_disabled_calls_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result: Result<(), _> = InferenceRetry::disabled()
            .run(|| {
                c.fetch_add(1, Ordering::SeqCst);
                async { Err(OrchestratorError::Inference("503".into())) }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_inference_retry_recovers_from_transient_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let retry = InferenceRetry::new(3, Duration::from_millis(1), Duration::from_millis(10));
        let result = retry
            .run(|| {
                let n = c.fetch_add(1, Ordering::SeqCst);
                async move {
                    match n {
                        0 => Err(OrchestratorError::Inference("503 Service Unavailable".into())),
                        1 => Err(OrchestratorError::RateLimited { retry_after_secs: 0 }),
                        _ => Ok("answer"),
                    }
                }
            })
            .await;
        assert_eq!(result.ok(), Some("answer"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_inference_retry_gives_up_after_max_retries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let retry = InferenceRetry::new(2, Duration::from_millis(1), Duration::from_millis(10));
        let result: Result<(), _> = retry
            .run(|| {
                c.fetch_add(1, Ordering::SeqCst);
                async { Err(OrchestratorError::Inference("503".into())) }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3, "first try plus 2 retries");
    }

    #[tokio::test]
    async fn test_inference_retry_fails_fast_when_retry_after_exceeds_cap() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let retry = InferenceRetry::new(5, Duration::from_millis(1), Duration::from_secs(5));
        let started = std::time::Instant::now();
        let result: Result<(), _> = retry
            .run(|| {
                c.fetch_add(1, Ordering::SeqCst);
                async { Err(OrchestratorError::RateLimited { retry_after_secs: 60 }) }
            })
            .await;
        assert!(matches!(result, Err(OrchestratorError::RateLimited { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a 60 s Retry-After is over the 5 s cap");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn test_inference_retry_from_resilience_config() {
        let r = ResilienceConfig {
            retry_attempts: 3,
            retry_base_ms: 50,
            retry_max_ms: 2000,
            circuit_breaker_threshold: 5,
            circuit_breaker_timeout_s: 60,
            circuit_breaker_success_rate: 0.8,
        };
        let retry = InferenceRetry::from_resilience(&r);
        assert_eq!(
            retry,
            InferenceRetry::new(3, Duration::from_millis(50), Duration::from_secs(2))
        );
    }
}
