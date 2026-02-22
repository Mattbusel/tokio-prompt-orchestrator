#![allow(
    missing_docs,
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    clippy::redundant_closure,
    clippy::derivable_impls,
    clippy::unwrap_or_default,
    dead_code,
    private_interfaces
)]
#![cfg(feature = "self-tune")]
//! # Parameter Controller (Task 1.2)
//!
//! ## Responsibility
//! PID controllers for 12 tunable pipeline parameters. Reads `TelemetrySnapshot`
//! from the telemetry bus, computes error signals (actual vs. target latency and
//! throughput), applies PID updates, and auto-rolls back any change that degrades
//! the target metric by more than 10% within 30 seconds.
//!
//! ## Parameters controlled
//! - Channel buffer sizes for 5 pipeline stages
//! - Backpressure shedding threshold
//! - Circuit-breaker failure threshold, success rate, and timeout
//! - Dedup TTL
//! - Rate-limiter token refill rate
//! - Priority-queue promotion interval
//!
//! ## Graceful degradation
//! If tuning fails the pipeline continues on the last known-good values.
//! All errors are logged; no `unwrap`/`expect` in non-test paths.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use tracing::{info, warn};

use crate::self_tune::telemetry_bus::{StageMetrics, TelemetrySnapshot};

// ─── Constants ───────────────────────────────────────────────────────────────

/// How long after a parameter change we watch for metric degradation.
const ROLLBACK_WINDOW: Duration = Duration::from_secs(30);

// ─── ParameterId ─────────────────────────────────────────────────────────────

/// One of the 12 tunable pipeline parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParameterId {
    /// Buffer capacity for pipeline stage 1 (RAG).
    /// Buffer capacity for pipeline stage 1 (RAG).
    ChannelBufStage1,
    /// Buffer capacity for pipeline stage 2 (Assemble).
    /// Buffer capacity for pipeline stage 2 (Assemble).
    ChannelBufStage2,
    /// Buffer capacity for pipeline stage 3 (Inference).
    /// Buffer capacity for pipeline stage 3 (Inference).
    ChannelBufStage3,
    /// Buffer capacity for pipeline stage 4 (Post-process).
    /// Buffer capacity for pipeline stage 4 (Post-process).
    ChannelBufStage4,
    /// Buffer capacity for pipeline stage 5 (Stream).
    /// Buffer capacity for pipeline stage 5 (Stream).
    ChannelBufStage5,
    /// Fraction of queue capacity at which load shedding activates.
    /// Fraction of queue capacity at which load shedding activates.
    BackpressureShedThreshold,
    /// Number of consecutive failures before the circuit breaker opens.
    /// Number of consecutive failures before the circuit breaker opens.
    CircuitBreakerFailureThreshold,
    /// Minimum success rate required to keep the circuit breaker closed.
    /// Minimum success rate required to keep the circuit breaker closed.
    CircuitBreakerSuccessRate,
    /// Per-request timeout in milliseconds enforced by the circuit breaker.
    /// Per-request timeout in milliseconds enforced by the circuit breaker.
    CircuitBreakerTimeoutMs,
    /// Time-to-live in milliseconds for deduplication cache entries.
    /// Time-to-live in milliseconds for deduplication cache entries.
    DedupTtlMs,
    /// Token refill rate (tokens per second) for the rate limiter.
    /// Token refill rate (tokens per second) for the rate limiter.
    RateLimiterRefillRate,
    /// Interval in milliseconds between priority-queue promotion sweeps.
    /// Interval in milliseconds between priority-queue promotion sweeps.
    PriorityQueuePromotionIntervalMs,
}

impl ParameterId {
    /// Return a slice containing all 12 tunable parameters in declaration order.
    pub fn all() -> &'static [ParameterId] {
        &[
            ParameterId::ChannelBufStage1,
            ParameterId::ChannelBufStage2,
            ParameterId::ChannelBufStage3,
            ParameterId::ChannelBufStage4,
            ParameterId::ChannelBufStage5,
            ParameterId::BackpressureShedThreshold,
            ParameterId::CircuitBreakerFailureThreshold,
            ParameterId::CircuitBreakerSuccessRate,
            ParameterId::CircuitBreakerTimeoutMs,
            ParameterId::DedupTtlMs,
            ParameterId::RateLimiterRefillRate,
            ParameterId::PriorityQueuePromotionIntervalMs,
        ]
    }

    /// Return the canonical snake_case name for this parameter.
    /// Return the canonical snake_case name for this parameter.
    pub fn name(self) -> &'static str {
        match self {
            ParameterId::ChannelBufStage1 => "channel_buf_stage1",
            ParameterId::ChannelBufStage2 => "channel_buf_stage2",
            ParameterId::ChannelBufStage3 => "channel_buf_stage3",
            ParameterId::ChannelBufStage4 => "channel_buf_stage4",
            ParameterId::ChannelBufStage5 => "channel_buf_stage5",
            ParameterId::BackpressureShedThreshold => "backpressure_shed_threshold",
            ParameterId::CircuitBreakerFailureThreshold => "circuit_breaker_failure_threshold",
            ParameterId::CircuitBreakerSuccessRate => "circuit_breaker_success_rate",
            ParameterId::CircuitBreakerTimeoutMs => "circuit_breaker_timeout_ms",
            ParameterId::DedupTtlMs => "dedup_ttl_ms",
            ParameterId::RateLimiterRefillRate => "rate_limiter_refill_rate",
            ParameterId::PriorityQueuePromotionIntervalMs => "priority_queue_promotion_interval_ms",
        }
    }
}

// ─── ParameterSpec ───────────────────────────────────────────────────────────

/// Constraints and rollback policy for a single tunable parameter.
#[derive(Debug, Clone)]
pub struct ParameterSpec {
    /// Minimum allowed value (inclusive).
    pub min: f64,
    /// Maximum allowed value (inclusive).
    pub max: f64,
    /// Smallest increment the PID output is quantised to.
    pub step: f64,
    /// Minimum time between consecutive changes to this parameter.
    pub cooldown: Duration,
    /// Relative degradation that triggers automatic rollback (e.g. 0.10 = 10%).
    pub rollback_threshold: f64,
}

impl ParameterSpec {
    /// Return the default [`ParameterSpec`] for the given parameter ID.
    pub fn default_for(id: ParameterId) -> Self {
        match id {
            ParameterId::ChannelBufStage1
            | ParameterId::ChannelBufStage2
            | ParameterId::ChannelBufStage3
            | ParameterId::ChannelBufStage4
            | ParameterId::ChannelBufStage5 => Self {
                min: 8.0,
                max: 4096.0,
                step: 8.0,
                cooldown: Duration::from_secs(10),
                rollback_threshold: 0.10,
            },
            ParameterId::BackpressureShedThreshold => Self {
                min: 0.5,
                max: 0.99,
                step: 0.01,
                cooldown: Duration::from_secs(15),
                rollback_threshold: 0.10,
            },
            ParameterId::CircuitBreakerFailureThreshold => Self {
                min: 1.0,
                max: 20.0,
                step: 1.0,
                cooldown: Duration::from_secs(30),
                rollback_threshold: 0.10,
            },
            ParameterId::CircuitBreakerSuccessRate => Self {
                min: 0.5,
                max: 0.99,
                step: 0.01,
                cooldown: Duration::from_secs(30),
                rollback_threshold: 0.10,
            },
            ParameterId::CircuitBreakerTimeoutMs => Self {
                min: 100.0,
                max: 60_000.0,
                step: 100.0,
                cooldown: Duration::from_secs(30),
                rollback_threshold: 0.10,
            },
            ParameterId::DedupTtlMs => Self {
                min: 1_000.0,
                max: 300_000.0,
                step: 1_000.0,
                cooldown: Duration::from_secs(20),
                rollback_threshold: 0.10,
            },
            ParameterId::RateLimiterRefillRate => Self {
                min: 1.0,
                max: 10_000.0,
                step: 1.0,
                cooldown: Duration::from_secs(10),
                rollback_threshold: 0.10,
            },
            ParameterId::PriorityQueuePromotionIntervalMs => Self {
                min: 100.0,
                max: 60_000.0,
                step: 100.0,
                cooldown: Duration::from_secs(15),
                rollback_threshold: 0.10,
            },
        }
    }

    fn midpoint(&self) -> f64 {
        pid_round_to_step((self.min + self.max) / 2.0, self.step).clamp(self.min, self.max)
    }
}

// ─── PidState ────────────────────────────────────────────────────────────────

/// State for a single PID controller instance.
#[derive(Debug, Clone)]
pub struct PidState {
    /// Proportional gain.
    pub kp: f64,
    /// Integral gain.
    pub ki: f64,
    /// Derivative gain.
    pub kd: f64,
    /// Accumulated integral term (clamped to +/-100 to prevent windup).
    pub integral: f64,
    /// Error value from the previous update tick.
    pub prev_error: f64,
    /// Wall-clock time of the last update call.
    pub last_update: Instant,
}

impl PidState {
    /// Create a new [`PidState`] with the given PID gains and zeroed accumulator.
    pub fn new(kp: f64, ki: f64, kd: f64) -> Self {
        Self {
            kp,
            ki,
            kd,
            integral: 0.0,
            prev_error: 0.0,
            last_update: Instant::now(),
        }
    }

    /// Advance the controller by one tick. `error` is target minus actual; `dt` is elapsed seconds.
    /// Returns control output clamped to `[-1.0, 1.0]`.
    pub fn update(&mut self, error: f64, dt: f64) -> f64 {
        if dt <= 0.0 {
            return 0.0;
        }
        self.integral = (self.integral + error * dt).clamp(-100.0, 100.0);
        let derivative = (error - self.prev_error) / dt;
        self.prev_error = error;
        self.last_update = Instant::now();
        (self.kp * error + self.ki * self.integral + self.kd * derivative).clamp(-1.0, 1.0)
    }
}

// ─── ParameterValue ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ParameterValue {
    current: f64,
    last_good: f64,
    last_changed: Instant,
    metric_at_change: Option<f64>,
}

impl ParameterValue {
    fn new(initial: f64) -> Self {
        Self {
            current: initial,
            last_good: initial,
            last_changed: Instant::now(),
            metric_at_change: None,
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Round `value` to the nearest multiple of `step`.
pub fn pid_round_to_step(value: f64, step: f64) -> f64 {
    if step <= 0.0 {
        return value;
    }
    (value / step).round() * step
}

/// Compute the mean p95 latency (ms) across all stages with samples.
pub fn avg_p95_ms(snap: &TelemetrySnapshot) -> f64 {
    let stages: Vec<&StageMetrics> = snap
        .stages
        .iter()
        .filter(|s| s.latency.sample_count > 0)
        .collect();
    if stages.is_empty() {
        return 0.0;
    }
    stages.iter().map(|s| s.latency.p95_ms as f64).sum::<f64>() / stages.len() as f64
}

/// Compute the mean throughput (requests/sec) across all pipeline stages.
pub fn avg_throughput_rps(snap: &TelemetrySnapshot) -> f64 {
    if snap.stages.is_empty() {
        return 0.0;
    }
    snap.stages.iter().map(|s| s.throughput_rps).sum::<f64>() / snap.stages.len() as f64
}

fn avg_error_rate(snap: &TelemetrySnapshot) -> f64 {
    if snap.stages.is_empty() {
        return 0.0;
    }
    snap.stages.iter().map(|s| s.error_rate).sum::<f64>() / snap.stages.len() as f64
}

fn avg_p99_ms(snap: &TelemetrySnapshot) -> f64 {
    let stages: Vec<&StageMetrics> = snap
        .stages
        .iter()
        .filter(|s| s.latency.sample_count > 0)
        .collect();
    if stages.is_empty() {
        return 0.0;
    }
    stages.iter().map(|s| s.latency.p99_ms as f64).sum::<f64>() / stages.len() as f64
}

// ─── TuningController ────────────────────────────────────────────────────────

/// Top-level PID controller that tunes all 12 pipeline parameters simultaneously.
pub struct TuningController {
    specs: HashMap<ParameterId, ParameterSpec>,
    values: HashMap<ParameterId, ParameterValue>,
    pid_states: HashMap<ParameterId, PidState>,
    /// Target p95 latency in milliseconds.
    pub target_p95_ms: f64,
    /// Target aggregate throughput in requests per second.
    pub target_throughput_rps: f64,
}

impl TuningController {
    /// Create a new controller with the given latency and throughput targets.
    pub fn new(target_p95_ms: f64, target_throughput_rps: f64) -> Self {
        let mut specs = HashMap::new();
        let mut values = HashMap::new();
        let mut pid_states = HashMap::new();
        for &id in ParameterId::all() {
            let spec = ParameterSpec::default_for(id);
            let initial = spec.midpoint();
            values.insert(id, ParameterValue::new(initial));
            pid_states.insert(id, PidState::new(0.1, 0.01, 0.05));
            specs.insert(id, spec);
        }
        Self {
            specs,
            values,
            pid_states,
            target_p95_ms,
            target_throughput_rps,
        }
    }

    /// Return the current recommended value for `id`, or `0.0` if not found.
    pub fn get(&self, id: ParameterId) -> f64 {
        self.values.get(&id).map(|v| v.current).unwrap_or(0.0)
    }

    /// Compute the composite error signal for a parameter given current metrics.
    fn error_signal(&self, id: ParameterId, snap: &TelemetrySnapshot) -> f64 {
        let p95 = avg_p95_ms(snap);
        let tput = avg_throughput_rps(snap);
        match id {
            ParameterId::ChannelBufStage1
            | ParameterId::ChannelBufStage2
            | ParameterId::ChannelBufStage3
            | ParameterId::ChannelBufStage4
            | ParameterId::ChannelBufStage5 => snap.pressure - 0.5,
            ParameterId::BackpressureShedThreshold => {
                if self.target_p95_ms > 0.0 {
                    (self.target_p95_ms - p95) / self.target_p95_ms
                } else {
                    0.0
                }
            }
            ParameterId::CircuitBreakerFailureThreshold => 0.01 - avg_error_rate(snap),
            ParameterId::CircuitBreakerSuccessRate => (1.0 - avg_error_rate(snap)) - 0.99,
            ParameterId::CircuitBreakerTimeoutMs => {
                let target_p99 = self.target_p95_ms * 2.0;
                if target_p99 > 0.0 {
                    (avg_p99_ms(snap) - target_p99) / target_p99
                } else {
                    0.0
                }
            }
            ParameterId::DedupTtlMs => snap.dedup_collision_rate - 0.05,
            ParameterId::RateLimiterRefillRate => {
                if self.target_throughput_rps > 0.0 {
                    (self.target_throughput_rps - tput) / self.target_throughput_rps
                } else {
                    0.0
                }
            }
            ParameterId::PriorityQueuePromotionIntervalMs => {
                if self.target_p95_ms > 0.0 {
                    (p95 - self.target_p95_ms) / self.target_p95_ms
                } else {
                    0.0
                }
            }
        }
    }

    /// Check if a parameter should be rolled back due to metric degradation.
    fn should_rollback(&self, id: ParameterId, snap: &TelemetrySnapshot, now: Instant) -> bool {
        let val = match self.values.get(&id) {
            Some(v) => v,
            None => return false,
        };
        let metric_at_change = match val.metric_at_change {
            Some(m) => m,
            None => return false,
        };
        let spec = match self.specs.get(&id) {
            Some(s) => s,
            None => return false,
        };
        if now.duration_since(val.last_changed) > ROLLBACK_WINDOW {
            return false;
        }
        if metric_at_change <= 0.0 {
            return false;
        }
        let current_p95 = avg_p95_ms(snap);
        current_p95 > metric_at_change * (1.0 + spec.rollback_threshold)
    }

    /// Process a snapshot: check rollbacks, then apply PID updates.
    /// Returns the list of parameters that changed and their new values.
    pub fn process(&mut self, snap: &TelemetrySnapshot) -> Vec<(ParameterId, f64)> {
        let now = Instant::now();
        let current_p95 = avg_p95_ms(snap);

        // Phase 1: collect which parameters need rollback
        let rollback_ids: Vec<ParameterId> = ParameterId::all()
            .iter()
            .copied()
            .filter(|&id| self.should_rollback(id, snap, now))
            .collect();

        let mut changed: Vec<(ParameterId, f64)> = Vec::new();

        // Phase 2: rollbacks
        for id in rollback_ids {
            if let Some(v) = self.rollback(id) {
                warn!(
                    param = id.name(),
                    restored = v,
                    current_p95_ms = current_p95,
                    "Auto-rollback: metric degraded >threshold within 30s"
                );
                changed.push((id, v));
            }
        }

        // Phase 3: PID adjustments for parameters not just rolled back
        let rolled_back: std::collections::HashSet<ParameterId> =
            changed.iter().map(|(id, _)| *id).collect();

        let ids: Vec<ParameterId> = ParameterId::all().to_vec();
        for id in ids {
            if rolled_back.contains(&id) {
                continue;
            }

            // Cooldown check
            let in_cooldown = self
                .values
                .get(&id)
                .zip(self.specs.get(&id))
                .map(|(v, s)| now.duration_since(v.last_changed) < s.cooldown)
                .unwrap_or(false);
            if in_cooldown {
                continue;
            }

            let error = self.error_signal(id, snap);

            let dt = self
                .pid_states
                .get(&id)
                .map(|p| now.duration_since(p.last_update).as_secs_f64())
                .unwrap_or(1.0);

            let pid_output = match self.pid_states.get_mut(&id) {
                Some(pid) => pid.update(error, dt),
                None => continue,
            };

            if pid_output.abs() < 1e-6 {
                continue;
            }

            let (spec_min, spec_max, spec_step) = match self.specs.get(&id) {
                Some(s) => (s.min, s.max, s.step),
                None => continue,
            };

            let current = self.values.get(&id).map(|v| v.current).unwrap_or(0.0);
            let delta = pid_output * (spec_max - spec_min) * 0.05; // max 5% of range per tick
            let proposed =
                pid_round_to_step((current + delta).clamp(spec_min, spec_max), spec_step);

            if (proposed - current).abs() < spec_step * 0.5 {
                continue;
            }

            if let Some(val) = self.values.get_mut(&id) {
                val.last_good = val.current;
                val.current = proposed;
                val.last_changed = now;
                val.metric_at_change = if current_p95 > 0.0 {
                    Some(current_p95)
                } else {
                    None
                };
            }

            info!(
                param = id.name(),
                before = current,
                after = proposed,
                error_signal = error,
                pid_output = pid_output,
                "Parameter adjusted by PID controller"
            );
            changed.push((id, proposed));
        }

        changed
    }

    /// Rollback a single parameter to its last known-good value.
    pub fn rollback(&mut self, id: ParameterId) -> Option<f64> {
        let val = self.values.get_mut(&id)?;
        val.current = val.last_good;
        val.last_changed = Instant::now();
        val.metric_at_change = None;
        if let Some(pid) = self.pid_states.get_mut(&id) {
            pid.integral = 0.0;
            pid.prev_error = 0.0;
        }
        Some(val.current)
    }

    /// Rollback all 12 parameters to their last known-good values.
    pub fn rollback_all(&mut self) -> Vec<(ParameterId, f64)> {
        ParameterId::all()
            .iter()
            .copied()
            .filter_map(|id| self.rollback(id).map(|v| (id, v)))
            .collect()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_tune::telemetry_bus::{LatencyStats, StageMetrics, TelemetrySnapshot};

    fn make_latency(p95: u64, samples: u64) -> LatencyStats {
        LatencyStats {
            p50_ms: p95 / 2,
            p95_ms: p95,
            p99_ms: p95.saturating_add(p95 / 5),
            avg_ms: p95 as f64 * 0.85,
            sample_count: samples,
        }
    }

    fn make_stage(name: &str, p95: u64, tput: f64, err: f64) -> StageMetrics {
        StageMetrics {
            stage: name.to_string(),
            queue_depth: 0,
            queue_capacity: 256,
            latency: make_latency(p95, 100),
            throughput_rps: tput,
            error_rate: err,
        }
    }

    fn empty_snap() -> TelemetrySnapshot {
        TelemetrySnapshot::default()
    }

    fn snap(p95: u64, tput: f64, pressure: f64) -> TelemetrySnapshot {
        TelemetrySnapshot {
            stages: vec![make_stage("s1", p95, tput, 0.0)],
            pressure,
            ..TelemetrySnapshot::default()
        }
    }

    // ── ParameterId ──────────────────────────────────────────────────────────

    #[test]
    fn test_parameter_id_all_has_12_elements() {
        assert_eq!(ParameterId::all().len(), 12);
    }

    #[test]
    fn test_parameter_id_all_are_unique() {
        let ids = ParameterId::all();
        let set: std::collections::HashSet<_> = ids.iter().copied().collect();
        assert_eq!(set.len(), ids.len());
    }

    #[test]
    fn test_parameter_id_name_non_empty_for_all() {
        for &id in ParameterId::all() {
            assert!(!id.name().is_empty(), "empty name for {:?}", id);
        }
    }

    #[test]
    fn test_parameter_id_names_are_unique() {
        let names: std::collections::HashSet<&str> =
            ParameterId::all().iter().map(|id| id.name()).collect();
        assert_eq!(names.len(), 12);
    }

    // ── ParameterSpec ────────────────────────────────────────────────────────

    #[test]
    fn test_parameter_spec_default_valid_for_all() {
        for &id in ParameterId::all() {
            let s = ParameterSpec::default_for(id);
            assert!(s.min < s.max, "{:?}: min >= max", id);
            assert!(s.step > 0.0, "{:?}: step <= 0", id);
            assert!(s.rollback_threshold > 0.0);
        }
    }

    #[test]
    fn test_parameter_spec_midpoint_in_bounds() {
        for &id in ParameterId::all() {
            let s = ParameterSpec::default_for(id);
            let mid = s.midpoint();
            assert!(mid >= s.min && mid <= s.max, "{:?}: mid={mid}", id);
        }
    }

    #[test]
    fn test_channel_buf_spec_bounds() {
        let s = ParameterSpec::default_for(ParameterId::ChannelBufStage1);
        assert_eq!(s.min, 8.0);
        assert_eq!(s.max, 4096.0);
        assert_eq!(s.step, 8.0);
    }

    #[test]
    fn test_dedup_ttl_spec_bounds() {
        let s = ParameterSpec::default_for(ParameterId::DedupTtlMs);
        assert_eq!(s.min, 1_000.0);
        assert_eq!(s.max, 300_000.0);
    }

    // ── PidState ─────────────────────────────────────────────────────────────

    #[test]
    fn test_pid_zero_error_zero_output() {
        let mut pid = PidState::new(0.1, 0.01, 0.05);
        assert_eq!(pid.update(0.0, 1.0), 0.0);
    }

    #[test]
    fn test_pid_positive_error_positive_output() {
        let mut pid = PidState::new(0.5, 0.0, 0.0);
        assert!(pid.update(1.0, 1.0) > 0.0);
    }

    #[test]
    fn test_pid_negative_error_negative_output() {
        let mut pid = PidState::new(0.5, 0.0, 0.0);
        assert!(pid.update(-1.0, 1.0) < 0.0);
    }

    #[test]
    fn test_pid_output_clamped_to_one() {
        let mut pid = PidState::new(1000.0, 0.0, 0.0);
        let out = pid.update(1.0, 1.0);
        assert!((out - 1.0).abs() < 1e-10, "out={out}");
    }

    #[test]
    fn test_pid_output_clamped_to_minus_one() {
        let mut pid = PidState::new(1000.0, 0.0, 0.0);
        let out = pid.update(-1.0, 1.0);
        assert!((out + 1.0).abs() < 1e-10, "out={out}");
    }

    #[test]
    fn test_pid_integral_anti_windup() {
        let mut pid = PidState::new(0.0, 1.0, 0.0);
        for _ in 0..100_000 {
            pid.update(100.0, 0.1);
        }
        assert!(pid.integral.abs() <= 100.0, "integral={}", pid.integral);
    }

    #[test]
    fn test_pid_integral_accumulates() {
        let mut pid = PidState::new(0.0, 1.0, 0.0);
        let out1 = pid.update(0.5, 1.0);
        let out2 = pid.update(0.5, 1.0);
        assert!(out2 > out1, "out1={out1}, out2={out2}");
    }

    #[test]
    fn test_pid_zero_dt_returns_zero() {
        let mut pid = PidState::new(1.0, 1.0, 1.0);
        assert_eq!(pid.update(100.0, 0.0), 0.0);
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    #[test]
    fn test_round_to_step_basic() {
        assert!((pid_round_to_step(7.0, 8.0) - 8.0).abs() < 1e-10);
        assert!((pid_round_to_step(3.9, 8.0) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_round_to_step_exact() {
        assert!((pid_round_to_step(16.0, 8.0) - 16.0).abs() < 1e-10);
    }

    #[test]
    fn test_round_to_step_zero_step_noop() {
        assert!((pid_round_to_step(7.7, 0.0) - 7.7).abs() < 1e-10);
    }

    #[test]
    fn test_avg_p95_ms_empty() {
        assert_eq!(avg_p95_ms(&empty_snap()), 0.0);
    }

    #[test]
    fn test_avg_p95_ms_single_stage() {
        let s = snap(150, 100.0, 0.3);
        assert!((avg_p95_ms(&s) - 150.0).abs() < 1e-6);
    }

    #[test]
    fn test_avg_p95_ms_excludes_zero_sample_stages() {
        let mut s = empty_snap();
        s.stages.push(StageMetrics {
            stage: "a".to_string(),
            queue_depth: 0,
            queue_capacity: 100,
            latency: LatencyStats {
                p50_ms: 0,
                p95_ms: 999,
                p99_ms: 0,
                avg_ms: 0.0,
                sample_count: 0,
            },
            throughput_rps: 0.0,
            error_rate: 0.0,
        });
        assert_eq!(avg_p95_ms(&s), 0.0);
    }

    #[test]
    fn test_avg_throughput_rps_empty() {
        assert_eq!(avg_throughput_rps(&empty_snap()), 0.0);
    }

    #[test]
    fn test_avg_throughput_rps_single_stage() {
        let s = snap(100, 42.5, 0.0);
        assert!((avg_throughput_rps(&s) - 42.5).abs() < 1e-6);
    }

    // ── TuningController ─────────────────────────────────────────────────────

    #[test]
    fn test_controller_new_initialises_all_12() {
        let ctrl = TuningController::new(200.0, 100.0);
        assert_eq!(ctrl.values.len(), 12);
        assert_eq!(ctrl.specs.len(), 12);
        assert_eq!(ctrl.pid_states.len(), 12);
    }

    #[test]
    fn test_get_returns_initial_in_bounds() {
        let ctrl = TuningController::new(200.0, 100.0);
        for &id in ParameterId::all() {
            let spec = ParameterSpec::default_for(id);
            let v = ctrl.get(id);
            assert!(v >= spec.min && v <= spec.max, "{:?} val={v}", id);
        }
    }

    #[test]
    fn test_process_empty_snap_no_panic() {
        let mut ctrl = TuningController::new(200.0, 100.0);
        let _ = ctrl.process(&empty_snap());
    }

    #[test]
    fn test_process_changed_values_in_bounds() {
        let mut ctrl = TuningController::new(200.0, 100.0);
        let changed = ctrl.process(&snap(500, 10.0, 0.9));
        for (id, v) in &changed {
            let spec = ParameterSpec::default_for(*id);
            assert!(*v >= spec.min && *v <= spec.max, "{:?} val={v}", id);
        }
    }

    #[test]
    fn test_process_respects_step_rounding() {
        let mut ctrl = TuningController::new(200.0, 100.0);
        ctrl.process(&snap(500, 10.0, 0.9));
        for &id in ParameterId::all() {
            let spec = ParameterSpec::default_for(id);
            let v = ctrl.get(id);
            let rounded = pid_round_to_step(v, spec.step);
            assert!(
                (v - rounded).abs() < 1e-9,
                "{:?} val={v} rounded={rounded}",
                id
            );
        }
    }

    #[test]
    fn test_process_cooldown_prevents_double_adjustment() {
        let mut ctrl = TuningController::new(50.0, 1000.0);
        let s = snap(999, 1.0, 0.95);
        let first: std::collections::HashSet<ParameterId> =
            ctrl.process(&s).into_iter().map(|(id, _)| id).collect();
        // Second call immediately — cooldowns should prevent re-adjusting same params
        for (id, _) in ctrl.process(&s) {
            assert!(
                !first.contains(&id),
                "{:?} adjusted twice despite cooldown",
                id
            );
        }
    }

    #[test]
    fn test_process_multiple_calls_stay_in_bounds() {
        let mut ctrl = TuningController::new(100.0, 500.0);
        let s = snap(1000, 1.0, 0.99);
        for _ in 0..10 {
            ctrl.process(&s);
        }
        for &id in ParameterId::all() {
            let spec = ParameterSpec::default_for(id);
            let v = ctrl.get(id);
            assert!(v >= spec.min && v <= spec.max, "{:?} = {v}", id);
        }
    }

    #[test]
    fn test_rollback_restores_last_good() {
        let mut ctrl = TuningController::new(200.0, 100.0);
        let original = ctrl.get(ParameterId::DedupTtlMs);
        if let Some(val) = ctrl.values.get_mut(&ParameterId::DedupTtlMs) {
            val.last_good = original;
            val.current = original + 1000.0;
        }
        let restored = ctrl.rollback(ParameterId::DedupTtlMs).unwrap();
        assert!((restored - original).abs() < 1e-9);
        assert!((ctrl.get(ParameterId::DedupTtlMs) - original).abs() < 1e-9);
    }

    #[test]
    fn test_rollback_clears_metric_at_change() {
        let mut ctrl = TuningController::new(200.0, 100.0);
        if let Some(val) = ctrl.values.get_mut(&ParameterId::RateLimiterRefillRate) {
            val.metric_at_change = Some(99.0);
        }
        ctrl.rollback(ParameterId::RateLimiterRefillRate);
        assert!(ctrl.values[&ParameterId::RateLimiterRefillRate]
            .metric_at_change
            .is_none());
    }

    #[test]
    fn test_rollback_resets_pid_integral() {
        let mut ctrl = TuningController::new(200.0, 100.0);
        if let Some(pid) = ctrl.pid_states.get_mut(&ParameterId::ChannelBufStage3) {
            pid.integral = 50.0;
        }
        ctrl.rollback(ParameterId::ChannelBufStage3);
        assert_eq!(
            ctrl.pid_states[&ParameterId::ChannelBufStage3].integral,
            0.0
        );
    }

    #[test]
    fn test_rollback_all_returns_12() {
        let mut ctrl = TuningController::new(200.0, 100.0);
        assert_eq!(ctrl.rollback_all().len(), 12);
    }

    #[test]
    fn test_rollback_all_values_in_bounds() {
        let mut ctrl = TuningController::new(200.0, 100.0);
        // dirty some values
        ctrl.process(&snap(999, 1.0, 0.99));
        for (id, v) in ctrl.rollback_all() {
            let spec = ParameterSpec::default_for(id);
            assert!(v >= spec.min && v <= spec.max, "{:?} = {v}", id);
        }
    }

    #[test]
    fn test_at_target_few_or_no_adjustments() {
        let mut ctrl = TuningController::new(100.0, 200.0);
        let changes = ctrl.process(&snap(100, 200.0, 0.5));
        // At target the error signals are near zero; most should not change
        assert!(changes.len() <= 5, "too many changes: {}", changes.len());
    }
}
