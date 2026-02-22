#![cfg(feature = "intelligence")]
//! # IntelligenceBridge — Closed-Loop Self-Improvement Coordinator
//!
//! ## Responsibility
//!
//! Wire all intelligence subsystems into a single coordinator so they
//! actually influence pipeline behaviour. Without this bridge, every module
//! collects data in isolation but nothing acts on it.
//!
//! ## Data Flow
//!
//! ```text
//! Request arrives
//!   └─ notify_request(rps) ──────────────────────► Autoscaler.record_rps()
//!
//! Routing decision needed
//!   └─ advise_model(prompt, category) ──────────► LearnedRouter.select_model()
//!                                                  (epsilon-greedy bandit)
//!
//! Inference completes
//!   └─ notify_completion(model, cat, prompt,
//!                        response, latency, ok)
//!       ├─ QualityEstimator.estimate() ──────────► quality score (0.0–1.0)
//!       ├─ FeedbackCollector.record() ───────────► bounded feedback history
//!       └─ LearnedRouter.record_outcome() ──────► bandit arm update
//!
//! Autoscale query
//!   └─ autoscale_recommendation() ─────────────► Autoscaler.forecast()
//! ```
//!
//! ## Guarantees
//!
//! - **Thread-safe**: all state via `Arc<_>` — clone is cheap and correct.
//! - **Non-panicking**: all fallible calls return `Result` or `Option`.
//! - **Zero-cost when absent**: the bridge is `Option<Arc<IntelligenceBridge>>`
//!   in callers; no overhead when the `intelligence` feature is disabled.
//! - **Bounded**: feedback history and autoscaler history are capped by their
//!   inner configurations.
//!
//! ## NOT Responsible For
//!
//! - Making actual HTTP calls to model backends (that is `worker`)
//! - Persisting learned state across restarts (future Redis integration)
//! - Multi-node consensus (that is `evolution::brain`)

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::intelligence::{
    autoscale::{AutoscaleRecommendation, Autoscaler, AutoscalerConfig},
    feedback::{FeedbackCollector, FeedbackEntry, FeedbackSource},
    quality::{QualityEstimator, QualityEstimatorConfig},
    router::{LearnedRouter, RequestFeatures, RouterConfig},
};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Configuration for the intelligence bridge.
///
/// Collects all sub-system config in one place.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Configuration for the epsilon-greedy learned router.
    pub router: RouterConfig,
    /// Configuration for the quality estimator.
    pub quality: QualityEstimatorConfig,
    /// Maximum feedback entries to retain.
    pub max_feedback_entries: usize,
    /// Autoscaler history capacity (number of RPS data-points to keep).
    pub autoscaler_history_capacity: usize,
    /// Minimum history points before autoscaler will produce a recommendation.
    pub autoscaler_min_history: usize,
}

impl Default for BridgeConfig {
    /// Sensible production defaults.
    ///
    /// # Panics
    ///
    /// This function never panics.
    fn default() -> Self {
        Self {
            router: RouterConfig::default(),
            quality: QualityEstimatorConfig::default(),
            max_feedback_entries: 10_000,
            autoscaler_history_capacity: 720, // 1h at 5-second samples
            autoscaler_min_history: 12,
        }
    }
}

/// Closed-loop intelligence coordinator.
///
/// Holds all intelligence subsystems and exposes a unified API that the
/// pipeline stages call. Cheap to clone — all internal state is `Arc`-wrapped.
#[derive(Debug, Clone)]
pub struct IntelligenceBridge {
    /// Epsilon-greedy bandit that learns which model wins per request category.
    pub learned_router: Arc<LearnedRouter>,
    /// Bounded feedback collector fed by quality estimates and error signals.
    pub feedback: Arc<FeedbackCollector>,
    /// Heuristic quality estimator: coherence, completeness, confidence.
    pub quality: Arc<QualityEstimator>,
    /// Predictive load forecaster using exponential smoothing.
    pub autoscaler: Arc<Autoscaler>,
}

impl IntelligenceBridge {
    /// Create a new bridge with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `config` — Combined configuration for all subsystems.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(config: BridgeConfig) -> Self {
        let autoscale_cfg = AutoscalerConfig {
            min_history_points: config.autoscaler_min_history,
            ..AutoscalerConfig::default()
        };
        Self {
            learned_router: Arc::new(LearnedRouter::new(config.router)),
            feedback: Arc::new(FeedbackCollector::new(config.max_feedback_entries)),
            quality: Arc::new(QualityEstimator::new(config.quality)),
            autoscaler: Arc::new(Autoscaler::new(
                autoscale_cfg,
                config.autoscaler_history_capacity,
            )),
        }
    }

    /// Create a bridge with default configuration.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn with_defaults() -> Self {
        Self::new(BridgeConfig::default())
    }

    /// Register a model name with the learned router.
    ///
    /// Must be called for every model that can be selected. Duplicate
    /// registrations are idempotent.
    ///
    /// # Arguments
    ///
    /// * `model` — Model identifier (e.g. `"claude-3-sonnet"`, `"llama-3-8b"`).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn register_model(&self, model: &str) {
        // Log but don't propagate lock-poison errors — the bridge degrades
        // gracefully to pass-through when the router is unavailable.
        if let Err(e) = self.learned_router.register_model(model) {
            tracing::warn!(model, error = %e, "bridge: register_model failed");
        }
    }

    /// Notify the bridge that a new request has arrived.
    ///
    /// Records the current RPS on the autoscaler so load forecasting stays
    /// up-to-date.
    ///
    /// # Arguments
    ///
    /// * `rps` — Current observed requests-per-second (may be approximate).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn notify_request(&self, rps: f64) {
        self.autoscaler.record_rps(rps);
    }

    /// Ask the learned router which model to use for a request.
    ///
    /// Returns `None` if no models are registered or the router lock is
    /// poisoned — callers should fall back to their default selection.
    ///
    /// # Arguments
    ///
    /// * `prompt` — The prompt text (used only for token-estimate heuristic).
    /// * `category` — Logical category (e.g. `"code"`, `"chat"`, `"local"`,
    ///   `"cloud"`).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn advise_model(&self, prompt: &str, category: &str) -> Option<String> {
        let features = RequestFeatures {
            category: category.to_string(),
            estimated_complexity: estimate_complexity(prompt),
            token_estimate: estimate_tokens(prompt),
        };
        self.learned_router.select_model(&features).ok()
    }

    /// Record the outcome of a completed inference call.
    ///
    /// This is the primary feedback path: it estimates response quality,
    /// injects a `FeedbackEntry`, and immediately updates the learned
    /// router's bandit arm so the next selection can exploit the result.
    ///
    /// # Arguments
    ///
    /// * `model` — The model that served this request.
    /// * `category` — The routing category used during selection.
    /// * `prompt` — The assembled prompt text (used for quality estimation).
    /// * `response` — The model's response text.
    /// * `latency_ms` — Observed end-to-end inference latency.
    /// * `success` — Whether the inference call succeeded without error.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn notify_completion(
        &self,
        model: &str,
        category: &str,
        prompt: &str,
        response: &str,
        latency_ms: f64,
        success: bool,
    ) {
        // 1. Quality estimate → feedback score
        let quality_score = if success && !response.is_empty() {
            self.quality
                .estimate(prompt, response, model)
                .ok()
                .map(|e| e.overall_score)
        } else {
            Some(0.0)
        };

        // 2. Inject into feedback collector
        let score = quality_score.unwrap_or(if success { 0.7 } else { 0.0 });
        let entry = FeedbackEntry {
            id: format!("bridge-{}", now_ms()),
            request_id: format!("{}-{}", model, now_ms()),
            source: FeedbackSource::AutoQuality,
            score,
            raw_value: latency_ms,
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("model".to_string(), model.to_string());
                m.insert("category".to_string(), category.to_string());
                m
            },
            timestamp_secs: now_ms() / 1000,
        };
        if let Err(e) = self.feedback.record(entry) {
            tracing::warn!(error = %e, "bridge: feedback record failed");
        }

        // 3. Update LearnedRouter bandit arm directly
        if let Err(e) = self
            .learned_router
            .record_outcome(model, category, success, latency_ms)
        {
            tracing::debug!(model, error = %e, "bridge: router outcome skipped (model not registered)");
        }

        // 4. Emit latency feedback for error signals
        if !success {
            let error_entry = FeedbackEntry {
                id: format!("bridge-err-{}", now_ms()),
                request_id: format!("{}-err-{}", model, now_ms()),
                source: FeedbackSource::ErrorReport,
                score: 0.0,
                raw_value: latency_ms,
                metadata: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("model".to_string(), model.to_string());
                    m
                },
                timestamp_secs: now_ms() / 1000,
            };
            if let Err(e) = self.feedback.record(error_entry) {
                tracing::warn!(error = %e, "bridge: error feedback record failed");
            }
        }
    }

    /// Return the latest autoscale recommendation, or `None` if there is
    /// insufficient history.
    ///
    /// Callers can use this to proactively adjust channel buffer sizes or
    /// spawn additional workers before a traffic spike hits.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn autoscale_recommendation(&self) -> Option<AutoscaleRecommendation> {
        self.autoscaler.forecast().ok()
    }

    /// Return the current average score across all recent feedback.
    ///
    /// Useful as a high-level "health of the system" metric for dashboards.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn average_feedback_score(&self) -> f64 {
        self.feedback.average_score()
    }

    /// Return the best-known model for a given category, or `None` if no
    /// data exists yet.
    ///
    /// Unlike `advise_model`, this never explores — it always returns the
    /// current exploitation choice. Useful for monitoring dashboards.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn best_model_for_category(&self, category: &str) -> Option<String> {
        self.learned_router.best_model(category)
    }

    /// Return total count of feedback entries recorded since startup.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn feedback_count(&self) -> usize {
        self.feedback.entry_count()
    }
}

/// Heuristic token estimate: word count × 1.3 (average sub-word tokens).
fn estimate_tokens(prompt: &str) -> u64 {
    let words = prompt.split_whitespace().count();
    ((words as f64) * 1.3) as u64
}

/// Heuristic complexity on 0.0–1.0 scale based on structural signals.
fn estimate_complexity(prompt: &str) -> f64 {
    let len_score = (prompt.len() as f64 / 2000.0).min(1.0) * 0.3;
    let code_score = if prompt.contains("```") { 0.3 } else { 0.0 };
    let question_score = if prompt.contains('?') { 0.1 } else { 0.0 };
    let list_score = if prompt.contains("\n1.") || prompt.contains("\n- ") {
        0.2
    } else {
        0.0
    };
    (len_score + code_score + question_score + list_score).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bridge() -> IntelligenceBridge {
        IntelligenceBridge::with_defaults()
    }

    // ── construction ────────────────────────────────────────────────────

    #[test]
    fn test_bridge_with_defaults_constructs() {
        let bridge = make_bridge();
        assert_eq!(bridge.feedback_count(), 0);
    }

    #[test]
    fn test_bridge_new_with_config() {
        let config = BridgeConfig {
            max_feedback_entries: 50,
            ..BridgeConfig::default()
        };
        let bridge = IntelligenceBridge::new(config);
        assert_eq!(bridge.feedback_count(), 0);
    }

    #[test]
    fn test_bridge_clone_shares_state() {
        let bridge = make_bridge();
        let clone = bridge.clone();
        bridge.register_model("gpt-4");
        // clone shares the same Arc so model count is shared
        assert_eq!(clone.learned_router.model_count(), 1);
    }

    // ── register_model ───────────────────────────────────────────────────

    #[test]
    fn test_register_model_once() {
        let bridge = make_bridge();
        bridge.register_model("model-a");
        assert_eq!(bridge.learned_router.model_count(), 1);
    }

    #[test]
    fn test_register_model_idempotent() {
        let bridge = make_bridge();
        bridge.register_model("model-a");
        bridge.register_model("model-a");
        assert_eq!(bridge.learned_router.model_count(), 1);
    }

    #[test]
    fn test_register_multiple_models() {
        let bridge = make_bridge();
        bridge.register_model("model-a");
        bridge.register_model("model-b");
        bridge.register_model("model-c");
        assert_eq!(bridge.learned_router.model_count(), 3);
    }

    // ── notify_request ───────────────────────────────────────────────────

    #[test]
    fn test_notify_request_records_rps() {
        let bridge = make_bridge();
        bridge.notify_request(10.0);
        assert!((bridge.autoscaler.current_avg_rps() - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_notify_request_accumulates_history() {
        let bridge = make_bridge();
        for i in 1..=5 {
            bridge.notify_request(i as f64 * 10.0);
        }
        // Average of 10+20+30+40+50 = 150/5 = 30
        assert!((bridge.autoscaler.current_avg_rps() - 30.0).abs() < 0.001);
    }

    #[test]
    fn test_notify_request_zero_rps_is_valid() {
        let bridge = make_bridge();
        bridge.notify_request(0.0);
        assert_eq!(bridge.autoscaler.current_avg_rps(), 0.0);
    }

    // ── advise_model ─────────────────────────────────────────────────────

    #[test]
    fn test_advise_model_no_models_returns_none() {
        let bridge = make_bridge();
        let advice = bridge.advise_model("hello", "chat");
        assert!(advice.is_none());
    }

    #[test]
    fn test_advise_model_single_model_always_returns_it() {
        let bridge = make_bridge();
        bridge.register_model("only-model");
        let advice = bridge.advise_model("hello world", "chat");
        assert_eq!(advice, Some("only-model".to_string()));
    }

    #[test]
    fn test_advise_model_returns_registered_model() {
        let bridge = make_bridge();
        bridge.register_model("model-a");
        bridge.register_model("model-b");
        let advice = bridge.advise_model("write some code please", "code");
        assert!(advice.is_some());
        let m = advice.unwrap();
        assert!(m == "model-a" || m == "model-b");
    }

    #[test]
    fn test_advise_model_after_training_prefers_winner() {
        let bridge = make_bridge();
        bridge.register_model("good-model");
        bridge.register_model("bad-model");

        // Train: good-model has all successes, bad-model has all failures
        for _ in 0..30 {
            bridge.notify_completion(
                "good-model",
                "chat",
                "test prompt",
                "A good long response here.",
                100.0,
                true,
            );
            bridge.notify_completion("bad-model", "chat", "test prompt", "", 500.0, false);
        }

        // With epsilon=0.1, majority of selections should be good-model
        let mut good_count = 0;
        for _ in 0..100 {
            if bridge.advise_model("test", "chat").as_deref() == Some("good-model") {
                good_count += 1;
            }
        }
        // At least 50% exploitation of the best model (epsilon=0.1 means 90% exploit)
        assert!(
            good_count > 50,
            "good-model should win majority: {good_count}/100"
        );
    }

    // ── notify_completion ────────────────────────────────────────────────

    #[test]
    fn test_notify_completion_success_increments_feedback() {
        let bridge = make_bridge();
        bridge.register_model("gpt-4");
        bridge.notify_completion(
            "gpt-4",
            "chat",
            "what is rust?",
            "Rust is a systems programming language.",
            200.0,
            true,
        );
        // One AutoQuality entry
        assert!(bridge.feedback_count() >= 1);
    }

    #[test]
    fn test_notify_completion_failure_records_error_entry() {
        let bridge = make_bridge();
        bridge.register_model("gpt-4");
        bridge.notify_completion("gpt-4", "chat", "query", "", 500.0, false);
        // Both AutoQuality (score=0) + ErrorReport entries
        assert!(bridge.feedback_count() >= 2);
    }

    #[test]
    fn test_notify_completion_success_score_positive() {
        let bridge = make_bridge();
        bridge.register_model("model-a");
        bridge.notify_completion(
            "model-a",
            "chat",
            "how does X work?",
            "X works by doing this carefully.",
            100.0,
            true,
        );
        let avg = bridge.average_feedback_score();
        assert!(avg > 0.0);
    }

    #[test]
    fn test_notify_completion_failure_score_zero() {
        let bridge = make_bridge();
        bridge.register_model("model-a");
        // Empty response on failure → score 0.0
        bridge.notify_completion("model-a", "chat", "query", "", 100.0, false);
        let entries = bridge.feedback.recent(10);
        let auto_quality: Vec<_> = entries
            .iter()
            .filter(|e| e.source == crate::intelligence::feedback::FeedbackSource::AutoQuality)
            .collect();
        assert!(!auto_quality.is_empty());
        assert!((auto_quality[0].score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_notify_completion_unregistered_model_does_not_panic() {
        let bridge = make_bridge();
        // Model not registered — should degrade gracefully
        bridge.notify_completion(
            "unknown-model",
            "chat",
            "test",
            "response text",
            100.0,
            true,
        );
        // Should still record feedback
        assert!(bridge.feedback_count() >= 1);
    }

    #[test]
    fn test_notify_completion_updates_router_after_multiple_calls() {
        let bridge = make_bridge();
        bridge.register_model("fast-model");

        for _ in 0..10 {
            bridge.notify_completion(
                "fast-model",
                "code",
                "write a function",
                "fn foo() { }",
                50.0,
                true,
            );
        }

        let scores = bridge.learned_router.scores_for_category("code");
        assert!(!scores.is_empty());
        let fast_score = scores.iter().find(|s| s.model == "fast-model").unwrap();
        assert!(fast_score.successes >= 10);
    }

    // ── autoscale_recommendation ─────────────────────────────────────────

    #[test]
    fn test_autoscale_recommendation_none_without_history() {
        let bridge = make_bridge();
        assert!(bridge.autoscale_recommendation().is_none());
    }

    #[test]
    fn test_autoscale_recommendation_present_with_sufficient_history() {
        let config = BridgeConfig {
            autoscaler_min_history: 3,
            autoscaler_history_capacity: 100,
            ..BridgeConfig::default()
        };
        let bridge = IntelligenceBridge::new(config);
        for _ in 0..6 {
            bridge.notify_request(20.0);
        }
        let rec = bridge.autoscale_recommendation();
        assert!(rec.is_some());
    }

    #[test]
    fn test_autoscale_recommendation_confidence_in_range() {
        let config = BridgeConfig {
            autoscaler_min_history: 3,
            autoscaler_history_capacity: 100,
            ..BridgeConfig::default()
        };
        let bridge = IntelligenceBridge::new(config);
        for _ in 0..10 {
            bridge.notify_request(15.0);
        }
        let rec = bridge.autoscale_recommendation().unwrap();
        assert!(rec.confidence >= 0.0 && rec.confidence <= 1.0);
    }

    // ── best_model_for_category ──────────────────────────────────────────

    #[test]
    fn test_best_model_unknown_category_returns_none() {
        let bridge = make_bridge();
        bridge.register_model("model-a");
        assert!(bridge.best_model_for_category("unknown").is_none());
    }

    #[test]
    fn test_best_model_after_training() {
        let bridge = make_bridge();
        bridge.register_model("winner");
        bridge.register_model("loser");

        for _ in 0..20 {
            bridge.notify_completion("winner", "analysis", "q", "good long answer.", 100.0, true);
            bridge.notify_completion("loser", "analysis", "q", "", 600.0, false);
        }

        let best = bridge.best_model_for_category("analysis");
        assert_eq!(best, Some("winner".to_string()));
    }

    // ── average_feedback_score ───────────────────────────────────────────

    #[test]
    fn test_average_feedback_score_empty_is_zero() {
        let bridge = make_bridge();
        assert_eq!(bridge.average_feedback_score(), 0.0);
    }

    #[test]
    fn test_average_feedback_score_reflects_outcomes() {
        let bridge = make_bridge();
        bridge.register_model("m");

        bridge.notify_completion("m", "chat", "q", "a good response here.", 100.0, true);
        let avg = bridge.average_feedback_score();
        assert!(avg > 0.0);
    }

    // ── estimate helpers ─────────────────────────────────────────────────

    #[test]
    fn test_estimate_tokens_empty_prompt() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_estimate_tokens_single_word() {
        // 1 word × 1.3 truncated = 1
        assert_eq!(estimate_tokens("hello"), 1);
    }

    #[test]
    fn test_estimate_tokens_multiple_words() {
        // 4 words × 1.3 = 5.2 → 5
        let t = estimate_tokens("one two three four");
        assert_eq!(t, 5);
    }

    #[test]
    fn test_estimate_complexity_empty_is_zero() {
        assert_eq!(estimate_complexity(""), 0.0);
    }

    #[test]
    fn test_estimate_complexity_code_block_increases_score() {
        let with_code = estimate_complexity("```\nfn foo() {}\n```");
        let without = estimate_complexity("foo");
        assert!(with_code > without);
    }

    #[test]
    fn test_estimate_complexity_capped_at_one() {
        let very_long = "a".repeat(10_000) + "```\n```\n? 1. ";
        let score = estimate_complexity(&very_long);
        assert!(score <= 1.0);
    }

    #[test]
    fn test_estimate_complexity_non_negative() {
        let score = estimate_complexity("simple prompt");
        assert!(score >= 0.0);
    }

    // ── feedback_count ───────────────────────────────────────────────────

    #[test]
    fn test_feedback_count_zero_initially() {
        let bridge = make_bridge();
        assert_eq!(bridge.feedback_count(), 0);
    }

    #[test]
    fn test_feedback_count_increases_on_completions() {
        let bridge = make_bridge();
        bridge.register_model("m");

        bridge.notify_completion("m", "chat", "q", "answer here", 100.0, true);
        bridge.notify_completion("m", "chat", "q", "another answer", 200.0, true);

        assert!(bridge.feedback_count() >= 2);
    }

    #[test]
    fn test_feedback_count_cap_respected() {
        let config = BridgeConfig {
            max_feedback_entries: 5,
            ..BridgeConfig::default()
        };
        let bridge = IntelligenceBridge::new(config);
        bridge.register_model("m");

        // Each success injects 1 entry; failure injects 2 (quality + error)
        for _ in 0..10 {
            bridge.notify_completion("m", "chat", "q", "response", 100.0, true);
        }

        // Capped at max_feedback_entries
        assert!(bridge.feedback_count() <= 5);
    }
}
