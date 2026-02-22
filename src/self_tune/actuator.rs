// ── Lint policy (inherited from crate root) ──────────────────────────────────
#![cfg(feature = "self-tune")]

//! # Live-Tuning Actuator (Task 1.x — Parameter Actuation)
//!
//! ## Responsibility
//!
//! Bridge the gap between the PID controller's *recommendations* and the
//! pipeline's *actual behaviour*.  The controller writes updated parameter
//! values here; pipeline components read them before each operation.
//!
//! ## Design
//!
//! ```text
//!  TelemetryBus ──snapshot──► TuningController ──adjustments──► LiveTuning
//!                                                                      │
//!               CircuitBreaker ◄── failure_threshold ─────────────────┤
//!               RateLimiter    ◄── refill_rate ──────────────────────┤
//!               DedupStore     ◄── ttl_ms ──────────────────────────┤
//!               ShedPolicy     ◄── shed_threshold ─────────────────-┘
//! ```
//!
//! All reads are lock-free (`AtomicU64` bit-cast to `f64`).  Writes use
//! sequential consistency so pipeline components never see torn state.
//!
//! ## Panics
//!
//! No function in this module ever panics.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use tracing::{debug, info};

use crate::self_tune::controller::{ParameterId, ParameterSpec};

// ─── LiveTuning ──────────────────────────────────────────────────────────────

/// Shared atomic parameter store.
///
/// Holds the latest PID-recommended values for every tunable pipeline
/// parameter.  All reads are lock-free.
///
/// # Cloning
///
/// Cheap — all state is behind `Arc`.  Callers should clone when they need
/// to hold a local copy across async yield points.
#[derive(Clone)]
pub struct LiveTuning {
    cells: Arc<HashMap<ParameterId, AtomicF64>>,
}

impl LiveTuning {
    /// Create a new `LiveTuning` populated with the defaults for each
    /// [`ParameterId`].
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new() -> Self {
        let cells = ParameterId::all()
            .iter()
            .map(|&id| {
                let spec = ParameterSpec::default_for(id);
                let default = (spec.min + spec.max) / 2.0;
                (id, AtomicF64::new(default))
            })
            .collect();
        Self {
            cells: Arc::new(cells),
        }
    }

    /// Return the current value for `id`.
    ///
    /// Returns the midpoint default if `id` is not yet present (should not
    /// happen with `new()`-constructed instances).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn get(&self, id: ParameterId) -> f64 {
        self.cells
            .get(&id)
            .map(|c| c.load())
            .unwrap_or_else(|| {
                let spec = ParameterSpec::default_for(id);
                (spec.min + spec.max) / 2.0
            })
    }

    /// Update the value for a single parameter, clamping to the allowed range.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn set(&self, id: ParameterId, value: f64) {
        let spec = ParameterSpec::default_for(id);
        let clamped = value.clamp(spec.min, spec.max);
        if let Some(cell) = self.cells.get(&id) {
            let old = cell.load();
            cell.store(clamped);
            if (old - clamped).abs() > f64::EPSILON {
                debug!(
                    parameter = ?id,
                    old_value = old,
                    new_value = clamped,
                    "LiveTuning: parameter updated"
                );
            }
        }
    }

    /// Apply a batch of controller adjustments atomically (one store per param).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn apply_adjustments(&self, adjustments: &[(ParameterId, f64)]) {
        if adjustments.is_empty() {
            return;
        }
        info!(count = adjustments.len(), "LiveTuning: applying controller adjustments");
        for &(id, value) in adjustments {
            self.set(id, value);
        }
    }

    /// Return a point-in-time snapshot of all current values.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn snapshot(&self) -> HashMap<ParameterId, f64> {
        self.cells
            .iter()
            .map(|(&id, cell)| (id, cell.load()))
            .collect()
    }

    // ── Convenience accessors ─────────────────────────────────────────────

    /// Circuit-breaker failure threshold (number of consecutive failures).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn circuit_breaker_failure_threshold(&self) -> usize {
        self.get(ParameterId::CircuitBreakerFailureThreshold) as usize
    }

    /// Circuit-breaker success rate (0.0–1.0).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn circuit_breaker_success_rate(&self) -> f64 {
        self.get(ParameterId::CircuitBreakerSuccessRate)
    }

    /// Circuit-breaker timeout in milliseconds.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn circuit_breaker_timeout_ms(&self) -> u64 {
        self.get(ParameterId::CircuitBreakerTimeoutMs) as u64
    }

    /// Dedup cache TTL in milliseconds.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn dedup_ttl_ms(&self) -> u64 {
        self.get(ParameterId::DedupTtlMs) as u64
    }

    /// Rate-limiter token refill rate (tokens per second).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn rate_limiter_refill_rate(&self) -> f64 {
        self.get(ParameterId::RateLimiterRefillRate)
    }

    /// Backpressure shedding threshold (fraction of queue capacity).
    ///
    /// A value of `1.0` means never shed proactively.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn backpressure_shed_threshold(&self) -> f64 {
        self.get(ParameterId::BackpressureShedThreshold)
    }

    /// Priority-queue promotion interval in milliseconds.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn priority_promotion_interval_ms(&self) -> u64 {
        self.get(ParameterId::PriorityQueuePromotionIntervalMs) as u64
    }

    /// Return the recommended channel buffer size for stage `n` (1-based, 1–5).
    ///
    /// Returns the channel_buf_stage1 default for out-of-range `n`.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn channel_buf(&self, stage: u8) -> usize {
        let id = match stage {
            1 => ParameterId::ChannelBufStage1,
            2 => ParameterId::ChannelBufStage2,
            3 => ParameterId::ChannelBufStage3,
            4 => ParameterId::ChannelBufStage4,
            5 => ParameterId::ChannelBufStage5,
            _ => ParameterId::ChannelBufStage1,
        };
        self.get(id) as usize
    }
}

impl Default for LiveTuning {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for LiveTuning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveTuning")
            .field("params", &self.cells.len())
            .finish()
    }
}

// ─── AtomicF64 ───────────────────────────────────────────────────────────────

/// Lock-free `f64` cell backed by an `AtomicU64` bit-cast.
///
/// Uses `SeqCst` ordering to prevent pipeline stages from observing torn
/// values across platforms.
#[derive(Debug)]
struct AtomicF64(AtomicU64);

impl AtomicF64 {
    fn new(v: f64) -> Self {
        Self(AtomicU64::new(v.to_bits()))
    }

    fn load(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::SeqCst))
    }

    fn store(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::SeqCst);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make() -> LiveTuning {
        LiveTuning::new()
    }

    // ── AtomicF64 ─────────────────────────────────────────────────────────

    #[test]
    fn test_atomic_f64_roundtrip_zero() {
        let a = AtomicF64::new(0.0);
        assert_eq!(a.load(), 0.0);
    }

    #[test]
    fn test_atomic_f64_roundtrip_positive() {
        let a = AtomicF64::new(42.5);
        assert!((a.load() - 42.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_atomic_f64_roundtrip_negative() {
        let a = AtomicF64::new(-3.14);
        assert!((a.load() - (-3.14)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_atomic_f64_store_updates_value() {
        let a = AtomicF64::new(1.0);
        a.store(99.0);
        assert!((a.load() - 99.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_atomic_f64_store_zero() {
        let a = AtomicF64::new(10.0);
        a.store(0.0);
        assert_eq!(a.load(), 0.0);
    }

    // ── LiveTuning::new ───────────────────────────────────────────────────

    #[test]
    fn test_new_initialises_all_params() {
        let lt = make();
        for &id in ParameterId::all() {
            let v = lt.get(id);
            let spec = ParameterSpec::default_for(id);
            assert!(v >= spec.min, "{id:?}: {v} < min {}", spec.min);
            assert!(v <= spec.max, "{id:?}: {v} > max {}", spec.max);
        }
    }

    #[test]
    fn test_default_equals_new() {
        let a = LiveTuning::new();
        let b = LiveTuning::default();
        for &id in ParameterId::all() {
            assert!((a.get(id) - b.get(id)).abs() < f64::EPSILON);
        }
    }

    // ── LiveTuning::set / get ────────────────────────────────────────────

    #[test]
    fn test_set_within_range_accepted() {
        let lt = make();
        let spec = ParameterSpec::default_for(ParameterId::DedupTtlMs);
        let target = (spec.min + spec.max) / 2.0;
        lt.set(ParameterId::DedupTtlMs, target);
        assert!((lt.get(ParameterId::DedupTtlMs) - target).abs() < f64::EPSILON);
    }

    #[test]
    fn test_set_above_max_clamped() {
        let lt = make();
        let spec = ParameterSpec::default_for(ParameterId::DedupTtlMs);
        lt.set(ParameterId::DedupTtlMs, spec.max + 999_999.0);
        assert!((lt.get(ParameterId::DedupTtlMs) - spec.max).abs() < f64::EPSILON);
    }

    #[test]
    fn test_set_below_min_clamped() {
        let lt = make();
        let spec = ParameterSpec::default_for(ParameterId::DedupTtlMs);
        lt.set(ParameterId::DedupTtlMs, spec.min - 999_999.0);
        assert!((lt.get(ParameterId::DedupTtlMs) - spec.min).abs() < f64::EPSILON);
    }

    #[test]
    fn test_set_exactly_min() {
        let lt = make();
        let spec = ParameterSpec::default_for(ParameterId::RateLimiterRefillRate);
        lt.set(ParameterId::RateLimiterRefillRate, spec.min);
        assert!((lt.get(ParameterId::RateLimiterRefillRate) - spec.min).abs() < f64::EPSILON);
    }

    #[test]
    fn test_set_exactly_max() {
        let lt = make();
        let spec = ParameterSpec::default_for(ParameterId::RateLimiterRefillRate);
        lt.set(ParameterId::RateLimiterRefillRate, spec.max);
        assert!((lt.get(ParameterId::RateLimiterRefillRate) - spec.max).abs() < f64::EPSILON);
    }

    // ── apply_adjustments ─────────────────────────────────────────────────

    #[test]
    fn test_apply_empty_adjustments_is_noop() {
        let lt = make();
        let before: HashMap<_, _> = lt.snapshot();
        lt.apply_adjustments(&[]);
        let after: HashMap<_, _> = lt.snapshot();
        for &id in ParameterId::all() {
            assert!((before[&id] - after[&id]).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn test_apply_single_adjustment() {
        let lt = make();
        let spec = ParameterSpec::default_for(ParameterId::DedupTtlMs);
        let target = (spec.min + spec.max) / 2.0;
        lt.apply_adjustments(&[(ParameterId::DedupTtlMs, target)]);
        assert!((lt.get(ParameterId::DedupTtlMs) - target).abs() < f64::EPSILON);
    }

    #[test]
    fn test_apply_multiple_adjustments() {
        let lt = make();
        let adjustments = vec![
            (ParameterId::DedupTtlMs, 5000.0),
            (ParameterId::RateLimiterRefillRate, 100.0),
            (ParameterId::CircuitBreakerFailureThreshold, 5.0),
        ];
        lt.apply_adjustments(&adjustments);
        assert!((lt.get(ParameterId::DedupTtlMs) - 5000.0).abs() < f64::EPSILON);
        assert!((lt.get(ParameterId::RateLimiterRefillRate) - 100.0).abs() < f64::EPSILON);
        assert!((lt.get(ParameterId::CircuitBreakerFailureThreshold) - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_apply_clamps_out_of_range_values() {
        let lt = make();
        let spec = ParameterSpec::default_for(ParameterId::CircuitBreakerSuccessRate);
        // Attempt to set 200% — must be clamped to max
        lt.apply_adjustments(&[(ParameterId::CircuitBreakerSuccessRate, 2.0)]);
        assert!(lt.get(ParameterId::CircuitBreakerSuccessRate) <= spec.max);
    }

    // ── snapshot ──────────────────────────────────────────────────────────

    #[test]
    fn test_snapshot_contains_all_params() {
        let lt = make();
        let snap = lt.snapshot();
        assert_eq!(snap.len(), ParameterId::all().len());
    }

    #[test]
    fn test_snapshot_reflects_recent_set() {
        let lt = make();
        lt.set(ParameterId::DedupTtlMs, 12345.0);
        let snap = lt.snapshot();
        // Get will clamp, so compare what we stored
        let spec = ParameterSpec::default_for(ParameterId::DedupTtlMs);
        let expected = 12345.0f64.clamp(spec.min, spec.max);
        assert!((snap[&ParameterId::DedupTtlMs] - expected).abs() < f64::EPSILON);
    }

    // ── convenience accessors ─────────────────────────────────────────────

    #[test]
    fn test_circuit_breaker_failure_threshold_in_range() {
        let lt = make();
        let v = lt.circuit_breaker_failure_threshold();
        let spec = ParameterSpec::default_for(ParameterId::CircuitBreakerFailureThreshold);
        assert!(v >= spec.min as usize);
        assert!(v <= spec.max as usize);
    }

    #[test]
    fn test_circuit_breaker_success_rate_in_range() {
        let lt = make();
        let v = lt.circuit_breaker_success_rate();
        assert!(v >= 0.0 && v <= 1.0);
    }

    #[test]
    fn test_circuit_breaker_timeout_ms_positive() {
        let lt = make();
        assert!(lt.circuit_breaker_timeout_ms() > 0);
    }

    #[test]
    fn test_dedup_ttl_ms_positive() {
        let lt = make();
        assert!(lt.dedup_ttl_ms() > 0);
    }

    #[test]
    fn test_rate_limiter_refill_rate_positive() {
        let lt = make();
        assert!(lt.rate_limiter_refill_rate() > 0.0);
    }

    #[test]
    fn test_backpressure_threshold_in_range() {
        let lt = make();
        let v = lt.backpressure_shed_threshold();
        assert!(v >= 0.0 && v <= 1.0);
    }

    #[test]
    fn test_priority_promotion_interval_positive() {
        let lt = make();
        assert!(lt.priority_promotion_interval_ms() > 0);
    }

    #[test]
    fn test_channel_buf_all_stages_in_range() {
        let lt = make();
        for stage in 1u8..=5 {
            let v = lt.channel_buf(stage);
            let spec = ParameterSpec::default_for(ParameterId::ChannelBufStage1);
            assert!(v >= spec.min as usize, "stage {stage}: {v} < min");
            assert!(v <= spec.max as usize, "stage {stage}: {v} > max");
        }
    }

    #[test]
    fn test_channel_buf_out_of_range_returns_default() {
        let lt = make();
        let v = lt.channel_buf(9); // invalid → stage1 default
        let spec = ParameterSpec::default_for(ParameterId::ChannelBufStage1);
        assert!(v >= spec.min as usize && v <= spec.max as usize);
    }

    // ── Clone / Arc sharing ───────────────────────────────────────────────

    #[test]
    fn test_clone_shares_state() {
        let a = make();
        let b = a.clone();
        a.set(ParameterId::DedupTtlMs, 77777.0);
        assert!((b.get(ParameterId::DedupTtlMs) - a.get(ParameterId::DedupTtlMs)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_debug_does_not_panic() {
        let lt = make();
        let s = format!("{lt:?}");
        assert!(s.contains("LiveTuning"));
    }

    // ── Concurrency ───────────────────────────────────────────────────────

    #[test]
    fn test_concurrent_reads_consistent() {
        use std::thread;
        let lt = Arc::new(make());
        lt.set(ParameterId::DedupTtlMs, 50000.0);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let lt2 = Arc::clone(&lt);
                thread::spawn(move || {
                    for _ in 0..1000 {
                        let v = lt2.get(ParameterId::DedupTtlMs);
                        // Value must always be within spec range
                        let spec = ParameterSpec::default_for(ParameterId::DedupTtlMs);
                        assert!(v >= spec.min && v <= spec.max);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread panicked");
        }
    }

    #[test]
    fn test_concurrent_writes_stay_in_range() {
        use std::thread;
        let lt = Arc::new(make());
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let lt2 = Arc::clone(&lt);
                thread::spawn(move || {
                    for j in 0..100u64 {
                        lt2.set(
                            ParameterId::CircuitBreakerFailureThreshold,
                            (i * 100 + j) as f64,
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread panicked");
        }
        // Final value must still be in range
        let spec = ParameterSpec::default_for(ParameterId::CircuitBreakerFailureThreshold);
        let v = lt.get(ParameterId::CircuitBreakerFailureThreshold);
        assert!(v >= spec.min && v <= spec.max);
    }
}
