//! # Circuit Breaker State Machine
//!
//! Per-model circuit breaker with sliding-window failure tracking, three-state
//! FSM, and a [`CircuitBreakerRegistry`] keyed by model name.
//!
//! ## States
//!
//! ```text
//! Closed ──(failure_rate > threshold)──> Open
//! Open   ──(timeout elapsed)──────────> HalfOpen
//! HalfOpen ──(success_threshold met)──> Closed
//! HalfOpen ──(any failure)────────────> Open
//! ```
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
//!
//! let cfg = CircuitBreakerConfig::default();
//! let cb = CircuitBreaker::new(cfg);
//!
//! assert!(cb.is_allowed());
//! cb.record_failure();
//! ```

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;

// ── State ─────────────────────────────────────────────────────────────────────

/// Current state of a circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests are allowed.
    Closed,
    /// Failure rate exceeded threshold — all requests are rejected.
    Open,
    /// Timeout elapsed — a single probe request is permitted.
    HalfOpen,
}

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for a [`CircuitBreaker`].
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of failures in the sliding window required to trip the breaker.
    pub failure_threshold: usize,
    /// Number of consecutive successes in HalfOpen required to close the
    /// breaker again.
    pub success_threshold: usize,
    /// How long the breaker stays Open before moving to HalfOpen.
    pub timeout_duration: Duration,
    /// Width of the sliding window used for failure-rate calculation.
    pub window_duration: Duration,
    /// Minimum number of calls in the window before the failure rate is
    /// evaluated (avoids tripping on the very first request).
    pub min_calls: usize,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 2,
            timeout_duration: Duration::from_secs(30),
            window_duration: Duration::from_secs(60),
            min_calls: 5,
        }
    }
}

// ── Inner mutable state ───────────────────────────────────────────────────────

struct Inner {
    state: CircuitState,
    /// Ring buffer of (timestamp, success) outcome pairs.
    window: VecDeque<(Instant, bool)>,
    /// When the breaker entered the Open state (used to compute timeout).
    opened_at: Option<Instant>,
    /// Consecutive successes recorded while in HalfOpen.
    half_open_successes: usize,
}

impl Inner {
    fn new() -> Self {
        Self {
            state: CircuitState::Closed,
            window: VecDeque::new(),
            opened_at: None,
            half_open_successes: 0,
        }
    }

    /// Remove entries older than `window_duration`.
    fn evict_old(&mut self, window_duration: Duration) {
        let cutoff = Instant::now() - window_duration;
        while let Some(&(ts, _)) = self.window.front() {
            if ts < cutoff {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    /// Number of failures currently in the sliding window.
    fn failure_count(&self) -> usize {
        self.window.iter().filter(|(_, ok)| !ok).count()
    }

    /// Total calls in the sliding window.
    fn total_count(&self) -> usize {
        self.window.len()
    }
}

// ── CircuitBreaker ────────────────────────────────────────────────────────────

/// A single per-model circuit breaker.
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    inner: Mutex<Inner>,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(Inner::new()),
        }
    }

    /// Returns `true` when a request should be allowed through.
    ///
    /// - **Closed**: always allowed.
    /// - **Open**: not allowed unless the timeout has elapsed, in which case
    ///   the breaker transitions to HalfOpen and the probe request is allowed.
    /// - **HalfOpen**: allowed (the probe).
    pub fn is_allowed(&self) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match g.state {
            CircuitState::Closed => true,
            CircuitState::HalfOpen => true,
            CircuitState::Open => {
                if let Some(opened_at) = g.opened_at {
                    if opened_at.elapsed() >= self.config.timeout_duration {
                        g.state = CircuitState::HalfOpen;
                        g.half_open_successes = 0;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        }
    }

    /// Record a successful call outcome and drive state transitions.
    pub fn record_success(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        g.evict_old(self.config.window_duration);
        g.window.push_back((now, true));

        match g.state {
            CircuitState::HalfOpen => {
                g.half_open_successes += 1;
                if g.half_open_successes >= self.config.success_threshold {
                    g.state = CircuitState::Closed;
                    g.opened_at = None;
                    g.half_open_successes = 0;
                }
            }
            CircuitState::Closed | CircuitState::Open => {}
        }
    }

    /// Record a failed call outcome and drive state transitions.
    pub fn record_failure(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        g.evict_old(self.config.window_duration);
        g.window.push_back((now, false));

        match g.state {
            CircuitState::HalfOpen => {
                // Any failure in HalfOpen immediately reopens the breaker.
                g.state = CircuitState::Open;
                g.opened_at = Some(now);
                g.half_open_successes = 0;
            }
            CircuitState::Closed => {
                let total = g.total_count();
                let failures = g.failure_count();
                if total >= self.config.min_calls
                    && failures >= self.config.failure_threshold
                {
                    g.state = CircuitState::Open;
                    g.opened_at = Some(now);
                }
            }
            CircuitState::Open => {}
        }
    }

    /// Return the current state of the circuit breaker.
    pub fn state(&self) -> CircuitState {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.state
    }

    /// Return the failure rate (0.0–1.0) in the current sliding window.
    ///
    /// Returns `0.0` if no calls have been recorded yet.
    pub fn failure_rate(&self) -> f64 {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.evict_old(self.config.window_duration);
        let total = g.total_count();
        if total == 0 {
            return 0.0;
        }
        g.failure_count() as f64 / total as f64
    }
}

// ── CircuitBreakerRegistry ────────────────────────────────────────────────────

/// A registry of circuit breakers keyed by model name.
///
/// New entries are created lazily with the supplied default configuration.
pub struct CircuitBreakerRegistry {
    map: DashMap<String, Arc<CircuitBreaker>>,
    default_config: CircuitBreakerConfig,
}

impl CircuitBreakerRegistry {
    /// Create a registry; new breakers will be created with `default_config`.
    pub fn new(default_config: CircuitBreakerConfig) -> Self {
        Self {
            map: DashMap::new(),
            default_config,
        }
    }

    /// Get (or lazily create) the circuit breaker for `model`.
    pub fn get(&self, model: &str) -> Arc<CircuitBreaker> {
        if let Some(cb) = self.map.get(model) {
            return Arc::clone(&cb);
        }
        let cb = Arc::new(CircuitBreaker::new(self.default_config.clone()));
        self.map.insert(model.to_string(), Arc::clone(&cb));
        cb
    }

    /// Register a circuit breaker for `model` with a specific configuration.
    pub fn register(&self, model: String, config: CircuitBreakerConfig) {
        self.map
            .insert(model, Arc::new(CircuitBreaker::new(config)));
    }

    /// Return the number of models tracked by this registry.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns `true` if no models have been registered yet.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn config_with(
        failure_threshold: usize,
        success_threshold: usize,
        min_calls: usize,
    ) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold,
            success_threshold,
            timeout_duration: Duration::from_millis(50),
            window_duration: Duration::from_secs(60),
            min_calls,
        }
    }

    #[test]
    fn starts_closed_and_allows_requests() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.is_allowed());
    }

    #[test]
    fn closed_to_open_on_failure_threshold() {
        // threshold=3, min_calls=3
        let cb = CircuitBreaker::new(config_with(3, 2, 3));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed, "not enough failures yet");
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(!cb.is_allowed());
    }

    #[test]
    fn open_to_half_open_after_timeout() {
        let cb = CircuitBreaker::new(config_with(3, 2, 3));
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);
        // Wait for timeout (50ms).
        thread::sleep(Duration::from_millis(80));
        assert!(cb.is_allowed(), "should probe after timeout");
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn half_open_to_closed_on_successes() {
        let cb = CircuitBreaker::new(config_with(3, 2, 3));
        for _ in 0..3 {
            cb.record_failure();
        }
        thread::sleep(Duration::from_millis(80));
        let _ = cb.is_allowed(); // transition to HalfOpen
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::HalfOpen, "one success not enough");
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn half_open_to_open_on_failure() {
        let cb = CircuitBreaker::new(config_with(3, 2, 3));
        for _ in 0..3 {
            cb.record_failure();
        }
        thread::sleep(Duration::from_millis(80));
        let _ = cb.is_allowed(); // transition to HalfOpen
        cb.record_failure(); // immediate reopen
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn failure_rate_calculation() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        assert_eq!(cb.failure_rate(), 0.0);
        cb.record_success();
        cb.record_failure();
        let rate = cb.failure_rate();
        assert!((rate - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn successes_do_not_trip_breaker() {
        let cb = CircuitBreaker::new(config_with(3, 2, 3));
        for _ in 0..10 {
            cb.record_success();
        }
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.is_allowed());
    }

    #[test]
    fn registry_lazy_creation() {
        let registry =
            CircuitBreakerRegistry::new(CircuitBreakerConfig::default());
        let cb = registry.get("gpt-4o");
        assert!(cb.is_allowed());
        assert_eq!(registry.len(), 1);
        // Second get returns the same Arc.
        let cb2 = registry.get("gpt-4o");
        assert!(Arc::ptr_eq(&cb, &cb2));
    }

    #[test]
    fn registry_register_custom_config() {
        let registry =
            CircuitBreakerRegistry::new(CircuitBreakerConfig::default());
        registry.register("claude-3".to_string(), config_with(1, 1, 1));
        let cb = registry.get("claude-3");
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }
}
