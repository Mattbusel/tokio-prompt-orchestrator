//! An async circuit breaker: stop calling a dependency that keeps failing,
//! then let a bounded number of probe calls test whether it recovered.
//!
//! ## States
//! - **Closed**: normal operation, calls go through.
//! - **Open**: the dependency is failing, calls are rejected at once without
//!   running.
//! - **Half-open**: after the open timeout, up to `max_probes` calls (one by
//!   default) run as probes. Everyone else is still rejected until the probes
//!   decide: enough successes close the breaker, a failure reopens it with a
//!   longer wait.
//!
//! ## Usage
//!
//! ```
//! use std::time::Duration;
//! use llm_resilience::{CircuitBreaker, CircuitBreakerError};
//! # #[tokio::main]
//! # async fn main() {
//! let breaker = CircuitBreaker::new(5, 0.5, Duration::from_secs(60));
//!
//! match breaker.call(|| async {
//!     // Your operation: an HTTP call, a model call, a database query.
//!     Ok::<&str, &str>("inference result")
//! }).await {
//!     Ok(result) => println!("{result}"),
//!     Err(CircuitBreakerError::Open) => { /* fail fast */ }
//!     Err(CircuitBreakerError::Failed(e)) => eprintln!("operation failed: {e}"),
//! }
//! # }
//! ```
//!
//! ## Generations
//!
//! Every state change starts a new generation. A call remembers the
//! generation it was admitted in, and its outcome only moves the breaker if
//! the breaker is still in that generation. So a slow call admitted while the
//! breaker was closed cannot close a recovering breaker when it finally
//! succeeds, or reopen it when it finally fails: only the probes decide.
//!
//! ## Observing transitions
//!
//! [`CircuitBreaker::with_event_hook`] calls a function on every state change
//! and rejection of one breaker; [`add_global_event_hook`] does it for every
//! breaker in the process (this is how `tokio-prompt-orchestrator` feeds its
//! Prometheus counters). Hooks run after the breaker's lock is released.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use tracing::{debug, info, warn};

/// Something that happened to a circuit breaker, passed to event hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BreakerEvent {
    /// The breaker opened (from closed, or from half-open after a failed probe).
    Opened,
    /// The open timeout elapsed and the breaker is letting probes through.
    HalfOpened,
    /// The probes succeeded and the breaker closed.
    Closed,
    /// A call was rejected without running because the breaker was open (or
    /// half-open with every probe slot taken).
    Rejected,
}

impl BreakerEvent {
    /// A short lowercase label: `"open"`, `"half_open"`, `"closed"` or
    /// `"rejected"`. Handy as a metrics label.
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerEvent::Opened => "open",
            BreakerEvent::HalfOpened => "half_open",
            BreakerEvent::Closed => "closed",
            BreakerEvent::Rejected => "rejected",
        }
    }
}

static GLOBAL_HOOKS: RwLock<Vec<fn(BreakerEvent)>> = RwLock::new(Vec::new());
static HAS_GLOBAL_HOOKS: AtomicBool = AtomicBool::new(false);

/// Call `hook` on every [`BreakerEvent`] of every [`CircuitBreaker`] in this
/// process, for example to count transitions in your metrics system.
///
/// Hooks are additive (several libraries can each add one) and cannot be
/// removed. Adding the same function twice is a no-op. Keep hooks cheap:
/// they run on the caller's task.
pub fn add_global_event_hook(hook: fn(BreakerEvent)) {
    let mut hooks = GLOBAL_HOOKS.write();
    if !hooks.iter().any(|h| std::ptr::fn_addr_eq(*h, hook)) {
        hooks.push(hook);
    }
    HAS_GLOBAL_HOOKS.store(true, Ordering::Release);
}

type EventHook = Arc<dyn Fn(BreakerEvent) + Send + Sync>;

/// A circuit breaker that stops calls to a failing dependency.
///
/// # State machine
///
/// ```text
/// Closed ──(failures ≥ threshold)──► Open
///   ▲                                  │
///   │                                  │ (timeout elapsed)
///   │                                  ▼
///   └──(success_rate ≥ threshold)── HalfOpen ──(probe fails)──► Open (longer wait)
/// ```
///
/// - **Closed**: all calls run.
/// - **Open**: all calls are rejected with [`CircuitBreakerError::Open`]
///   without running.
/// - **HalfOpen**: a bounded number of probe calls run at a time (one by
///   default, see [`with_max_probes`](Self::with_max_probes)); the rest are
///   rejected. A probe that is cancelled frees its slot. Each failed probe
///   doubles the next open wait, up to 64 times the base timeout.
///
/// # Cloning
///
/// `CircuitBreaker` is `Clone + Send + Sync`. All clones share state, so you
/// can keep one to watch or reset a breaker you handed to a layer.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use llm_resilience::{CircuitBreaker, CircuitBreakerError};
///
/// # #[tokio::main]
/// # async fn main() {
/// let cb = CircuitBreaker::new(5, 0.8, Duration::from_secs(30));
///
/// let result = cb.call(|| async {
///     // reqwest::get("https://api.example.com/infer").await
///     Ok::<_, std::io::Error>("response")
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
    state: Arc<Mutex<CircuitState>>,
    config: CircuitBreakerConfig,
}

impl fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = self.state.lock().status;
        f.debug_struct("CircuitBreaker")
            .field("status", &status)
            .field("failure_threshold", &self.config.failure_threshold)
            .field("success_threshold", &self.config.success_threshold)
            .field("timeout", &self.config.timeout)
            .field("window_size", &self.config.window_size)
            .field("max_probes", &self.config.max_probes)
            .finish()
    }
}

#[derive(Clone)]
struct CircuitBreakerConfig {
    /// Number of failures before opening circuit
    failure_threshold: usize,
    /// Success rate threshold (0.0 - 1.0) to close circuit
    success_threshold: f64,
    /// How long to wait before testing if service recovered
    timeout: Duration,
    /// Window size for tracking metrics
    window_size: usize,
    /// Probes allowed at once while half-open.
    max_probes: usize,
    /// Per-breaker event hook.
    hook: Option<EventHook>,
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
    /// Bumped on every state change; outcomes from an older generation are
    /// ignored.
    generation: u64,
    /// Probes running right now (half-open only).
    probes_in_flight: usize,
}

impl CircuitState {
    fn change(&mut self, to: CircuitStatus) {
        self.status = to;
        self.generation = self.generation.wrapping_add(1);
        self.probes_in_flight = 0;
        self.last_state_change = Instant::now();
    }

    fn probe_interval(&self, base: Duration) -> Duration {
        base * (1u32 << self.probe_failures.min(6))
    }
}

/// Current state of a circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CircuitStatus {
    /// Circuit is closed: requests flow through normally.
    Closed,
    /// Circuit is open: requests are rejected immediately without calling the operation.
    Open,
    /// Circuit is half-open: a bounded number of probe requests test recovery.
    HalfOpen,
}

/// The error [`CircuitBreaker::call`] returns.
#[derive(Debug)]
pub enum CircuitBreakerError<E> {
    /// Circuit is open, request rejected
    Open,
    /// Operation failed
    Failed(E),
}

impl<E: fmt::Display> fmt::Display for CircuitBreakerError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CircuitBreakerError::Open => f.write_str("circuit breaker open"),
            CircuitBreakerError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl<E> std::error::Error for CircuitBreakerError<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CircuitBreakerError::Open => None,
            CircuitBreakerError::Failed(e) => Some(e),
        }
    }
}

/// An admitted call. Dropping it without an outcome (the future was
/// cancelled, or the operation panicked) frees its probe slot.
struct Permit<'a> {
    breaker: &'a CircuitBreaker,
    generation: u64,
    probe: bool,
    done: bool,
}

impl Permit<'_> {
    fn finish(mut self, success: bool) {
        self.done = true;
        self.breaker.record(success, self.generation, self.probe);
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if !self.done && self.probe {
            let mut state = self.breaker.state.lock();
            if state.generation == self.generation {
                state.probes_in_flight = state.probes_in_flight.saturating_sub(1);
            }
        }
    }
}

impl CircuitBreaker {
    /// Create a new `CircuitBreaker` in the `Closed` state.
    ///
    /// # Arguments
    ///
    /// * `failure_threshold`: number of consecutive failures that opens the
    ///   circuit. A value of `1` opens on the first error.
    /// * `success_threshold`: success rate (`0.0..=1.0`) the probes must
    ///   reach before the circuit closes from `HalfOpen`. Typical values:
    ///   `0.8` (80 %) or `1.0` (100 %).
    /// * `timeout`: how long to stay `Open` before letting a probe through.
    ///
    /// The default rolling-window size is 100 results
    /// ([`with_window_size`](Self::with_window_size)) and one probe runs at a
    /// time while half-open ([`with_max_probes`](Self::with_max_probes)).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use llm_resilience::CircuitBreaker;
    ///
    /// // Open after 5 failures; require 80 % success rate to close; probe after 60 s.
    /// let cb = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
    /// ```
    pub fn new(failure_threshold: usize, success_threshold: f64, timeout: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(CircuitState {
                status: CircuitStatus::Closed,
                failures: 0,
                successes: 0,
                last_failure_time: None,
                last_state_change: Instant::now(),
                recent_results: VecDeque::new(),
                probe_failures: 0,
                generation: 0,
                probes_in_flight: 0,
            })),
            config: CircuitBreakerConfig {
                failure_threshold,
                success_threshold,
                timeout,
                window_size: 100,
                max_probes: 1,
                hook: None,
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
    /// use llm_resilience::CircuitBreaker;
    ///
    /// // Tighter window: reacts faster to short failure bursts.
    /// let cb = CircuitBreaker::new(5, 0.8, Duration::from_secs(30))
    ///     .with_window_size(20);
    /// ```
    #[must_use]
    pub fn with_window_size(mut self, size: usize) -> Self {
        self.config.window_size = size.max(1);
        self
    }

    /// How many probe calls may run at once while half-open (default 1,
    /// minimum 1). Calls beyond that are rejected with
    /// [`CircuitBreakerError::Open`] until the probes decide.
    ///
    /// Call this before cloning the breaker: clones made earlier keep the
    /// old setting.
    #[must_use]
    pub fn with_max_probes(mut self, n: usize) -> Self {
        self.config.max_probes = n.max(1);
        self
    }

    /// Call `hook` on every [`BreakerEvent`] of this breaker (and its clones
    /// made after this call). Runs after the breaker's lock is released, so
    /// the hook may call back into the breaker.
    ///
    /// ```
    /// use std::sync::atomic::{AtomicUsize, Ordering};
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use llm_resilience::{BreakerEvent, CircuitBreaker};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let opened = Arc::new(AtomicUsize::new(0));
    /// let seen = opened.clone();
    /// let cb = CircuitBreaker::new(1, 0.8, Duration::from_secs(30)).with_event_hook(move |e| {
    ///     if e == BreakerEvent::Opened {
    ///         seen.fetch_add(1, Ordering::Relaxed);
    ///     }
    /// });
    /// let _ = cb.call(|| async { Err::<(), _>("boom") }).await;
    /// assert_eq!(opened.load(Ordering::Relaxed), 1);
    /// # }
    /// ```
    #[must_use]
    pub fn with_event_hook<F>(mut self, hook: F) -> Self
    where
        F: Fn(BreakerEvent) + Send + Sync + 'static,
    {
        self.config.hook = Some(Arc::new(hook));
        self
    }

    fn emit(&self, event: Option<BreakerEvent>) {
        let Some(event) = event else { return };
        if let Some(hook) = &self.config.hook {
            hook(event);
        }
        if HAS_GLOBAL_HOOKS.load(Ordering::Acquire) {
            // Copy the list out so a hook may add another hook.
            let hooks: Vec<fn(BreakerEvent)> = GLOBAL_HOOKS.read().clone();
            for hook in hooks {
                hook(event);
            }
        }
    }

    /// Admit a call, or say why not. Returns the permit and any event to emit.
    fn admit(&self) -> (Option<Permit<'_>>, Option<BreakerEvent>) {
        let mut state = self.state.lock();
        let mut event = None;
        if state.status == CircuitStatus::Open {
            let due = state
                .last_failure_time
                .is_none_or(|t| t.elapsed() >= state.probe_interval(self.config.timeout));
            if !due {
                debug!("circuit breaker: request rejected (open)");
                return (None, Some(BreakerEvent::Rejected));
            }
            // Probe window: clear the history so the success rate counts
            // only post-recovery calls.
            state.change(CircuitStatus::HalfOpen);
            state.recent_results.clear();
            state.failures = 0;
            info!(
                probe_failures = state.probe_failures,
                "circuit breaker: transitioning to half-open"
            );
            event = Some(BreakerEvent::HalfOpened);
        }
        let probe = state.status == CircuitStatus::HalfOpen;
        if probe {
            if state.probes_in_flight >= self.config.max_probes {
                debug!("circuit breaker: request rejected (half-open, probe slots taken)");
                drop(state);
                self.emit(event);
                return (None, Some(BreakerEvent::Rejected));
            }
            state.probes_in_flight += 1;
        }
        let permit = Permit {
            breaker: self,
            generation: state.generation,
            probe,
            done: false,
        };
        (Some(permit), event)
    }

    /// Execute a fallible async operation through the circuit breaker.
    ///
    /// If the circuit is `Open` (or `HalfOpen` with every probe slot taken)
    /// the operation is **not called** and `Err(CircuitBreakerError::Open)`
    /// is returned immediately. Otherwise the closure is invoked; its
    /// `Ok`/`Err` outcome is recorded and may cause a state transition.
    ///
    /// If the returned future is dropped before the operation finishes, no
    /// outcome is recorded and a probe slot it held is freed.
    ///
    /// # Returns
    ///
    /// - `Ok(value)`: operation succeeded.
    /// - `Err(CircuitBreakerError::Open)`: circuit is open; request rejected.
    /// - `Err(CircuitBreakerError::Failed(e))`: operation returned `Err(e)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use llm_resilience::CircuitBreaker;
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
        let (permit, event) = self.admit();
        let Some(permit) = permit else {
            self.emit(event);
            return Err(CircuitBreakerError::Open);
        };
        self.emit(event);

        let result = f().await;
        permit.finish(result.is_ok());
        result.map_err(CircuitBreakerError::Failed)
    }

    fn record(&self, success: bool, generation: u64, probe: bool) {
        let event = {
            let mut state = self.state.lock();
            if state.generation != generation {
                debug!(
                    success,
                    "circuit breaker: outcome from an earlier state ignored"
                );
                return;
            }
            if probe {
                state.probes_in_flight = state.probes_in_flight.saturating_sub(1);
            }
            if success {
                self.record_success(&mut state)
            } else {
                self.record_failure(&mut state)
            }
        };
        self.emit(event);
    }

    fn push_result(&self, state: &mut CircuitState, ok: bool) {
        state.recent_results.push_back(ok);
        while state.recent_results.len() > self.config.window_size {
            state.recent_results.pop_front();
        }
    }

    fn record_success(&self, state: &mut CircuitState) -> Option<BreakerEvent> {
        state.successes += 1;
        self.push_result(state, true);

        debug!(
            status = ?state.status,
            successes = state.successes,
            failures = state.failures,
            "circuit breaker: success recorded"
        );

        match state.status {
            CircuitStatus::HalfOpen => {
                let success_rate = Self::success_rate(state);
                if success_rate >= self.config.success_threshold {
                    state.change(CircuitStatus::Closed);
                    state.failures = 0;
                    state.successes = 0;
                    state.probe_failures = 0; // recovered: reset backoff
                    info!(success_rate, "circuit breaker: closing (service recovered)");
                    return Some(BreakerEvent::Closed);
                }
            }
            CircuitStatus::Closed => {
                // Consecutive-failure count resets on success.
                state.failures = 0;
            }
            CircuitStatus::Open => {}
        }
        None
    }

    fn record_failure(&self, state: &mut CircuitState) -> Option<BreakerEvent> {
        state.failures += 1;
        state.last_failure_time = Some(Instant::now());
        self.push_result(state, false);

        warn!(
            status = ?state.status,
            failures = state.failures,
            threshold = self.config.failure_threshold,
            "circuit breaker: failure recorded"
        );

        match state.status {
            CircuitStatus::Closed if state.failures >= self.config.failure_threshold => {
                state.change(CircuitStatus::Open);
                warn!(
                    failures = state.failures,
                    threshold = self.config.failure_threshold,
                    "circuit breaker: opening (threshold exceeded)"
                );
                Some(BreakerEvent::Opened)
            }
            CircuitStatus::HalfOpen => {
                // A failed probe: back to open and back off longer.
                state.probe_failures = state.probe_failures.saturating_add(1);
                state.change(CircuitStatus::Open);
                let next_backoff = 1u32 << state.probe_failures.min(6);
                warn!(
                    probe_failures = state.probe_failures,
                    next_wait_factor = next_backoff,
                    "circuit breaker: reopening (half-open probe failed, next probe in {}x timeout)",
                    next_backoff
                );
                Some(BreakerEvent::Opened)
            }
            _ => None,
        }
    }

    fn success_rate(state: &CircuitState) -> f64 {
        if state.recent_results.is_empty() {
            return 0.0;
        }
        let successes = state.recent_results.iter().filter(|&&x| x).count();
        successes as f64 / state.recent_results.len() as f64
    }

    /// Returns `true` if the circuit is currently closed (normal operation).
    pub fn is_closed_sync(&self) -> bool {
        self.state.lock().status == CircuitStatus::Closed
    }

    /// Returns `true` if the circuit is currently open (rejecting requests).
    pub fn is_open_sync(&self) -> bool {
        self.state.lock().status == CircuitStatus::Open
    }

    /// Returns `true` if the circuit is in half-open state (testing recovery).
    pub fn is_half_open_sync(&self) -> bool {
        self.state.lock().status == CircuitStatus::HalfOpen
    }

    /// The current status, without awaiting.
    pub fn status_sync(&self) -> CircuitStatus {
        self.state.lock().status
    }

    /// Probe calls running right now (non-zero only while half-open).
    pub fn probes_in_flight(&self) -> usize {
        self.state.lock().probes_in_flight
    }

    /// How long until an open circuit lets the next probe request through.
    ///
    /// `None` when the circuit is closed or half-open. `Some(Duration::ZERO)`
    /// when the probe is already due: the next request will be let through.
    /// Servers use this for a `Retry-After` header on requests the breaker
    /// refuses.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use llm_resilience::CircuitBreaker;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let cb = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
    /// assert_eq!(cb.retry_after_sync(), None);
    /// cb.trip().await;
    /// let wait = cb.retry_after_sync().expect("open");
    /// assert!(wait > Duration::from_secs(59) && wait <= Duration::from_secs(60));
    /// # }
    /// ```
    pub fn retry_after_sync(&self) -> Option<Duration> {
        let state = self.state.lock();
        if state.status != CircuitStatus::Open {
            return None;
        }
        let last_failure = state.last_failure_time?;
        Some(
            state
                .probe_interval(self.config.timeout)
                .saturating_sub(last_failure.elapsed()),
        )
    }

    /// Get current circuit status
    pub async fn status(&self) -> CircuitStatus {
        self.status_sync()
    }

    /// Get circuit breaker statistics
    pub async fn stats(&self) -> CircuitBreakerStats {
        let state = self.state.lock();
        CircuitBreakerStats {
            status: state.status,
            failures: state.failures,
            successes: state.successes,
            success_rate: Self::success_rate(&state),
            time_in_current_state: state.last_state_change.elapsed(),
            probe_failures: state.probe_failures,
        }
    }

    /// Manually reset circuit breaker to closed state
    pub async fn reset(&self) {
        let event = {
            let mut state = self.state.lock();
            let was_closed = state.status == CircuitStatus::Closed;
            state.change(CircuitStatus::Closed);
            state.failures = 0;
            state.successes = 0;
            state.probe_failures = 0;
            state.recent_results.clear();
            info!("circuit breaker: manually reset to closed");
            (!was_closed).then_some(BreakerEvent::Closed)
        };
        self.emit(event);
    }

    /// Force circuit to open state (for testing/maintenance)
    pub async fn trip(&self) {
        let event = {
            let mut state = self.state.lock();
            let was_open = state.status == CircuitStatus::Open;
            state.change(CircuitStatus::Open);
            state.last_failure_time = Some(Instant::now());
            warn!("circuit breaker: manually tripped to open");
            (!was_open).then_some(BreakerEvent::Opened)
        };
        self.emit(event);
    }
}

/// A point-in-time snapshot of [`CircuitBreaker`] metrics.
///
/// Obtain via [`CircuitBreaker::stats`].
#[derive(Debug)]
pub struct CircuitBreakerStats {
    /// Current state of the circuit breaker.
    pub status: CircuitStatus,
    /// Consecutive failures recorded in the current state.
    pub failures: usize,
    /// Successes recorded in the current state.
    pub successes: usize,
    /// Fraction of recent requests that succeeded (0.0 to 1.0).
    pub success_rate: f64,
    /// Wall-clock time spent in the current state.
    pub time_in_current_state: Duration,
    /// Consecutive half-open probe failures. The next probe interval is
    /// `timeout * 2^min(probe_failures, 6)`. Zero when the circuit is
    /// closed or has not yet attempted any half-open probes.
    pub probe_failures: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    async fn fail(cb: &CircuitBreaker) {
        let _: Result<(), CircuitBreakerError<()>> = cb.call(|| async { Err(()) }).await;
    }

    #[tokio::test]
    async fn test_retry_after_counts_down_while_open_and_is_none_when_closed() {
        let breaker = CircuitBreaker::new(2, 0.8, Duration::from_secs(30));
        assert_eq!(breaker.retry_after_sync(), None, "closed breaker has no reset time");
        for _ in 0..2 {
            fail(&breaker).await;
        }
        let wait = breaker.retry_after_sync().expect("open breaker knows its reset time");
        assert!(wait <= Duration::from_secs(30) && wait > Duration::from_secs(29), "{wait:?}");

        // Past the probe interval the probe is due now.
        let quick = CircuitBreaker::new(1, 0.8, Duration::from_millis(20));
        fail(&quick).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(quick.retry_after_sync(), Some(Duration::ZERO));
        breaker.reset().await;
        assert_eq!(breaker.retry_after_sync(), None);
    }

    #[tokio::test]
    async fn test_circuit_opens_on_failures() {
        let breaker = CircuitBreaker::new(3, 0.8, Duration::from_secs(5));
        for _ in 0..3 {
            let result: Result<(), CircuitBreakerError<()>> =
                breaker.call(|| async { Err(()) }).await;
            assert!(result.is_err());
        }
        assert_eq!(breaker.status().await, CircuitStatus::Open);
        let result: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
        assert!(matches!(result, Err(CircuitBreakerError::Open)));
    }

    #[tokio::test]
    async fn test_circuit_closes_on_recovery() {
        let breaker = CircuitBreaker::new(2, 0.8, Duration::from_millis(100));
        for _ in 0..2 {
            fail(&breaker).await;
        }
        assert_eq!(breaker.status().await, CircuitStatus::Open);
        tokio::time::sleep(Duration::from_millis(150)).await;

        let result: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
        assert!(result.is_ok());
        for _ in 0..5 {
            let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
        }
        assert_eq!(breaker.status().await, CircuitStatus::Closed);
    }

    #[tokio::test]
    async fn test_manual_reset() {
        let breaker = CircuitBreaker::new(2, 0.8, Duration::from_secs(60));
        for _ in 0..2 {
            fail(&breaker).await;
        }
        assert_eq!(breaker.status().await, CircuitStatus::Open);
        breaker.reset().await;
        assert_eq!(breaker.status().await, CircuitStatus::Closed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_clears_history_on_half_open() {
        let breaker = CircuitBreaker::new(2, 0.8, Duration::from_millis(50));
        for _ in 0..2 {
            fail(&breaker).await;
        }
        assert_eq!(breaker.status().await, CircuitStatus::Open);
        assert!(
            !breaker.state.lock().recent_results.is_empty(),
            "should have failure history before half-open"
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;

        let state = breaker.state.lock();
        assert!(
            !state.recent_results.contains(&false),
            "old failures must be cleared on half-open transition; results: {:?}",
            state.recent_results
        );
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_failure_reopens() {
        let cb = CircuitBreaker::new(1, 0.8, Duration::from_millis(10));
        let _ = cb.call(|| async { Err::<(), &str>("fail") }).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let result = cb.call(|| async { Err::<(), &str>("fail again") }).await;
        assert!(result.is_err());

        let result = cb.call(|| async { Ok::<(), &str>(()) }).await;
        assert!(matches!(result, Err(CircuitBreakerError::Open)));
    }

    #[tokio::test]
    async fn test_stats() {
        let breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;

        let stats = breaker.stats().await;
        assert_eq!(stats.successes, 2);
        // A success while closed resets the consecutive-failure count.
        assert_eq!(stats.failures, 0);
        assert_eq!(stats.status, CircuitStatus::Closed);
    }

    /// While the one probe runs, every other caller is rejected and the
    /// dependency sees exactly one call.
    #[tokio::test]
    async fn half_open_admits_one_probe_at_a_time() {
        let cb = CircuitBreaker::new(1, 1.0, Duration::from_millis(10));
        fail(&cb).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let probe = {
            let cb = cb.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                cb.call(|| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let _ = release_rx.await;
                    Ok::<_, ()>(())
                })
                .await
            })
        };
        // Wait until the probe is running.
        while cb.probes_in_flight() == 0 {
            tokio::task::yield_now().await;
        }
        assert!(cb.is_half_open_sync());

        for _ in 0..10 {
            let calls = calls.clone();
            let r = cb
                .call(|| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ()>(())
                })
                .await;
            assert!(matches!(r, Err(CircuitBreakerError::Open)));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "only the probe reached the dependency");

        let _ = release_tx.send(());
        assert!(probe.await.expect("join").is_ok());
        assert!(cb.is_closed_sync());
    }

    #[tokio::test]
    async fn with_max_probes_allows_that_many() {
        let cb = CircuitBreaker::new(1, 1.0, Duration::from_millis(10)).with_max_probes(2);
        fail(&cb).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let (tx, rx) = tokio::sync::watch::channel(false);
        let mut probes = Vec::new();
        for _ in 0..2 {
            let cb = cb.clone();
            let mut rx = rx.clone();
            probes.push(tokio::spawn(async move {
                cb.call(|| async move {
                    let _ = rx.wait_for(|go| *go).await;
                    Ok::<_, ()>(())
                })
                .await
            }));
        }
        while cb.probes_in_flight() < 2 {
            tokio::task::yield_now().await;
        }
        let third: Result<(), CircuitBreakerError<()>> = cb.call(|| async { Ok(()) }).await;
        assert!(matches!(third, Err(CircuitBreakerError::Open)));
        let _ = tx.send(true);
        for p in probes {
            assert!(p.await.expect("join").is_ok());
        }
        assert!(cb.is_closed_sync());
    }

    /// A probe whose future is dropped frees its slot, so the breaker does
    /// not stay half-open with nobody allowed through.
    #[tokio::test]
    async fn cancelled_probe_frees_its_slot() {
        let cb = CircuitBreaker::new(1, 1.0, Duration::from_millis(10));
        fail(&cb).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let hung = cb.call(std::future::pending::<Result<(), ()>>);
        let timed_out = tokio::time::timeout(Duration::from_millis(10), hung).await;
        assert!(timed_out.is_err());
        assert!(cb.is_half_open_sync());
        assert_eq!(cb.probes_in_flight(), 0);

        let r: Result<(), CircuitBreakerError<()>> = cb.call(|| async { Ok(()) }).await;
        assert!(r.is_ok());
        assert!(cb.is_closed_sync());
    }

    /// A slow call admitted while closed must not close a recovering breaker
    /// when it finally succeeds.
    #[tokio::test]
    async fn stale_success_cannot_close_a_recovering_breaker() {
        let cb = CircuitBreaker::new(1, 1.0, Duration::from_millis(10));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let slow = {
            let cb = cb.clone();
            tokio::spawn(async move {
                cb.call(|| async move {
                    let _ = started_tx.send(());
                    let _ = rx.await;
                    Ok::<_, ()>(())
                })
                .await
            })
        };
        started_rx.await.expect("slow call admitted while closed");
        fail(&cb).await; // opens
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Start a probe that hangs, so the breaker stays half-open.
        let probe_cb = cb.clone();
        let probe = tokio::spawn(async move {
            probe_cb
                .call(std::future::pending::<Result<(), ()>>)
                .await
        });
        while cb.probes_in_flight() == 0 {
            tokio::task::yield_now().await;
        }

        let _ = tx.send(());
        assert!(slow.await.expect("join").is_ok());
        assert!(cb.is_half_open_sync(), "the stale success must not close the breaker");
        probe.abort();
    }

    #[tokio::test]
    async fn event_hook_sees_transitions_and_rejections() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let cb = CircuitBreaker::new(1, 1.0, Duration::from_millis(10))
            .with_event_hook(move |e| sink.lock().push(e));
        fail(&cb).await;
        let _: Result<(), CircuitBreakerError<()>> = cb.call(|| async { Ok(()) }).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _: Result<(), CircuitBreakerError<()>> = cb.call(|| async { Ok(()) }).await;
        assert_eq!(
            *events.lock(),
            vec![
                BreakerEvent::Opened,
                BreakerEvent::Rejected,
                BreakerEvent::HalfOpened,
                BreakerEvent::Closed
            ]
        );
    }

    #[test]
    fn error_display_and_source() {
        let open: CircuitBreakerError<std::io::Error> = CircuitBreakerError::Open;
        assert_eq!(open.to_string(), "circuit breaker open");
        let failed = CircuitBreakerError::Failed(std::io::Error::other("boom"));
        assert_eq!(failed.to_string(), "boom");
        assert!(std::error::Error::source(&failed).is_some());
    }
}
