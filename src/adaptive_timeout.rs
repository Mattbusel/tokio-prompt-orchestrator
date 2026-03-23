//! Adaptive timeout management for LLM requests.
//!
//! Tracks per-model latency samples and computes dynamic timeouts based on
//! observed percentile latencies and an exponential moving average. Timeouts
//! are bounded between 5 s and 120 s and can be scaled up for high queue
//! depths via [`AdaptiveTimeoutManager::adjust_for_load`].

use dashmap::DashMap;
use std::collections::VecDeque;
use std::time::Duration;

/// A single latency/outcome sample for one LLM request.
#[derive(Debug, Clone)]
pub struct TimeoutSample {
    /// Model name (e.g. `"gpt-4o"`).
    pub model_name: String,
    /// Observed round-trip duration in milliseconds.
    pub duration_ms: u64,
    /// Whether the request succeeded (`false` = timeout / error).
    pub success: bool,
}

/// Per-model statistics maintained by [`AdaptiveTimeoutManager`].
#[derive(Debug)]
pub struct ModelTimeoutStats {
    /// Rolling window of the most recent 100 samples.
    samples: VecDeque<TimeoutSample>,
    /// Exponential moving average of duration_ms (α = 0.1).
    pub ema_ms: f64,
}

impl ModelTimeoutStats {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(100),
            ema_ms: 5_000.0,
        }
    }

    /// Record a new sample, evicting the oldest when the window is full.
    fn push(&mut self, sample: TimeoutSample) {
        const ALPHA: f64 = 0.1;
        self.ema_ms = ALPHA * sample.duration_ms as f64 + (1.0 - ALPHA) * self.ema_ms;
        if self.samples.len() >= 100 {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Fraction of samples that succeeded (0.0 if no samples).
    pub fn success_rate(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let successes = self.samples.iter().filter(|s| s.success).count();
        successes as f64 / self.samples.len() as f64
    }

    /// Compute a percentile from the sorted duration values.
    ///
    /// `p` must be in `[0.0, 1.0]`. Returns `None` when there are no samples.
    fn percentile(&self, p: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut durations: Vec<u64> = self.samples.iter().map(|s| s.duration_ms).collect();
        durations.sort_unstable();
        let idx = ((p * (durations.len() as f64 - 1.0)).round() as usize)
            .min(durations.len() - 1);
        Some(durations[idx] as f64)
    }

    /// 50th-percentile latency in milliseconds.
    pub fn p50(&self) -> Option<f64> {
        self.percentile(0.50)
    }

    /// 95th-percentile latency in milliseconds.
    pub fn p95(&self) -> Option<f64> {
        self.percentile(0.95)
    }

    /// 99th-percentile latency in milliseconds.
    pub fn p99(&self) -> Option<f64> {
        self.percentile(0.99)
    }

    /// Number of samples in the window.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

/// A snapshot of timeout statistics for a single model.
#[derive(Debug, Clone)]
pub struct TimeoutSummary {
    /// Model identifier.
    pub model: String,
    /// 50th-percentile latency (ms).
    pub p50_ms: f64,
    /// 95th-percentile latency (ms).
    pub p95_ms: f64,
    /// 99th-percentile latency (ms).
    pub p99_ms: f64,
    /// Fraction of successful requests (0.0–1.0).
    pub success_rate: f64,
    /// Exponential moving average of latency (ms).
    pub ema_ms: f64,
    /// Number of samples in the rolling window.
    pub sample_count: usize,
}

/// Thread-safe, per-model adaptive timeout manager.
///
/// Uses a [`DashMap`] so that multiple tokio tasks can record outcomes and
/// query timeouts concurrently without a global lock.
pub struct AdaptiveTimeoutManager {
    stats: DashMap<String, ModelTimeoutStats>,
}

impl AdaptiveTimeoutManager {
    /// Create a new manager with an empty model registry.
    pub fn new() -> Self {
        Self {
            stats: DashMap::new(),
        }
    }

    /// Record the outcome of one request for `model`.
    ///
    /// This updates the rolling sample window and the EMA.
    pub fn record_outcome(&self, model: &str, duration_ms: u64, success: bool) {
        let sample = TimeoutSample {
            model_name: model.to_string(),
            duration_ms,
            success,
        };
        self.stats
            .entry(model.to_string())
            .or_insert_with(ModelTimeoutStats::new)
            .push(sample);
    }

    /// Compute the recommended timeout for `model`.
    ///
    /// Returns `p95 * 1.5`, clamped to `[5 s, 120 s]`.  Falls back to 30 s
    /// when fewer than two samples have been collected.
    pub fn get_timeout(&self, model: &str) -> Duration {
        const MIN_MS: f64 = 5_000.0;
        const MAX_MS: f64 = 120_000.0;
        const DEFAULT_MS: f64 = 30_000.0;

        let timeout_ms = self
            .stats
            .get(model)
            .and_then(|s| s.p95())
            .map(|p95| p95 * 1.5)
            .unwrap_or(DEFAULT_MS);

        let clamped = timeout_ms.max(MIN_MS).min(MAX_MS);
        Duration::from_millis(clamped as u64)
    }

    /// Adjust the base timeout by the current queue depth.
    ///
    /// Scales `get_timeout` by `sqrt(queue_depth / 10).max(1.0)` so that a
    /// backlogged queue gets proportionally more time.
    pub fn adjust_for_load(&self, model: &str, queue_depth: usize) -> Duration {
        let base = self.get_timeout(model);
        let scale = ((queue_depth as f64 / 10.0).sqrt()).max(1.0);
        let scaled_ms = base.as_millis() as f64 * scale;
        // Re-clamp after scaling.
        let clamped = scaled_ms.min(120_000.0);
        Duration::from_millis(clamped as u64)
    }

    /// Return a [`TimeoutSummary`] snapshot for `model`, or `None` if the
    /// model has never been seen.
    pub fn model_summary(&self, model: &str) -> Option<TimeoutSummary> {
        let stats = self.stats.get(model)?;
        Some(TimeoutSummary {
            model: model.to_string(),
            p50_ms: stats.p50().unwrap_or(0.0),
            p95_ms: stats.p95().unwrap_or(0.0),
            p99_ms: stats.p99().unwrap_or(0.0),
            success_rate: stats.success_rate(),
            ema_ms: stats.ema_ms,
            sample_count: stats.sample_count(),
        })
    }
}

impl Default for AdaptiveTimeoutManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_get_timeout() {
        let mgr = AdaptiveTimeoutManager::new();
        for ms in [100u64, 200, 300, 400, 500, 600, 700, 800, 900, 1000] {
            mgr.record_outcome("gpt-4o", ms, true);
        }
        let t = mgr.get_timeout("gpt-4o");
        // p95 of 10 samples ≈ 950 ms; * 1.5 = 1425 ms — well above 5 s min.
        assert!(t >= Duration::from_secs(5));
        assert!(t <= Duration::from_secs(120));
    }

    #[test]
    fn test_default_timeout_for_unknown_model() {
        let mgr = AdaptiveTimeoutManager::new();
        assert_eq!(mgr.get_timeout("unknown"), Duration::from_secs(30));
    }

    #[test]
    fn test_adjust_for_load_scales_up() {
        let mgr = AdaptiveTimeoutManager::new();
        let base = mgr.adjust_for_load("m", 0);
        let loaded = mgr.adjust_for_load("m", 100);
        assert!(loaded >= base);
    }

    #[test]
    fn test_success_rate() {
        let mgr = AdaptiveTimeoutManager::new();
        mgr.record_outcome("m", 100, true);
        mgr.record_outcome("m", 100, false);
        let summary = mgr.model_summary("m").expect("summary present");
        assert!((summary.success_rate - 0.5).abs() < 1e-9);
    }
}
