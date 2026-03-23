//! # Provider Arbitrage Engine
//!
//! Routes inference requests to the **cheapest provider that historically meets
//! a caller-specified latency SLA**.
//!
//! ## Problem
//!
//! Multiple providers (Anthropic, OpenAI, vLLM, llama.cpp) have different
//! per-token prices and different latency characteristics.  A naïve router
//! always uses the cheapest provider, but that provider may be slower or less
//! reliable.  A naïve latency router always uses the fastest provider, but
//! wastes money.
//!
//! The arbitrage engine tracks a rolling P95 latency window for each registered
//! provider and, given a latency budget, selects whichever eligible provider
//! (i.e., whose P95 is below the budget) has the lowest per-token cost.
//!
//! ## Guarantees
//!
//! - Thread-safe: all hot-path state uses atomics; the latency ring buffer uses
//!   a `Mutex` but the lock is held for microseconds.
//! - Non-blocking: `select_provider` never performs I/O.
//! - Graceful degradation: if no provider meets the SLA, the one with the
//!   lowest P95 latency is returned (best-effort).
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::routing::arbitrage::{ArbitrageEngine, ProviderProfile};
//! use std::time::Duration;
//!
//! let engine = ArbitrageEngine::new();
//!
//! engine.register(ProviderProfile {
//!     name: "anthropic".to_string(),
//!     cost_per_1k_input_tokens: 0.003,
//!     cost_per_1k_output_tokens: 0.015,
//!     priority: 0,
//! });
//! engine.register(ProviderProfile {
//!     name: "openai".to_string(),
//!     cost_per_1k_input_tokens: 0.005,
//!     cost_per_1k_output_tokens: 0.015,
//!     priority: 0,
//! });
//!
//! // Record observed latencies
//! engine.record_latency("anthropic", Duration::from_millis(320));
//! engine.record_latency("openai", Duration::from_millis(180));
//!
//! // With a 500ms SLA budget, pick the cheapest provider that historically
//! // completes within 500ms.
//! let sla = Duration::from_millis(500);
//! let winner = engine.select_provider(Some(sla));
//! // anthropic is cheaper AND within the 500ms SLA → selected
//! assert_eq!(winner.map(|p| p.name.as_str()), Some("anthropic"));
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

// ── Types ───────────────────────────────────────────────────────────────────

/// Static profile for a single inference provider.
#[derive(Debug, Clone)]
pub struct ProviderProfile {
    /// Unique name identifying this provider (e.g., `"anthropic"`, `"openai"`).
    pub name: String,
    /// Cost in USD per 1 000 input tokens.
    pub cost_per_1k_input_tokens: f64,
    /// Cost in USD per 1 000 output tokens.
    pub cost_per_1k_output_tokens: f64,
    /// Lower priority value = preferred when cost is equal.
    /// Use to break ties (e.g., prefer local vLLM over cloud at the same cost).
    pub priority: u32,
}

impl ProviderProfile {
    /// Estimated total cost in USD for a request with the given token counts.
    pub fn estimate_cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64 / 1000.0) * self.cost_per_1k_input_tokens
            + (output_tokens as f64 / 1000.0) * self.cost_per_1k_output_tokens
    }
}

/// Runtime state tracked per provider.
#[derive(Debug)]
struct ProviderState {
    profile: ProviderProfile,
    /// Rolling ring buffer of observed latencies in microseconds.
    latency_ring: Mutex<LatencyRing>,
    /// Total requests routed to this provider.
    requests: AtomicU64,
    /// Total errors from this provider.
    errors: AtomicU64,
    /// Total input tokens sent to this provider.
    input_tokens: AtomicU64,
    /// Total output tokens received from this provider.
    output_tokens: AtomicU64,
}

/// Fixed-capacity ring buffer for latency samples (microseconds).
const RING_CAPACITY: usize = 128;

#[derive(Debug)]
struct LatencyRing {
    samples: [u64; RING_CAPACITY],
    head: usize,
    len: usize,
}

impl LatencyRing {
    fn new() -> Self {
        Self {
            samples: [0u64; RING_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, micros: u64) {
        self.samples[self.head] = micros;
        self.head = (self.head + 1) % RING_CAPACITY;
        if self.len < RING_CAPACITY {
            self.len += 1;
        }
    }

    /// Returns the P95 latency in microseconds.  Returns `u64::MAX` if no
    /// samples have been recorded yet.
    fn p95_micros(&self) -> u64 {
        if self.len == 0 {
            return u64::MAX;
        }
        let mut sorted: Vec<u64> = self.samples[..self.len].to_vec();
        sorted.sort_unstable();
        let idx = ((self.len as f64 * 0.95) as usize).min(self.len - 1);
        sorted[idx]
    }

    /// Returns the P50 (median) latency in microseconds.
    fn p50_micros(&self) -> u64 {
        if self.len == 0 {
            return u64::MAX;
        }
        let mut sorted: Vec<u64> = self.samples[..self.len].to_vec();
        sorted.sort_unstable();
        sorted[self.len / 2]
    }
}

// ── Engine ──────────────────────────────────────────────────────────────────

/// The provider arbitrage engine.
///
/// Thread-safe; wrap in `Arc` to share across pipeline stages.
///
/// # Panics
///
/// No method panics.
#[derive(Debug)]
pub struct ArbitrageEngine {
    providers: Mutex<HashMap<String, ProviderState>>,
    /// Total `select_provider` calls.
    selections: AtomicU64,
    /// Calls where all providers exceeded the SLA (fell back to fastest).
    sla_misses: AtomicU64,
}

impl Default for ArbitrageEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ArbitrageEngine {
    /// Create a new, empty arbitrage engine.
    pub fn new() -> Self {
        Self {
            providers: Mutex::new(HashMap::new()),
            selections: AtomicU64::new(0),
            sla_misses: AtomicU64::new(0),
        }
    }

    /// Register a provider.  If a provider with the same name already exists,
    /// its profile is updated (existing runtime state is preserved).
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn register(&self, profile: ProviderProfile) {
        let mut map = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(profile.name.clone())
            .and_modify(|s| s.profile = profile.clone())
            .or_insert_with(|| ProviderState {
                profile,
                latency_ring: Mutex::new(LatencyRing::new()),
                requests: AtomicU64::new(0),
                errors: AtomicU64::new(0),
                input_tokens: AtomicU64::new(0),
                output_tokens: AtomicU64::new(0),
            });
    }

    /// Record an observed end-to-end latency for a provider.
    ///
    /// Call this after every inference call completes (success or failure).
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn record_latency(&self, provider: &str, latency: Duration) {
        let map = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = map.get(provider) {
            let micros = latency.as_micros().min(u64::MAX as u128) as u64;
            let mut ring = state
                .latency_ring
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            ring.push(micros);
        }
    }

    /// Record a successful inference call for accounting purposes.
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn record_success(
        &self,
        provider: &str,
        input_tokens: u64,
        output_tokens: u64,
        latency: Duration,
    ) {
        self.record_latency(provider, latency);
        let map = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = map.get(provider) {
            state.requests.fetch_add(1, Ordering::Relaxed);
            state.input_tokens.fetch_add(input_tokens, Ordering::Relaxed);
            state
                .output_tokens
                .fetch_add(output_tokens, Ordering::Relaxed);
        }
    }

    /// Record a failed inference call for a provider.
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn record_error(&self, provider: &str, latency: Duration) {
        self.record_latency(provider, latency);
        let map = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = map.get(provider) {
            state.requests.fetch_add(1, Ordering::Relaxed);
            state.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Select the cheapest provider whose P95 latency is at or below
    /// `sla_budget`.
    ///
    /// ## Selection algorithm
    ///
    /// 1. Filter providers whose P95 latency ≤ `sla_budget`.
    /// 2. Among those, pick the one with the lowest cost (input + output rate).
    ///    Ties are broken by `priority` (lower = preferred), then name
    ///    (alphabetical, for determinism).
    /// 3. If no provider meets the SLA (or `sla_budget` is `None`), fall back
    ///    to the provider with the lowest P95 latency.
    /// 4. If no providers are registered, returns `None`.
    ///
    /// # Returns
    ///
    /// A cloned [`ProviderProfile`] for the selected provider, or `None` if
    /// no providers are registered.
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn select_provider(&self, sla_budget: Option<Duration>) -> Option<ProviderProfile> {
        self.selections.fetch_add(1, Ordering::Relaxed);

        let map = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        if map.is_empty() {
            return None;
        }

        let sla_micros: Option<u64> = sla_budget.map(|d| {
            d.as_micros().min(u64::MAX as u128) as u64
        });

        // Compute P95 for each provider
        let mut candidates: Vec<(&ProviderState, u64)> = map
            .values()
            .map(|state| {
                let p95 = {
                    let ring = state.latency_ring.lock().unwrap_or_else(|e| e.into_inner());
                    ring.p95_micros()
                };
                (state, p95)
            })
            .collect();

        // Try to find providers within SLA
        let within_sla: Vec<_> = if let Some(budget) = sla_micros {
            candidates
                .iter()
                .filter(|(_, p95)| *p95 <= budget)
                .collect()
        } else {
            candidates.iter().collect()
        };

        let selected = if within_sla.is_empty() {
            // SLA miss — fall back to fastest
            self.sla_misses.fetch_add(1, Ordering::Relaxed);
            candidates
                .iter()
                .min_by(|(_, a_p95), (_, b_p95)| a_p95.cmp(b_p95))
                .map(|(state, _)| state)
        } else {
            // Among SLA-meeting providers, pick cheapest (then priority, then name)
            within_sla
                .iter()
                .min_by(|(a_state, _), (b_state, _)| {
                    let a_cost = a_state.profile.cost_per_1k_input_tokens
                        + a_state.profile.cost_per_1k_output_tokens;
                    let b_cost = b_state.profile.cost_per_1k_input_tokens
                        + b_state.profile.cost_per_1k_output_tokens;
                    a_cost
                        .partial_cmp(&b_cost)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a_state.profile.priority.cmp(&b_state.profile.priority))
                        .then(a_state.profile.name.cmp(&b_state.profile.name))
                })
                .map(|(state, _)| state)
        };

        selected.map(|s| s.profile.clone())
    }

    /// Return a point-in-time snapshot of all registered providers.
    ///
    /// # Panics
    ///
    /// Does not panic.
    pub fn snapshot(&self) -> Vec<ProviderSnapshot> {
        let map = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        map.values()
            .map(|state| {
                let (p50, p95) = {
                    let ring = state.latency_ring.lock().unwrap_or_else(|e| e.into_inner());
                    (ring.p50_micros(), ring.p95_micros())
                };
                let requests = state.requests.load(Ordering::Relaxed);
                let errors = state.errors.load(Ordering::Relaxed);
                ProviderSnapshot {
                    name: state.profile.name.clone(),
                    cost_per_1k_input_tokens: state.profile.cost_per_1k_input_tokens,
                    cost_per_1k_output_tokens: state.profile.cost_per_1k_output_tokens,
                    priority: state.profile.priority,
                    p50_latency_ms: if p50 == u64::MAX {
                        None
                    } else {
                        Some(p50 as f64 / 1000.0)
                    },
                    p95_latency_ms: if p95 == u64::MAX {
                        None
                    } else {
                        Some(p95 as f64 / 1000.0)
                    },
                    total_requests: requests,
                    error_rate: if requests > 0 {
                        errors as f64 / requests as f64
                    } else {
                        0.0
                    },
                    input_tokens: state.input_tokens.load(Ordering::Relaxed),
                    output_tokens: state.output_tokens.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    /// Total `select_provider` calls since engine creation.
    pub fn total_selections(&self) -> u64 {
        self.selections.load(Ordering::Relaxed)
    }

    /// Total selections that fell back to fastest (SLA could not be met).
    pub fn total_sla_misses(&self) -> u64 {
        self.sla_misses.load(Ordering::Relaxed)
    }
}

/// Point-in-time snapshot of a single provider's state.
#[derive(Debug, Clone)]
pub struct ProviderSnapshot {
    /// Provider name.
    pub name: String,
    /// Input token cost per 1K tokens (USD).
    pub cost_per_1k_input_tokens: f64,
    /// Output token cost per 1K tokens (USD).
    pub cost_per_1k_output_tokens: f64,
    /// Priority for tie-breaking (lower = preferred).
    pub priority: u32,
    /// P50 (median) latency in milliseconds, or `None` if no samples yet.
    pub p50_latency_ms: Option<f64>,
    /// P95 latency in milliseconds, or `None` if no samples yet.
    pub p95_latency_ms: Option<f64>,
    /// Total requests routed to this provider.
    pub total_requests: u64,
    /// Error rate (0.0 – 1.0).
    pub error_rate: f64,
    /// Total input tokens sent.
    pub input_tokens: u64,
    /// Total output tokens received.
    pub output_tokens: u64,
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_profile(name: &str, input_cost: f64, output_cost: f64) -> ProviderProfile {
        ProviderProfile {
            name: name.to_string(),
            cost_per_1k_input_tokens: input_cost,
            cost_per_1k_output_tokens: output_cost,
            priority: 0,
        }
    }

    #[test]
    fn empty_engine_returns_none() {
        let engine = ArbitrageEngine::new();
        assert!(engine.select_provider(None).is_none());
    }

    #[test]
    fn single_provider_always_selected() {
        let engine = ArbitrageEngine::new();
        engine.register(make_profile("anthropic", 0.003, 0.015));
        engine.record_latency("anthropic", Duration::from_millis(300));
        let result = engine.select_provider(Some(Duration::from_secs(1)));
        assert_eq!(result.map(|p| p.name), Some("anthropic".to_string()));
    }

    #[test]
    fn cheaper_provider_wins_when_both_meet_sla() {
        let engine = ArbitrageEngine::new();
        engine.register(make_profile("anthropic", 0.003, 0.015)); // cheaper
        engine.register(make_profile("openai", 0.005, 0.020));    // more expensive
        // Both have P95 = 200ms which is below 500ms SLA
        engine.record_latency("anthropic", Duration::from_millis(200));
        engine.record_latency("openai", Duration::from_millis(150));
        let result = engine.select_provider(Some(Duration::from_millis(500)));
        assert_eq!(result.map(|p| p.name), Some("anthropic".to_string()));
    }

    #[test]
    fn sla_miss_falls_back_to_fastest() {
        let engine = ArbitrageEngine::new();
        engine.register(make_profile("slow_cheap", 0.001, 0.003));
        engine.register(make_profile("fast_expensive", 0.010, 0.030));
        for _ in 0..10 {
            engine.record_latency("slow_cheap", Duration::from_millis(800));
            engine.record_latency("fast_expensive", Duration::from_millis(150));
        }
        // SLA = 100ms; both providers exceed it, so falls back to fastest (fast_expensive)
        let result = engine.select_provider(Some(Duration::from_millis(100)));
        assert_eq!(result.map(|p| p.name), Some("fast_expensive".to_string()));
        assert_eq!(engine.total_sla_misses(), 1);
    }

    #[test]
    fn no_sla_budget_picks_cheapest() {
        let engine = ArbitrageEngine::new();
        engine.register(make_profile("cheap", 0.001, 0.002));
        engine.register(make_profile("expensive", 0.010, 0.020));
        let result = engine.select_provider(None);
        assert_eq!(result.map(|p| p.name), Some("cheap".to_string()));
    }

    #[test]
    fn priority_breaks_cost_tie() {
        let engine = ArbitrageEngine::new();
        engine.register(ProviderProfile {
            name: "local_vllm".to_string(),
            cost_per_1k_input_tokens: 0.0,
            cost_per_1k_output_tokens: 0.0,
            priority: 0, // preferred
        });
        engine.register(ProviderProfile {
            name: "other_local".to_string(),
            cost_per_1k_input_tokens: 0.0,
            cost_per_1k_output_tokens: 0.0,
            priority: 1,
        });
        let result = engine.select_provider(None);
        assert_eq!(result.map(|p| p.name), Some("local_vllm".to_string()));
    }

    #[test]
    fn record_success_updates_accounting() {
        let engine = ArbitrageEngine::new();
        engine.register(make_profile("a", 0.003, 0.015));
        engine.record_success("a", 500, 200, Duration::from_millis(250));
        let snap: Vec<_> = engine.snapshot();
        let a = snap.iter().find(|s| s.name == "a").unwrap();
        assert_eq!(a.total_requests, 1);
        assert!((a.error_rate).abs() < f64::EPSILON);
        assert_eq!(a.input_tokens, 500);
        assert_eq!(a.output_tokens, 200);
    }

    #[test]
    fn record_error_increments_error_rate() {
        let engine = ArbitrageEngine::new();
        engine.register(make_profile("b", 0.003, 0.015));
        engine.record_success("b", 100, 50, Duration::from_millis(100));
        engine.record_error("b", Duration::from_millis(100));
        let snap: Vec<_> = engine.snapshot();
        let b = snap.iter().find(|s| s.name == "b").unwrap();
        assert_eq!(b.total_requests, 2);
        assert!((b.error_rate - 0.5).abs() < 1e-6);
    }

    #[test]
    fn provider_estimate_cost() {
        let p = make_profile("x", 0.003, 0.015);
        let cost = p.estimate_cost(1000, 500);
        // 1000/1000 * 0.003 + 500/1000 * 0.015 = 0.003 + 0.0075 = 0.0105
        assert!((cost - 0.0105).abs() < 1e-9);
    }

    #[test]
    fn snapshot_no_latency_returns_none_for_p95() {
        let engine = ArbitrageEngine::new();
        engine.register(make_profile("c", 0.003, 0.015));
        let snap = engine.snapshot();
        let c = snap.iter().find(|s| s.name == "c").unwrap();
        assert!(c.p95_latency_ms.is_none());
        assert!(c.p50_latency_ms.is_none());
    }
}
