#![allow(dead_code)]
//! # Module: Cost Optimizer
//!
//! ## Responsibility
//! Analyses historical request cost patterns and produces actionable
//! optimisation suggestions. Can optionally apply suggestions automatically
//! when `auto_optimize = true`.
//!
//! ## Detections
//! 1. **Cache candidates** — prompts whose responses are highly similar across
//!    invocations (response fingerprint collision rate above threshold).
//! 2. **Model overuse** — expensive models being used for structurally simple
//!    tasks (short prompt + short response).
//!
//! ## Guarantees
//! - Thread-safe (`Arc<CostOptimizer>` shareable across tasks)
//! - Bounded memory: observation windows capped at `window_size`
//! - Non-blocking: all methods are O(window_size), no I/O
//! - Advisory only unless `auto_optimize = true`

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by the cost optimizer.
#[derive(Debug, Error)]
pub enum CostOptimizerError {
    /// The internal lock was poisoned.
    #[error("internal lock poisoned")]
    LockPoisoned,
    /// The supplied configuration is invalid.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}

// ─── Configuration ────────────────────────────────────────────────────────────

/// Cost optimizer configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostOptimizerConfig {
    /// When `true`, suggestions are applied automatically.
    pub auto_optimize: bool,
    /// Number of recent observations to retain per prompt intent.
    pub window_size: usize,
    /// Fraction of observations that must have matching fingerprints before
    /// a prompt is flagged as a cache candidate (0.0–1.0).
    pub cache_candidate_threshold: f64,
    /// Maximum prompt+response token count (combined chars / 4) to classify
    /// a task as "simple". Tasks below this threshold on an expensive model
    /// are flagged for model downgrade.
    pub simple_task_token_threshold: usize,
    /// Model tier definitions: (model_name, cost_per_1k_tokens, tier).
    pub model_tiers: Vec<ModelTierEntry>,
}

impl Default for CostOptimizerConfig {
    fn default() -> Self {
        Self {
            auto_optimize: false,
            window_size: 200,
            cache_candidate_threshold: 0.70,
            simple_task_token_threshold: 512,
            model_tiers: vec![
                ModelTierEntry {
                    model: "gpt-4o".to_string(),
                    cost_per_1k_tokens: 0.005,
                    tier: ModelTier::Expensive,
                },
                ModelTierEntry {
                    model: "gpt-4o-mini".to_string(),
                    cost_per_1k_tokens: 0.00015,
                    tier: ModelTier::Cheap,
                },
                ModelTierEntry {
                    model: "claude-3-5-sonnet".to_string(),
                    cost_per_1k_tokens: 0.003,
                    tier: ModelTier::Expensive,
                },
                ModelTierEntry {
                    model: "claude-3-haiku".to_string(),
                    cost_per_1k_tokens: 0.00025,
                    tier: ModelTier::Cheap,
                },
            ],
        }
    }
}

/// Tier classification for a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTier {
    /// High-capability, high-cost model.
    Expensive,
    /// Lower-cost model suitable for simpler tasks.
    Cheap,
}

/// An entry in the model tier table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTierEntry {
    /// Model identifier as used in routing (e.g. `"gpt-4o"`).
    pub model: String,
    /// Cost per 1,000 tokens in USD.
    pub cost_per_1k_tokens: f64,
    /// Whether this model is expensive or cheap.
    pub tier: ModelTier,
}

// ─── Observation ─────────────────────────────────────────────────────────────

/// A single cost observation recorded for one inference call.
#[derive(Debug, Clone)]
pub struct CostObservation {
    /// First 64 chars of the prompt (intent key).
    pub intent: String,
    /// Model used.
    pub model: String,
    /// Approximate token count (prompt_chars + response_chars) / 4.
    pub tokens_approx: usize,
    /// Actual cost in USD (computed from tokens × model rate).
    pub cost_usd: f64,
    /// A simple fingerprint of the response (first 32 chars, lowercased).
    pub response_fingerprint: String,
    /// When this observation was recorded.
    pub recorded_at: Instant,
}

fn compute_fingerprint(response: &str) -> String {
    response
        .chars()
        .take(32)
        .collect::<String>()
        .to_lowercase()
        .trim()
        .to_string()
}

fn derive_intent(prompt: &str) -> String {
    prompt.chars().take(64).collect::<String>().to_lowercase()
}

// ─── Suggestions ─────────────────────────────────────────────────────────────

/// The kind of cost optimisation suggested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionKind {
    /// This prompt should be cached — responses are highly repetitive.
    EnableCaching,
    /// A cheaper model would be sufficient for this task.
    DowngradeModel,
}

/// A single optimisation suggestion with computed ROI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationSuggestion {
    /// The prompt intent this suggestion applies to.
    pub intent: String,
    /// What kind of optimisation is recommended.
    pub kind: SuggestionKind,
    /// Human-readable description of the suggestion.
    pub description: String,
    /// Estimated monthly cost savings in USD.
    pub estimated_monthly_savings_usd: f64,
    /// The model currently in use (for DowngradeModel suggestions).
    pub current_model: Option<String>,
    /// The recommended cheaper model (for DowngradeModel suggestions).
    pub suggested_model: Option<String>,
    /// Whether this suggestion was automatically applied.
    pub auto_applied: bool,
}

// ─── Auto-apply actions ──────────────────────────────────────────────────────

/// An automatically applied optimisation action.
#[derive(Debug, Clone)]
pub struct AutoApplyAction {
    /// The suggestion that triggered this action.
    pub suggestion: OptimizationSuggestion,
    /// When the action was applied.
    pub applied_at: Instant,
}

// ─── Per-intent stats ────────────────────────────────────────────────────────

#[derive(Debug)]
struct IntentWindow {
    observations: std::collections::VecDeque<CostObservation>,
    max_size: usize,
}

impl IntentWindow {
    fn new(max_size: usize) -> Self {
        Self {
            observations: std::collections::VecDeque::new(),
            max_size: max_size.max(1),
        }
    }

    fn push(&mut self, obs: CostObservation) {
        if self.observations.len() >= self.max_size {
            self.observations.pop_front();
        }
        self.observations.push_back(obs);
    }

    fn len(&self) -> usize {
        self.observations.len()
    }

    /// Fraction of observations whose fingerprint matches the most common one.
    fn fingerprint_collision_rate(&self) -> f64 {
        if self.observations.is_empty() {
            return 0.0;
        }
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for obs in &self.observations {
            *counts.entry(obs.response_fingerprint.as_str()).or_insert(0) += 1;
        }
        let max_count = counts.values().copied().max().unwrap_or(0);
        max_count as f64 / self.observations.len() as f64
    }

    /// Most recently used model.
    fn latest_model(&self) -> Option<&str> {
        self.observations.back().map(|o| o.model.as_str())
    }

    /// Average token count.
    fn avg_tokens(&self) -> f64 {
        if self.observations.is_empty() {
            return 0.0;
        }
        let sum: usize = self.observations.iter().map(|o| o.tokens_approx).sum();
        sum as f64 / self.observations.len() as f64
    }

    /// Average cost per call.
    fn avg_cost(&self) -> f64 {
        if self.observations.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.observations.iter().map(|o| o.cost_usd).sum();
        sum / self.observations.len() as f64
    }

    /// Estimated calls per month (extrapolated from observation timestamps).
    fn estimated_calls_per_month(&self) -> f64 {
        if self.observations.len() < 2 {
            return 0.0;
        }
        let first = self.observations.front().map(|o| o.recorded_at);
        let last = self.observations.back().map(|o| o.recorded_at);
        if let (Some(first), Some(last)) = (first, last) {
            let elapsed = last.duration_since(first);
            if elapsed.as_secs() == 0 {
                return 0.0;
            }
            let calls_per_sec =
                (self.observations.len() - 1) as f64 / elapsed.as_secs_f64();
            calls_per_sec * 86_400.0 * 30.0
        } else {
            0.0
        }
    }
}

// ─── Cost optimizer ──────────────────────────────────────────────────────────

/// Analyses cost patterns and generates (or applies) optimisation suggestions.
pub struct CostOptimizer {
    config: CostOptimizerConfig,
    inner: Mutex<CostOptimizerInner>,
}

#[derive(Debug)]
struct CostOptimizerInner {
    windows: HashMap<String, IntentWindow>, // intent → window
    auto_applied: Vec<AutoApplyAction>,
    overridden_models: HashMap<String, String>, // intent → forced cheaper model
}

impl CostOptimizer {
    /// Create a new optimizer with the given configuration.
    ///
    /// # Errors
    /// Returns [`CostOptimizerError::InvalidConfig`] if `window_size` is zero
    /// or `cache_candidate_threshold` is outside [0, 1].
    pub fn new(config: CostOptimizerConfig) -> Result<Arc<Self>, CostOptimizerError> {
        if config.window_size == 0 {
            return Err(CostOptimizerError::InvalidConfig(
                "window_size must be > 0".to_string(),
            ));
        }
        if !(0.0..=1.0).contains(&config.cache_candidate_threshold) {
            return Err(CostOptimizerError::InvalidConfig(
                "cache_candidate_threshold must be in [0, 1]".to_string(),
            ));
        }
        Ok(Arc::new(Self {
            config,
            inner: Mutex::new(CostOptimizerInner {
                windows: HashMap::new(),
                auto_applied: Vec::new(),
                overridden_models: HashMap::new(),
            }),
        }))
    }

    /// Record a completed inference call.
    pub fn record(
        &self,
        prompt: &str,
        model: &str,
        response: &str,
    ) -> Result<(), CostOptimizerError> {
        let intent = derive_intent(prompt);
        let tokens_approx = (prompt.len() + response.len()) / 4;
        let cost_usd = self.compute_cost(model, tokens_approx);
        let obs = CostObservation {
            intent: intent.clone(),
            model: model.to_string(),
            tokens_approx,
            cost_usd,
            response_fingerprint: compute_fingerprint(response),
            recorded_at: Instant::now(),
        };

        let mut inner = self.inner.lock().map_err(|_| CostOptimizerError::LockPoisoned)?;
        let window = inner
            .windows
            .entry(intent)
            .or_insert_with(|| IntentWindow::new(self.config.window_size));
        window.push(obs);
        Ok(())
    }

    /// Return a list of optimisation suggestions based on observed patterns.
    pub fn suggestions(&self) -> Result<Vec<OptimizationSuggestion>, CostOptimizerError> {
        let inner = self.inner.lock().map_err(|_| CostOptimizerError::LockPoisoned)?;
        let mut suggestions = Vec::new();

        for (intent, window) in &inner.windows {
            // Require at least 5 observations before generating suggestions
            if window.len() < 5 {
                continue;
            }

            // Cache candidate detection
            let collision_rate = window.fingerprint_collision_rate();
            if collision_rate >= self.config.cache_candidate_threshold {
                let avg_cost = window.avg_cost();
                let calls_per_month = window.estimated_calls_per_month();
                let savings = avg_cost * calls_per_month * collision_rate;
                suggestions.push(OptimizationSuggestion {
                    intent: intent.clone(),
                    kind: SuggestionKind::EnableCaching,
                    description: format!(
                        "Prompt '{intent:.40}…' produces identical responses {:.0}% of the time. \
                         Enable result caching to save ~${savings:.2}/month.",
                        collision_rate * 100.0
                    ),
                    estimated_monthly_savings_usd: savings,
                    current_model: window.latest_model().map(str::to_string),
                    suggested_model: None,
                    auto_applied: false,
                });
            }

            // Model downgrade detection
            if let Some(model) = window.latest_model() {
                if let Some(tier_entry) = self.config.model_tiers.iter().find(|t| t.model == model) {
                    if tier_entry.tier == ModelTier::Expensive {
                        let avg_tokens = window.avg_tokens();
                        if avg_tokens < self.config.simple_task_token_threshold as f64 {
                            // Find cheapest alternative
                            if let Some(cheap) = self
                                .config
                                .model_tiers
                                .iter()
                                .filter(|t| t.tier == ModelTier::Cheap)
                                .min_by(|a, b| {
                                    a.cost_per_1k_tokens
                                        .partial_cmp(&b.cost_per_1k_tokens)
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                })
                            {
                                let current_cost_per_call =
                                    avg_tokens / 1000.0 * tier_entry.cost_per_1k_tokens;
                                let cheap_cost_per_call =
                                    avg_tokens / 1000.0 * cheap.cost_per_1k_tokens;
                                let calls_per_month = window.estimated_calls_per_month();
                                let savings = (current_cost_per_call - cheap_cost_per_call)
                                    * calls_per_month;
                                suggestions.push(OptimizationSuggestion {
                                    intent: intent.clone(),
                                    kind: SuggestionKind::DowngradeModel,
                                    description: format!(
                                        "Simple task (avg {avg_tokens:.0} tokens) is using expensive model \
                                         '{model}'. Switch to '{}' to save ~${savings:.2}/month.",
                                        cheap.model
                                    ),
                                    estimated_monthly_savings_usd: savings,
                                    current_model: Some(model.to_string()),
                                    suggested_model: Some(cheap.model.clone()),
                                    auto_applied: false,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Sort by highest savings first
        suggestions.sort_by(|a, b| {
            b.estimated_monthly_savings_usd
                .partial_cmp(&a.estimated_monthly_savings_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(suggestions)
    }

    /// Apply all pending suggestions (only when `auto_optimize = true`).
    ///
    /// Returns the list of actions taken.
    pub fn auto_apply(&self) -> Result<Vec<AutoApplyAction>, CostOptimizerError> {
        if !self.config.auto_optimize {
            return Ok(vec![]);
        }
        let pending = self.suggestions()?;
        let mut inner = self.inner.lock().map_err(|_| CostOptimizerError::LockPoisoned)?;
        let mut applied = Vec::new();

        for mut suggestion in pending {
            match suggestion.kind {
                SuggestionKind::EnableCaching => {
                    // In a real system this would toggle a cache flag; here we log it
                    info!(
                        intent = %suggestion.intent,
                        savings_usd = suggestion.estimated_monthly_savings_usd,
                        "auto-optimizer: enabling caching for intent"
                    );
                    suggestion.auto_applied = true;
                    let action = AutoApplyAction {
                        suggestion: suggestion.clone(),
                        applied_at: Instant::now(),
                    };
                    inner.auto_applied.push(action.clone());
                    applied.push(action);
                }
                SuggestionKind::DowngradeModel => {
                    if let Some(ref cheap_model) = suggestion.suggested_model {
                        info!(
                            intent = %suggestion.intent,
                            from = suggestion.current_model.as_deref().unwrap_or("?"),
                            to = %cheap_model,
                            savings_usd = suggestion.estimated_monthly_savings_usd,
                            "auto-optimizer: overriding model for intent"
                        );
                        inner
                            .overridden_models
                            .insert(suggestion.intent.clone(), cheap_model.clone());
                        suggestion.auto_applied = true;
                        let action = AutoApplyAction {
                            suggestion: suggestion.clone(),
                            applied_at: Instant::now(),
                        };
                        inner.auto_applied.push(action.clone());
                        applied.push(action);
                    }
                }
            }
        }

        Ok(applied)
    }

    /// Return the model override for an intent (applied by auto-optimizer), if any.
    #[must_use]
    pub fn model_override(&self, prompt: &str) -> Option<String> {
        let intent = derive_intent(prompt);
        let Ok(inner) = self.inner.lock() else {
            return None;
        };
        inner.overridden_models.get(&intent).cloned()
    }

    /// Return total cost recorded across all observations.
    pub fn total_cost_usd(&self) -> Result<f64, CostOptimizerError> {
        let inner = self.inner.lock().map_err(|_| CostOptimizerError::LockPoisoned)?;
        let total: f64 = inner
            .windows
            .values()
            .flat_map(|w| w.observations.iter().map(|o| o.cost_usd))
            .sum();
        Ok(total)
    }

    /// Number of distinct intents tracked.
    pub fn intent_count(&self) -> Result<usize, CostOptimizerError> {
        let inner = self.inner.lock().map_err(|_| CostOptimizerError::LockPoisoned)?;
        Ok(inner.windows.len())
    }

    fn compute_cost(&self, model: &str, tokens_approx: usize) -> f64 {
        let rate = self
            .config
            .model_tiers
            .iter()
            .find(|t| t.model == model)
            .map(|t| t.cost_per_1k_tokens)
            .unwrap_or(0.002); // fallback mid-tier rate
        tokens_approx as f64 / 1000.0 * rate
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_optimizer(auto: bool) -> Arc<CostOptimizer> {
        CostOptimizer::new(CostOptimizerConfig {
            auto_optimize: auto,
            window_size: 20,
            cache_candidate_threshold: 0.6,
            simple_task_token_threshold: 200,
            ..Default::default()
        })
        .expect("optimizer should construct")
    }

    #[test]
    fn records_observations() {
        let opt = make_optimizer(false);
        opt.record("Tell me a joke", "gpt-4o", "Why did the chicken...").unwrap();
        opt.record("Tell me a joke", "gpt-4o", "Why did the chicken...").unwrap();
        assert_eq!(opt.intent_count().unwrap(), 1);
        assert!(opt.total_cost_usd().unwrap() > 0.0);
    }

    #[test]
    fn detects_cache_candidate() {
        let opt = make_optimizer(false);
        for _ in 0..8 {
            opt.record("Repeat after me: hello", "gpt-4o", "hello").unwrap();
        }
        let suggestions = opt.suggestions().unwrap();
        let cache_sugg = suggestions
            .iter()
            .find(|s| s.kind == SuggestionKind::EnableCaching);
        assert!(cache_sugg.is_some(), "should detect cache candidate");
    }

    #[test]
    fn detects_model_downgrade() {
        let opt = make_optimizer(false);
        // Very short prompt+response → simple task
        for _ in 0..8 {
            opt.record("Hi", "gpt-4o", "Hello").unwrap();
        }
        let suggestions = opt.suggestions().unwrap();
        let downgrade = suggestions
            .iter()
            .find(|s| s.kind == SuggestionKind::DowngradeModel);
        assert!(downgrade.is_some(), "should suggest model downgrade");
        assert!(downgrade.unwrap().suggested_model.is_some());
    }

    #[test]
    fn auto_apply_writes_override() {
        let opt = make_optimizer(true);
        for _ in 0..8 {
            opt.record("Hi", "gpt-4o", "Hello").unwrap();
        }
        let applied = opt.auto_apply().unwrap();
        assert!(!applied.is_empty());
        // At least one downgrade action should exist
        let has_override = opt.model_override("Hi").is_some();
        assert!(has_override, "model override should be set after auto-apply");
    }

    #[test]
    fn invalid_config_rejected() {
        let result = CostOptimizer::new(CostOptimizerConfig {
            window_size: 0,
            ..Default::default()
        });
        assert!(result.is_err());
    }
}
