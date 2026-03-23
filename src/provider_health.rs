//! # Provider Health Monitor
//!
//! Tracks latency, error rates, and availability for each configured
//! LLM provider. Exposes a health score (0.0–1.0) per provider.
//!
//! The monitor maintains a rolling window of latency samples (in milliseconds)
//! per provider. After every [`record`] call the p50/p95/p99 latencies and
//! the rolling error rate are recomputed from the current window, and a fresh
//! health score is derived via the formula:
//!
//! ```text
//! score = (1.0 - error_rate) * latency_weight
//! where latency_weight = 1.0 / (1.0 + p95_latency_ms / 1000.0)
//! ```
//!
//! [`record`]: ProviderHealthMonitor::record

use std::{
    collections::HashMap,
    sync::Arc,
    time::Instant,
};
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// ProviderHealth snapshot
// ---------------------------------------------------------------------------

/// A point-in-time health snapshot for a single provider.
#[derive(Debug, Clone)]
pub struct ProviderHealth {
    /// Stable identifier matching the provider key used when calling [`record`].
    ///
    /// [`record`]: ProviderHealthMonitor::record
    pub provider_id: String,
    /// `true` while `consecutive_failures` is below the hard-failure threshold
    /// (currently 5) **and** `health_score > 0.0`.
    pub is_reachable: bool,
    /// 50th-percentile latency over the current sample window (milliseconds).
    pub p50_latency_ms: f64,
    /// 95th-percentile latency over the current sample window (milliseconds).
    pub p95_latency_ms: f64,
    /// 99th-percentile latency over the current sample window (milliseconds).
    pub p99_latency_ms: f64,
    /// Rolling error rate in `[0.0, 1.0]` computed from the sample window.
    pub error_rate: f32,
    /// Composite health score in `[0.0, 1.0]`; higher is better.
    pub health_score: f32,
    /// Monotonic timestamp of the most recent [`record`] call.
    ///
    /// [`record`]: ProviderHealthMonitor::record
    pub last_check: Instant,
    /// Number of consecutive failures since the last success.
    pub consecutive_failures: u32,
    /// Total successful + failed requests recorded since monitor creation.
    pub total_requests: u64,
    /// Total failed requests recorded since monitor creation.
    pub total_errors: u64,
}

/// Hard-failure threshold: providers with this many consecutive failures in a
/// row are marked `is_reachable = false` regardless of their score.
const CONSECUTIVE_FAILURE_LIMIT: u32 = 5;

impl ProviderHealth {
    /// Returns `true` when `health_score > 0.5`.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn is_healthy(&self) -> bool {
        self.health_score > 0.5
    }

    /// Returns `true` when `0.2 < health_score <= 0.5`.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn is_degraded(&self) -> bool {
        self.health_score > 0.2 && self.health_score <= 0.5
    }

    /// Returns `true` when `health_score <= 0.2`.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn is_critical(&self) -> bool {
        self.health_score <= 0.2
    }
}

// ---------------------------------------------------------------------------
// Internal per-provider mutable state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ProviderState {
    /// Rolling window of round-trip latency samples (milliseconds).
    /// Only successful requests contribute a latency sample; failures
    /// still increment `error_count` and `request_count`.
    latency_window: Vec<u64>,
    /// Parallel window tracking whether each slot was an error (`true`) or
    /// success (`false`).  Kept in sync with `latency_window` length by
    /// storing `0` latency for errors; however we track errors separately
    /// so percentile computation is not skewed by zero-latency error slots.
    error_window: Vec<bool>,
    consecutive_failures: u32,
    total_requests: u64,
    total_errors: u64,
    last_check: Instant,
}

impl ProviderState {
    fn new() -> Self {
        Self {
            latency_window: Vec::new(),
            error_window: Vec::new(),
            consecutive_failures: 0,
            total_requests: 0,
            total_errors: 0,
            last_check: Instant::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// ProviderHealthMonitor
// ---------------------------------------------------------------------------

/// A thread-safe, cloneable monitor that tracks per-provider health.
///
/// All public methods are `async` and acquire an internal `RwLock` for the
/// minimum duration necessary.
///
/// # Clone semantics
///
/// Cloning shares the same underlying `Arc`-wrapped state, so all clones
/// observe the same data.
#[derive(Clone)]
pub struct ProviderHealthMonitor {
    /// Keyed by provider_id.
    state: Arc<RwLock<HashMap<String, ProviderState>>>,
    /// Maximum number of samples retained per provider.
    window_size: usize,
}

impl ProviderHealthMonitor {
    /// Create a new monitor with the given rolling-window size.
    ///
    /// `window_size` is the maximum number of samples (requests) retained per
    /// provider when computing percentiles and error rate.  Older samples are
    /// evicted when the window is full (FIFO).  A minimum of `1` is enforced.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn new(window_size: usize) -> Self {
        Self {
            state: Arc::new(RwLock::new(HashMap::new())),
            window_size: window_size.max(1),
        }
    }

    /// Record the outcome of a single completed request for `provider_id`.
    ///
    /// * `latency_ms` — round-trip time in milliseconds.  Ignored (not added
    ///   to the latency window) when `success` is `false`.
    /// * `success` — `true` for a 2xx response, `false` for any error.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub async fn record(&self, provider_id: &str, latency_ms: u64, success: bool) {
        let mut map = self.state.write().await;
        let entry = map
            .entry(provider_id.to_string())
            .or_insert_with(ProviderState::new);

        entry.total_requests += 1;
        entry.last_check = Instant::now();

        if success {
            entry.consecutive_failures = 0;
            // Add latency sample, evict oldest if window is full.
            if entry.latency_window.len() >= self.window_size {
                entry.latency_window.remove(0);
            }
            entry.latency_window.push(latency_ms);
        } else {
            entry.total_errors += 1;
            entry.consecutive_failures += 1;
        }

        // Error window tracks success/failure for every request.
        if entry.error_window.len() >= self.window_size {
            entry.error_window.remove(0);
        }
        entry.error_window.push(!success);
    }

    /// Return a [`ProviderHealth`] snapshot for `provider_id`, or `None` if no
    /// data has been recorded for that provider yet.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub async fn get_health(&self, provider_id: &str) -> Option<ProviderHealth> {
        let map = self.state.read().await;
        let entry = map.get(provider_id)?;
        Some(Self::build_health(provider_id, entry))
    }

    /// Return all known providers sorted by health score, best first.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub async fn ranked_providers(&self) -> Vec<ProviderHealth> {
        let map = self.state.read().await;
        let mut snapshots: Vec<ProviderHealth> = map
            .iter()
            .map(|(id, state)| Self::build_health(id, state))
            .collect();
        // Sort descending by health_score; ties broken by p95 latency ascending.
        snapshots.sort_by(|a, b| {
            b.health_score
                .partial_cmp(&a.health_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    a.p95_latency_ms
                        .partial_cmp(&b.p95_latency_ms)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        snapshots
    }

    /// Return the healthiest provider from the given `candidates` list.
    ///
    /// Returns `None` if `candidates` is empty or none of the candidates have
    /// recorded data.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub async fn best_provider<'a>(&self, candidates: &'a [String]) -> Option<&'a String> {
        if candidates.is_empty() {
            return None;
        }
        let map = self.state.read().await;
        let mut best_idx: Option<usize> = None;
        let mut best_score = -1.0_f32;
        for (i, id) in candidates.iter().enumerate() {
            let score = map
                .get(id.as_str())
                .map(|s| Self::build_health(id, s).health_score)
                .unwrap_or(0.0);
            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }
        best_idx.map(|i| &candidates[i])
    }

    /// Returns `true` if `provider_id` has a health score above the "healthy"
    /// threshold (`> 0.5`) and is not in a hard-failure state.
    ///
    /// Providers with no recorded data are considered **usable** (optimistic
    /// default) so new providers are tried immediately.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub async fn is_usable(&self, provider_id: &str) -> bool {
        let map = self.state.read().await;
        match map.get(provider_id) {
            None => true, // no data yet — assume healthy
            Some(entry) => {
                let health = Self::build_health(provider_id, entry);
                health.is_reachable && health.is_healthy()
            }
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Build a [`ProviderHealth`] snapshot from a [`ProviderState`] reference.
    fn build_health(provider_id: &str, entry: &ProviderState) -> ProviderHealth {
        let p50 = Self::compute_percentile(&entry.latency_window, 50.0);
        let p95 = Self::compute_percentile(&entry.latency_window, 95.0);
        let p99 = Self::compute_percentile(&entry.latency_window, 99.0);

        let error_rate = if entry.error_window.is_empty() {
            0.0_f32
        } else {
            let errors = entry.error_window.iter().filter(|&&e| e).count();
            errors as f32 / entry.error_window.len() as f32
        };

        let health_score = Self::compute_health_score(error_rate, p95);

        let is_reachable = entry.consecutive_failures < CONSECUTIVE_FAILURE_LIMIT
            && health_score > 0.0;

        ProviderHealth {
            provider_id: provider_id.to_string(),
            is_reachable,
            p50_latency_ms: p50,
            p95_latency_ms: p95,
            p99_latency_ms: p99,
            error_rate,
            health_score,
            last_check: entry.last_check,
            consecutive_failures: entry.consecutive_failures,
            total_requests: entry.total_requests,
            total_errors: entry.total_errors,
        }
    }

    /// Compute the `pct`-th percentile of `samples` (e.g. `95.0` for p95).
    ///
    /// Returns `0.0` when `samples` is empty.
    ///
    /// Uses the nearest-rank method: sort the slice then index at
    /// `ceil(pct/100 * n) - 1` (clamped to `[0, n-1]`).
    ///
    /// # Panics
    ///
    /// Never panics.
    fn compute_percentile(samples: &[u64], pct: f64) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();
        // nearest-rank formula
        let rank = ((pct / 100.0) * n as f64).ceil() as usize;
        let idx = rank.saturating_sub(1).min(n - 1);
        sorted[idx] as f64
    }

    /// Compute the composite health score from an error rate and p95 latency.
    ///
    /// ```text
    /// score = (1.0 - error_rate) * (1.0 / (1.0 + p95_latency_ms / 1000.0))
    /// ```
    ///
    /// Result is clamped to `[0.0, 1.0]`.
    ///
    /// # Panics
    ///
    /// Never panics.
    fn compute_health_score(error_rate: f32, p95_latency_ms: f64) -> f32 {
        let availability = (1.0 - error_rate).clamp(0.0, 1.0);
        let latency_weight = (1.0 / (1.0 + p95_latency_ms / 1000.0)) as f32;
        (availability * latency_weight).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a monitor and record N successes with the given latency.
    async fn monitor_with_successes(latencies: &[u64]) -> ProviderHealthMonitor {
        let m = ProviderHealthMonitor::new(100);
        for &ms in latencies {
            m.record("p1", ms, true).await;
        }
        m
    }

    #[tokio::test]
    async fn test_no_data_returns_none() {
        let m = ProviderHealthMonitor::new(10);
        assert!(m.get_health("unknown").await.is_none());
    }

    #[tokio::test]
    async fn test_is_usable_with_no_data() {
        let m = ProviderHealthMonitor::new(10);
        // Providers with no data are optimistically considered usable.
        assert!(m.is_usable("new_provider").await);
    }

    #[tokio::test]
    async fn test_single_success_is_healthy() {
        let m = monitor_with_successes(&[200]).await;
        let h = m.get_health("p1").await.unwrap();
        assert!(h.is_healthy(), "single success should be healthy");
        assert_eq!(h.error_rate, 0.0);
        assert_eq!(h.consecutive_failures, 0);
        assert_eq!(h.total_requests, 1);
        assert_eq!(h.total_errors, 0);
    }

    #[tokio::test]
    async fn test_percentile_computation() {
        // 10 samples: 100..1000 ms in steps of 100.
        let latencies: Vec<u64> = (1..=10).map(|i| i * 100).collect();
        let m = monitor_with_successes(&latencies).await;
        let h = m.get_health("p1").await.unwrap();
        // p50 = 500 ms (nearest-rank of 10 samples at 50%)
        assert_eq!(h.p50_latency_ms, 500.0);
        // p95 = 1000 ms (ceil(0.95*10)=10, idx=9 → 1000)
        assert_eq!(h.p95_latency_ms, 1000.0);
        // p99 = 1000 ms
        assert_eq!(h.p99_latency_ms, 1000.0);
    }

    #[tokio::test]
    async fn test_error_rate_all_failures() {
        let m = ProviderHealthMonitor::new(10);
        for _ in 0..5 {
            m.record("p1", 0, false).await;
        }
        let h = m.get_health("p1").await.unwrap();
        assert_eq!(h.error_rate, 1.0);
        assert!(h.is_critical(), "all failures → score should be critical");
    }

    #[tokio::test]
    async fn test_mixed_error_rate() {
        let m = ProviderHealthMonitor::new(10);
        // 8 successes, 2 failures → error_rate = 0.2
        for _ in 0..8 {
            m.record("p1", 100, true).await;
        }
        for _ in 0..2 {
            m.record("p1", 0, false).await;
        }
        let h = m.get_health("p1").await.unwrap();
        assert!((h.error_rate - 0.2).abs() < 1e-5, "error_rate should be 0.2");
    }

    #[tokio::test]
    async fn test_consecutive_failures_marks_unreachable() {
        let m = ProviderHealthMonitor::new(20);
        for _ in 0..CONSECUTIVE_FAILURE_LIMIT {
            m.record("p1", 0, false).await;
        }
        let h = m.get_health("p1").await.unwrap();
        assert!(!h.is_reachable, "should be unreachable after too many consecutive failures");
    }

    #[tokio::test]
    async fn test_ranked_providers_order() {
        let m = ProviderHealthMonitor::new(20);
        // good provider: fast, no errors
        for _ in 0..10 {
            m.record("good", 50, true).await;
        }
        // bad provider: high error rate
        for _ in 0..8 {
            m.record("bad", 50, false).await;
        }
        for _ in 0..2 {
            m.record("bad", 50, true).await;
        }
        let ranked = m.ranked_providers().await;
        assert_eq!(ranked[0].provider_id, "good");
        assert_eq!(ranked[1].provider_id, "bad");
    }

    #[tokio::test]
    async fn test_best_provider_from_candidates() {
        let m = ProviderHealthMonitor::new(20);
        for _ in 0..10 {
            m.record("fast", 50, true).await;
        }
        for _ in 0..5 {
            m.record("slow", 0, false).await;
        }
        for _ in 0..5 {
            m.record("slow", 5000, true).await;
        }
        let candidates = vec!["slow".to_string(), "fast".to_string()];
        let best = m.best_provider(&candidates).await;
        assert_eq!(best.map(|s| s.as_str()), Some("fast"));
    }

    #[tokio::test]
    async fn test_window_eviction() {
        // Window size of 3: only the last 3 samples should affect percentiles.
        let m = ProviderHealthMonitor::new(3);
        // Push 10 large latencies, then 3 small ones.
        for _ in 0..10 {
            m.record("p1", 9000, true).await;
        }
        for _ in 0..3 {
            m.record("p1", 10, true).await;
        }
        let h = m.get_health("p1").await.unwrap();
        // All window samples should now be 10 ms.
        assert_eq!(h.p50_latency_ms, 10.0);
        assert_eq!(h.p95_latency_ms, 10.0);
    }

    #[tokio::test]
    async fn test_health_score_formula() {
        // Zero error rate, 0 ms latency → perfect score of 1.0
        let score = ProviderHealthMonitor::compute_health_score(0.0, 0.0);
        assert!((score - 1.0).abs() < 1e-5, "perfect conditions should score 1.0");

        // 100% error rate → score of 0.0
        let score = ProviderHealthMonitor::compute_health_score(1.0, 0.0);
        assert!((score - 0.0).abs() < 1e-5, "all errors should score 0.0");

        // 0% error, 1000 ms p95 → latency_weight = 1/(1+1) = 0.5
        let score = ProviderHealthMonitor::compute_health_score(0.0, 1000.0);
        assert!((score - 0.5).abs() < 1e-4, "1000 ms p95 should give score 0.5");
    }
}
