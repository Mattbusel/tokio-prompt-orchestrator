//! # AIMD Adaptive Admission Control
//!
//! Additive-Increase / Multiplicative-Decrease concurrency limiter with
//! gradient-based overload detection and a rolling acceptance-rate window.
//!
//! ## Design
//!
//! [`AdmissionController`] tracks an adaptive concurrency `limit` that starts
//! at `min_limit` and grows linearly on success (`+additive_increase`) and
//! shrinks geometrically on overload (`* multiplicative_decrease`).
//!
//! Callers call [`AdmissionController::try_acquire`] which returns an
//! [`AcquireGuard`] RAII handle.  Dropping the guard decrements `in_flight`.
//! The caller then signals success via [`AcquireGuard::success`] (or lets it
//! drop silently for a neutral outcome).
//!
//! Latency gradient: every time a request completes the controller computes
//! the ratio of the current latency against an exponential moving average.
//! When the ratio exceeds `gradient_threshold` (default 2.0) it calls
//! `on_overload`.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::admission_control::{
//!     AdmissionController, AdmissionConfig,
//! };
//! use std::time::Duration;
//!
//! let ctrl = AdmissionController::new(AdmissionConfig::default());
//! if let Some(guard) = ctrl.try_acquire() {
//!     guard.success(); // signal a successful, fast response
//! }
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for [`AdmissionController`].
#[derive(Debug, Clone)]
pub struct AdmissionConfig {
    /// Starting (and minimum) concurrency limit.
    pub min_limit: usize,
    /// Hard cap on the concurrency limit.
    pub max_limit: usize,
    /// Amount added to `limit` on each successful call (additive increase).
    pub additive_increase: usize,
    /// Factor applied to `limit` on overload (multiplicative decrease, 0–1).
    pub multiplicative_decrease: f64,
    /// Ratio of current latency to EWMA latency above which overload is
    /// declared (gradient trigger).
    pub gradient_threshold: f64,
    /// EWMA smoothing factor for the latency average (0–1; smaller = slower).
    pub latency_ewma_alpha: f64,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            min_limit: 4,
            max_limit: 512,
            additive_increase: 1,
            multiplicative_decrease: 0.9,
            gradient_threshold: 2.0,
            latency_ewma_alpha: 0.1,
        }
    }
}

// ── Rolling acceptance window ─────────────────────────────────────────────────

/// 100-sample rolling acceptance window for computing `acceptance_rate`.
struct AcceptanceWindow {
    buf: [bool; 100],
    head: usize,
    filled: bool,
}

impl AcceptanceWindow {
    fn new() -> Self {
        Self {
            buf: [false; 100],
            head: 0,
            filled: false,
        }
    }

    fn record(&mut self, accepted: bool) {
        self.buf[self.head] = accepted;
        self.head = (self.head + 1) % 100;
        if self.head == 0 {
            self.filled = true;
        }
    }

    fn rate(&self) -> f64 {
        let n = if self.filled { 100 } else { self.head };
        if n == 0 {
            return 1.0;
        }
        let accepted = self.buf[..n].iter().filter(|&&b| b).count();
        accepted as f64 / n as f64
    }
}

// ── Inner mutable state ───────────────────────────────────────────────────────

struct Inner {
    /// Current adaptive limit (cached copy for atomic-free fast path).
    limit_snapshot: usize,
    /// EWMA of request latency in nanoseconds.
    latency_ewma_ns: f64,
    /// Rolling acceptance window.
    acceptance: AcceptanceWindow,
}

// ── AdmissionController ───────────────────────────────────────────────────────

/// AIMD adaptive concurrency controller.
pub struct AdmissionController {
    config: AdmissionConfig,
    /// Authoritative concurrency limit (updated by on_success / on_overload).
    limit: AtomicUsize,
    /// Number of requests currently in flight.
    in_flight: Arc<AtomicUsize>,
    inner: Mutex<Inner>,
}

impl AdmissionController {
    /// Create a new controller with the given configuration.
    pub fn new(config: AdmissionConfig) -> Arc<Self> {
        let start = config.min_limit;
        Arc::new(Self {
            config: config.clone(),
            limit: AtomicUsize::new(start),
            in_flight: Arc::new(AtomicUsize::new(0)),
            inner: Mutex::new(Inner {
                limit_snapshot: start,
                latency_ewma_ns: 0.0,
                acceptance: AcceptanceWindow::new(),
            }),
        })
    }

    /// Try to acquire a slot.
    ///
    /// Returns `None` if `in_flight >= limit`, otherwise returns an
    /// [`AcquireGuard`] that decrements `in_flight` on drop.
    pub fn try_acquire(self: &Arc<Self>) -> Option<AcquireGuard> {
        let limit = self.limit.load(Ordering::Relaxed);
        // Optimistic increment then check.
        let prev = self.in_flight.fetch_add(1, Ordering::AcqRel);
        if prev < limit {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.acceptance.record(true);
            drop(g);
            Some(AcquireGuard {
                ctrl: Arc::clone(self),
                start: Instant::now(),
                signalled: false,
            })
        } else {
            // Revert the increment — no slot available.
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.acceptance.record(false);
            None
        }
    }

    /// Additive increase: grow the limit by `additive_increase`, up to
    /// `max_limit`.
    pub fn on_success(&self) {
        let current = self.limit.load(Ordering::Relaxed);
        let next = (current + self.config.additive_increase)
            .min(self.config.max_limit);
        self.limit.store(next, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.limit_snapshot = next;
    }

    /// Multiplicative decrease: shrink the limit by `multiplicative_decrease`,
    /// floored at `min_limit`.
    pub fn on_overload(&self) {
        let current = self.limit.load(Ordering::Relaxed);
        let next = ((current as f64 * self.config.multiplicative_decrease)
            .floor() as usize)
            .max(self.config.min_limit);
        self.limit.store(next, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.limit_snapshot = next;
    }

    /// Record a completed request with its observed latency and apply AIMD +
    /// gradient logic.
    ///
    /// Called internally by [`AcquireGuard::success`].
    fn record_latency(&self, latency: Duration) {
        let ns = latency.as_nanos() as f64;
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.latency_ewma_ns == 0.0 {
            g.latency_ewma_ns = ns;
        } else {
            let alpha = self.config.latency_ewma_alpha;
            g.latency_ewma_ns = alpha * ns + (1.0 - alpha) * g.latency_ewma_ns;
        }
        let ewma = g.latency_ewma_ns;
        drop(g);

        if ewma > 0.0 && ns / ewma > self.config.gradient_threshold {
            self.on_overload();
        } else {
            self.on_success();
        }
    }

    /// Return a snapshot of current admission statistics.
    pub fn stats(&self) -> AdmissionStats {
        let limit = self.limit.load(Ordering::Relaxed);
        let in_flight = self.in_flight.load(Ordering::Relaxed);
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        AdmissionStats {
            current_limit: limit,
            in_flight,
            acceptance_rate: g.acceptance.rate(),
        }
    }

    /// Current adaptive concurrency limit.
    pub fn current_limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// Number of requests currently in flight.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }
}

// ── AcquireGuard ──────────────────────────────────────────────────────────────

/// RAII guard returned by [`AdmissionController::try_acquire`].
///
/// Dropping this guard decrements `in_flight`.  Call [`AcquireGuard::success`]
/// before dropping to signal a successful (fast) completion and trigger the
/// AIMD increase or gradient calculation.
pub struct AcquireGuard {
    ctrl: Arc<AdmissionController>,
    start: Instant,
    signalled: bool,
}

impl AcquireGuard {
    /// Signal that this request completed successfully.  Triggers latency
    /// gradient evaluation and potentially AIMD increase.
    pub fn success(mut self) {
        self.signalled = true;
        let latency = self.start.elapsed();
        self.ctrl.record_latency(latency);
        // Drop will still fire and decrement in_flight.
    }
}

impl Drop for AcquireGuard {
    fn drop(&mut self) {
        self.ctrl.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

// ── AdmissionStats ────────────────────────────────────────────────────────────

/// Snapshot of admission controller metrics.
#[derive(Debug, Clone)]
pub struct AdmissionStats {
    /// Current adaptive concurrency limit.
    pub current_limit: usize,
    /// Number of requests currently in flight.
    pub in_flight: usize,
    /// Rolling acceptance rate over the last 100 try_acquire calls (0–1).
    pub acceptance_rate: f64,
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    #[test]
    fn try_acquire_within_limit() {
        let ctrl = AdmissionController::new(AdmissionConfig {
            min_limit: 2,
            max_limit: 10,
            ..Default::default()
        });
        let g1 = ctrl.try_acquire().expect("slot 1");
        let g2 = ctrl.try_acquire().expect("slot 2");
        assert!(ctrl.try_acquire().is_none(), "limit reached");
        drop(g1);
        assert!(ctrl.try_acquire().is_some(), "slot freed");
        drop(g2);
    }

    #[test]
    fn on_success_increases_limit() {
        let ctrl = AdmissionController::new(AdmissionConfig {
            min_limit: 4,
            max_limit: 10,
            additive_increase: 2,
            ..Default::default()
        });
        let start = ctrl.current_limit();
        ctrl.on_success();
        assert_eq!(ctrl.current_limit(), start + 2);
    }

    #[test]
    fn on_overload_decreases_limit() {
        let ctrl = AdmissionController::new(AdmissionConfig {
            min_limit: 4,
            max_limit: 100,
            multiplicative_decrease: 0.5,
            ..Default::default()
        });
        // Start at min_limit=4, manually push the limit up.
        ctrl.limit.store(20, Ordering::Relaxed);
        ctrl.on_overload();
        assert_eq!(ctrl.current_limit(), 10);
    }

    #[test]
    fn limit_capped_at_max() {
        let ctrl = AdmissionController::new(AdmissionConfig {
            min_limit: 4,
            max_limit: 5,
            additive_increase: 10,
            ..Default::default()
        });
        ctrl.on_success();
        assert_eq!(ctrl.current_limit(), 5);
    }

    #[test]
    fn limit_floored_at_min() {
        let ctrl = AdmissionController::new(AdmissionConfig {
            min_limit: 4,
            max_limit: 100,
            multiplicative_decrease: 0.01,
            ..Default::default()
        });
        ctrl.on_overload();
        assert_eq!(ctrl.current_limit(), 4);
    }

    #[test]
    fn in_flight_decrements_on_drop() {
        let ctrl = AdmissionController::new(AdmissionConfig {
            min_limit: 4,
            max_limit: 100,
            ..Default::default()
        });
        {
            let _g = ctrl.try_acquire().unwrap();
            assert_eq!(ctrl.in_flight(), 1);
        }
        assert_eq!(ctrl.in_flight(), 0);
    }

    #[test]
    fn acceptance_rate_tracks_rejections() {
        let ctrl = AdmissionController::new(AdmissionConfig {
            min_limit: 1,
            max_limit: 1,
            ..Default::default()
        });
        // First acquire succeeds, subsequent ones are rejected until we drop.
        let g = ctrl.try_acquire().unwrap();
        for _ in 0..9 {
            let _ = ctrl.try_acquire(); // rejected
        }
        drop(g);
        let stats = ctrl.stats();
        // 1 accepted out of 10 calls = 0.1
        assert!(stats.acceptance_rate < 0.5);
    }

    #[test]
    fn concurrent_acquire_release() {
        let ctrl = AdmissionController::new(AdmissionConfig {
            min_limit: 8,
            max_limit: 8,
            ..Default::default()
        });
        let success = Arc::new(AtomicBool::new(true));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let ctrl2 = Arc::clone(&ctrl);
            let ok = Arc::clone(&success);
            handles.push(thread::spawn(move || {
                if let Some(g) = ctrl2.try_acquire() {
                    thread::sleep(Duration::from_millis(5));
                    g.success();
                } else {
                    ok.store(false, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().expect("thread panicked");
        }
        assert!(success.load(Ordering::Relaxed));
        assert_eq!(ctrl.in_flight(), 0);
    }

    #[test]
    fn aimd_convergence_under_normal_load() {
        // Under fast responses the limit should grow from min to max.
        let ctrl = AdmissionController::new(AdmissionConfig {
            min_limit: 4,
            max_limit: 20,
            additive_increase: 1,
            multiplicative_decrease: 0.9,
            gradient_threshold: 10.0, // high threshold: never overload
            latency_ewma_alpha: 0.5,
            ..Default::default()
        });
        for _ in 0..20 {
            if let Some(g) = ctrl.try_acquire() {
                g.success();
            } else {
                ctrl.on_success();
            }
        }
        // Limit should have grown well above min.
        assert!(ctrl.current_limit() > 4);
    }
}
