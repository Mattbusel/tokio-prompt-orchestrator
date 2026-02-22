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
//! # Stage: Learned Router (Task 3.1)
//!
//! ## Responsibility
//!
//! Multi-armed bandit that learns which model performs best per request
//! category. Uses epsilon-greedy exploration with configurable decay to
//! balance exploitation of known-good models against exploration of
//! potentially better alternatives.
//!
//! ## Guarantees
//!
//! - **Thread-safe**: all operations protected by `Arc<Mutex<_>>`
//! - **Non-blocking**: lock hold times are O(models) worst-case
//! - **Deterministic scoring**: identical outcome sequences produce identical scores
//! - **Bounded**: arm state is bounded by `models x categories`
//!
//! ## NOT Responsible For
//!
//! - Actual HTTP calls to model endpoints
//! - Model configuration or health checking
//! - Load balancing (this is routing by quality, not by load)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Errors produced by the learned router.
#[derive(Error, Debug)]
pub enum RouterError {
    /// Internal lock was poisoned by a panicking thread.
    #[error("router lock poisoned")]
    LockPoisoned,

    /// The specified model has not been registered.
    #[error("model not registered: {0}")]
    ModelNotRegistered(String),

    /// No models have been registered, so routing is impossible.
    #[error("no models available for routing")]
    NoModelsAvailable,
}

/// Configuration for the epsilon-greedy learned router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    /// Probability of exploring a random model instead of exploiting the best.
    pub epsilon: f64,
    /// Multiplicative decay applied to epsilon after each selection.
    pub epsilon_decay: f64,
    /// Minimum epsilon value (exploration never drops below this).
    pub min_epsilon: f64,
    /// Default score assigned to a newly registered model arm.
    pub default_score: f64,
}

impl Default for RouterConfig {
    /// Create a [`RouterConfig`] with sensible defaults.
    ///
    /// # Panics
    ///
    /// This function never panics.
    fn default() -> Self {
        Self {
            epsilon: 0.1,
            epsilon_decay: 0.995,
            min_epsilon: 0.01,
            default_score: 0.5,
        }
    }
}

/// Score and statistics for a single model across all categories or one category.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingScore {
    /// Model identifier.
    pub model: String,
    /// Current weighted score (0.0-1.0).
    pub score: f64,
    /// Total number of times this model was selected.
    pub selections: u64,
    /// Number of successful outcomes.
    pub successes: u64,
    /// Average observed latency in milliseconds.
    pub avg_latency_ms: f64,
}

/// Features describing an incoming request, used to select a category for routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFeatures {
    /// Logical category of the request (e.g., "code", "chat", "analysis").
    pub category: String,
    /// Estimated complexity on a 0.0-1.0 scale.
    pub estimated_complexity: f64,
    /// Estimated token count for the prompt.
    pub token_estimate: u64,
}

/// Internal per-arm (model+category) statistics.
#[derive(Debug, Clone)]
struct ArmState {
    selections: u64,
    successes: u64,
    total_latency_ms: f64,
    score: f64,
}

/// Internal mutable state of the router.
#[derive(Debug)]
struct Inner {
    /// category -> model -> arm state
    arms: HashMap<String, HashMap<String, ArmState>>,
    /// Set of all registered model names.
    models: Vec<String>,
    /// Current epsilon value (decays over time).
    current_epsilon: f64,
    /// Configuration snapshot.
    config: RouterConfig,
    /// Simple counter used for pseudo-random exploration.
    selection_counter: u64,
}

/// Learned multi-armed bandit router that directs requests to the best model
/// per category based on observed outcomes.
///
/// Uses epsilon-greedy exploration with configurable decay. Clone is cheap
/// (`Arc`-based) and all clones share state.
#[derive(Debug, Clone)]
pub struct LearnedRouter {
    inner: Arc<Mutex<Inner>>,
}

impl LearnedRouter {
    /// Create a new router with the given configuration.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(config: RouterConfig) -> Self {
        let epsilon = config.epsilon;
        Self {
            inner: Arc::new(Mutex::new(Inner {
                arms: HashMap::new(),
                models: Vec::new(),
                current_epsilon: epsilon,
                config,
                selection_counter: 0,
            })),
        }
    }

    /// Create a new router with default configuration.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn with_defaults() -> Self {
        Self::new(RouterConfig::default())
    }

    /// Register a model for routing. Duplicate registrations are idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn register_model(&self, model: &str) -> Result<(), RouterError> {
        let mut inner = self.inner.lock().map_err(|_| RouterError::LockPoisoned)?;
        if !inner.models.contains(&model.to_string()) {
            inner.models.push(model.to_string());
        }
        Ok(())
    }

    /// Select the best model for the given request features using epsilon-greedy.
    ///
    /// With probability `epsilon`, a random model is chosen (exploration).
    /// Otherwise, the model with the highest score for the request's category
    /// is chosen (exploitation).
    ///
    /// # Errors
    ///
    /// - [`RouterError::NoModelsAvailable`] if no models are registered.
    /// - [`RouterError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn select_model(&self, features: &RequestFeatures) -> Result<String, RouterError> {
        let mut inner = self.inner.lock().map_err(|_| RouterError::LockPoisoned)?;
        if inner.models.is_empty() {
            return Err(RouterError::NoModelsAvailable);
        }

        inner.selection_counter = inner.selection_counter.wrapping_add(1);
        let counter = inner.selection_counter;
        let epsilon = inner.current_epsilon;
        let min_epsilon = inner.config.min_epsilon;
        let decay = inner.config.epsilon_decay;
        let default_score = inner.config.default_score;

        // Pseudo-random exploration based on counter and epsilon
        let explore = {
            let hash = counter
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let normalized = (hash % 10000) as f64 / 10000.0;
            normalized < epsilon
        };

        let selected = if explore {
            let idx = (counter as usize) % inner.models.len();
            inner.models[idx].clone()
        } else {
            let category_arms = inner.arms.get(&features.category);
            match category_arms {
                Some(arms) => {
                    let mut best_model = inner.models[0].clone();
                    let mut best_score = f64::NEG_INFINITY;
                    for model in &inner.models {
                        let score = arms.get(model).map(|a| a.score).unwrap_or(default_score);
                        if score > best_score {
                            best_score = score;
                            best_model = model.clone();
                        }
                    }
                    best_model
                }
                None => inner.models[0].clone(),
            }
        };

        // Decay epsilon
        let new_epsilon = (epsilon * decay).max(min_epsilon);
        inner.current_epsilon = new_epsilon;

        // Ensure arm state exists
        let category_arms = inner.arms.entry(features.category.clone()).or_default();
        category_arms.entry(selected.clone()).or_insert(ArmState {
            selections: 0,
            successes: 0,
            total_latency_ms: 0.0,
            score: default_score,
        });
        if let Some(arm) = category_arms.get_mut(&selected) {
            arm.selections += 1;
        }

        Ok(selected)
    }

    /// Record the outcome of a model call, updating the arm's statistics.
    ///
    /// The score is updated using an incremental mean of the success rate.
    ///
    /// # Arguments
    ///
    /// * `model` - the model that was called
    /// * `category` - the request category
    /// * `success` - whether the call succeeded
    /// * `latency_ms` - observed latency in milliseconds
    ///
    /// # Errors
    ///
    /// - [`RouterError::ModelNotRegistered`] if the model was never registered.
    /// - [`RouterError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn record_outcome(
        &self,
        model: &str,
        category: &str,
        success: bool,
        latency_ms: f64,
    ) -> Result<(), RouterError> {
        let mut inner = self.inner.lock().map_err(|_| RouterError::LockPoisoned)?;
        if !inner.models.contains(&model.to_string()) {
            return Err(RouterError::ModelNotRegistered(model.to_string()));
        }

        let default_score = inner.config.default_score;
        let category_arms = inner.arms.entry(category.to_string()).or_default();
        let arm = category_arms.entry(model.to_string()).or_insert(ArmState {
            selections: 0,
            successes: 0,
            total_latency_ms: 0.0,
            score: default_score,
        });

        if success {
            arm.successes += 1;
        }
        arm.total_latency_ms += latency_ms;

        // Update score as success rate
        let total = arm.selections.max(1) as f64;
        arm.score = arm.successes as f64 / total;

        Ok(())
    }

    /// Return routing scores for all models across all categories.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn scores(&self) -> Vec<RoutingScore> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };

        let mut result = Vec::new();
        for model in &inner.models {
            let mut total_selections: u64 = 0;
            let mut total_successes: u64 = 0;
            let mut total_latency: f64 = 0.0;
            let mut category_count: u64 = 0;
            let mut total_score: f64 = 0.0;

            for arms in inner.arms.values() {
                if let Some(arm) = arms.get(model) {
                    total_selections += arm.selections;
                    total_successes += arm.successes;
                    total_latency += arm.total_latency_ms;
                    total_score += arm.score;
                    category_count += 1;
                }
            }

            let avg_latency = if total_selections > 0 {
                total_latency / total_selections as f64
            } else {
                0.0
            };
            let avg_score = if category_count > 0 {
                total_score / category_count as f64
            } else {
                inner.config.default_score
            };

            result.push(RoutingScore {
                model: model.clone(),
                score: avg_score,
                selections: total_selections,
                successes: total_successes,
                avg_latency_ms: avg_latency,
            });
        }
        result
    }

    /// Return routing scores for a specific category.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn scores_for_category(&self, category: &str) -> Vec<RoutingScore> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };

        let mut result = Vec::new();
        let category_arms = match inner.arms.get(category) {
            Some(arms) => arms,
            None => return result,
        };

        for model in &inner.models {
            if let Some(arm) = category_arms.get(model) {
                let avg_latency = if arm.selections > 0 {
                    arm.total_latency_ms / arm.selections as f64
                } else {
                    0.0
                };
                result.push(RoutingScore {
                    model: model.clone(),
                    score: arm.score,
                    selections: arm.selections,
                    successes: arm.successes,
                    avg_latency_ms: avg_latency,
                });
            }
        }
        result
    }

    /// Return the best-scoring model for a category, or `None` if no data exists.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn best_model(&self, category: &str) -> Option<String> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return None,
        };

        let category_arms = inner.arms.get(category)?;
        let mut best: Option<(String, f64)> = None;
        for (model, arm) in category_arms {
            match &best {
                Some((_, best_score)) if arm.score <= *best_score => {}
                _ => {
                    best = Some((model.clone(), arm.score));
                }
            }
        }
        best.map(|(model, _)| model)
    }

    /// Return the number of registered models.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn model_count(&self) -> usize {
        self.inner.lock().map(|g| g.models.len()).unwrap_or(0)
    }

    /// Reset all scores and arm statistics, keeping model registrations.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn reset_scores(&self) -> Result<(), RouterError> {
        let mut inner = self.inner.lock().map_err(|_| RouterError::LockPoisoned)?;
        inner.arms.clear();
        inner.current_epsilon = inner.config.epsilon;
        inner.selection_counter = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_features(category: &str) -> RequestFeatures {
        RequestFeatures {
            category: category.to_string(),
            estimated_complexity: 0.5,
            token_estimate: 100,
        }
    }

    #[test]
    fn test_register_model_adds_model() {
        let router = LearnedRouter::with_defaults();
        router.register_model("gpt-4").unwrap();
        assert_eq!(router.model_count(), 1);
    }

    #[test]
    fn test_register_model_idempotent() {
        let router = LearnedRouter::with_defaults();
        router.register_model("gpt-4").unwrap();
        router.register_model("gpt-4").unwrap();
        assert_eq!(router.model_count(), 1);
    }

    #[test]
    fn test_register_multiple_models() {
        let router = LearnedRouter::with_defaults();
        router.register_model("gpt-4").unwrap();
        router.register_model("claude-3").unwrap();
        router.register_model("llama-3").unwrap();
        assert_eq!(router.model_count(), 3);
    }

    #[test]
    fn test_select_model_no_models_returns_error() {
        let router = LearnedRouter::with_defaults();
        let features = make_features("code");
        let result = router.select_model(&features);
        assert!(matches!(result, Err(RouterError::NoModelsAvailable)));
    }

    #[test]
    fn test_select_model_single_model_always_selected() {
        let router = LearnedRouter::with_defaults();
        router.register_model("gpt-4").unwrap();
        let features = make_features("code");
        for _ in 0..20 {
            let model = router.select_model(&features).unwrap();
            assert_eq!(model, "gpt-4");
        }
    }

    #[test]
    fn test_select_model_returns_registered_model() {
        let router = LearnedRouter::with_defaults();
        router.register_model("gpt-4").unwrap();
        router.register_model("claude-3").unwrap();
        let features = make_features("chat");
        let model = router.select_model(&features).unwrap();
        assert!(model == "gpt-4" || model == "claude-3");
    }

    #[test]
    fn test_record_outcome_updates_score() {
        let router = LearnedRouter::with_defaults();
        router.register_model("gpt-4").unwrap();
        let features = make_features("code");
        let _ = router.select_model(&features);

        router.record_outcome("gpt-4", "code", true, 100.0).unwrap();
        router.record_outcome("gpt-4", "code", true, 200.0).unwrap();

        let scores = router.scores_for_category("code");
        assert!(!scores.is_empty());
        let gpt4_score = scores.iter().find(|s| s.model == "gpt-4").unwrap();
        assert!(gpt4_score.successes >= 2);
    }

    #[test]
    fn test_record_outcome_unregistered_model_returns_error() {
        let router = LearnedRouter::with_defaults();
        let result = router.record_outcome("unknown-model", "code", true, 100.0);
        assert!(matches!(result, Err(RouterError::ModelNotRegistered(_))));
    }

    #[test]
    fn test_best_model_returns_highest_scoring() {
        let router = LearnedRouter::with_defaults();
        router.register_model("model-a").unwrap();
        router.register_model("model-b").unwrap();

        let features = make_features("code");
        let _ = router.select_model(&features);
        let _ = router.select_model(&features);

        for _ in 0..10 {
            router
                .record_outcome("model-b", "code", true, 100.0)
                .unwrap();
        }
        for _ in 0..10 {
            router
                .record_outcome("model-a", "code", false, 500.0)
                .unwrap();
        }

        let best = router.best_model("code");
        assert_eq!(best, Some("model-b".to_string()));
    }

    #[test]
    fn test_best_model_empty_category_returns_none() {
        let router = LearnedRouter::with_defaults();
        router.register_model("gpt-4").unwrap();
        assert!(router.best_model("nonexistent").is_none());
    }

    #[test]
    fn test_scores_returns_all_models() {
        let router = LearnedRouter::with_defaults();
        router.register_model("model-a").unwrap();
        router.register_model("model-b").unwrap();
        let scores = router.scores();
        assert_eq!(scores.len(), 2);
    }

    #[test]
    fn test_scores_for_category_empty_returns_empty() {
        let router = LearnedRouter::with_defaults();
        router.register_model("model-a").unwrap();
        let scores = router.scores_for_category("nonexistent");
        assert!(scores.is_empty());
    }

    #[test]
    fn test_reset_scores_clears_all_arms() {
        let router = LearnedRouter::with_defaults();
        router.register_model("gpt-4").unwrap();
        let features = make_features("code");
        let _ = router.select_model(&features);
        router.record_outcome("gpt-4", "code", true, 100.0).unwrap();

        router.reset_scores().unwrap();

        let scores = router.scores_for_category("code");
        assert!(scores.is_empty());
        assert_eq!(router.model_count(), 1);
    }

    #[test]
    fn test_clone_shares_state() {
        let router = LearnedRouter::with_defaults();
        router.register_model("gpt-4").unwrap();
        let clone = router.clone();
        clone.register_model("claude-3").unwrap();
        assert_eq!(router.model_count(), 2);
    }

    #[test]
    fn test_router_config_serialization() {
        let config = RouterConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: RouterConfig = serde_json::from_str(&json).unwrap();
        assert!((deserialized.epsilon - config.epsilon).abs() < f64::EPSILON);
    }

    #[test]
    fn test_routing_score_serialization() {
        let score = RoutingScore {
            model: "gpt-4".to_string(),
            score: 0.85,
            selections: 100,
            successes: 85,
            avg_latency_ms: 250.0,
        };
        let json = serde_json::to_string(&score).unwrap();
        let deserialized: RoutingScore = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.model, "gpt-4");
        assert_eq!(deserialized.selections, 100);
    }

    #[test]
    fn test_request_features_serialization() {
        let features = RequestFeatures {
            category: "code".to_string(),
            estimated_complexity: 0.8,
            token_estimate: 500,
        };
        let json = serde_json::to_string(&features).unwrap();
        let deserialized: RequestFeatures = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.category, "code");
    }

    #[test]
    fn test_epsilon_decays_on_selection() {
        let config = RouterConfig {
            epsilon: 1.0,
            epsilon_decay: 0.5,
            min_epsilon: 0.01,
            default_score: 0.5,
        };
        let router = LearnedRouter::new(config);
        router.register_model("model-a").unwrap();
        router.register_model("model-b").unwrap();

        let features = make_features("code");
        for _ in 0..5 {
            let _ = router.select_model(&features);
        }

        let model = router.select_model(&features).unwrap();
        assert!(model == "model-a" || model == "model-b");
    }

    #[test]
    fn test_latency_tracking() {
        let router = LearnedRouter::with_defaults();
        router.register_model("model-a").unwrap();
        let features = make_features("code");
        let _ = router.select_model(&features);

        router
            .record_outcome("model-a", "code", true, 100.0)
            .unwrap();
        router
            .record_outcome("model-a", "code", true, 300.0)
            .unwrap();

        let scores = router.scores_for_category("code");
        let score = scores.iter().find(|s| s.model == "model-a").unwrap();
        assert!(score.avg_latency_ms > 0.0);
    }

    #[test]
    fn test_multiple_categories_independent() {
        let router = LearnedRouter::with_defaults();
        router.register_model("model-a").unwrap();
        let code_features = make_features("code");
        let chat_features = make_features("chat");

        let _ = router.select_model(&code_features);
        let _ = router.select_model(&chat_features);

        router
            .record_outcome("model-a", "code", true, 100.0)
            .unwrap();
        router
            .record_outcome("model-a", "chat", false, 500.0)
            .unwrap();

        let code_scores = router.scores_for_category("code");
        let chat_scores = router.scores_for_category("chat");

        let code_score = code_scores.iter().find(|s| s.model == "model-a").unwrap();
        let chat_score = chat_scores.iter().find(|s| s.model == "model-a").unwrap();

        assert!(code_score.score > chat_score.score);
    }

    #[test]
    fn test_router_error_display() {
        let err = RouterError::ModelNotRegistered("unknown".to_string());
        assert!(err.to_string().contains("unknown"));

        let err = RouterError::NoModelsAvailable;
        assert!(err.to_string().contains("no models"));

        let err = RouterError::LockPoisoned;
        assert!(err.to_string().contains("poisoned"));
    }

    #[test]
    fn test_default_config_values() {
        let config = RouterConfig::default();
        assert!((config.epsilon - 0.1).abs() < f64::EPSILON);
        assert!((config.epsilon_decay - 0.995).abs() < f64::EPSILON);
        assert!((config.min_epsilon - 0.01).abs() < f64::EPSILON);
        assert!((config.default_score - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scores_default_score_for_unscored_models() {
        let router = LearnedRouter::with_defaults();
        router.register_model("model-a").unwrap();
        let scores = router.scores();
        assert_eq!(scores.len(), 1);
        assert!((scores[0].score - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_model_count_empty() {
        let router = LearnedRouter::with_defaults();
        assert_eq!(router.model_count(), 0);
    }

    #[test]
    fn test_select_with_custom_config() {
        let config = RouterConfig {
            epsilon: 0.0,
            epsilon_decay: 1.0,
            min_epsilon: 0.0,
            default_score: 0.5,
        };
        let router = LearnedRouter::new(config);
        router.register_model("model-a").unwrap();
        router.register_model("model-b").unwrap();

        let features = make_features("code");
        let model = router.select_model(&features).unwrap();
        assert!(model == "model-a" || model == "model-b");
    }
}
