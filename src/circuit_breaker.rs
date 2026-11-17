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
//! ## Recovery
//!
//! While HalfOpen only a bounded number of probe calls run at once (one by default,
//! [`CircuitBreaker::with_max_probes`]). Use [`CircuitBreaker::try_acquire`]: it
//! admits a call and hands back a [`CallPermit`] tied to the breaker's current
//! generation, under the same lock that makes the state transitions. Report the
//! outcome on the permit. A call admitted before the breaker tripped carries an older
//! generation, so when it finishes during recovery its result goes into the failure
//! window but cannot close (or reopen) the breaker; only the probes decide. A probe
//! permit dropped without an outcome (a cancelled future) frees its slot.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
//!
//! let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
//!
//! if let Some(permit) = cb.try_acquire() {
//!     let ok = true; // ... make the provider call ...
//!     if ok { permit.success(); } else { permit.failure(); }
//! }
//!
//! // The older boolean API still works (probes are bounded there too):
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
    /// Timeout elapsed — a bounded number of probe requests is permitted.
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
    /// Bumped on every state transition; a permit records the value it was admitted
    /// under, so outcomes of calls from an earlier state can be told apart.
    generation: u64,
    /// HalfOpen probe calls admitted and not yet finished.
    probes_in_flight: usize,
}

impl Inner {
    fn new() -> Self {
        Self {
            state: CircuitState::Closed,
            window: VecDeque::new(),
            opened_at: None,
            half_open_successes: 0,
            generation: 0,
            probes_in_flight: 0,
        }
    }

    fn transition(&mut self, to: CircuitState, now: Instant) {
        self.state = to;
        self.generation = self.generation.wrapping_add(1);
        self.half_open_successes = 0;
        self.probes_in_flight = 0;
        self.opened_at = if to == CircuitState::Open { Some(now) } else { None };
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
    max_probes: usize,
}

/// What [`CircuitBreaker::admit`] decided.
#[derive(Clone, Copy)]
struct Admission {
    generation: u64,
    probe: bool,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration. One probe call at a
    /// time is let through while HalfOpen.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(Inner::new()),
            max_probes: 1,
        }
    }

    /// Let up to `n` probe calls (at least 1) run at once while HalfOpen.
    pub fn with_max_probes(mut self, n: usize) -> Self {
        self.max_probes = n.max(1);
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admission decision and bookkeeping under one lock.
    fn admit(&self) -> Option<Admission> {
        let mut g = self.lock();
        let now = Instant::now();
        if g.state == CircuitState::Open {
            match g.opened_at {
                Some(t) if t.elapsed() >= self.config.timeout_duration => {
                    g.transition(CircuitState::HalfOpen, now);
                }
                _ => return None,
            }
        }
        match g.state {
            CircuitState::Closed => Some(Admission { generation: g.generation, probe: false }),
            CircuitState::HalfOpen => {
                if g.probes_in_flight >= self.max_probes {
                    return None;
                }
                g.probes_in_flight += 1;
                Some(Admission { generation: g.generation, probe: true })
            }
            CircuitState::Open => None,
        }
    }

    /// Admit a call, or `None` when the breaker rejects it (Open, or HalfOpen with
    /// every probe slot taken). Report the call's outcome on the returned permit.
    pub fn try_acquire(&self) -> Option<CallPermit<'_>> {
        self.admit().map(|a| CallPermit { breaker: self, admission: a, done: false })
    }

    /// Returns `true` when a request should be allowed through.
    ///
    /// - **Closed**: always allowed.
    /// - **Open**: not allowed unless the timeout has elapsed, in which case
    ///   the breaker transitions to HalfOpen and a probe request is allowed.
    /// - **HalfOpen**: allowed while a probe slot is free; the slot is taken until
    ///   [`record_success`](Self::record_success) or
    ///   [`record_failure`](Self::record_failure) is called.
    ///
    /// This API cannot tell a probe's outcome from that of a call admitted before the
    /// breaker tripped; [`try_acquire`](Self::try_acquire) can, and is preferred.
    pub fn is_allowed(&self) -> bool {
        self.admit().is_some()
    }

    /// Record a successful call outcome and drive state transitions.
    pub fn record_success(&self) {
        self.record(true, None);
    }

    /// Record a failed call outcome and drive state transitions.
    pub fn record_failure(&self) {
        self.record(false, None);
    }

    /// Record an outcome. `admission` is `None` for the boolean API, which is treated
    /// as a call of the current generation. Returns `true` when the breaker is Open
    /// after this outcome.
    fn record(&self, ok: bool, admission: Option<Admission>) -> bool {
        let mut g = self.lock();
        let now = Instant::now();
        g.evict_old(self.config.window_duration);
        g.window.push_back((now, ok));
        let current = admission.is_none_or(|a| a.generation == g.generation);
        let probe = admission.map_or(g.state == CircuitState::HalfOpen, |a| a.probe);

        match g.state {
            CircuitState::HalfOpen if current && probe => {
                g.probes_in_flight = g.probes_in_flight.saturating_sub(1);
                if ok {
                    g.half_open_successes += 1;
                    if g.half_open_successes >= self.config.success_threshold {
                        g.transition(CircuitState::Closed, now);
                    }
                } else {
                    // Any probe failure reopens the breaker.
                    g.transition(CircuitState::Open, now);
                }
            }
            CircuitState::Closed if !ok => {
                let total = g.total_count();
                let failures = g.failure_count();
                if total >= self.config.min_calls && failures >= self.config.failure_threshold {
                    g.transition(CircuitState::Open, now);
                }
            }
            // Outcomes of calls admitted under an earlier state, and anything while
            // Open, stay in the window but do not move the breaker.
            _ => {}
        }
        g.state == CircuitState::Open
    }

    /// A probe permit dropped without an outcome: free its slot, record nothing.
    fn release(&self, admission: Admission) {
        if !admission.probe {
            return;
        }
        let mut g = self.lock();
        if g.generation == admission.generation && g.state == CircuitState::HalfOpen {
            g.probes_in_flight = g.probes_in_flight.saturating_sub(1);
        }
    }

    /// Return the current state of the circuit breaker.
    pub fn state(&self) -> CircuitState {
        self.lock().state
    }

    /// HalfOpen probe calls admitted and not yet finished (0 outside HalfOpen).
    pub fn probes_in_flight(&self) -> usize {
        self.lock().probes_in_flight
    }

    /// Return the failure rate (0.0–1.0) in the current sliding window.
    ///
    /// Returns `0.0` if no calls have been recorded yet.
    pub fn failure_rate(&self) -> f64 {
        let mut g = self.lock();
        g.evict_old(self.config.window_duration);
        let total = g.total_count();
        if total == 0 {
            return 0.0;
        }
        g.failure_count() as f64 / total as f64
    }
}

/// Admission to make one call through a [`CircuitBreaker`], from
/// [`CircuitBreaker::try_acquire`].
///
/// Report the outcome with [`success`](Self::success) or [`failure`](Self::failure).
/// Dropping the permit without either (the call was cancelled) records nothing and,
/// for a HalfOpen probe, frees the probe slot.
#[must_use = "report the call's outcome with success() or failure()"]
pub struct CallPermit<'a> {
    breaker: &'a CircuitBreaker,
    admission: Admission,
    done: bool,
}

impl CallPermit<'_> {
    /// `true` when this call is a HalfOpen recovery probe.
    pub fn is_probe(&self) -> bool {
        self.admission.probe
    }

    /// The call succeeded. Returns `true` if the breaker is Open afterwards.
    pub fn success(mut self) -> bool {
        self.done = true;
        self.breaker.record(true, Some(self.admission))
    }

    /// The call failed. Returns `true` if the breaker is Open afterwards (this failure
    /// tripped it, or it already was), so a retry loop can stop instead of sleeping.
    pub fn failure(mut self) -> bool {
        self.done = true;
        self.breaker.record(false, Some(self.admission))
    }
}

impl Drop for CallPermit<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.breaker.release(self.admission);
        }
    }
}

impl std::fmt::Debug for CallPermit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallPermit")
            .field("generation", &self.admission.generation)
            .field("probe", &self.admission.probe)
            .finish()
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
    // ── permits: bounded probes and stale outcomes ──────────────────────────────

    fn tripped(success_threshold: usize) -> CircuitBreaker {
        let cb = CircuitBreaker::new(config_with(2, success_threshold, 2));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        cb
    }

    #[test]
    fn half_open_admits_one_probe_at_a_time() {
        let cb = tripped(1);
        thread::sleep(Duration::from_millis(60));
        let probe = cb.try_acquire().expect("first call after the timeout is the probe");
        assert!(probe.is_probe());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        assert!(cb.try_acquire().is_none(), "second concurrent probe rejected");
        assert!(!cb.is_allowed(), "boolean API respects the probe slot too");
        assert!(!probe.success());
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.try_acquire().is_some());
    }

    #[test]
    fn max_probes_is_configurable() {
        let cb = CircuitBreaker::new(config_with(2, 3, 2)).with_max_probes(2);
        cb.record_failure();
        cb.record_failure();
        thread::sleep(Duration::from_millis(60));
        let a = cb.try_acquire().unwrap();
        let b = cb.try_acquire().unwrap();
        assert!(cb.try_acquire().is_none());
        assert_eq!(cb.probes_in_flight(), 2);
        a.success();
        let c = cb.try_acquire().expect("a finished probe frees its slot");
        b.success();
        c.success();
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn dropped_probe_frees_its_slot() {
        let cb = tripped(1);
        thread::sleep(Duration::from_millis(60));
        let probe = cb.try_acquire().unwrap();
        drop(probe); // cancelled future
        assert_eq!(cb.probes_in_flight(), 0);
        assert_eq!(cb.state(), CircuitState::HalfOpen, "nothing recorded");
        assert!(cb.try_acquire().is_some());
    }

    #[test]
    fn call_started_before_the_trip_cannot_close_the_breaker() {
        let cb = CircuitBreaker::new(config_with(2, 1, 2));
        let slow = cb.try_acquire().unwrap(); // admitted while Closed
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        thread::sleep(Duration::from_millis(60));
        let probe = cb.try_acquire().unwrap();
        // The pre-trip call finishes during recovery, with success_threshold = 1.
        slow.success();
        assert_eq!(cb.state(), CircuitState::HalfOpen, "stale success ignored for recovery");
        assert_eq!(cb.probes_in_flight(), 1, "the real probe still holds the slot");
        probe.success();
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn call_started_before_the_trip_cannot_reopen_during_recovery() {
        let cb = CircuitBreaker::new(config_with(2, 1, 2));
        let slow = cb.try_acquire().unwrap();
        cb.record_failure();
        cb.record_failure();
        thread::sleep(Duration::from_millis(60));
        let probe = cb.try_acquire().unwrap();
        assert!(!slow.failure(), "stale failure leaves the breaker HalfOpen");
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        assert!(!probe.success());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn failing_probe_reopens_and_reports_open() {
        let cb = tripped(1);
        thread::sleep(Duration::from_millis(60));
        let probe = cb.try_acquire().unwrap();
        assert!(probe.failure(), "failure() says the breaker is open");
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(cb.try_acquire().is_none());
    }

    #[test]
    fn tripping_failure_reports_open() {
        let cb = CircuitBreaker::new(config_with(2, 1, 2));
        assert!(!cb.try_acquire().unwrap().failure());
        assert!(cb.try_acquire().unwrap().failure(), "this failure opened the breaker");
    }

    #[test]
    fn concurrent_probe_admission_is_bounded() {
        let cb = Arc::new(tripped(1));
        thread::sleep(Duration::from_millis(60));
        let admitted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let (cb, admitted, barrier) = (Arc::clone(&cb), Arc::clone(&admitted), Arc::clone(&barrier));
                thread::spawn(move || {
                    barrier.wait();
                    if let Some(p) = cb.try_acquire() {
                        admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        drop(p);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(admitted.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
