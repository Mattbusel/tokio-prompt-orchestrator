//! Circuit Breaker
//!
//! Prevents cascading failures by stopping requests to failing services.
//!
//! ## States
//! - **Closed**: Normal operation, requests flow through
//! - **Open**: Service failing, requests rejected immediately
//! - **Half-Open**: Testing if service recovered
//!
//! ## Usage
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_prompt_orchestrator::enhanced::CircuitBreaker;
//! use tokio_prompt_orchestrator::enhanced::circuit_breaker::CircuitBreakerError;
//! # #[tokio::main]
//! # async fn main() {
//! let breaker = CircuitBreaker::new(5, 0.5, Duration::from_secs(60));
//!
//! match breaker.call(|| async {
//!     // Your operation  -  replace with a real async call
//!     Ok::<&str, &str>("inference result")
//! }).await {
//!     Ok(result) => println!("{result}"), // Success
//!     Err(CircuitBreakerError::Open) => {
//!         // Circuit open, fail fast
//!     }
//!     Err(CircuitBreakerError::Failed(e)) => {
//!         eprintln!("Operation failed: {e}");
//!     }
//! }
//! # }
//! ```

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// A circuit breaker that prevents cascading failures by stopping requests to
/// a failing downstream service.
///
/// # State machine
///
/// ```text
/// Closed ──(failures ≥ threshold)──► Open
///   ▲                                  │
///   │                                  │ (timeout elapsed)
///   │                                  ▼
///   └──(success_rate ≥ threshold)── HalfOpen
/// ```
///
/// - **Closed** — normal operation; all requests flow through.
/// - **Open** — all requests are rejected immediately with
///   [`CircuitBreakerError::Open`] without calling the wrapped operation.
/// - **HalfOpen** — a single probe request is allowed; on success the circuit
///   closes; on failure it reopens.
///
/// # Cloning
///
/// `CircuitBreaker` is `Clone + Send + Sync`.  All clones share the same
/// underlying `Arc<RwLock<CircuitState>>`.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::CircuitBreaker;
/// use tokio_prompt_orchestrator::enhanced::circuit_breaker::CircuitBreakerError;
///
/// # #[tokio::main]
/// # async fn main() {
/// let cb = CircuitBreaker::new(5, 0.8, Duration::from_secs(30));
///
/// let result = cb.call(|| async {
///     reqwest::get("https://api.example.com/infer")
///         .await
///         .map_err(|e| e.to_string())
/// }).await;
///
/// match result {
///     Ok(resp) => { /* handle response */ }
///     Err(CircuitBreakerError::Open) => { /* fail fast */ }
///     Err(CircuitBreakerError::Failed(e)) => { /* handle error */ }
/// }
/// # }
/// ```
#[derive(Clone)]
pub struct CircuitBreaker {
    state: Arc<RwLock<CircuitState>>,
    config: CircuitBreakerConfig,
}

#[derive(Debug, Clone)]
struct CircuitBreakerConfig {
    /// Number of failures before opening circuit
    failure_threshold: usize,
    /// Success rate threshold (0.0 - 1.0) to close circuit
    success_threshold: f64,
    /// How long to wait before testing if service recovered
    timeout: Duration,
    /// Window size for tracking metrics
    window_size: usize,
}

#[derive(Debug)]
struct CircuitState {
    status: CircuitStatus,
    failures: usize,
    successes: usize,
    last_failure_time: Option<Instant>,
    last_state_change: Instant,
    /// Recent results (true = success, false = failure)
    recent_results: VecDeque<bool>,
    /// Number of consecutive half-open probe failures.
    ///
    /// Drives exponential backoff: the probe interval after the Nth failed
    /// probe is `timeout * 2^min(probe_failures, 6)` (capped at 64×).
    /// Resets to 0 when the circuit successfully closes.
    probe_failures: usize,
}

/// Current state of a circuit breaker.
#[derive(Debug, Clone, PartialEq)]
pub enum CircuitStatus {
    /// Circuit is closed  -  requests flow through normally.
    Closed,
    /// Circuit is open  -  requests are rejected immediately without calling the operation.
    Open,
    /// Circuit is half-open  -  one probe request is allowed through to test recovery.
    HalfOpen,
}

/// Circuit breaker errors
#[derive(Debug)]
pub enum CircuitBreakerError<E> {
    /// Circuit is open, request rejected
    Open,
    /// Operation failed
    Failed(E),
}

impl CircuitBreaker {
    /// Create a new `CircuitBreaker` in the `Closed` state.
    ///
    /// # Arguments
    ///
    /// * `failure_threshold` — Number of consecutive failures required to open
    ///   the circuit.  A value of `1` opens immediately on the first error.
    /// * `success_threshold` — Required success rate (`0.0..=1.0`) over the
    ///   recent-results window before the circuit closes from `HalfOpen`.
    ///   Typical values: `0.8` (80 %) or `1.0` (100 %).
    /// * `timeout` — How long to stay in the `Open` state before transitioning
    ///   to `HalfOpen` and allowing one probe request through.
    ///
    /// The default rolling-window size is 100 results.  Use
    /// [`with_window_size`](Self::with_window_size) to customise it.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::CircuitBreaker;
    ///
    /// // Open after 5 failures; require 80 % success rate to close; probe after 60 s.
    /// let cb = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
    /// ```
    pub fn new(failure_threshold: usize, success_threshold: f64, timeout: Duration) -> Self {
        Self {
            state: Arc::new(RwLock::new(CircuitState {
                status: CircuitStatus::Closed,
                failures: 0,
                successes: 0,
                last_failure_time: None,
                last_state_change: Instant::now(),
                recent_results: VecDeque::new(),
                probe_failures: 0,
            })),
            config: CircuitBreakerConfig {
                failure_threshold,
                success_threshold,
                timeout,
                window_size: 100,
            },
        }
    }

    /// Set the rolling-window size used to calculate the success rate.
    ///
    /// The window keeps the most recent `size` call outcomes. A smaller window
    /// reacts faster to bursts of failures but is more sensitive to noise. The
    /// default is `100`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::CircuitBreaker;
    ///
    /// // Tighter window — reacts faster to short failure bursts.
    /// let cb = CircuitBreaker::new(5, 0.8, Duration::from_secs(30))
    ///     .with_window_size(20);
    /// ```
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn with_window_size(mut self, size: usize) -> Self {
        self.config.window_size = size.max(1);
        self
    }

    /// Execute a fallible async operation through the circuit breaker.
    ///
    /// If the circuit is `Open` the operation is **not called** and
    /// `Err(CircuitBreakerError::Open)` is returned immediately.
    /// Otherwise the closure is invoked; its `Ok`/`Err` outcome is recorded
    /// and may cause a state transition.
    ///
    /// # Arguments
    ///
    /// * `f` — A `FnOnce` that returns a `Future<Output = Result<T, E>>`.
    ///   The closure is called at most once per `call` invocation.
    ///
    /// # Returns
    ///
    /// - `Ok(value)` — operation succeeded.
    /// - `Err(CircuitBreakerError::Open)` — circuit is open; request rejected.
    /// - `Err(CircuitBreakerError::Failed(e))` — operation returned `Err(e)`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::CircuitBreaker;
    /// use tokio_prompt_orchestrator::enhanced::circuit_breaker::CircuitBreakerError;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let cb = CircuitBreaker::new(3, 0.8, Duration::from_secs(10));
    /// let result = cb.call(|| async { Ok::<_, String>("ok") }).await;
    /// assert!(matches!(result, Ok("ok")));
    /// # }
    /// ```
    pub async fn call<F, Fut, T, E>(&self, f: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        // Check if we should allow the request
        {
            let mut state = self.state.write().await;

            match state.status {
                CircuitStatus::Open => {
                    // Exponential backoff: each failed half-open probe doubles the
                    // wait, capped at 64× the base timeout.  This prevents flapping
                    // when a downstream service recovers slowly.
                    if let Some(last_failure) = state.last_failure_time {
                        let backoff_factor = 1u32 << state.probe_failures.min(6);
                        let probe_interval = self.config.timeout * backoff_factor;
                        if last_failure.elapsed() >= probe_interval {
                            // Try half-open  -  clear window so success-rate is
                            // calculated only from post-recovery requests.
                            state.status = CircuitStatus::HalfOpen;
                            state.recent_results.clear();
                            state.failures = 0;
                            state.last_state_change = Instant::now();
                            info!(
                                probe_failures = state.probe_failures,
                                backoff_factor = backoff_factor,
                                "circuit breaker: transitioning to half-open"
                            );
                            crate::metrics::inc_cb_transition("half_open");
                        } else {
                            // Still open, reject
                            debug!("circuit breaker: request rejected (open)");
                            crate::metrics::inc_cb_rejected();
                            return Err(CircuitBreakerError::Open);
                        }
                    }
                }
                CircuitStatus::HalfOpen | CircuitStatus::Closed => {
                    // Allow request
                }
            }
        }

        // Execute operation
        let result = f().await;

        // Record result
        match &result {
            Ok(_) => self.record_success().await,
            Err(_) => self.record_failure().await,
        }

        result.map_err(CircuitBreakerError::Failed)
    }

    async fn record_success(&self) {
        let mut state = self.state.write().await;

        state.successes += 1;
        state.recent_results.push_back(true);
        if state.recent_results.len() > self.config.window_size {
            state.recent_results.pop_front();
        }

        debug!(
            status = ?state.status,
            successes = state.successes,
            failures = state.failures,
            "circuit breaker: success recorded"
        );

        match state.status {
            CircuitStatus::HalfOpen => {
                // Check if we can close the circuit
                let success_rate = self.calculate_success_rate(&state);
                if success_rate >= self.config.success_threshold {
                    state.status = CircuitStatus::Closed;
                    state.failures = 0;
                    state.successes = 0;
                    state.probe_failures = 0; // Service recovered — reset backoff
                    state.last_state_change = Instant::now();
                    info!(
                        success_rate = success_rate,
                        "circuit breaker: closing (service recovered)"
                    );
                    crate::metrics::inc_cb_transition("closed");
                }
            }
            CircuitStatus::Closed => {
                // Reset failure count on success
                if state.failures > 0 {
                    state.failures = 0;
                }
            }
            _ => {}
        }
    }

    async fn record_failure(&self) {
        let mut state = self.state.write().await;

        state.failures += 1;
        state.last_failure_time = Some(Instant::now());
        state.recent_results.push_back(false);
        if state.recent_results.len() > self.config.window_size {
            state.recent_results.pop_front();
        }

        warn!(
            status = ?state.status,
            failures = state.failures,
            threshold = self.config.failure_threshold,
            "circuit breaker: failure recorded"
        );

        match state.status {
            CircuitStatus::Closed => {
                if state.failures >= self.config.failure_threshold {
                    state.status = CircuitStatus::Open;
                    state.last_state_change = Instant::now();
                    warn!(
                        failures = state.failures,
                        threshold = self.config.failure_threshold,
                        "circuit breaker: opening (threshold exceeded)"
                    );
                    crate::metrics::inc_cb_transition("open");
                }
            }
            CircuitStatus::HalfOpen => {
                // Failure in half-open — go back to open and back off longer.
                state.probe_failures = state.probe_failures.saturating_add(1);
                state.status = CircuitStatus::Open;
                state.last_state_change = Instant::now();
                let next_backoff = 1u32 << state.probe_failures.min(6);
                warn!(
                    probe_failures = state.probe_failures,
                    next_wait_factor = next_backoff,
                    "circuit breaker: reopening (half-open test failed — next probe in {}× timeout)",
                    next_backoff
                );
                crate::metrics::inc_cb_transition("open");
            }
            _ => {}
        }
    }

    fn calculate_success_rate(&self, state: &CircuitState) -> f64 {
        if state.recent_results.is_empty() {
            return 0.0;
        }

        let successes = state.recent_results.iter().filter(|&&x| x).count();
        successes as f64 / state.recent_results.len() as f64
    }

    /// Returns `true` if the circuit is currently closed (normal operation).
    ///
    /// This is a best-effort synchronous read — use `status()` for authoritative state.
    pub fn is_closed_sync(&self) -> bool {
        self.state
            .try_read()
            .map(|s| matches!(s.status, CircuitStatus::Closed))
            .unwrap_or(false)
    }

    /// Returns `true` if the circuit is currently open (rejecting requests).
    pub fn is_open_sync(&self) -> bool {
        self.state
            .try_read()
            .map(|s| matches!(s.status, CircuitStatus::Open))
            .unwrap_or(false)
    }

    /// Returns `true` if the circuit is in half-open state (testing recovery).
    pub fn is_half_open_sync(&self) -> bool {
        self.state
            .try_read()
            .map(|s| matches!(s.status, CircuitStatus::HalfOpen))
            .unwrap_or(false)
    }

    /// Get current circuit status
    pub async fn status(&self) -> CircuitStatus {
        self.state.read().await.status.clone()
    }

    /// Get circuit breaker statistics
    pub async fn stats(&self) -> CircuitBreakerStats {
        let state = self.state.read().await;

        CircuitBreakerStats {
            status: state.status.clone(),
            failures: state.failures,
            successes: state.successes,
            success_rate: self.calculate_success_rate(&state),
            time_in_current_state: state.last_state_change.elapsed(),
            probe_failures: state.probe_failures,
        }
    }

    /// Manually reset circuit breaker to closed state
    pub async fn reset(&self) {
        let mut state = self.state.write().await;
        state.status = CircuitStatus::Closed;
        state.failures = 0;
        state.successes = 0;
        state.probe_failures = 0;
        state.recent_results.clear();
        state.last_state_change = Instant::now();
        info!("circuit breaker: manually reset to closed");
    }

    /// Force circuit to open state (for testing/maintenance)
    pub async fn trip(&self) {
        let mut state = self.state.write().await;
        state.status = CircuitStatus::Open;
        state.last_failure_time = Some(Instant::now());
        state.last_state_change = Instant::now();
        warn!("circuit breaker: manually tripped to open");
    }
}

/// A point-in-time snapshot of [`CircuitBreaker`] metrics.
///
/// Obtain via [`CircuitBreaker::stats`].
#[derive(Debug)]
pub struct CircuitBreakerStats {
    /// Current state of the circuit breaker.
    pub status: CircuitStatus,
    /// Total failures recorded in the current window.
    pub failures: usize,
    /// Total successes recorded in the current window.
    pub successes: usize,
    /// Fraction of recent requests that succeeded (0.0  -  1.0).
    pub success_rate: f64,
    /// Wall-clock time spent in the current state.
    pub time_in_current_state: Duration,
    /// Consecutive half-open probe failures.  The next probe interval is
    /// `timeout * 2^min(probe_failures, 6)`.  Zero when the circuit is
    /// closed or has not yet attempted any half-open probes.
    pub probe_failures: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_opens_on_failures() {
        let breaker = CircuitBreaker::new(3, 0.8, Duration::from_secs(5));

        // Record 3 failures
        for _ in 0..3 {
            let result: Result<(), CircuitBreakerError<()>> =
                breaker.call(|| async { Err(()) }).await;
            assert!(result.is_err());
        }

        // Circuit should be open now
        assert_eq!(breaker.status().await, CircuitStatus::Open);

        // Next request should be rejected
        let result: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
        assert!(matches!(result, Err(CircuitBreakerError::Open)));
    }

    #[tokio::test]
    async fn test_circuit_closes_on_recovery() {
        let breaker = CircuitBreaker::new(2, 0.8, Duration::from_millis(100));

        // Open circuit
        for _ in 0..2 {
            let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
        }
        assert_eq!(breaker.status().await, CircuitStatus::Open);

        // Wait for timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should transition to half-open and allow test request
        let result: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
        assert!(result.is_ok());

        // Record more successes to close circuit
        for _ in 0..5 {
            let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
        }

        assert_eq!(breaker.status().await, CircuitStatus::Closed);
    }

    #[tokio::test]
    async fn test_manual_reset() {
        let breaker = CircuitBreaker::new(2, 0.8, Duration::from_secs(60));

        // Open circuit
        for _ in 0..2 {
            let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
        }
        assert_eq!(breaker.status().await, CircuitStatus::Open);

        // Manual reset
        breaker.reset().await;
        assert_eq!(breaker.status().await, CircuitStatus::Closed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_clears_history_on_half_open() {
        // Open the breaker with 2 failures, then wait for the timeout.
        let breaker = CircuitBreaker::new(2, 0.8, Duration::from_millis(50));

        for _ in 0..2 {
            let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
        }
        assert_eq!(breaker.status().await, CircuitStatus::Open);

        // The recent_results window should have 2 failures recorded.
        {
            let state = breaker.state.read().await;
            assert!(
                !state.recent_results.is_empty(),
                "should have failure history before half-open"
            );
        }

        // Wait for the open timeout to elapse.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // First successful probe transitions to HalfOpen and clears history.
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;

        // After the transition the history must contain only the single probe
        // result (the success above), not the old failures.
        {
            let state = breaker.state.read().await;
            assert!(
                !state.recent_results.contains(&false),
                "old failures must be cleared on half-open transition; results: {:?}",
                state.recent_results
            );
        }
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_failure_reopens() {
        let cb = CircuitBreaker::new(1, 0.8, Duration::from_millis(10));

        // Trip the circuit
        let _ = cb.call(|| async { Err::<(), &str>("fail") }).await;

        // Wait for timeout
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Should be half-open now — a failure here should re-open
        let result = cb.call(|| async { Err::<(), &str>("fail again") }).await;
        assert!(result.is_err());

        // Should be open again
        let result = cb.call(|| async { Ok::<(), &str>(()) }).await;
        assert!(matches!(result, Err(CircuitBreakerError::Open)));
    }

    #[tokio::test]
    async fn test_stats() {
        let breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));

        // Record some results
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;

        let stats = breaker.stats().await;
        assert_eq!(stats.successes, 2);
        // record_success() resets the failure counter when status is Closed,
        // so after the 3rd call (a success) the failure count is back to 0.
        assert_eq!(stats.failures, 0);
        assert_eq!(stats.status, CircuitStatus::Closed);
    }
}
