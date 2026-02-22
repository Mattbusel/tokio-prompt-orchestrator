//! # HelixRouter Metrics
//!
//! ## Responsibility
//! Track per-strategy latency (running stats, p95, EMA), dropped/completed
//! counters, cost prediction accuracy, and composite pressure scoring.
//! Expose metrics as snapshots and Prometheus text format.
//!
//! ## Guarantees
//! - Thread-safe: all fields use `Mutex` or atomics for concurrent access.
//! - Bounded: latency samples are capped to prevent unbounded memory growth.
//! - Non-blocking: `record_latency()` acquires a short-held lock.
//!
//! ## NOT Responsible For
//! - Routing decisions (see `router`)
//! - Global Prometheus registry (see `crate::metrics`)

use crate::helix::types::{JobKind, Strategy};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Maximum number of latency samples retained per strategy.
/// Older samples are dropped when this limit is reached.
const MAX_SAMPLES: usize = 10_000;

/// Maximum number of recent outcomes tracked for drop rate calculation.
const RECENT_WINDOW: usize = 100;

// ── LatencyAgg ─────────────────────────────────────────────────────────

/// Running latency aggregator for a single strategy.
///
/// Tracks min, max, sum, count, and retains samples for percentile calculation.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone)]
pub struct LatencyAgg {
    samples: Vec<f64>,
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
}

impl LatencyAgg {
    /// Create a new empty aggregator.
    pub fn new() -> Self {
        Self {
            samples: Vec::new(),
            sum: 0.0,
            count: 0,
            min: f64::MAX,
            max: 0.0,
        }
    }

    /// Record a latency sample in milliseconds.
    pub fn record(&mut self, ms: f64) {
        if self.samples.len() >= MAX_SAMPLES {
            // Ring-buffer style: overwrite oldest
            let idx = (self.count as usize) % MAX_SAMPLES;
            self.samples[idx] = ms;
        } else {
            self.samples.push(ms);
        }
        self.sum += ms;
        self.count += 1;
        if ms < self.min {
            self.min = ms;
        }
        if ms > self.max {
            self.max = ms;
        }
    }

    /// Return the number of recorded samples.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Return the mean latency, or 0.0 if no samples.
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }

    /// Return the minimum latency, or 0.0 if no samples.
    pub fn min(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.min
        }
    }

    /// Return the maximum latency, or 0.0 if no samples.
    pub fn max(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.max
        }
    }

    /// Compute the p-th percentile (0–100) from retained samples.
    ///
    /// Uses nearest-rank method. Returns 0.0 if no samples.
    pub fn percentile(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        let idx = rank.saturating_sub(1).min(sorted.len() - 1);
        sorted[idx]
    }

    /// Compute p95.
    pub fn p95(&self) -> f64 {
        self.percentile(95.0)
    }

    /// Compute p99.
    pub fn p99(&self) -> f64 {
        self.percentile(99.0)
    }
}

impl Default for LatencyAgg {
    fn default() -> Self {
        Self::new()
    }
}

// ── LatencySummary ─────────────────────────────────────────────────────

/// A point-in-time snapshot of latency statistics for one strategy.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone)]
pub struct LatencySummary {
    /// Number of samples.
    pub count: u64,
    /// Mean latency in ms.
    pub mean_ms: f64,
    /// Minimum latency in ms.
    pub min_ms: f64,
    /// Maximum latency in ms.
    pub max_ms: f64,
    /// P95 latency in ms.
    pub p95_ms: f64,
    /// P99 latency in ms.
    pub p99_ms: f64,
}

// ── MetricsStore ───────────────────────────────────────────────────────

/// Central metrics store for the HelixRouter.
///
/// Thread-safe. All public methods acquire short-held locks.
///
/// # Panics
///
/// This type never panics.
pub struct MetricsStore {
    latencies: Mutex<HashMap<Strategy, LatencyAgg>>,
    ema_latencies: Mutex<HashMap<Strategy, f64>>,
    ema_alpha: f64,
    completed: AtomicU64,
    dropped: AtomicU64,
    recent_outcomes: Mutex<Vec<bool>>,
    cost_predicted: Mutex<HashMap<JobKind, Vec<f64>>>,
    cost_actual: Mutex<HashMap<JobKind, Vec<f64>>>,
}

impl MetricsStore {
    /// Create a new empty metrics store.
    ///
    /// # Arguments
    ///
    /// * `ema_alpha` — EMA smoothing factor (0.0–1.0).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(ema_alpha: f64) -> Self {
        Self {
            latencies: Mutex::new(HashMap::new()),
            ema_latencies: Mutex::new(HashMap::new()),
            ema_alpha,
            completed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            recent_outcomes: Mutex::new(Vec::with_capacity(RECENT_WINDOW)),
            cost_predicted: Mutex::new(HashMap::new()),
            cost_actual: Mutex::new(HashMap::new()),
        }
    }

    /// Record a latency sample for a strategy.
    ///
    /// Also updates the EMA latency and marks the outcome as completed.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn record_latency(&self, strategy: Strategy, ms: f64) {
        // Update latency aggregator
        if let Ok(mut latencies) = self.latencies.lock() {
            latencies.entry(strategy).or_default().record(ms);
        }

        // Update EMA
        if let Ok(mut emas) = self.ema_latencies.lock() {
            let ema = emas.entry(strategy).or_insert(ms);
            *ema = self.ema_alpha * ms + (1.0 - self.ema_alpha) * *ema;
        }

        // Track as completed
        self.completed.fetch_add(1, Ordering::Relaxed);
        self.record_outcome(true);
    }

    /// Record a dropped job.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn record_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        self.record_outcome(false);
    }

    /// Record a cost prediction vs actual outcome.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn record_cost(&self, kind: JobKind, predicted: f64, actual: f64) {
        if let Ok(mut preds) = self.cost_predicted.lock() {
            preds.entry(kind).or_default().push(predicted);
        }
        if let Ok(mut acts) = self.cost_actual.lock() {
            acts.entry(kind).or_default().push(actual);
        }
    }

    /// Get the EMA latency for a strategy, or 0.0 if no data.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn ema_latency(&self, strategy: Strategy) -> f64 {
        self.ema_latencies
            .lock()
            .ok()
            .and_then(|m| m.get(&strategy).copied())
            .unwrap_or(0.0)
    }

    /// Get the p95 latency for a strategy, or 0.0 if no data.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn p95(&self, strategy: Strategy) -> f64 {
        self.latencies
            .lock()
            .ok()
            .and_then(|m| m.get(&strategy).map(|a| a.p95()))
            .unwrap_or(0.0)
    }

    /// Get a latency summary for a strategy.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn latency_summary(&self, strategy: Strategy) -> LatencySummary {
        self.latencies
            .lock()
            .ok()
            .and_then(|m| {
                m.get(&strategy).map(|a| LatencySummary {
                    count: a.count(),
                    mean_ms: a.mean(),
                    min_ms: a.min(),
                    max_ms: a.max(),
                    p95_ms: a.p95(),
                    p99_ms: a.p99(),
                })
            })
            .unwrap_or(LatencySummary {
                count: 0,
                mean_ms: 0.0,
                min_ms: 0.0,
                max_ms: 0.0,
                p95_ms: 0.0,
                p99_ms: 0.0,
            })
    }

    /// Get the total completed count.
    pub fn completed(&self) -> u64 {
        self.completed.load(Ordering::Relaxed)
    }

    /// Get the total dropped count.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Get the recent drop rate (sliding window).
    ///
    /// Returns 0.0 if no recent outcomes.
    pub fn drop_rate(&self) -> f64 {
        self.recent_outcomes
            .lock()
            .ok()
            .map(|outcomes| {
                if outcomes.is_empty() {
                    0.0
                } else {
                    let drops = outcomes.iter().filter(|&&ok| !ok).count();
                    drops as f64 / outcomes.len() as f64
                }
            })
            .unwrap_or(0.0)
    }

    /// Get the cost prediction accuracy for a job kind.
    ///
    /// Returns the ratio of mean(predicted) / mean(actual), or 1.0 if no data.
    pub fn cost_accuracy(&self, kind: JobKind) -> f64 {
        let pred_mean = self
            .cost_predicted
            .lock()
            .ok()
            .and_then(|m| {
                m.get(&kind).map(|v| {
                    if v.is_empty() {
                        0.0
                    } else {
                        v.iter().sum::<f64>() / v.len() as f64
                    }
                })
            })
            .unwrap_or(0.0);

        let actual_mean = self
            .cost_actual
            .lock()
            .ok()
            .and_then(|m| {
                m.get(&kind).map(|v| {
                    if v.is_empty() {
                        0.0
                    } else {
                        v.iter().sum::<f64>() / v.len() as f64
                    }
                })
            })
            .unwrap_or(0.0);

        if actual_mean <= 0.0 {
            1.0
        } else {
            pred_mean / actual_mean
        }
    }

    /// Compute the composite pressure score in [0, 1].
    ///
    /// # Arguments
    ///
    /// * `cpu_busy` — Number of currently busy CPU workers.
    /// * `cpu_parallelism` — Total CPU parallelism.
    /// * `weights` — Pressure scoring weights.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn pressure_score(
        &self,
        cpu_busy: usize,
        cpu_parallelism: usize,
        weights: &super::config::PressureConfig,
    ) -> f64 {
        let queue_depth_norm = if cpu_parallelism == 0 {
            0.0
        } else {
            (cpu_busy as f64 / cpu_parallelism as f64).min(1.0)
        };

        // Latency trend: compare current EMA to baseline (mean of all strategies)
        let latency_trend = self
            .ema_latencies
            .lock()
            .ok()
            .map(|emas| {
                if emas.is_empty() {
                    0.0
                } else {
                    let values: Vec<f64> = emas.values().copied().collect();
                    let mean = values.iter().sum::<f64>() / values.len() as f64;
                    if mean <= 0.0 {
                        0.0
                    } else {
                        // Trend > 0 means worsening; cap at 1.0
                        let max_val = values.iter().cloned().fold(0.0_f64, f64::max);
                        ((max_val - mean) / mean).clamp(0.0, 1.0)
                    }
                }
            })
            .unwrap_or(0.0);

        let drop_rate = self.drop_rate();

        let raw = weights.weight_queue_depth * queue_depth_norm
            + weights.weight_latency_trend * latency_trend
            + weights.weight_drop_rate * drop_rate;

        raw.clamp(0.0, 1.0)
    }

    /// Generate a full stats snapshot.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn stats_snapshot(&self) -> RouterStats {
        let mut latencies = HashMap::new();
        if let Ok(lat_map) = self.latencies.lock() {
            for (&strategy, agg) in lat_map.iter() {
                latencies.insert(
                    strategy,
                    LatencySummary {
                        count: agg.count(),
                        mean_ms: agg.mean(),
                        min_ms: agg.min(),
                        max_ms: agg.max(),
                        p95_ms: agg.p95(),
                        p99_ms: agg.p99(),
                    },
                );
            }
        }

        RouterStats {
            latencies,
            completed: self.completed(),
            dropped: self.dropped(),
            drop_rate: self.drop_rate(),
        }
    }

    /// Render metrics in Prometheus text exposition format.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();

        out.push_str("# HELP helix_completed_total Total completed jobs\n");
        out.push_str("# TYPE helix_completed_total counter\n");
        out.push_str(&format!("helix_completed_total {}\n", self.completed()));

        out.push_str("# HELP helix_dropped_total Total dropped jobs\n");
        out.push_str("# TYPE helix_dropped_total counter\n");
        out.push_str(&format!("helix_dropped_total {}\n", self.dropped()));

        out.push_str("# HELP helix_drop_rate Current drop rate\n");
        out.push_str("# TYPE helix_drop_rate gauge\n");
        out.push_str(&format!("helix_drop_rate {:.4}\n", self.drop_rate()));

        if let Ok(lat_map) = self.latencies.lock() {
            out.push_str("# HELP helix_latency_p95_ms P95 latency per strategy\n");
            out.push_str("# TYPE helix_latency_p95_ms gauge\n");
            for (&strategy, agg) in lat_map.iter() {
                out.push_str(&format!(
                    "helix_latency_p95_ms{{strategy=\"{}\"}} {:.4}\n",
                    strategy,
                    agg.p95()
                ));
            }

            out.push_str("# HELP helix_latency_mean_ms Mean latency per strategy\n");
            out.push_str("# TYPE helix_latency_mean_ms gauge\n");
            for (&strategy, agg) in lat_map.iter() {
                out.push_str(&format!(
                    "helix_latency_mean_ms{{strategy=\"{}\"}} {:.4}\n",
                    strategy,
                    agg.mean()
                ));
            }

            out.push_str("# HELP helix_strategy_count Total jobs per strategy\n");
            out.push_str("# TYPE helix_strategy_count counter\n");
            for (&strategy, agg) in lat_map.iter() {
                out.push_str(&format!(
                    "helix_strategy_count{{strategy=\"{}\"}} {}\n",
                    strategy,
                    agg.count()
                ));
            }
        }

        out
    }

    // ── Private helpers ────────────────────────────────────────────────

    fn record_outcome(&self, success: bool) {
        if let Ok(mut outcomes) = self.recent_outcomes.lock() {
            if outcomes.len() >= RECENT_WINDOW {
                outcomes.remove(0);
            }
            outcomes.push(success);
        }
    }
}

impl std::fmt::Debug for MetricsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsStore")
            .field("completed", &self.completed())
            .field("dropped", &self.dropped())
            .finish()
    }
}

/// A point-in-time stats snapshot.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone)]
pub struct RouterStats {
    /// Per-strategy latency summaries.
    pub latencies: HashMap<Strategy, LatencySummary>,
    /// Total completed jobs.
    pub completed: u64,
    /// Total dropped jobs.
    pub dropped: u64,
    /// Recent drop rate.
    pub drop_rate: f64,
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- LatencyAgg -------------------------------------------------------

    #[test]
    fn test_latency_agg_new_is_empty() {
        let agg = LatencyAgg::new();
        assert_eq!(agg.count(), 0);
        assert!(agg.mean().abs() < f64::EPSILON);
        assert!(agg.min().abs() < f64::EPSILON);
        assert!(agg.max().abs() < f64::EPSILON);
    }

    #[test]
    fn test_latency_agg_single_sample() {
        let mut agg = LatencyAgg::new();
        agg.record(5.0);
        assert_eq!(agg.count(), 1);
        assert!((agg.mean() - 5.0).abs() < f64::EPSILON);
        assert!((agg.min() - 5.0).abs() < f64::EPSILON);
        assert!((agg.max() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_latency_agg_multiple_samples() {
        let mut agg = LatencyAgg::new();
        agg.record(1.0);
        agg.record(2.0);
        agg.record(3.0);
        assert_eq!(agg.count(), 3);
        assert!((agg.mean() - 2.0).abs() < f64::EPSILON);
        assert!((agg.min() - 1.0).abs() < f64::EPSILON);
        assert!((agg.max() - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_latency_agg_p95_single_sample_returns_that_sample() {
        let mut agg = LatencyAgg::new();
        agg.record(10.0);
        assert!((agg.p95() - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_latency_agg_p95_ordered_samples() {
        let mut agg = LatencyAgg::new();
        for i in 1..=100 {
            agg.record(i as f64);
        }
        // P95 of 1..100 = 95th value = 95.0
        let p95 = agg.p95();
        assert!((p95 - 95.0).abs() < 1.0, "p95 should be ~95.0, got {p95}");
    }

    #[test]
    fn test_latency_agg_p99_ordered_samples() {
        let mut agg = LatencyAgg::new();
        for i in 1..=100 {
            agg.record(i as f64);
        }
        let p99 = agg.p99();
        assert!((p99 - 99.0).abs() < 1.0, "p99 should be ~99.0, got {p99}");
    }

    #[test]
    fn test_latency_agg_percentile_empty_returns_zero() {
        let agg = LatencyAgg::new();
        assert!(agg.percentile(50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_latency_agg_ring_buffer_capped() {
        let mut agg = LatencyAgg::new();
        for i in 0..(MAX_SAMPLES + 100) {
            agg.record(i as f64);
        }
        assert!(
            agg.samples.len() <= MAX_SAMPLES,
            "samples should not exceed MAX_SAMPLES"
        );
        assert_eq!(agg.count() as usize, MAX_SAMPLES + 100);
    }

    // -- MetricsStore ----------------------------------------------------

    #[test]
    fn test_metrics_store_new_is_empty() {
        let store = MetricsStore::new(0.3);
        assert_eq!(store.completed(), 0);
        assert_eq!(store.dropped(), 0);
        assert!(store.drop_rate().abs() < f64::EPSILON);
    }

    #[test]
    fn test_metrics_store_record_latency_increments_completed() {
        let store = MetricsStore::new(0.3);
        store.record_latency(Strategy::Inline, 1.0);
        store.record_latency(Strategy::Inline, 2.0);
        assert_eq!(store.completed(), 2);
    }

    #[test]
    fn test_metrics_store_record_drop_increments_dropped() {
        let store = MetricsStore::new(0.3);
        store.record_drop();
        store.record_drop();
        assert_eq!(store.dropped(), 2);
    }

    #[test]
    fn test_metrics_store_ema_latency_converges() {
        let store = MetricsStore::new(0.3);
        // Feed constant 10ms samples — EMA should converge to 10.0
        for _ in 0..100 {
            store.record_latency(Strategy::Spawn, 10.0);
        }
        let ema = store.ema_latency(Strategy::Spawn);
        assert!(
            (ema - 10.0).abs() < 0.1,
            "EMA should converge to 10.0, got {ema}"
        );
    }

    #[test]
    fn test_metrics_store_ema_latency_no_data_returns_zero() {
        let store = MetricsStore::new(0.3);
        assert!(store.ema_latency(Strategy::CpuPool).abs() < f64::EPSILON);
    }

    #[test]
    fn test_metrics_store_p95_returns_correct_value() {
        let store = MetricsStore::new(0.3);
        for i in 1..=100 {
            store.record_latency(Strategy::Inline, i as f64);
        }
        let p95 = store.p95(Strategy::Inline);
        assert!((p95 - 95.0).abs() < 1.0, "p95 should be ~95.0, got {p95}");
    }

    #[test]
    fn test_metrics_store_drop_rate_all_drops() {
        let store = MetricsStore::new(0.3);
        for _ in 0..10 {
            store.record_drop();
        }
        assert!(
            (store.drop_rate() - 1.0).abs() < f64::EPSILON,
            "all drops should give rate 1.0"
        );
    }

    #[test]
    fn test_metrics_store_drop_rate_mixed() {
        let store = MetricsStore::new(0.3);
        for _ in 0..5 {
            store.record_latency(Strategy::Inline, 1.0);
        }
        for _ in 0..5 {
            store.record_drop();
        }
        assert!(
            (store.drop_rate() - 0.5).abs() < f64::EPSILON,
            "50% drops should give rate 0.5, got {}",
            store.drop_rate()
        );
    }

    #[test]
    fn test_metrics_store_drop_rate_window_slides() {
        let store = MetricsStore::new(0.3);
        // Fill window with successes
        for _ in 0..RECENT_WINDOW {
            store.record_latency(Strategy::Inline, 1.0);
        }
        assert!(store.drop_rate().abs() < f64::EPSILON);

        // Now add drops — they push out successes
        for _ in 0..RECENT_WINDOW {
            store.record_drop();
        }
        assert!(
            (store.drop_rate() - 1.0).abs() < f64::EPSILON,
            "after sliding window, drop rate should be 1.0"
        );
    }

    #[test]
    fn test_metrics_store_cost_accuracy_no_data_returns_one() {
        let store = MetricsStore::new(0.3);
        assert!((store.cost_accuracy(JobKind::Hash) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_metrics_store_cost_accuracy_perfect_prediction() {
        let store = MetricsStore::new(0.3);
        store.record_cost(JobKind::Hash, 10.0, 10.0);
        store.record_cost(JobKind::Hash, 20.0, 20.0);
        let acc = store.cost_accuracy(JobKind::Hash);
        assert!(
            (acc - 1.0).abs() < f64::EPSILON,
            "perfect prediction should give accuracy 1.0"
        );
    }

    #[test]
    fn test_metrics_store_cost_accuracy_overestimate() {
        let store = MetricsStore::new(0.3);
        store.record_cost(JobKind::Prime, 20.0, 10.0);
        let acc = store.cost_accuracy(JobKind::Prime);
        assert!(
            (acc - 2.0).abs() < f64::EPSILON,
            "2x overestimate should give accuracy 2.0"
        );
    }

    #[test]
    fn test_metrics_store_pressure_score_zero_when_idle() {
        let store = MetricsStore::new(0.3);
        let weights = super::super::config::PressureConfig::default();
        let pressure = store.pressure_score(0, 4, &weights);
        assert!(
            pressure.abs() < f64::EPSILON,
            "idle system should have zero pressure"
        );
    }

    #[test]
    fn test_metrics_store_pressure_score_max_when_saturated() {
        let store = MetricsStore::new(0.3);
        // Drive all outcomes as drops
        for _ in 0..RECENT_WINDOW {
            store.record_drop();
        }
        let weights = super::super::config::PressureConfig::default();
        // cpu_busy == cpu_parallelism → queue_depth_norm = 1.0, drop_rate = 1.0
        let pressure = store.pressure_score(4, 4, &weights);
        assert!(
            pressure > 0.5,
            "saturated system should have high pressure, got {pressure}"
        );
    }

    #[test]
    fn test_metrics_store_pressure_score_bounded_zero_one() {
        let store = MetricsStore::new(0.3);
        let weights = super::super::config::PressureConfig::default();
        for cpu_busy in 0..=10 {
            let p = store.pressure_score(cpu_busy, 4, &weights);
            assert!(
                (0.0..=1.0).contains(&p),
                "pressure must be in [0,1], got {p}"
            );
        }
    }

    #[test]
    fn test_metrics_store_stats_snapshot() {
        let store = MetricsStore::new(0.3);
        store.record_latency(Strategy::Inline, 1.0);
        store.record_latency(Strategy::Spawn, 2.0);
        store.record_drop();

        let snap = store.stats_snapshot();
        assert_eq!(snap.completed, 2);
        assert_eq!(snap.dropped, 1);
        assert!(snap.latencies.contains_key(&Strategy::Inline));
        assert!(snap.latencies.contains_key(&Strategy::Spawn));
    }

    #[test]
    fn test_metrics_store_latency_summary_no_data() {
        let store = MetricsStore::new(0.3);
        let summary = store.latency_summary(Strategy::Batch);
        assert_eq!(summary.count, 0);
        assert!(summary.mean_ms.abs() < f64::EPSILON);
    }

    #[test]
    fn test_metrics_store_render_prometheus_not_empty() {
        let store = MetricsStore::new(0.3);
        store.record_latency(Strategy::Inline, 1.0);
        store.record_drop();
        let prom = store.render_prometheus();
        assert!(prom.contains("helix_completed_total"));
        assert!(prom.contains("helix_dropped_total"));
        assert!(prom.contains("helix_drop_rate"));
        assert!(prom.contains("helix_latency_p95_ms"));
    }

    #[test]
    fn test_metrics_store_render_prometheus_empty_store() {
        let store = MetricsStore::new(0.3);
        let prom = store.render_prometheus();
        assert!(prom.contains("helix_completed_total 0"));
        assert!(prom.contains("helix_dropped_total 0"));
    }

    #[test]
    fn test_metrics_store_debug_does_not_panic() {
        let store = MetricsStore::new(0.3);
        let _ = format!("{store:?}");
    }

    // -- Additional LatencyAgg tests ----------------------------------------

    #[test]
    fn test_latency_agg_min_max_tracking() {
        let mut agg = LatencyAgg::new();
        agg.record(1.0);
        agg.record(5.0);
        agg.record(3.0);
        assert!(
            (agg.min() - 1.0).abs() < f64::EPSILON,
            "min should be 1.0, got {}",
            agg.min()
        );
        assert!(
            (agg.max() - 5.0).abs() < f64::EPSILON,
            "max should be 5.0, got {}",
            agg.max()
        );
    }

    #[test]
    fn test_latency_agg_mean_calculation() {
        let mut agg = LatencyAgg::new();
        agg.record(2.0);
        agg.record(4.0);
        agg.record(6.0);
        assert!(
            (agg.mean() - 4.0).abs() < f64::EPSILON,
            "mean of [2,4,6] should be 4.0, got {}",
            agg.mean()
        );
    }

    #[test]
    fn test_latency_agg_p99_calculation() {
        let mut agg = LatencyAgg::new();
        for i in 0..100 {
            agg.record(i as f64);
        }
        let p99 = agg.p99();
        assert!(
            (p99 - 99.0).abs() < 1.5,
            "p99 of 0..100 should be ~99, got {p99}"
        );
    }

    #[test]
    fn test_latency_agg_ring_buffer_overflow() {
        let mut agg = LatencyAgg::new();
        let total = MAX_SAMPLES + 100;
        for i in 0..total {
            agg.record(i as f64);
        }
        assert_eq!(
            agg.samples.len(),
            MAX_SAMPLES,
            "samples vec should be exactly MAX_SAMPLES after overflow"
        );
        assert_eq!(
            agg.count() as usize,
            total,
            "count should reflect all recorded values including overflows"
        );
    }

    // -- Additional MetricsStore tests --------------------------------------

    #[test]
    fn test_metrics_store_ema_converges() {
        let store = MetricsStore::new(0.5);
        // Record many identical values — EMA must converge to that value
        for _ in 0..200 {
            store.record_latency(Strategy::Inline, 42.0);
        }
        let ema = store.ema_latency(Strategy::Inline);
        assert!(
            (ema - 42.0).abs() < 0.01,
            "EMA should converge to 42.0 with alpha=0.5 after 200 samples, got {ema}"
        );
    }

    #[test]
    fn test_metrics_store_drop_rate_window() {
        let store = MetricsStore::new(0.3);
        // Record 80 successes then 20 drops = 100 total outcomes
        for _ in 0..80 {
            store.record_latency(Strategy::Inline, 1.0);
        }
        for _ in 0..20 {
            store.record_drop();
        }
        let rate = store.drop_rate();
        assert!(
            (rate - 0.2).abs() < f64::EPSILON,
            "drop rate should be 0.2 (20 of 100), got {rate}"
        );
    }

    #[test]
    fn test_metrics_store_pressure_score_zero_when_idle_no_data() {
        let store = MetricsStore::new(0.3);
        let weights = super::super::config::PressureConfig::default();
        let pressure = store.pressure_score(0, 8, &weights);
        assert!(
            pressure.abs() < f64::EPSILON,
            "fresh store with no busy workers should have pressure=0, got {pressure}"
        );
    }

    #[test]
    fn test_metrics_store_pressure_score_capped_at_one() {
        let store = MetricsStore::new(0.3);
        // Create extreme conditions: all drops, high busy count
        for _ in 0..RECENT_WINDOW {
            store.record_drop();
        }
        // Record divergent EMA latencies to create latency trend signal
        store.record_latency(Strategy::Inline, 1.0);
        store.record_latency(Strategy::CpuPool, 1000.0);

        let weights = super::super::config::PressureConfig {
            weight_queue_depth: 10.0,
            weight_latency_trend: 10.0,
            weight_drop_rate: 10.0,
            drop_threshold: 0.8,
        };
        let pressure = store.pressure_score(100, 4, &weights);
        assert!(
            pressure <= 1.0,
            "pressure score must be capped at 1.0, got {pressure}"
        );
    }

    #[test]
    fn test_metrics_store_stats_snapshot_includes_all_strategies() {
        let store = MetricsStore::new(0.3);
        store.record_latency(Strategy::Inline, 1.0);
        store.record_latency(Strategy::Spawn, 2.0);
        store.record_latency(Strategy::CpuPool, 3.0);
        store.record_latency(Strategy::Batch, 4.0);

        let snap = store.stats_snapshot();
        assert!(
            snap.latencies.contains_key(&Strategy::Inline),
            "snapshot should include Inline"
        );
        assert!(
            snap.latencies.contains_key(&Strategy::Spawn),
            "snapshot should include Spawn"
        );
        assert!(
            snap.latencies.contains_key(&Strategy::CpuPool),
            "snapshot should include CpuPool"
        );
        assert!(
            snap.latencies.contains_key(&Strategy::Batch),
            "snapshot should include Batch"
        );
        assert_eq!(snap.completed, 4);
    }

    #[test]
    fn test_metrics_store_cost_accuracy_no_data_returns_one_explicit() {
        let store = MetricsStore::new(0.3);
        let acc = store.cost_accuracy(JobKind::MonteCarlo);
        assert!(
            (acc - 1.0).abs() < f64::EPSILON,
            "cost_accuracy with no data should default to 1.0, got {acc}"
        );
    }

    #[test]
    fn test_metrics_store_cost_accuracy_with_data() {
        let store = MetricsStore::new(0.3);
        store.record_cost(JobKind::Hash, 10.0, 5.0);
        let acc = store.cost_accuracy(JobKind::Hash);
        assert!(
            (acc - 2.0).abs() < f64::EPSILON,
            "predicted=10, actual=5 should give ratio=2.0, got {acc}"
        );
    }

    #[test]
    fn test_render_prometheus_contains_headers() {
        let store = MetricsStore::new(0.3);
        store.record_latency(Strategy::Inline, 5.0);
        let output = store.render_prometheus();
        assert!(
            output.contains("# HELP"),
            "prometheus output should contain # HELP header"
        );
        assert!(
            output.contains("# TYPE"),
            "prometheus output should contain # TYPE header"
        );
    }

    #[test]
    fn test_render_prometheus_contains_counters() {
        let store = MetricsStore::new(0.3);
        store.record_latency(Strategy::Inline, 1.0);
        store.record_drop();
        let output = store.render_prometheus();
        assert!(
            output.contains("helix_completed_total 1"),
            "prometheus output should contain completed counter"
        );
        assert!(
            output.contains("helix_dropped_total 1"),
            "prometheus output should contain dropped counter"
        );
    }

    #[test]
    fn test_latency_summary_default_values() {
        let store = MetricsStore::new(0.3);
        // Request summary for a strategy with no data
        let summary = store.latency_summary(Strategy::Drop);
        assert_eq!(summary.count, 0, "count should be 0 for empty strategy");
        assert!(
            summary.mean_ms.abs() < f64::EPSILON,
            "mean should be 0.0 for empty strategy"
        );
        assert!(
            summary.min_ms.abs() < f64::EPSILON,
            "min should be 0.0 for empty strategy"
        );
        assert!(
            summary.max_ms.abs() < f64::EPSILON,
            "max should be 0.0 for empty strategy"
        );
        assert!(
            summary.p95_ms.abs() < f64::EPSILON,
            "p95 should be 0.0 for empty strategy"
        );
        assert!(
            summary.p99_ms.abs() < f64::EPSILON,
            "p99 should be 0.0 for empty strategy"
        );
    }
}
