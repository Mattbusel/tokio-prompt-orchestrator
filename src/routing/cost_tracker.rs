//! Cost tracking and savings computation.
//!
//! Tracks per-model token usage and estimated spend, computes what the
//! all-cloud baseline cost would have been, and reports savings.
//!
//! Thread-safe: all counters use atomic operations for lock-free reads
//! and writes under concurrent pipeline access.

use std::sync::atomic::{AtomicU64, Ordering};

/// Per-model cost tracking and savings computation.
///
/// All operations are lock-free via atomics.
/// Costs are stored as micro-dollars (1 USD = 1 000 000 micro-dollars) to
/// avoid floating-point drift in long-running aggregations.
///
/// # Panics
///
/// This type and its methods never panic.
#[derive(Debug)]
pub struct CostTracker {
    /// Cost-per-1K-tokens for the local worker, in micro-dollars.
    local_rate_micro: u64,
    /// Cost-per-1K-tokens for the cloud worker, in micro-dollars.
    cloud_rate_micro: u64,

    /// Total tokens routed to the local worker.
    local_tokens: AtomicU64,
    /// Total tokens routed to the cloud worker.
    cloud_tokens: AtomicU64,

    /// Number of requests routed locally.
    local_requests: AtomicU64,
    /// Number of requests routed to cloud.
    cloud_requests: AtomicU64,
    /// Number of fallback requests (local failed, sent to cloud).
    fallback_requests: AtomicU64,
}

impl CostTracker {
    /// Create a new cost tracker.
    ///
    /// # Arguments
    ///
    /// * `local_cost_per_1k` — Cost per 1 000 tokens for the local model (USD).
    /// * `cloud_cost_per_1k` — Cost per 1 000 tokens for the cloud model (USD).
    ///
    /// # Returns
    ///
    /// A new [`CostTracker`] with all counters at zero.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(local_cost_per_1k: f64, cloud_cost_per_1k: f64) -> Self {
        Self {
            local_rate_micro: f64_to_micro(local_cost_per_1k),
            cloud_rate_micro: f64_to_micro(cloud_cost_per_1k),
            local_tokens: AtomicU64::new(0),
            cloud_tokens: AtomicU64::new(0),
            local_requests: AtomicU64::new(0),
            cloud_requests: AtomicU64::new(0),
            fallback_requests: AtomicU64::new(0),
        }
    }

    /// Record tokens processed by the local worker.
    ///
    /// # Arguments
    ///
    /// * `tokens` — Number of tokens processed.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn record_local(&self, tokens: u64) {
        self.local_tokens.fetch_add(tokens, Ordering::Relaxed);
        self.local_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Record tokens processed by the cloud worker.
    ///
    /// # Arguments
    ///
    /// * `tokens` — Number of tokens processed.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn record_cloud(&self, tokens: u64) {
        self.cloud_tokens.fetch_add(tokens, Ordering::Relaxed);
        self.cloud_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a fallback event (local failed, retried on cloud).
    ///
    /// The token count is attributed to the cloud bucket since the cloud
    /// worker ultimately served the request.
    ///
    /// # Arguments
    ///
    /// * `tokens` — Number of tokens processed by the cloud fallback.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn record_fallback(&self, tokens: u64) {
        self.cloud_tokens.fetch_add(tokens, Ordering::Relaxed);
        self.fallback_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Return a snapshot of current cost metrics.
    ///
    /// # Returns
    ///
    /// A [`CostSnapshot`] with all current counters and computed costs.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn snapshot(&self) -> CostSnapshot {
        let local_tokens = self.local_tokens.load(Ordering::Relaxed);
        let cloud_tokens = self.cloud_tokens.load(Ordering::Relaxed);
        let local_requests = self.local_requests.load(Ordering::Relaxed);
        let cloud_requests = self.cloud_requests.load(Ordering::Relaxed);
        let fallback_requests = self.fallback_requests.load(Ordering::Relaxed);

        let local_cost_micro = (local_tokens as u128 * self.local_rate_micro as u128) / 1000;
        let cloud_cost_micro = (cloud_tokens as u128 * self.cloud_rate_micro as u128) / 1000;
        let actual_cost_micro = local_cost_micro + cloud_cost_micro;

        // Baseline: what it would cost if ALL tokens went to cloud
        let total_tokens = local_tokens + cloud_tokens;
        let baseline_cost_micro = (total_tokens as u128 * self.cloud_rate_micro as u128) / 1000;

        let savings_micro = baseline_cost_micro.saturating_sub(actual_cost_micro);

        CostSnapshot {
            local_tokens,
            cloud_tokens,
            local_requests,
            cloud_requests,
            fallback_requests,
            actual_cost_usd: micro_to_f64(actual_cost_micro as u64),
            baseline_cost_usd: micro_to_f64(baseline_cost_micro as u64),
            savings_usd: micro_to_f64(savings_micro as u64),
            savings_percent: if baseline_cost_micro > 0 {
                (savings_micro as f64 / baseline_cost_micro as f64) * 100.0
            } else {
                0.0
            },
        }
    }

    /// Reset all counters to zero.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn reset(&self) {
        self.local_tokens.store(0, Ordering::Relaxed);
        self.cloud_tokens.store(0, Ordering::Relaxed);
        self.local_requests.store(0, Ordering::Relaxed);
        self.cloud_requests.store(0, Ordering::Relaxed);
        self.fallback_requests.store(0, Ordering::Relaxed);
    }
}

/// Point-in-time snapshot of cost tracking metrics.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, PartialEq)]
pub struct CostSnapshot {
    /// Total tokens processed by the local worker.
    pub local_tokens: u64,
    /// Total tokens processed by the cloud worker.
    pub cloud_tokens: u64,
    /// Number of requests served by the local worker.
    pub local_requests: u64,
    /// Number of requests served by the cloud worker.
    pub cloud_requests: u64,
    /// Number of fallback requests (local → cloud).
    pub fallback_requests: u64,
    /// Actual total cost in USD.
    pub actual_cost_usd: f64,
    /// Hypothetical all-cloud baseline cost in USD.
    pub baseline_cost_usd: f64,
    /// Money saved vs the all-cloud baseline in USD.
    pub savings_usd: f64,
    /// Savings as a percentage of baseline.
    pub savings_percent: f64,
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Convert a USD-per-1K-tokens rate to micro-dollars-per-1K-tokens.
fn f64_to_micro(usd: f64) -> u64 {
    (usd * 1_000_000.0) as u64
}

/// Convert micro-dollars to USD.
fn micro_to_f64(micro: u64) -> f64 {
    micro as f64 / 1_000_000.0
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- helpers ---------------------------------------------------------

    #[test]
    fn test_f64_to_micro_zero() {
        assert_eq!(f64_to_micro(0.0), 0);
    }

    #[test]
    fn test_f64_to_micro_one_dollar() {
        assert_eq!(f64_to_micro(1.0), 1_000_000);
    }

    #[test]
    fn test_f64_to_micro_fractional() {
        assert_eq!(f64_to_micro(0.015), 15_000);
    }

    #[test]
    fn test_micro_to_f64_zero() {
        assert!(micro_to_f64(0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_micro_to_f64_one_dollar() {
        assert!((micro_to_f64(1_000_000) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_micro_to_f64_round_trip() {
        let original = 0.015;
        let micro = f64_to_micro(original);
        let back = micro_to_f64(micro);
        assert!((back - original).abs() < 1e-6);
    }

    // -- construction ----------------------------------------------------

    #[test]
    fn test_new_tracker_all_counters_zero() {
        let t = CostTracker::new(0.0, 0.015);
        let s = t.snapshot();
        assert_eq!(s.local_tokens, 0);
        assert_eq!(s.cloud_tokens, 0);
        assert_eq!(s.local_requests, 0);
        assert_eq!(s.cloud_requests, 0);
        assert_eq!(s.fallback_requests, 0);
        assert!(s.actual_cost_usd.abs() < f64::EPSILON);
    }

    // -- recording -------------------------------------------------------

    #[test]
    fn test_record_local_increments_tokens_and_requests() {
        let t = CostTracker::new(0.0, 0.015);
        t.record_local(100);
        t.record_local(200);
        let s = t.snapshot();
        assert_eq!(s.local_tokens, 300);
        assert_eq!(s.local_requests, 2);
    }

    #[test]
    fn test_record_cloud_increments_tokens_and_requests() {
        let t = CostTracker::new(0.0, 0.015);
        t.record_cloud(500);
        let s = t.snapshot();
        assert_eq!(s.cloud_tokens, 500);
        assert_eq!(s.cloud_requests, 1);
    }

    #[test]
    fn test_record_fallback_increments_cloud_tokens_and_fallback_requests() {
        let t = CostTracker::new(0.0, 0.015);
        t.record_fallback(300);
        let s = t.snapshot();
        assert_eq!(s.cloud_tokens, 300);
        assert_eq!(s.fallback_requests, 1);
        // Fallback does NOT increment cloud_requests
        assert_eq!(s.cloud_requests, 0);
    }

    // -- cost computation ------------------------------------------------

    #[test]
    fn test_savings_when_all_local() {
        // Local is free, cloud is $0.015/1K tokens
        let t = CostTracker::new(0.0, 0.015);
        t.record_local(10_000);
        let s = t.snapshot();

        // Baseline: 10K tokens * $0.015/1K = $0.15
        assert!((s.baseline_cost_usd - 0.15).abs() < 0.001);
        // Actual: $0 (local is free)
        assert!(s.actual_cost_usd.abs() < 0.001);
        // Savings: $0.15
        assert!((s.savings_usd - 0.15).abs() < 0.001);
        // Savings percent: 100%
        assert!((s.savings_percent - 100.0).abs() < 0.1);
    }

    #[test]
    fn test_savings_when_all_cloud() {
        let t = CostTracker::new(0.0, 0.015);
        t.record_cloud(10_000);
        let s = t.snapshot();

        // Baseline == Actual, savings = 0
        assert!((s.baseline_cost_usd - s.actual_cost_usd).abs() < 0.001);
        assert!(s.savings_usd.abs() < 0.001);
        assert!(s.savings_percent.abs() < 0.1);
    }

    #[test]
    fn test_savings_mixed_routing() {
        let t = CostTracker::new(0.0, 0.015);
        // 7000 local, 3000 cloud
        t.record_local(7_000);
        t.record_cloud(3_000);
        let s = t.snapshot();

        // Baseline: 10K * 0.015/1K = $0.15
        assert!((s.baseline_cost_usd - 0.15).abs() < 0.001);
        // Actual: 0 + 3K * 0.015/1K = $0.045
        assert!((s.actual_cost_usd - 0.045).abs() < 0.001);
        // Savings: $0.105
        assert!((s.savings_usd - 0.105).abs() < 0.001);
        // Savings percent: 70%
        assert!((s.savings_percent - 70.0).abs() < 0.1);
    }

    #[test]
    fn test_savings_with_local_cost() {
        // Local costs something too
        let t = CostTracker::new(0.005, 0.015);
        t.record_local(10_000);
        let s = t.snapshot();

        // Baseline: 10K * 0.015/1K = $0.15
        assert!((s.baseline_cost_usd - 0.15).abs() < 0.001);
        // Actual: 10K * 0.005/1K = $0.05
        assert!((s.actual_cost_usd - 0.05).abs() < 0.001);
        // Savings: $0.10
        assert!((s.savings_usd - 0.10).abs() < 0.001);
    }

    #[test]
    fn test_savings_percent_zero_when_no_tokens() {
        let t = CostTracker::new(0.0, 0.015);
        let s = t.snapshot();
        assert!(s.savings_percent.abs() < f64::EPSILON);
    }

    // -- reset -----------------------------------------------------------

    #[test]
    fn test_reset_clears_all_counters() {
        let t = CostTracker::new(0.0, 0.015);
        t.record_local(1000);
        t.record_cloud(500);
        t.record_fallback(200);
        t.reset();
        let s = t.snapshot();
        assert_eq!(s.local_tokens, 0);
        assert_eq!(s.cloud_tokens, 0);
        assert_eq!(s.local_requests, 0);
        assert_eq!(s.cloud_requests, 0);
        assert_eq!(s.fallback_requests, 0);
    }

    // -- thread safety ---------------------------------------------------

    #[test]
    fn test_concurrent_recording_no_data_loss() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(CostTracker::new(0.0, 0.015));
        let n_threads = 10;
        let n_ops = 1_000;

        let mut handles = Vec::new();
        for _ in 0..n_threads {
            let t = Arc::clone(&tracker);
            handles.push(thread::spawn(move || {
                for _ in 0..n_ops {
                    t.record_local(1);
                    t.record_cloud(1);
                }
            }));
        }

        for h in handles {
            h.join().map_err(|_| "thread panicked").unwrap_or_else(|_| {
                std::process::abort();
            });
        }

        let s = tracker.snapshot();
        let expected = (n_threads * n_ops) as u64;
        assert_eq!(s.local_tokens, expected);
        assert_eq!(s.cloud_tokens, expected);
        assert_eq!(s.local_requests, expected);
        assert_eq!(s.cloud_requests, expected);
    }

    // -- snapshot clone independence ------------------------------------

    #[test]
    fn test_snapshot_is_independent_of_tracker() {
        let t = CostTracker::new(0.0, 0.015);
        t.record_local(100);
        let s1 = t.snapshot();
        t.record_local(200);
        let s2 = t.snapshot();
        assert_eq!(s1.local_tokens, 100);
        assert_eq!(s2.local_tokens, 300);
    }

    // -- fallback counting -----------------------------------------------

    #[test]
    fn test_fallback_adds_to_cloud_tokens_not_local() {
        let t = CostTracker::new(0.0, 0.015);
        t.record_fallback(500);
        let s = t.snapshot();
        assert_eq!(s.local_tokens, 0);
        assert_eq!(s.cloud_tokens, 500);
    }
}
