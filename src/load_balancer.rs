//! # Multi-Model Load Balancer
//!
//! Weighted round-robin selection of model endpoints with health tracking
//! and automatic failover.
//!
//! ## Selection Algorithm
//!
//! Uses the smooth weighted round-robin algorithm (Nginx-style):
//! each endpoint's `current_weight` is incremented by its `weight` on every
//! call to [`LoadBalancer::select`]; the endpoint with the highest
//! `current_weight` is chosen and then has `total_weight` subtracted from
//! its `current_weight`.
//!
//! ## Health Tracking
//!
//! - After **3 consecutive failures** an endpoint is marked unhealthy.
//! - **1 success** recovers an endpoint.
//! - When `failover = true` unhealthy endpoints are skipped during selection.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A single model inference endpoint.
#[derive(Debug, Clone)]
pub struct ModelEndpoint {
    /// Unique identifier for this endpoint (e.g. `"gpt-4o-primary"`).
    pub id: String,
    /// Base URL of the endpoint (e.g. `"https://api.openai.com/v1"`).
    pub url: String,
    /// Relative weight used for weighted round-robin selection.
    /// Higher weight = more traffic. Must be > 0.
    pub weight: u32,
    /// Maximum requests per second this endpoint can sustain.
    pub max_rps: f64,
    /// Whether this endpoint is currently considered healthy.
    pub healthy: bool,
    /// 99th-percentile observed latency in milliseconds.
    pub latency_p99_ms: f64,
}

/// Configuration for the [`LoadBalancer`].
#[derive(Debug, Clone)]
pub struct BalancerConfig {
    /// The set of model endpoints to balance across.
    pub endpoints: Vec<ModelEndpoint>,
    /// How often to poll endpoints for health status.
    pub health_check_interval: Duration,
    /// When `true`, unhealthy endpoints are skipped during selection.
    /// When `false`, all endpoints participate regardless of health.
    pub failover: bool,
}

impl Default for BalancerConfig {
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
            health_check_interval: Duration::from_secs(30),
            failover: true,
        }
    }
}

/// Per-endpoint statistics tracked by the [`LoadBalancer`].
#[derive(Debug, Clone, Default)]
pub struct EndpointStats {
    /// Total number of requests dispatched to this endpoint.
    pub requests: u64,
    /// Total number of failures recorded for this endpoint.
    pub failures: u64,
    /// Current consecutive failure run (reset on success).
    pub consecutive_failures: u32,
    /// Exponential-moving-average latency in milliseconds.
    pub avg_latency_ms: f64,
    /// Whether the endpoint is currently healthy.
    pub is_healthy: bool,
}

/// Aggregate statistics for the entire balancer.
#[derive(Debug, Clone, Default)]
pub struct LoadBalancerStats {
    /// Total requests dispatched across all endpoints.
    pub total_requests: u64,
    /// Per-endpoint breakdown.
    pub by_endpoint: HashMap<String, EndpointStats>,
    /// Number of endpoints currently marked unhealthy.
    pub unhealthy_count: usize,
}

// ── Internal state ────────────────────────────────────────────────────────────

/// Number of consecutive failures before an endpoint is marked unhealthy.
const FAILURE_THRESHOLD: u32 = 3;

/// Exponential moving average α for latency tracking.
const LATENCY_EMA_ALPHA: f64 = 0.2;

struct EndpointState {
    endpoint: ModelEndpoint,
    current_weight: i64,
    stats: EndpointStats,
}

struct BalancerInner {
    endpoints: Vec<EndpointState>,
    config: BalancerConfig,
    total_requests: u64,
}

impl BalancerInner {
    fn new(config: BalancerConfig) -> Self {
        let endpoints = config
            .endpoints
            .iter()
            .map(|ep| EndpointState {
                endpoint: ep.clone(),
                current_weight: 0,
                stats: EndpointStats {
                    is_healthy: ep.healthy,
                    ..Default::default()
                },
            })
            .collect();
        Self {
            endpoints,
            config,
            total_requests: 0,
        }
    }

    /// Weighted round-robin selection. Returns the index of the selected endpoint.
    fn select_index(&mut self) -> Option<usize> {
        if self.endpoints.is_empty() {
            return None;
        }

        let total_weight: i64 = self
            .endpoints
            .iter()
            .filter(|s| !self.config.failover || s.endpoint.healthy)
            .map(|s| s.endpoint.weight as i64)
            .sum();

        if total_weight == 0 {
            return None;
        }

        // Increment current_weight by own weight for all eligible endpoints.
        for state in self.endpoints.iter_mut() {
            if !self.config.failover || state.endpoint.healthy {
                state.current_weight += state.endpoint.weight as i64;
            }
        }

        // Pick the endpoint with the highest current_weight.
        let idx = self
            .endpoints
            .iter()
            .enumerate()
            .filter(|(_, s)| !self.config.failover || s.endpoint.healthy)
            .max_by_key(|(_, s)| s.current_weight)
            .map(|(i, _)| i)?;

        // Subtract total_weight from the winner.
        self.endpoints[idx].current_weight -= total_weight;
        self.endpoints[idx].stats.requests += 1;
        self.total_requests += 1;

        Some(idx)
    }

    fn mark_success(&mut self, id: &str, latency_ms: f64) {
        if let Some(state) = self.endpoints.iter_mut().find(|s| s.endpoint.id == id) {
            state.stats.consecutive_failures = 0;
            state.endpoint.healthy = true;
            state.stats.is_healthy = true;
            // Update EMA latency.
            if state.stats.avg_latency_ms == 0.0 {
                state.stats.avg_latency_ms = latency_ms;
            } else {
                state.stats.avg_latency_ms = LATENCY_EMA_ALPHA * latency_ms
                    + (1.0 - LATENCY_EMA_ALPHA) * state.stats.avg_latency_ms;
            }
            state.endpoint.latency_p99_ms = state.stats.avg_latency_ms;
        }
    }

    fn mark_failure(&mut self, id: &str) {
        if let Some(state) = self.endpoints.iter_mut().find(|s| s.endpoint.id == id) {
            state.stats.failures += 1;
            state.stats.consecutive_failures += 1;
            if state.stats.consecutive_failures >= FAILURE_THRESHOLD {
                state.endpoint.healthy = false;
                state.stats.is_healthy = false;
            }
        }
    }

    fn stats(&self) -> LoadBalancerStats {
        let by_endpoint: HashMap<String, EndpointStats> = self
            .endpoints
            .iter()
            .map(|s| (s.endpoint.id.clone(), s.stats.clone()))
            .collect();

        let unhealthy_count = self
            .endpoints
            .iter()
            .filter(|s| !s.endpoint.healthy)
            .count();

        LoadBalancerStats {
            total_requests: self.total_requests,
            by_endpoint,
            unhealthy_count,
        }
    }

    fn endpoint_at(&self, idx: usize) -> Option<ModelEndpoint> {
        self.endpoints.get(idx).map(|s| s.endpoint.clone())
    }
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Thread-safe weighted round-robin load balancer for model endpoints.
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::load_balancer::{
///     BalancerConfig, LoadBalancer, ModelEndpoint,
/// };
/// use std::time::Duration;
///
/// let config = BalancerConfig {
///     endpoints: vec![
///         ModelEndpoint {
///             id: "ep-a".to_string(),
///             url: "http://a".to_string(),
///             weight: 2,
///             max_rps: 100.0,
///             healthy: true,
///             latency_p99_ms: 0.0,
///         },
///         ModelEndpoint {
///             id: "ep-b".to_string(),
///             url: "http://b".to_string(),
///             weight: 1,
///             max_rps: 50.0,
///             healthy: true,
///             latency_p99_ms: 0.0,
///         },
///     ],
///     health_check_interval: Duration::from_secs(30),
///     failover: true,
/// };
///
/// let lb = LoadBalancer::new(config);
/// let ep = lb.select();
/// assert!(ep.is_some());
/// ```
#[derive(Clone)]
pub struct LoadBalancer {
    inner: Arc<Mutex<BalancerInner>>,
}

impl LoadBalancer {
    /// Create a new load balancer from the given configuration.
    pub fn new(config: BalancerConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BalancerInner::new(config))),
        }
    }

    /// Select the next endpoint using weighted round-robin.
    ///
    /// Returns `None` when:
    /// - No endpoints are configured.
    /// - All endpoints are unhealthy and `failover = true`.
    pub fn select(&self) -> Option<ModelEndpoint> {
        let mut guard = self.inner.lock().ok()?;
        let idx = guard.select_index()?;
        guard.endpoint_at(idx)
    }

    /// Record a successful response from endpoint `id` with the given latency.
    ///
    /// Resets the consecutive-failure counter and marks the endpoint healthy.
    pub fn mark_success(&self, id: &str, latency_ms: f64) {
        if let Ok(mut g) = self.inner.lock() {
            g.mark_success(id, latency_ms);
        }
    }

    /// Record a failure from endpoint `id`.
    ///
    /// After 3 consecutive failures the endpoint is marked unhealthy.
    pub fn mark_failure(&self, id: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.mark_failure(id);
        }
    }

    /// Return a snapshot of current balancer statistics.
    pub fn stats(&self) -> LoadBalancerStats {
        self.inner
            .lock()
            .map(|g| g.stats())
            .unwrap_or_default()
    }

    /// Return the configured failover setting.
    pub fn failover_enabled(&self) -> bool {
        self.inner
            .lock()
            .map(|g| g.config.failover)
            .unwrap_or(true)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ep(id: &str, weight: u32) -> ModelEndpoint {
        ModelEndpoint {
            id: id.to_string(),
            url: format!("http://{id}"),
            weight,
            max_rps: 100.0,
            healthy: true,
            latency_p99_ms: 0.0,
        }
    }

    fn lb(endpoints: Vec<ModelEndpoint>, failover: bool) -> LoadBalancer {
        LoadBalancer::new(BalancerConfig {
            endpoints,
            health_check_interval: Duration::from_secs(30),
            failover,
        })
    }

    #[test]
    fn test_empty_returns_none() {
        let b = lb(vec![], true);
        assert!(b.select().is_none());
    }

    #[test]
    fn test_single_endpoint_always_selected() {
        let b = lb(vec![ep("a", 1)], true);
        for _ in 0..10 {
            assert_eq!(b.select().unwrap().id, "a");
        }
    }

    #[test]
    fn test_equal_weights_round_robin() {
        let b = lb(vec![ep("a", 1), ep("b", 1)], true);
        let ids: Vec<String> = (0..4).map(|_| b.select().unwrap().id).collect();
        // Each endpoint should appear exactly twice in 4 selections.
        let a_count = ids.iter().filter(|s| s.as_str() == "a").count();
        let b_count = ids.iter().filter(|s| s.as_str() == "b").count();
        assert_eq!(a_count, 2);
        assert_eq!(b_count, 2);
    }

    #[test]
    fn test_weighted_distribution() {
        let b = lb(vec![ep("heavy", 2), ep("light", 1)], true);
        let selections: Vec<String> = (0..9).map(|_| b.select().unwrap().id).collect();
        let heavy = selections.iter().filter(|s| s.as_str() == "heavy").count();
        let light = selections.iter().filter(|s| s.as_str() == "light").count();
        assert_eq!(heavy, 6);
        assert_eq!(light, 3);
    }

    #[test]
    fn test_mark_failure_three_times_marks_unhealthy() {
        let b = lb(vec![ep("a", 1)], true);
        b.mark_failure("a");
        b.mark_failure("a");
        assert!(b.select().is_some()); // still healthy after 2
        b.mark_failure("a");
        assert!(b.select().is_none()); // unhealthy after 3
    }

    #[test]
    fn test_mark_success_recovers_unhealthy() {
        let b = lb(vec![ep("a", 1)], true);
        b.mark_failure("a");
        b.mark_failure("a");
        b.mark_failure("a");
        assert!(b.select().is_none());
        b.mark_success("a", 10.0);
        assert!(b.select().is_some());
    }

    #[test]
    fn test_failover_skips_unhealthy() {
        let b = lb(vec![ep("a", 1), ep("b", 1)], true);
        b.mark_failure("a");
        b.mark_failure("a");
        b.mark_failure("a");
        // Only "b" should be selected now.
        for _ in 0..5 {
            assert_eq!(b.select().unwrap().id, "b");
        }
    }

    #[test]
    fn test_no_failover_selects_unhealthy() {
        let b = lb(vec![ep("a", 1), ep("b", 1)], false);
        b.mark_failure("a");
        b.mark_failure("a");
        b.mark_failure("a");
        // Both should still participate.
        let ids: Vec<String> = (0..4).map(|_| b.select().unwrap().id).collect();
        assert!(ids.iter().any(|s| s == "a"));
        assert!(ids.iter().any(|s| s == "b"));
    }

    #[test]
    fn test_total_request_counter() {
        let b = lb(vec![ep("a", 1)], true);
        b.select();
        b.select();
        b.select();
        assert_eq!(b.stats().total_requests, 3);
    }

    #[test]
    fn test_per_endpoint_request_counter() {
        let b = lb(vec![ep("a", 1)], true);
        b.select();
        b.select();
        let stats = b.stats();
        assert_eq!(stats.by_endpoint["a"].requests, 2);
    }

    #[test]
    fn test_failure_counter_increments() {
        let b = lb(vec![ep("a", 1)], true);
        b.mark_failure("a");
        b.mark_failure("a");
        let stats = b.stats();
        assert_eq!(stats.by_endpoint["a"].failures, 2);
    }

    #[test]
    fn test_consecutive_failures_reset_on_success() {
        let b = lb(vec![ep("a", 1)], true);
        b.mark_failure("a");
        b.mark_failure("a");
        b.mark_success("a", 5.0);
        let stats = b.stats();
        assert_eq!(stats.by_endpoint["a"].consecutive_failures, 0);
    }

    #[test]
    fn test_unhealthy_count_in_stats() {
        let b = lb(vec![ep("a", 1), ep("b", 1)], true);
        b.mark_failure("a");
        b.mark_failure("a");
        b.mark_failure("a");
        let stats = b.stats();
        assert_eq!(stats.unhealthy_count, 1);
    }

    #[test]
    fn test_latency_tracking() {
        let b = lb(vec![ep("a", 1)], true);
        b.mark_success("a", 100.0);
        let stats = b.stats();
        assert!(stats.by_endpoint["a"].avg_latency_ms > 0.0);
    }

    #[test]
    fn test_latency_ema_updates() {
        let b = lb(vec![ep("a", 1)], true);
        b.mark_success("a", 100.0);
        b.mark_success("a", 200.0);
        let stats = b.stats();
        // EMA should be between 100 and 200.
        let avg = stats.by_endpoint["a"].avg_latency_ms;
        assert!(avg > 100.0 && avg < 200.0);
    }

    #[test]
    fn test_all_unhealthy_returns_none_with_failover() {
        let b = lb(vec![ep("a", 1), ep("b", 1)], true);
        for _ in 0..3 {
            b.mark_failure("a");
            b.mark_failure("b");
        }
        assert!(b.select().is_none());
    }

    #[test]
    fn test_recovery_after_all_unhealthy() {
        let b = lb(vec![ep("a", 1)], true);
        for _ in 0..3 {
            b.mark_failure("a");
        }
        assert!(b.select().is_none());
        b.mark_success("a", 10.0);
        assert!(b.select().is_some());
    }

    #[test]
    fn test_stats_healthy_status_reflects_mark_failure() {
        let b = lb(vec![ep("a", 1)], true);
        assert!(b.stats().by_endpoint["a"].is_healthy);
        for _ in 0..3 {
            b.mark_failure("a");
        }
        assert!(!b.stats().by_endpoint["a"].is_healthy);
    }

    #[test]
    fn test_failover_enabled_flag() {
        let b = lb(vec![ep("a", 1)], true);
        assert!(b.failover_enabled());
        let b2 = lb(vec![ep("a", 1)], false);
        assert!(!b2.failover_enabled());
    }

    #[test]
    fn test_three_endpoints_weighted() {
        let b = lb(
            vec![ep("a", 3), ep("b", 2), ep("c", 1)],
            true,
        );
        let selections: Vec<String> = (0..6).map(|_| b.select().unwrap().id).collect();
        let a = selections.iter().filter(|s| s.as_str() == "a").count();
        let b_cnt = selections.iter().filter(|s| s.as_str() == "b").count();
        let c = selections.iter().filter(|s| s.as_str() == "c").count();
        assert_eq!(a, 3);
        assert_eq!(b_cnt, 2);
        assert_eq!(c, 1);
    }

    #[test]
    fn test_clone_shares_state() {
        let b = lb(vec![ep("a", 1)], true);
        let b2 = b.clone();
        b.mark_failure("a");
        b.mark_failure("a");
        b.mark_failure("a");
        // b2 should see the same state.
        assert!(b2.select().is_none());
    }

    #[test]
    fn test_unknown_endpoint_mark_does_not_panic() {
        let b = lb(vec![ep("a", 1)], true);
        // Should not panic.
        b.mark_failure("does-not-exist");
        b.mark_success("does-not-exist", 0.0);
    }

    #[test]
    fn test_zero_weight_endpoint_ignored() {
        // An endpoint with weight 0 contributes 0 to total_weight and is never selected.
        let b = lb(vec![ep("zero", 0), ep("ok", 1)], true);
        // Should never return "zero".
        for _ in 0..10 {
            let id = b.select().unwrap().id;
            assert_eq!(id, "ok");
        }
    }
}
