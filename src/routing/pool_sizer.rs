//! # Adaptive Worker Pool Sizer
//!
//! Monitors pipeline queue depth and recommends scale-up or scale-down events
//! for the inference worker pool.
//!
//! ## Algorithm
//!
//! The sizer tracks an **Exponentially Weighted Moving Average (EWMA)** of
//! queue fill rate (fraction of channel capacity consumed).  Each call to
//! [`PoolSizer::observe`] feeds a new sample.  [`PoolSizer::recommend`] maps
//! the smoothed fill rate to a target worker count:
//!
//! | Smoothed fill | Recommendation |
//! |---------------|----------------|
//! | < `scale_down_threshold` (default 0.20) | Scale down by 1 |
//! | ≥ `scale_down_threshold` and < `scale_up_threshold` (default 0.70) | Hold |
//! | ≥ `scale_up_threshold` | Scale up by `scale_step` (default 1) |
//!
//! Scale events are debounced: a minimum of `cooldown_observations` samples
//! must pass between consecutive scale actions (default 10).
//!
//! ## Guarantees
//!
//! - Thread-safe (`Send + Sync`).
//! - No I/O — purely in-process metrics.
//! - Bounded: recommended pool size is clamped to `[min_workers, max_workers]`.
//! - Panic-free.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::routing::pool_sizer::{PoolSizer, PoolSizerConfig, ScaleAction};
//!
//! let mut sizer = PoolSizer::new(
//!     PoolSizerConfig {
//!         initial_workers: 2,
//!         min_workers: 1,
//!         max_workers: 8,
//!         ..Default::default()
//!     },
//! );
//!
//! // Simulate a spike — queue filling fast
//! for _ in 0..15 {
//!     sizer.observe(0.90, 1024);
//! }
//!
//! let recommendation = sizer.recommend();
//! assert_eq!(recommendation.action, ScaleAction::ScaleUp);
//! ```

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

// ── Types ───────────────────────────────────────────────────────────────────

/// Configuration for the adaptive pool sizer.
#[derive(Debug, Clone)]
pub struct PoolSizerConfig {
    /// Starting number of workers.
    pub initial_workers: usize,
    /// Minimum allowed pool size.
    pub min_workers: usize,
    /// Maximum allowed pool size.
    pub max_workers: usize,
    /// EWMA smoothing factor α ∈ (0, 1].  Higher = more reactive.  Default: 0.2.
    pub ewma_alpha: f64,
    /// Fill rate below which a scale-down is recommended.  Default: 0.20.
    pub scale_down_threshold: f64,
    /// Fill rate at or above which a scale-up is recommended.  Default: 0.70.
    pub scale_up_threshold: f64,
    /// Number of workers to add/remove per scale event.  Default: 1.
    pub scale_step: usize,
    /// Minimum number of `observe` calls between consecutive scale actions.
    /// Prevents rapid oscillation.  Default: 10.
    pub cooldown_observations: u64,
}

impl Default for PoolSizerConfig {
    fn default() -> Self {
        Self {
            initial_workers: 2,
            min_workers: 1,
            max_workers: 16,
            ewma_alpha: 0.2,
            scale_down_threshold: 0.20,
            scale_up_threshold: 0.70,
            scale_step: 1,
            cooldown_observations: 10,
        }
    }
}

/// The action recommended by [`PoolSizer::recommend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScaleAction {
    /// Increase the worker pool size.
    ScaleUp,
    /// Keep the current pool size.
    Hold,
    /// Decrease the worker pool size.
    ScaleDown,
}

/// Recommendation returned by [`PoolSizer::recommend`].
#[derive(Debug, Clone)]
pub struct ScaleRecommendation {
    /// The recommended action.
    pub action: ScaleAction,
    /// Current (before-action) pool size.
    pub current_workers: usize,
    /// Target pool size after applying the action.
    pub target_workers: usize,
    /// Current EWMA fill rate.
    pub smoothed_fill_rate: f64,
    /// Human-readable reason.
    pub reason: String,
}

// ── Core ─────────────────────────────────────────────────────────────────────

/// Adaptive pool sizer.
///
/// Call [`observe`](PoolSizer::observe) every time you sample the queue depth,
/// then call [`recommend`](PoolSizer::recommend) to get a scaling decision.
///
/// # Thread safety
///
/// [`PoolSizer`] is `Send + Sync`.  The EWMA state is protected by a `Mutex`
/// but the lock is held for nanoseconds.  Scale counters use atomics.
///
/// # Panics
///
/// No method panics.
#[derive(Debug)]
pub struct PoolSizer {
    config: PoolSizerConfig,
    inner: Mutex<SizerInner>,
    // Atomics for lock-free reads
    current_workers: AtomicI64,
    total_observations: AtomicU64,
    total_scale_ups: AtomicU64,
    total_scale_downs: AtomicU64,
}

#[derive(Debug)]
struct SizerInner {
    /// Smoothed fill rate (EWMA).
    ewma: f64,
    /// Current number of workers.
    current_workers: usize,
    /// Observation count since last scale action.
    since_last_scale: u64,
}

impl PoolSizer {
    /// Create a new sizer with the given configuration.
    pub fn new(config: PoolSizerConfig) -> Self {
        let initial = config.initial_workers;
        Self {
            config: config.clone(),
            inner: Mutex::new(SizerInner {
                ewma: 0.0,
                current_workers: initial,
                since_last_scale: 0,
            }),
            current_workers: AtomicI64::new(initial as i64),
            total_observations: AtomicU64::new(0),
            total_scale_ups: AtomicU64::new(0),
            total_scale_downs: AtomicU64::new(0),
        }
    }

    /// Feed a new queue depth observation.
    ///
    /// # Arguments
    ///
    /// * `fill_rate` — Current queue fill fraction in `[0.0, 1.0]`.
    ///   Typically `current_depth as f64 / channel_capacity as f64`.
    /// * `_channel_capacity` — Ignored; reserved for future hysteresis
    ///   calculations.  Pass the actual channel capacity for forwards
    ///   compatibility.
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn observe(&self, fill_rate: f64, _channel_capacity: usize) {
        let fill_rate = fill_rate.clamp(0.0, 1.0);
        self.total_observations.fetch_add(1, Ordering::Relaxed);

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let alpha = self.config.ewma_alpha;
        inner.ewma = alpha * fill_rate + (1.0 - alpha) * inner.ewma;
        inner.since_last_scale += 1;
    }

    /// Return a scaling recommendation based on the current EWMA.
    ///
    /// Applying the recommendation is the caller's responsibility.  After
    /// acting on a scale-up or scale-down, call
    /// [`apply_scale`](PoolSizer::apply_scale) so the sizer tracks the new
    /// pool size.
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn recommend(&self) -> ScaleRecommendation {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ewma = inner.ewma;
        let current = inner.current_workers;
        let since = inner.since_last_scale;

        // Cooldown gate
        if since < self.config.cooldown_observations {
            return ScaleRecommendation {
                action: ScaleAction::Hold,
                current_workers: current,
                target_workers: current,
                smoothed_fill_rate: ewma,
                reason: format!(
                    "Cooldown active ({}/{} observations since last scale)",
                    since, self.config.cooldown_observations
                ),
            };
        }

        if ewma >= self.config.scale_up_threshold {
            let target = (current + self.config.scale_step).min(self.config.max_workers);
            ScaleRecommendation {
                action: ScaleAction::ScaleUp,
                current_workers: current,
                target_workers: target,
                smoothed_fill_rate: ewma,
                reason: format!(
                    "EWMA fill rate {:.1}% ≥ scale-up threshold {:.1}%",
                    ewma * 100.0,
                    self.config.scale_up_threshold * 100.0
                ),
            }
        } else if ewma < self.config.scale_down_threshold {
            let target = current
                .saturating_sub(self.config.scale_step)
                .max(self.config.min_workers);
            ScaleRecommendation {
                action: ScaleAction::ScaleDown,
                current_workers: current,
                target_workers: target,
                smoothed_fill_rate: ewma,
                reason: format!(
                    "EWMA fill rate {:.1}% < scale-down threshold {:.1}%",
                    ewma * 100.0,
                    self.config.scale_down_threshold * 100.0
                ),
            }
        } else {
            ScaleRecommendation {
                action: ScaleAction::Hold,
                current_workers: current,
                target_workers: current,
                smoothed_fill_rate: ewma,
                reason: format!(
                    "EWMA fill rate {:.1}% within stable band [{:.1}%, {:.1}%)",
                    ewma * 100.0,
                    self.config.scale_down_threshold * 100.0,
                    self.config.scale_up_threshold * 100.0
                ),
            }
        }
    }

    /// Inform the sizer that a scale action has been applied.
    ///
    /// Call this after your orchestration code has actually added or removed
    /// workers.  The sizer uses this to reset the cooldown counter and track
    /// the current pool size.
    ///
    /// # Arguments
    ///
    /// * `new_worker_count` — The actual new pool size after the scale action.
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn apply_scale(&self, new_worker_count: usize) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let prev = inner.current_workers;
        inner.current_workers = new_worker_count.clamp(self.config.min_workers, self.config.max_workers);
        inner.since_last_scale = 0;

        if new_worker_count > prev {
            self.total_scale_ups.fetch_add(1, Ordering::Relaxed);
        } else if new_worker_count < prev {
            self.total_scale_downs.fetch_add(1, Ordering::Relaxed);
        }
        self.current_workers
            .store(inner.current_workers as i64, Ordering::Relaxed);
    }

    /// Current pool size (lock-free read).
    pub fn current_workers(&self) -> usize {
        self.current_workers.load(Ordering::Relaxed).max(0) as usize
    }

    /// Total observations fed to the sizer.
    pub fn total_observations(&self) -> u64 {
        self.total_observations.load(Ordering::Relaxed)
    }

    /// Total scale-up events applied.
    pub fn total_scale_ups(&self) -> u64 {
        self.total_scale_ups.load(Ordering::Relaxed)
    }

    /// Total scale-down events applied.
    pub fn total_scale_downs(&self) -> u64 {
        self.total_scale_downs.load(Ordering::Relaxed)
    }

    /// Current smoothed fill rate (EWMA).
    pub fn smoothed_fill_rate(&self) -> f64 {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.ewma
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_sizer() -> PoolSizer {
        PoolSizer::new(PoolSizerConfig {
            initial_workers: 2,
            min_workers: 1,
            max_workers: 8,
            cooldown_observations: 5,
            ..Default::default()
        })
    }

    #[test]
    fn initial_state_holds() {
        let s = default_sizer();
        assert_eq!(s.current_workers(), 2);
        let rec = s.recommend();
        // No observations yet, cooldown not exceeded, should hold
        assert_eq!(rec.action, ScaleAction::Hold);
    }

    #[test]
    fn high_fill_rate_recommends_scale_up() {
        let s = default_sizer();
        for _ in 0..10 {
            s.observe(0.95, 1024);
        }
        let rec = s.recommend();
        assert_eq!(rec.action, ScaleAction::ScaleUp);
        assert_eq!(rec.target_workers, 3);
    }

    #[test]
    fn low_fill_rate_recommends_scale_down() {
        let s = default_sizer();
        for _ in 0..15 {
            s.observe(0.05, 1024);
        }
        let rec = s.recommend();
        assert_eq!(rec.action, ScaleAction::ScaleDown);
        assert_eq!(rec.target_workers, 1);
    }

    #[test]
    fn moderate_fill_rate_holds() {
        let s = default_sizer();
        for _ in 0..10 {
            s.observe(0.50, 1024);
        }
        let rec = s.recommend();
        assert_eq!(rec.action, ScaleAction::Hold);
        assert_eq!(rec.target_workers, 2);
    }

    #[test]
    fn scale_up_clamped_to_max() {
        let s = PoolSizer::new(PoolSizerConfig {
            initial_workers: 8,
            min_workers: 1,
            max_workers: 8,
            cooldown_observations: 3,
            ..Default::default()
        });
        for _ in 0..10 {
            s.observe(0.99, 1024);
        }
        let rec = s.recommend();
        assert_eq!(rec.action, ScaleAction::ScaleUp);
        assert_eq!(rec.target_workers, 8); // clamped to max
    }

    #[test]
    fn scale_down_clamped_to_min() {
        let s = PoolSizer::new(PoolSizerConfig {
            initial_workers: 1,
            min_workers: 1,
            max_workers: 8,
            cooldown_observations: 3,
            ..Default::default()
        });
        for _ in 0..10 {
            s.observe(0.0, 1024);
        }
        let rec = s.recommend();
        assert_eq!(rec.action, ScaleAction::ScaleDown);
        assert_eq!(rec.target_workers, 1); // clamped to min
    }

    #[test]
    fn apply_scale_resets_cooldown() {
        let s = default_sizer();
        for _ in 0..10 {
            s.observe(0.95, 1024);
        }
        let rec = s.recommend();
        assert_eq!(rec.action, ScaleAction::ScaleUp);

        s.apply_scale(rec.target_workers);
        assert_eq!(s.current_workers(), 3);

        // Immediately after apply, cooldown kicks in
        let rec2 = s.recommend();
        assert_eq!(rec2.action, ScaleAction::Hold);
        assert!(rec2.reason.contains("Cooldown active"));
    }

    #[test]
    fn ewma_smooths_noisy_observations() {
        let s = default_sizer();
        // Alternate between 0.0 and 1.0 — EWMA should stay between thresholds
        for i in 0..20usize {
            s.observe(if i % 2 == 0 { 1.0 } else { 0.0 }, 512);
        }
        // EWMA of alternating [1.0, 0.0, 1.0, ...] converges to ~0.5
        let ewma = s.smoothed_fill_rate();
        assert!(ewma > 0.2 && ewma < 0.8, "EWMA should be ~0.5, got {}", ewma);
    }

    #[test]
    fn accounting_tracks_scale_events() {
        let s = default_sizer();
        for _ in 0..10 {
            s.observe(0.90, 512);
        }
        s.apply_scale(3);
        assert_eq!(s.total_scale_ups(), 1);
        assert_eq!(s.total_scale_downs(), 0);

        for _ in 0..10 {
            s.observe(0.05, 512);
        }
        s.apply_scale(2);
        assert_eq!(s.total_scale_ups(), 1);
        assert_eq!(s.total_scale_downs(), 1);
    }
}
