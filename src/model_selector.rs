//! Intelligent model selection based on cost / quality / latency tradeoffs.
//!
//! ## Key types
//!
//! - [`ModelProfile`] — static description of a model's characteristics.
//! - [`SelectionCriteria`] — caller-supplied hard constraints (max cost, min quality, …).
//! - [`SelectionStrategy`] — ranking algorithm applied to the candidate set.
//! - [`ModelSelector`] — registry + selection engine.
//! - [`ModelUsageTracker`] — records actual runtime usage to compare with predictions.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::model_selector::{
//!     ModelProfile, ModelSelector, SelectionCriteria, SelectionStrategy,
//! };
//!
//! let mut selector = ModelSelector::new(SelectionStrategy::CheapestFirst);
//! selector.register(ModelProfile {
//!     id: "fast-cheap".to_string(),
//!     cost_per_1k_tokens: 0.001,
//!     avg_latency_ms: 200.0,
//!     quality_score: 0.75,
//!     max_context_tokens: 8192,
//!     supports_streaming: true,
//!     supports_tools: false,
//! });
//! let criteria = SelectionCriteria::default();
//! let model = selector.select(&criteria).unwrap();
//! assert_eq!(model.id, "fast-cheap");
//! ```

use std::collections::HashMap;

use dashmap::DashMap;

// ── ModelProfile ──────────────────────────────────────────────────────────────

/// Static description of a model's cost, performance, and capability profile.
#[derive(Debug, Clone)]
pub struct ModelProfile {
    /// Unique model identifier (e.g. `"claude-3-haiku"`, `"gpt-4o"`).
    pub id: String,
    /// Cost in USD per 1 000 tokens (blended input + output, or input-only if
    /// that is the pricing model; callers can always override via `estimate_cost`).
    pub cost_per_1k_tokens: f64,
    /// Average observed or documented end-to-end latency in milliseconds.
    pub avg_latency_ms: f64,
    /// Normalised quality score in `[0, 1]` (e.g. from benchmark results).
    pub quality_score: f64,
    /// Maximum context window size in tokens.
    pub max_context_tokens: usize,
    /// Whether the model supports streaming token output.
    pub supports_streaming: bool,
    /// Whether the model supports function / tool calling.
    pub supports_tools: bool,
}

// ── SelectionCriteria ─────────────────────────────────────────────────────────

/// Hard constraints that a model must satisfy to be eligible for selection.
#[derive(Debug, Clone, Default)]
pub struct SelectionCriteria {
    /// Reject models whose `cost_per_1k_tokens` exceeds this value.
    pub max_cost_per_1k: Option<f64>,
    /// Reject models whose `avg_latency_ms` exceeds this value.
    pub max_latency_ms: Option<u64>,
    /// Reject models whose `quality_score` is below this value.
    pub min_quality: Option<f64>,
    /// Reject models whose `max_context_tokens` is below this value.
    pub min_context_tokens: Option<usize>,
    /// If `true`, only models with `supports_streaming = true` are eligible.
    pub requires_streaming: bool,
    /// If `true`, only models with `supports_tools = true` are eligible.
    pub requires_tools: bool,
}

// ── SelectionStrategy ─────────────────────────────────────────────────────────

/// Algorithm used to rank the eligible model candidates.
#[derive(Debug, Clone)]
pub enum SelectionStrategy {
    /// Pick the model with the lowest `cost_per_1k_tokens`.
    CheapestFirst,
    /// Pick the model with the lowest `avg_latency_ms`.
    FastestFirst,
    /// Pick the model with the highest `quality_score`.
    BestQuality,
    /// Weighted linear combination of quality, cost, and latency.
    ///
    /// Score = `quality_weight * quality`
    ///       − `cost_weight   * (cost    / max_cost_in_set)`
    ///       − `latency_weight * (latency / max_latency_in_set)`
    ///
    /// Weights need not sum to 1.0; they are relative.
    Balanced {
        /// Weight applied to `quality_score`.
        cost_weight: f64,
        /// Weight applied to normalised latency (negatively).
        latency_weight: f64,
        /// Weight applied to normalised cost (negatively).
        quality_weight: f64,
    },
}

// ── ModelSelector ─────────────────────────────────────────────────────────────

/// Registry and selection engine for [`ModelProfile`]s.
pub struct ModelSelector {
    profiles: HashMap<String, ModelProfile>,
    strategy: SelectionStrategy,
}

impl ModelSelector {
    /// Create a new selector with no registered profiles.
    pub fn new(strategy: SelectionStrategy) -> Self {
        Self {
            profiles: HashMap::new(),
            strategy,
        }
    }

    /// Register (or replace) a model profile.
    pub fn register(&mut self, profile: ModelProfile) {
        self.profiles.insert(profile.id.clone(), profile);
    }

    /// Return the best model according to the active strategy that satisfies
    /// all hard constraints in `criteria`, or `None` if no model qualifies.
    pub fn select(&self, criteria: &SelectionCriteria) -> Option<&ModelProfile> {
        self.rank_all(criteria).into_iter().next()
    }

    /// Return all eligible profiles ranked from best to worst by the active strategy.
    pub fn rank_all(&self, criteria: &SelectionCriteria) -> Vec<&ModelProfile> {
        let mut candidates: Vec<&ModelProfile> = self
            .profiles
            .values()
            .filter(|p| self.satisfies(p, criteria))
            .collect();

        match &self.strategy {
            SelectionStrategy::CheapestFirst => {
                candidates.sort_by(|a, b| {
                    a.cost_per_1k_tokens
                        .partial_cmp(&b.cost_per_1k_tokens)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SelectionStrategy::FastestFirst => {
                candidates.sort_by(|a, b| {
                    a.avg_latency_ms
                        .partial_cmp(&b.avg_latency_ms)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SelectionStrategy::BestQuality => {
                candidates.sort_by(|a, b| {
                    b.quality_score
                        .partial_cmp(&a.quality_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SelectionStrategy::Balanced {
                cost_weight,
                latency_weight,
                quality_weight,
            } => {
                let max_cost = candidates
                    .iter()
                    .map(|p| p.cost_per_1k_tokens)
                    .fold(f64::NEG_INFINITY, f64::max);
                let max_latency = candidates
                    .iter()
                    .map(|p| p.avg_latency_ms)
                    .fold(f64::NEG_INFINITY, f64::max);

                let score = |p: &ModelProfile| -> f64 {
                    let norm_cost = if max_cost > 0.0 {
                        p.cost_per_1k_tokens / max_cost
                    } else {
                        0.0
                    };
                    let norm_latency = if max_latency > 0.0 {
                        p.avg_latency_ms / max_latency
                    } else {
                        0.0
                    };
                    quality_weight * p.quality_score
                        - cost_weight * norm_cost
                        - latency_weight * norm_latency
                };

                candidates.sort_by(|a, b| {
                    score(b)
                        .partial_cmp(&score(a))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
        }

        candidates
    }

    /// Estimate the cost in USD for a single request on `model_id`.
    ///
    /// Uses the profile's blended `cost_per_1k_tokens` applied to the total
    /// token count (`tokens_in + tokens_out`).  Returns `0.0` if the model ID
    /// is not registered.
    pub fn estimate_cost(&self, model_id: &str, tokens_in: usize, tokens_out: usize) -> f64 {
        self.profiles
            .get(model_id)
            .map(|p| p.cost_per_1k_tokens * (tokens_in + tokens_out) as f64 / 1000.0)
            .unwrap_or(0.0)
    }

    /// Return the cheapest registered model whose `quality_score >= min_quality`,
    /// or `None` if no model meets the threshold.
    pub fn cheapest_for_quality(&self, min_quality: f64) -> Option<&ModelProfile> {
        self.profiles
            .values()
            .filter(|p| p.quality_score >= min_quality)
            .min_by(|a, b| {
                a.cost_per_1k_tokens
                    .partial_cmp(&b.cost_per_1k_tokens)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn satisfies(&self, profile: &ModelProfile, criteria: &SelectionCriteria) -> bool {
        if let Some(max_cost) = criteria.max_cost_per_1k {
            if profile.cost_per_1k_tokens > max_cost {
                return false;
            }
        }
        if let Some(max_latency) = criteria.max_latency_ms {
            if profile.avg_latency_ms > max_latency as f64 {
                return false;
            }
        }
        if let Some(min_quality) = criteria.min_quality {
            if profile.quality_score < min_quality {
                return false;
            }
        }
        if let Some(min_ctx) = criteria.min_context_tokens {
            if profile.max_context_tokens < min_ctx {
                return false;
            }
        }
        if criteria.requires_streaming && !profile.supports_streaming {
            return false;
        }
        if criteria.requires_tools && !profile.supports_tools {
            return false;
        }
        true
    }
}

// ── ModelUsageTracker ─────────────────────────────────────────────────────────

/// Accumulated runtime usage for a single model.
#[derive(Debug, Default, Clone)]
pub struct ModelUsage {
    /// Total number of API calls recorded.
    pub calls: u64,
    /// Total tokens consumed across all calls.
    pub total_tokens: u64,
    /// Total cost in USD across all calls.
    pub total_cost: f64,
    /// Rolling average latency in milliseconds.
    pub avg_latency_ms: f64,
}

/// Thread-safe tracker for actual model usage.
///
/// Useful for comparing predicted vs. actual costs and computing
/// cost-efficiency ratios.
pub struct ModelUsageTracker {
    data: DashMap<String, ModelUsage>,
    /// Reference back to profiles so we can compute efficiency ratios.
    profiles: DashMap<String, ModelProfile>,
}

impl ModelUsageTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self {
            data: DashMap::new(),
            profiles: DashMap::new(),
        }
    }

    /// Register a model profile so that efficiency ratios can be computed.
    pub fn register_profile(&self, profile: ModelProfile) {
        self.profiles.insert(profile.id.clone(), profile);
    }

    /// Record a completed API call for `model_id`.
    pub fn record(&self, model_id: &str, tokens: u64, cost: f64, latency_ms: u64) {
        let mut entry = self.data.entry(model_id.to_string()).or_default();
        let prev_calls = entry.calls;
        entry.calls += 1;
        entry.total_tokens += tokens;
        entry.total_cost += cost;
        // Update rolling average: new_avg = (prev_avg * prev_calls + new_val) / new_calls
        entry.avg_latency_ms = (entry.avg_latency_ms * prev_calls as f64 + latency_ms as f64)
            / entry.calls as f64;
    }

    /// Return the cost-efficiency ratio for `model_id`.
    ///
    /// Efficiency = `quality_score / actual_cost_per_1k_tokens`.
    ///
    /// Returns `None` if the model has no recorded usage or no registered profile.
    pub fn cost_efficiency(&self, model_id: &str) -> Option<f64> {
        let usage = self.data.get(model_id)?;
        let profile = self.profiles.get(model_id)?;

        let actual_cost_per_1k = if usage.total_tokens > 0 {
            usage.total_cost / (usage.total_tokens as f64 / 1000.0)
        } else {
            return None;
        };

        if actual_cost_per_1k == 0.0 {
            return None;
        }

        Some(profile.quality_score / actual_cost_per_1k)
    }

    /// Return the recorded usage for `model_id`, if any.
    pub fn usage(&self, model_id: &str) -> Option<ModelUsage> {
        self.data.get(model_id).map(|u| u.clone())
    }
}

impl Default for ModelUsageTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, cost: f64, latency: f64, quality: f64) -> ModelProfile {
        ModelProfile {
            id: id.to_string(),
            cost_per_1k_tokens: cost,
            avg_latency_ms: latency,
            quality_score: quality,
            max_context_tokens: 8192,
            supports_streaming: true,
            supports_tools: true,
        }
    }

    fn make_selector(strategy: SelectionStrategy) -> ModelSelector {
        let mut sel = ModelSelector::new(strategy);
        sel.register(profile("cheap", 0.001, 500.0, 0.70));
        sel.register(profile("fast", 0.005, 100.0, 0.80));
        sel.register(profile("best", 0.020, 800.0, 0.95));
        sel
    }

    #[test]
    fn cheapest_model_selected() {
        let sel = make_selector(SelectionStrategy::CheapestFirst);
        let m = sel.select(&SelectionCriteria::default()).unwrap();
        assert_eq!(m.id, "cheap");
    }

    #[test]
    fn fastest_model_selected() {
        let sel = make_selector(SelectionStrategy::FastestFirst);
        let m = sel.select(&SelectionCriteria::default()).unwrap();
        assert_eq!(m.id, "fast");
    }

    #[test]
    fn best_quality_selected() {
        let sel = make_selector(SelectionStrategy::BestQuality);
        let m = sel.select(&SelectionCriteria::default()).unwrap();
        assert_eq!(m.id, "best");
    }

    #[test]
    fn quality_threshold_filtering() {
        let sel = make_selector(SelectionStrategy::CheapestFirst);
        let criteria = SelectionCriteria {
            min_quality: Some(0.85),
            ..Default::default()
        };
        let m = sel.select(&criteria).unwrap();
        assert_eq!(m.id, "best");
    }

    #[test]
    fn cost_filtering_excludes_expensive() {
        let sel = make_selector(SelectionStrategy::BestQuality);
        let criteria = SelectionCriteria {
            max_cost_per_1k: Some(0.006),
            ..Default::default()
        };
        let ranked = sel.rank_all(&criteria);
        assert!(ranked.iter().all(|p| p.cost_per_1k_tokens <= 0.006));
        // "best" (0.020) should be excluded; top pick should be "fast" (highest quality in set)
        assert_eq!(ranked[0].id, "fast");
    }

    #[test]
    fn balanced_score_ranking() {
        // With cost_weight and latency_weight very small and quality_weight large,
        // "best" (highest quality) should dominate.  We use near-zero penalties so
        // that cost/latency differences cannot overcome the quality advantage.
        let sel = make_selector(SelectionStrategy::Balanced {
            cost_weight: 0.01,
            latency_weight: 0.01,
            quality_weight: 10.0,
        });
        let ranked = sel.rank_all(&SelectionCriteria::default());
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].id, "best");

        // Sanity: with cost dominant and quality ignored, "cheap" should win.
        let sel2 = make_selector(SelectionStrategy::Balanced {
            cost_weight: 10.0,
            latency_weight: 0.01,
            quality_weight: 0.01,
        });
        let ranked2 = sel2.rank_all(&SelectionCriteria::default());
        assert_eq!(ranked2[0].id, "cheap");
    }

    #[test]
    fn cost_estimation() {
        let sel = make_selector(SelectionStrategy::CheapestFirst);
        // "cheap" costs 0.001 per 1k tokens; 500 in + 500 out = 1000 tokens total
        let cost = sel.estimate_cost("cheap", 500, 500);
        assert!((cost - 0.001).abs() < 1e-9);
    }

    #[test]
    fn cheapest_for_quality_threshold() {
        let sel = make_selector(SelectionStrategy::CheapestFirst);
        let m = sel.cheapest_for_quality(0.79).unwrap();
        // "cheap" has quality 0.70 (below), "fast" has 0.80 (above) and is cheaper than "best"
        assert_eq!(m.id, "fast");
    }

    #[test]
    fn usage_tracker_records_and_efficiency() {
        let tracker = ModelUsageTracker::new();
        tracker.register_profile(profile("fast", 0.005, 100.0, 0.80));
        tracker.record("fast", 1000, 0.005, 100);
        tracker.record("fast", 1000, 0.005, 200);
        let usage = tracker.usage("fast").unwrap();
        assert_eq!(usage.calls, 2);
        assert_eq!(usage.total_tokens, 2000);
        assert!((usage.avg_latency_ms - 150.0).abs() < 1e-6);
        let eff = tracker.cost_efficiency("fast");
        assert!(eff.is_some());
        assert!(eff.unwrap() > 0.0);
    }

    #[test]
    fn no_match_returns_none() {
        let sel = make_selector(SelectionStrategy::CheapestFirst);
        let criteria = SelectionCriteria {
            min_quality: Some(1.1), // impossible
            ..Default::default()
        };
        assert!(sel.select(&criteria).is_none());
    }
}
