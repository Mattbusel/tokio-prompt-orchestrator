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
//!     // Your operation — replace with a real async call
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

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Circuit breaker for preventing cascading failures
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
    recent_results: Vec<bool>,
}

/// Current state of a circuit breaker.
#[derive(Debug, Clone, PartialEq)]
pub enum CircuitStatus {
    /// Circuit is closed — requests flow through normally.
    Closed,
    /// Circuit is open — requests are rejected immediately without calling the operation.
    Open,
    /// Circuit is half-open — one probe request is allowed through to test recovery.
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
    /// Create new circuit breaker
    ///
    /// # Arguments
    /// * `failure_threshold` - Number of consecutive failures before opening
    /// * `success_threshold` - Success rate (0.0-1.0) needed to close circuit
    /// * `timeout` - How long to wait in open state before testing recovery
    pub fn new(failure_threshold: usize, success_threshold: f64, timeout: Duration) -> Self {
        Self {
            state: Arc::new(RwLock::new(CircuitState {
                status: CircuitStatus::Closed,
                failures: 0,
                successes: 0,
                last_failure_time: None,
                last_state_change: Instant::now(),
                recent_results: Vec::new(),
            })),
            config: CircuitBreakerConfig {
                failure_threshold,
                success_threshold,
                timeout,
                window_size: 100,
            },
        }
    }

    /// Execute operation through circuit breaker
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
                    // Check if timeout elapsed
                    if let Some(last_failure) = state.last_failure_time {
                        if last_failure.elapsed() >= self.config.timeout {
                            // Try half-open — clear window so success-rate is
                            // calculated only from post-recovery requests.
                            state.status = CircuitStatus::HalfOpen;
                            state.recent_results.clear();
                            state.last_state_change = Instant::now();
                            info!("circuit breaker: transitioning to half-open");
                        } else {
                            // Still open, reject
                            debug!("circuit breaker: request rejected (open)");
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
        state.recent_results.push(true);
        if state.recent_results.len() > self.config.window_size {
            state.recent_results.remove(0);
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
                    state.last_state_change = Instant::now();
                    info!(
                        success_rate = success_rate,
                        "circuit breaker: closing (service recovered)"
                    );
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
        state.recent_results.push(false);
        if state.recent_results.len() > self.config.window_size {
            state.recent_results.remove(0);
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
                }
            }
            CircuitStatus::HalfOpen => {
                // Failure in half-open, go back to open
                state.status = CircuitStatus::Open;
                state.last_state_change = Instant::now();
                warn!("circuit breaker: reopening (half-open test failed)");
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
        }
    }

    /// Manually reset circuit breaker to closed state
    pub async fn reset(&self) {
        let mut state = self.state.write().await;
        state.status = CircuitStatus::Closed;
        state.failures = 0;
        state.successes = 0;
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

/// Circuit breaker statistics
#[derive(Debug)]
pub struct CircuitBreakerStats {
    /// Current state of the circuit breaker.
    pub status: CircuitStatus,
    /// Total failures recorded in the current window.
    pub failures: usize,
    /// Total successes recorded in the current window.
    pub successes: usize,
    /// Fraction of recent requests that succeeded (0.0 – 1.0).
    pub success_rate: f64,
    /// Wall-clock time spent in the current state.
    pub time_in_current_state: Duration,
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
