//! Multi-provider LLM manager with failover, load-balancing, and rate-limit tracking.

use std::collections::HashMap;

/// Configuration for a single LLM provider.
#[derive(Debug, Clone)]
pub struct Provider {
    /// Unique stable identifier (e.g. `"openai"`, `"anthropic"`).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Base URL of the provider's inference API.
    pub api_endpoint: String,
    /// Maximum requests per minute allowed by the provider.
    pub max_rpm: u32,
    /// Maximum tokens per minute allowed by the provider.
    pub max_tpm: u64,
    /// Lower number = higher priority (0 is highest).
    pub priority: u8,
    /// Whether this provider is currently enabled.
    pub enabled: bool,
}

/// Point-in-time health snapshot for a provider.
#[derive(Debug, Clone)]
pub struct ProviderHealth {
    /// ID of the provider this snapshot belongs to.
    pub provider_id: String,
    /// Whether the provider is currently reachable and responding.
    pub is_healthy: bool,
    /// Fraction of recent requests that failed (0.0 – 1.0).
    pub error_rate: f64,
    /// Rolling average round-trip latency in milliseconds.
    pub avg_latency_ms: u64,
    /// Wall-clock timestamp when the health check ran (ms since epoch).
    pub last_checked: u64,
}

/// Strategy used to choose which provider handles a request.
#[derive(Debug, Clone)]
pub enum ProviderSelection {
    /// Always use the highest-priority healthy provider.
    Primary,
    /// Use the next available provider; `reason` explains why the primary was skipped.
    Fallback {
        /// Human-readable reason for the fallback (e.g. `"primary rate-limited"`).
        reason: String,
    },
    /// Distribute load proportionally according to the supplied weights.
    LoadBalanced {
        /// `(provider_id, weight)` pairs; weights need not sum to 1.
        weights: Vec<(String, f64)>,
    },
}

/// Cumulative statistics for a single provider.
#[derive(Debug, Clone, Default)]
pub struct ProviderStats {
    /// Total requests sent to this provider.
    pub requests: u64,
    /// Total failed requests.
    pub errors: u64,
    /// Total tokens consumed.
    pub total_tokens: u64,
    /// Exponentially-weighted moving average of latency in ms.
    pub avg_latency_ms: u64,
}

/// Per-provider rate-limit window state (sliding 60-second window, simplified).
#[derive(Debug, Default)]
struct RateLimitState {
    /// Requests issued in the current minute window.
    requests_this_window: u32,
    /// Tokens issued in the current minute window.
    tokens_this_window: u64,
    /// Start of the current 60-second window (ms since epoch).
    window_start_ms: u64,
}

impl RateLimitState {
    /// Advance the window if more than 60 seconds have passed, then return
    /// whether a request consuming `tokens` would stay within the limits.
    fn would_exceed(&mut self, max_rpm: u32, max_tpm: u64, tokens: u64, now: u64) -> bool {
        const WINDOW_MS: u64 = 60_000;
        if now.saturating_sub(self.window_start_ms) >= WINDOW_MS {
            self.requests_this_window = 0;
            self.tokens_this_window = 0;
            self.window_start_ms = now;
        }
        self.requests_this_window >= max_rpm || self.tokens_this_window + tokens > max_tpm
    }

    fn record(&mut self, tokens: u64, now: u64) {
        const WINDOW_MS: u64 = 60_000;
        if now.saturating_sub(self.window_start_ms) >= WINDOW_MS {
            self.requests_this_window = 0;
            self.tokens_this_window = 0;
            self.window_start_ms = now;
        }
        self.requests_this_window += 1;
        self.tokens_this_window += tokens;
    }
}

/// Central registry of LLM providers with health, stats, and rate-limit tracking.
#[derive(Debug, Default)]
pub struct ProviderManager {
    providers: HashMap<String, Provider>,
    health: HashMap<String, ProviderHealth>,
    stats: HashMap<String, ProviderStats>,
    rate_limits: HashMap<String, RateLimitState>,
    /// Estimated cost-per-token for each provider (USD).  Set externally.
    cost_per_token: HashMap<String, f64>,
}

impl ProviderManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a provider.
    pub fn register(&mut self, provider: Provider) {
        let id = provider.id.clone();
        self.providers.insert(id.clone(), provider);
        self.rate_limits.entry(id).or_default();
    }

    /// Set the estimated cost per token (USD) for a provider.
    ///
    /// Used by [`best_provider_for_budget`].
    pub fn set_cost_per_token(&mut self, provider_id: &str, cost: f64) {
        self.cost_per_token.insert(provider_id.to_string(), cost);
    }

    /// Replace the stored health snapshot for a provider.
    pub fn update_health(&mut self, health: ProviderHealth) {
        self.health.insert(health.provider_id.clone(), health);
    }

    /// Choose a provider according to `selection`.
    ///
    /// Rate-limited or disabled providers are skipped.  Returns `None` if no
    /// eligible provider exists.
    pub fn select_provider(
        &mut self,
        selection: &ProviderSelection,
        now: u64,
    ) -> Option<&Provider> {
        match selection {
            ProviderSelection::Primary => {
                // Highest-priority (lowest priority number) healthy enabled provider.
                self.failover_ids(now).into_iter().next().map(|id| &self.providers[&id])
            }
            ProviderSelection::Fallback { .. } => {
                // Same as failover but skip the first (primary).
                let chain = self.failover_ids(now);
                chain.into_iter().nth(1).map(|id| &self.providers[&id])
            }
            ProviderSelection::LoadBalanced { weights } => {
                // Pick the highest-weight enabled+healthy provider from the list.
                let best = weights
                    .iter()
                    .filter(|(pid, _)| self.is_eligible(pid, 0, now))
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(pid, _)| pid.clone());
                best.and_then(|id| self.providers.get(&id))
            }
        }
    }

    /// Return healthy, enabled providers sorted by ascending priority (lowest = best).
    pub fn failover_chain(&self) -> Vec<&Provider> {
        let mut eligible: Vec<&Provider> = self
            .providers
            .values()
            .filter(|p| p.enabled && self.is_healthy(&p.id))
            .collect();
        eligible.sort_by_key(|p| p.priority);
        eligible
    }

    /// Record the outcome of a request to a provider.
    pub fn record_request(
        &mut self,
        provider_id: &str,
        tokens: u64,
        latency_ms: u64,
        success: bool,
    ) {
        let rl = self.rate_limits.entry(provider_id.to_string()).or_default();
        // Use a dummy `now` of 0 for recording — the window was already checked at selection time.
        rl.record(tokens, 0);

        let stats = self.stats.entry(provider_id.to_string()).or_default();
        stats.requests += 1;
        if !success {
            stats.errors += 1;
        }
        stats.total_tokens += tokens;
        // Exponential moving average: α = 0.1
        const ALPHA: f64 = 0.1;
        stats.avg_latency_ms = (ALPHA * latency_ms as f64
            + (1.0 - ALPHA) * stats.avg_latency_ms as f64)
            .round() as u64;
    }

    /// Return the cumulative stats for a provider, if any requests have been recorded.
    pub fn provider_stats(&self, provider_id: &str) -> Option<ProviderStats> {
        self.stats.get(provider_id).cloned()
    }

    /// Return the cheapest healthy provider whose cost-per-token is ≤ `cost_per_token_limit`.
    pub fn best_provider_for_budget(&self, cost_per_token_limit: f64) -> Option<&Provider> {
        self.providers
            .values()
            .filter(|p| p.enabled && self.is_healthy(&p.id))
            .filter(|p| {
                self.cost_per_token
                    .get(&p.id)
                    .map_or(false, |&c| c <= cost_per_token_limit)
            })
            .min_by(|a, b| {
                let ca = self.cost_per_token.get(&a.id).copied().unwrap_or(f64::MAX);
                let cb = self.cost_per_token.get(&b.id).copied().unwrap_or(f64::MAX);
                ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    // ── internal helpers ──────────────────────────────────────────────────────

    fn is_healthy(&self, provider_id: &str) -> bool {
        self.health
            .get(provider_id)
            .map_or(true, |h| h.is_healthy)
    }

    fn is_eligible(&self, provider_id: &str, tokens: u64, now: u64) -> bool {
        let provider = match self.providers.get(provider_id) {
            Some(p) => p,
            None => return false,
        };
        if !provider.enabled {
            return false;
        }
        if !self.is_healthy(provider_id) {
            return false;
        }
        // We need mutable access for the rate-limit check; we approximate by
        // checking the current window counts without advancing the window.
        if let Some(rl) = self.rate_limits.get(provider_id) {
            const WINDOW_MS: u64 = 60_000;
            let in_window = now.saturating_sub(rl.window_start_ms) < WINDOW_MS;
            if in_window {
                if rl.requests_this_window >= provider.max_rpm {
                    return false;
                }
                if rl.tokens_this_window + tokens > provider.max_tpm {
                    return false;
                }
            }
        }
        true
    }

    /// Build the failover chain as a list of IDs (cheaply cloned).
    fn failover_ids(&mut self, now: u64) -> Vec<String> {
        let mut eligible: Vec<(u8, String)> = self
            .providers
            .values()
            .filter(|p| p.enabled && self.is_healthy(&p.id))
            .map(|p| (p.priority, p.id.clone()))
            .collect();
        eligible.sort_by_key(|(pri, _)| *pri);

        // Filter by rate limits (read-only check; we don't consume quota here).
        eligible
            .into_iter()
            .filter(|(_, id)| self.is_eligible(id, 0, now))
            .map(|(_, id)| id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_provider(id: &str, priority: u8, enabled: bool) -> Provider {
        Provider {
            id: id.to_string(),
            name: id.to_string(),
            api_endpoint: format!("https://{}.example.com/v1", id),
            max_rpm: 60,
            max_tpm: 100_000,
            priority,
            enabled,
        }
    }

    fn healthy(id: &str) -> ProviderHealth {
        ProviderHealth {
            provider_id: id.to_string(),
            is_healthy: true,
            error_rate: 0.0,
            avg_latency_ms: 50,
            last_checked: 0,
        }
    }

    fn unhealthy(id: &str) -> ProviderHealth {
        ProviderHealth {
            provider_id: id.to_string(),
            is_healthy: false,
            error_rate: 1.0,
            avg_latency_ms: 5000,
            last_checked: 0,
        }
    }

    #[test]
    fn register_and_select_primary() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("openai", 0, true));
        mgr.update_health(healthy("openai"));
        let p = mgr.select_provider(&ProviderSelection::Primary, 0).unwrap();
        assert_eq!(p.id, "openai");
    }

    #[test]
    fn select_primary_skips_disabled() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("openai", 0, false));
        mgr.register(make_provider("anthropic", 1, true));
        mgr.update_health(healthy("openai"));
        mgr.update_health(healthy("anthropic"));
        let p = mgr.select_provider(&ProviderSelection::Primary, 0).unwrap();
        assert_eq!(p.id, "anthropic");
    }

    #[test]
    fn select_primary_skips_unhealthy() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("openai", 0, true));
        mgr.register(make_provider("anthropic", 1, true));
        mgr.update_health(unhealthy("openai"));
        mgr.update_health(healthy("anthropic"));
        let p = mgr.select_provider(&ProviderSelection::Primary, 0).unwrap();
        assert_eq!(p.id, "anthropic");
    }

    #[test]
    fn select_primary_none_when_all_unhealthy() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("openai", 0, true));
        mgr.update_health(unhealthy("openai"));
        assert!(mgr.select_provider(&ProviderSelection::Primary, 0).is_none());
    }

    #[test]
    fn failover_chain_sorted_by_priority() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("b", 2, true));
        mgr.register(make_provider("a", 0, true));
        mgr.register(make_provider("c", 1, true));
        mgr.update_health(healthy("a"));
        mgr.update_health(healthy("b"));
        mgr.update_health(healthy("c"));
        let chain: Vec<&str> = mgr.failover_chain().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(chain, vec!["a", "c", "b"]);
    }

    #[test]
    fn fallback_selection_skips_primary() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("primary", 0, true));
        mgr.register(make_provider("backup", 1, true));
        mgr.update_health(healthy("primary"));
        mgr.update_health(healthy("backup"));
        let p = mgr
            .select_provider(
                &ProviderSelection::Fallback {
                    reason: "test".to_string(),
                },
                0,
            )
            .unwrap();
        assert_eq!(p.id, "backup");
    }

    #[test]
    fn load_balanced_picks_highest_weight() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("a", 0, true));
        mgr.register(make_provider("b", 1, true));
        mgr.update_health(healthy("a"));
        mgr.update_health(healthy("b"));
        let weights = vec![("a".to_string(), 0.3), ("b".to_string(), 0.7)];
        let p = mgr
            .select_provider(&ProviderSelection::LoadBalanced { weights }, 0)
            .unwrap();
        assert_eq!(p.id, "b");
    }

    #[test]
    fn record_request_updates_stats() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("openai", 0, true));
        mgr.record_request("openai", 500, 100, true);
        mgr.record_request("openai", 200, 200, false);
        let stats = mgr.provider_stats("openai").unwrap();
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.total_tokens, 700);
    }

    #[test]
    fn provider_stats_none_for_unknown() {
        let mgr = ProviderManager::new();
        assert!(mgr.provider_stats("ghost").is_none());
    }

    #[test]
    fn best_provider_for_budget_finds_cheapest() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("cheap", 1, true));
        mgr.register(make_provider("expensive", 0, true));
        mgr.update_health(healthy("cheap"));
        mgr.update_health(healthy("expensive"));
        mgr.set_cost_per_token("cheap", 0.000001);
        mgr.set_cost_per_token("expensive", 0.00001);
        let p = mgr.best_provider_for_budget(0.000005).unwrap();
        assert_eq!(p.id, "cheap");
    }

    #[test]
    fn best_provider_for_budget_none_when_all_exceed() {
        let mut mgr = ProviderManager::new();
        mgr.register(make_provider("pricey", 0, true));
        mgr.update_health(healthy("pricey"));
        mgr.set_cost_per_token("pricey", 0.01);
        assert!(mgr.best_provider_for_budget(0.000001).is_none());
    }

    #[test]
    fn rate_limit_state_advances_window() {
        let mut rl = RateLimitState::default();
        // Within the window, one request should not exceed rpm=1.
        rl.record(100, 0);
        // Now at rpm=1; another request should exceed.
        assert!(rl.would_exceed(1, 10_000, 100, 0));
        // After 60s, window resets and request is allowed again.
        assert!(!rl.would_exceed(1, 10_000, 100, 60_001));
    }
}
