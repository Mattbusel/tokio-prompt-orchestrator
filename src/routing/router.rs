//! Model routing logic.
//!
//! The [`ModelRouter`] combines a [`ComplexityScorer`](super::ComplexityScorer)
//! with a [`RoutingConfig`](super::RoutingConfig) and a
//! [`CostTracker`](super::CostTracker) to decide which worker backend should
//! serve each prompt and to adaptively adjust routing thresholds based on
//! observed outcomes.

use crate::config::WorkerKind;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use super::config::RoutingConfig;
use super::cost_tracker::CostTracker;
use super::scorer::ComplexityScorer;

/// The routing decision for a single prompt.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, PartialEq)]
pub enum RoutingDecision {
    /// Route to the local model (complexity below `local_threshold`).
    Local {
        /// The complexity score that drove this decision.
        score: f64,
        /// The worker kind selected.
        worker: WorkerKind,
    },
    /// Route to the local model first, fall back to cloud on failure
    /// (complexity between `local_threshold` and `cloud_threshold`).
    LocalWithFallback {
        /// The complexity score that drove this decision.
        score: f64,
        /// Primary worker (local).
        primary: WorkerKind,
        /// Fallback worker (cloud).
        fallback: WorkerKind,
    },
    /// Route directly to the cloud model (complexity above `cloud_threshold`).
    Cloud {
        /// The complexity score that drove this decision.
        score: f64,
        /// The worker kind selected.
        worker: WorkerKind,
    },
}

impl RoutingDecision {
    /// Return the complexity score associated with this decision.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn score(&self) -> f64 {
        match self {
            Self::Local { score, .. }
            | Self::LocalWithFallback { score, .. }
            | Self::Cloud { score, .. } => *score,
        }
    }

    /// Return `true` if the decision routes to the local worker exclusively.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    /// Return `true` if the decision routes to the cloud worker exclusively.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn is_cloud(&self) -> bool {
        matches!(self, Self::Cloud { .. })
    }

    /// Return `true` if the decision uses local-with-fallback routing.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn is_fallback(&self) -> bool {
        matches!(self, Self::LocalWithFallback { .. })
    }
}

/// Intelligent model router.
///
/// Combines prompt complexity scoring, threshold-based routing, adaptive
/// threshold adjustment, and cost tracking into a single entry point.
///
/// Thread-safe: all mutable state uses atomics or interior `RwLock`.
///
/// # Panics
///
/// This type and its methods never panic.
pub struct ModelRouter {
    scorer: ComplexityScorer,
    config: RoutingConfig,
    cost_tracker: CostTracker,

    /// Effective cloud threshold — may diverge from config when adaptive
    /// adjustment is active.
    effective_cloud_threshold: RwLock<f64>,

    // Adaptive tracking: fallback-zone outcomes.
    fallback_successes: AtomicU64,
    fallback_failures: AtomicU64,
}

impl std::fmt::Debug for ModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRouter")
            .field("config", &self.config)
            .field("effective_cloud_threshold", &self.effective_cloud_threshold)
            .finish()
    }
}

impl ModelRouter {
    /// Create a new router with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `config` — Routing thresholds, cost rates, and adaptive settings.
    ///
    /// # Returns
    ///
    /// A new [`ModelRouter`] ready to route prompts.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(config: RoutingConfig) -> Self {
        let cost_tracker = CostTracker::new(
            config.local_cost_per_1k_tokens,
            config.cloud_cost_per_1k_tokens,
        );
        let effective_cloud_threshold = RwLock::new(config.cloud_threshold);

        Self {
            scorer: ComplexityScorer::new(),
            config,
            cost_tracker,
            effective_cloud_threshold,
            fallback_successes: AtomicU64::new(0),
            fallback_failures: AtomicU64::new(0),
        }
    }

    /// Route a prompt to the appropriate worker.
    ///
    /// # Arguments
    ///
    /// * `prompt` — The raw prompt text to analyse and route.
    ///
    /// # Returns
    ///
    /// A [`RoutingDecision`] indicating which worker(s) to use.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn route(&self, prompt: &str) -> RoutingDecision {
        let score = self.scorer.score(prompt);

        let effective_threshold = self
            .effective_cloud_threshold
            .read()
            .map(|g| *g)
            .unwrap_or(self.config.cloud_threshold);

        if score < self.config.local_threshold {
            RoutingDecision::Local {
                score,
                worker: WorkerKind::LlamaCpp,
            }
        } else if score > effective_threshold {
            RoutingDecision::Cloud {
                score,
                worker: WorkerKind::Anthropic,
            }
        } else {
            RoutingDecision::LocalWithFallback {
                score,
                primary: WorkerKind::LlamaCpp,
                fallback: WorkerKind::Anthropic,
            }
        }
    }

    /// Record the outcome of a routing decision.
    ///
    /// For [`RoutingDecision::LocalWithFallback`] outcomes, this feeds the
    /// adaptive threshold algorithm. When enough samples accumulate and the
    /// local failure rate exceeds `adaptive.failure_ceiling`, the effective
    /// cloud threshold is lowered (sending more requests directly to cloud).
    ///
    /// # Arguments
    ///
    /// * `decision` — The routing decision that was made.
    /// * `success` — Whether the primary worker succeeded.
    /// * `tokens` — Number of tokens processed (for cost tracking).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn record_outcome(&self, decision: &RoutingDecision, success: bool, tokens: u64) {
        match decision {
            RoutingDecision::Local { .. } => {
                self.cost_tracker.record_local(tokens);
            }
            RoutingDecision::Cloud { .. } => {
                self.cost_tracker.record_cloud(tokens);
            }
            RoutingDecision::LocalWithFallback { .. } => {
                if success {
                    self.fallback_successes.fetch_add(1, Ordering::Relaxed);
                    self.cost_tracker.record_local(tokens);
                } else {
                    self.fallback_failures.fetch_add(1, Ordering::Relaxed);
                    self.cost_tracker.record_fallback(tokens);
                }
                self.maybe_adapt();
            }
        }
    }

    /// Return a reference to the underlying cost tracker.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn cost_tracker(&self) -> &CostTracker {
        &self.cost_tracker
    }

    /// Return the current effective cloud threshold.
    ///
    /// This may differ from `config.cloud_threshold` if adaptive adjustment
    /// has been triggered.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn effective_cloud_threshold(&self) -> f64 {
        self.effective_cloud_threshold
            .read()
            .map(|g| *g)
            .unwrap_or(self.config.cloud_threshold)
    }

    /// Return a reference to the scorer for external breakdown queries.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn scorer(&self) -> &ComplexityScorer {
        &self.scorer
    }

    // ── Adaptive threshold adjustment ──────────────────────────────────

    /// Check whether the adaptive threshold should be adjusted.
    fn maybe_adapt(&self) {
        if !self.config.adaptive.enabled {
            return;
        }

        let successes = self.fallback_successes.load(Ordering::Relaxed);
        let failures = self.fallback_failures.load(Ordering::Relaxed);
        let total = successes + failures;

        if total < self.config.adaptive.min_samples {
            return;
        }

        let failure_rate = failures as f64 / total as f64;

        if let Ok(mut threshold) = self.effective_cloud_threshold.write() {
            if failure_rate > self.config.adaptive.failure_ceiling {
                // Too many local failures in fallback zone → lower threshold
                // so more requests go directly to cloud.
                let new = (*threshold - self.config.adaptive.step).max(self.config.local_threshold);
                *threshold = new;
            } else if failure_rate < self.config.adaptive.failure_ceiling / 2.0 {
                // Local is doing well → raise threshold (save more money).
                let new = (*threshold + self.config.adaptive.step).min(1.0);
                *threshold = new;
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_router() -> ModelRouter {
        ModelRouter::new(RoutingConfig::default())
    }

    // -- routing decisions -----------------------------------------------

    #[test]
    fn test_route_simple_greeting_returns_local() {
        let router = default_router();
        let decision = router.route("Say hello");
        assert!(
            decision.is_local(),
            "simple greeting should route local, got: {decision:?}"
        );
        assert_eq!(
            match &decision {
                RoutingDecision::Local { worker, .. } => worker,
                _ => &WorkerKind::Echo,
            },
            &WorkerKind::LlamaCpp
        );
    }

    #[test]
    fn test_route_complex_rust_prompt_returns_cloud() {
        let router = default_router();
        let prompt = r#"Debug this Rust code that has a borrow checker error with lifetime issues:

```rust
fn process<'a>(data: &'a [u8]) -> &'a str {
    let owned = String::from_utf8_lossy(data).to_string();
    &owned
}
```

1. Explain why the borrow checker rejects this
2. Show the correct implementation with proper lifetime annotations
3. Add unit tests for the fix

Also, that function should handle it properly when those bytes are invalid UTF-8. Fix the thing so it works with tokio async fn properly."#;
        let decision = router.route(prompt);
        assert!(
            decision.is_cloud(),
            "complex Rust debugging should route to cloud, got: {decision:?}"
        );
        assert_eq!(
            match &decision {
                RoutingDecision::Cloud { worker, .. } => worker,
                _ => &WorkerKind::Echo,
            },
            &WorkerKind::Anthropic
        );
    }

    #[test]
    fn test_route_medium_complexity_returns_fallback() {
        let router = default_router();
        // Code block alone gives 0.2, numbered list gives 0.2 → 0.4 (boundary)
        let prompt = "Fix this:\n```\nfoo()\n```\n1. Step one\n2. Step two";
        let decision = router.route(prompt);
        // Score = 0.4, which is >= local_threshold(0.4) and <= cloud_threshold(0.7)
        assert!(
            decision.is_fallback(),
            "medium complexity should route to fallback, got: {decision:?}"
        );
    }

    #[test]
    fn test_route_empty_prompt_returns_local() {
        let router = default_router();
        let decision = router.route("");
        assert!(decision.is_local());
    }

    // -- score accessor --------------------------------------------------

    #[test]
    fn test_routing_decision_score_accessor() {
        let router = default_router();
        let decision = router.route("Hello");
        assert!(decision.score() >= 0.0 && decision.score() <= 1.0);
    }

    // -- outcome recording -----------------------------------------------

    #[test]
    fn test_record_outcome_local_success_tracks_local_cost() {
        let router = default_router();
        let decision = RoutingDecision::Local {
            score: 0.1,
            worker: WorkerKind::LlamaCpp,
        };
        router.record_outcome(&decision, true, 100);
        let snap = router.cost_tracker().snapshot();
        assert_eq!(snap.local_tokens, 100);
        assert_eq!(snap.local_requests, 1);
    }

    #[test]
    fn test_record_outcome_cloud_tracks_cloud_cost() {
        let router = default_router();
        let decision = RoutingDecision::Cloud {
            score: 0.9,
            worker: WorkerKind::Anthropic,
        };
        router.record_outcome(&decision, true, 500);
        let snap = router.cost_tracker().snapshot();
        assert_eq!(snap.cloud_tokens, 500);
        assert_eq!(snap.cloud_requests, 1);
    }

    #[test]
    fn test_record_outcome_fallback_success_tracks_local() {
        let router = default_router();
        let decision = RoutingDecision::LocalWithFallback {
            score: 0.5,
            primary: WorkerKind::LlamaCpp,
            fallback: WorkerKind::Anthropic,
        };
        router.record_outcome(&decision, true, 200);
        let snap = router.cost_tracker().snapshot();
        assert_eq!(snap.local_tokens, 200);
    }

    #[test]
    fn test_record_outcome_fallback_failure_tracks_cloud() {
        let router = default_router();
        let decision = RoutingDecision::LocalWithFallback {
            score: 0.5,
            primary: WorkerKind::LlamaCpp,
            fallback: WorkerKind::Anthropic,
        };
        router.record_outcome(&decision, false, 200);
        let snap = router.cost_tracker().snapshot();
        assert_eq!(snap.cloud_tokens, 200);
        assert_eq!(snap.fallback_requests, 1);
    }

    // -- adaptive threshold -----------------------------------------------

    #[test]
    fn test_adaptive_lowers_threshold_on_high_failure_rate() {
        let config = RoutingConfig {
            adaptive: super::super::config::AdaptiveConfig {
                enabled: true,
                step: 0.05,
                min_samples: 5,
                failure_ceiling: 0.3,
            },
            ..RoutingConfig::default()
        };
        let router = ModelRouter::new(config);
        let original_threshold = router.effective_cloud_threshold();

        let decision = RoutingDecision::LocalWithFallback {
            score: 0.5,
            primary: WorkerKind::LlamaCpp,
            fallback: WorkerKind::Anthropic,
        };

        // Record 5 failures out of 5 (100% failure rate, > 30% ceiling)
        for _ in 0..5 {
            router.record_outcome(&decision, false, 100);
        }

        let new_threshold = router.effective_cloud_threshold();
        assert!(
            new_threshold < original_threshold,
            "threshold should decrease: was {original_threshold}, now {new_threshold}"
        );
    }

    #[test]
    fn test_adaptive_raises_threshold_on_low_failure_rate() {
        let config = RoutingConfig {
            adaptive: super::super::config::AdaptiveConfig {
                enabled: true,
                step: 0.05,
                min_samples: 5,
                failure_ceiling: 0.3,
            },
            ..RoutingConfig::default()
        };
        let router = ModelRouter::new(config);
        let original_threshold = router.effective_cloud_threshold();

        let decision = RoutingDecision::LocalWithFallback {
            score: 0.5,
            primary: WorkerKind::LlamaCpp,
            fallback: WorkerKind::Anthropic,
        };

        // Record 5 successes out of 5 (0% failure rate, well below ceiling/2)
        for _ in 0..5 {
            router.record_outcome(&decision, true, 100);
        }

        let new_threshold = router.effective_cloud_threshold();
        assert!(
            new_threshold > original_threshold,
            "threshold should increase: was {original_threshold}, now {new_threshold}"
        );
    }

    #[test]
    fn test_adaptive_no_change_below_min_samples() {
        let config = RoutingConfig {
            adaptive: super::super::config::AdaptiveConfig {
                enabled: true,
                step: 0.05,
                min_samples: 100,
                failure_ceiling: 0.3,
            },
            ..RoutingConfig::default()
        };
        let router = ModelRouter::new(config);
        let original = router.effective_cloud_threshold();

        let decision = RoutingDecision::LocalWithFallback {
            score: 0.5,
            primary: WorkerKind::LlamaCpp,
            fallback: WorkerKind::Anthropic,
        };

        // Only 5 outcomes, below min_samples of 100
        for _ in 0..5 {
            router.record_outcome(&decision, false, 100);
        }

        assert!(
            (router.effective_cloud_threshold() - original).abs() < f64::EPSILON,
            "threshold should not change with insufficient samples"
        );
    }

    #[test]
    fn test_adaptive_disabled_no_threshold_change() {
        let config = RoutingConfig {
            adaptive: super::super::config::AdaptiveConfig {
                enabled: false,
                step: 0.05,
                min_samples: 5,
                failure_ceiling: 0.3,
            },
            ..RoutingConfig::default()
        };
        let router = ModelRouter::new(config);
        let original = router.effective_cloud_threshold();

        let decision = RoutingDecision::LocalWithFallback {
            score: 0.5,
            primary: WorkerKind::LlamaCpp,
            fallback: WorkerKind::Anthropic,
        };

        for _ in 0..10 {
            router.record_outcome(&decision, false, 100);
        }

        assert!(
            (router.effective_cloud_threshold() - original).abs() < f64::EPSILON,
            "threshold should not change when adaptive is disabled"
        );
    }

    #[test]
    fn test_adaptive_threshold_never_below_local_threshold() {
        let config = RoutingConfig {
            local_threshold: 0.4,
            cloud_threshold: 0.45,
            adaptive: super::super::config::AdaptiveConfig {
                enabled: true,
                step: 0.1,
                min_samples: 5,
                failure_ceiling: 0.3,
            },
            ..RoutingConfig::default()
        };
        let router = ModelRouter::new(config);

        let decision = RoutingDecision::LocalWithFallback {
            score: 0.5,
            primary: WorkerKind::LlamaCpp,
            fallback: WorkerKind::Anthropic,
        };

        // Drive many failures to push threshold down
        for _ in 0..50 {
            router.record_outcome(&decision, false, 100);
        }

        assert!(
            router.effective_cloud_threshold() >= 0.4,
            "threshold must not drop below local_threshold"
        );
    }

    #[test]
    fn test_adaptive_threshold_never_above_1_0() {
        let config = RoutingConfig {
            cloud_threshold: 0.95,
            adaptive: super::super::config::AdaptiveConfig {
                enabled: true,
                step: 0.1,
                min_samples: 5,
                failure_ceiling: 0.3,
            },
            ..RoutingConfig::default()
        };
        let router = ModelRouter::new(config);

        let decision = RoutingDecision::LocalWithFallback {
            score: 0.5,
            primary: WorkerKind::LlamaCpp,
            fallback: WorkerKind::Anthropic,
        };

        // Drive many successes to push threshold up
        for _ in 0..50 {
            router.record_outcome(&decision, true, 100);
        }

        assert!(
            router.effective_cloud_threshold() <= 1.0,
            "threshold must not exceed 1.0"
        );
    }

    // -- custom thresholds -----------------------------------------------

    #[test]
    fn test_route_with_custom_thresholds() {
        let config = RoutingConfig {
            local_threshold: 0.2,
            cloud_threshold: 0.5,
            ..RoutingConfig::default()
        };
        let router = ModelRouter::new(config);

        // Code block alone = 0.2, which is >= 0.2 local threshold
        let prompt = "```\ncode\n```";
        let decision = router.route(prompt);
        assert!(
            decision.is_fallback() || decision.is_cloud(),
            "score 0.2 with local_threshold 0.2 should not be Local"
        );
    }

    // -- RoutingDecision predicates --------------------------------------

    #[test]
    fn test_routing_decision_is_local() {
        let d = RoutingDecision::Local {
            score: 0.1,
            worker: WorkerKind::LlamaCpp,
        };
        assert!(d.is_local());
        assert!(!d.is_cloud());
        assert!(!d.is_fallback());
    }

    #[test]
    fn test_routing_decision_is_cloud() {
        let d = RoutingDecision::Cloud {
            score: 0.9,
            worker: WorkerKind::Anthropic,
        };
        assert!(d.is_cloud());
        assert!(!d.is_local());
        assert!(!d.is_fallback());
    }

    #[test]
    fn test_routing_decision_is_fallback() {
        let d = RoutingDecision::LocalWithFallback {
            score: 0.5,
            primary: WorkerKind::LlamaCpp,
            fallback: WorkerKind::Anthropic,
        };
        assert!(d.is_fallback());
        assert!(!d.is_local());
        assert!(!d.is_cloud());
    }

    // -- cost tracking integration ---------------------------------------

    #[test]
    fn test_cost_savings_after_routing_decisions() {
        let router = default_router();

        // Route 10 simple prompts locally
        for _ in 0..10 {
            let d = RoutingDecision::Local {
                score: 0.1,
                worker: WorkerKind::LlamaCpp,
            };
            router.record_outcome(&d, true, 500);
        }

        // Route 2 complex prompts to cloud
        for _ in 0..2 {
            let d = RoutingDecision::Cloud {
                score: 0.9,
                worker: WorkerKind::Anthropic,
            };
            router.record_outcome(&d, true, 500);
        }

        let snap = router.cost_tracker().snapshot();
        // 5000 local + 1000 cloud = 6000 total tokens
        assert_eq!(snap.local_tokens, 5000);
        assert_eq!(snap.cloud_tokens, 1000);
        // Savings should be positive (local is free)
        assert!(snap.savings_usd > 0.0);
        // Savings should be ~83.3% (10/12 of tokens were free)
        assert!(snap.savings_percent > 80.0);
    }

    // -- debug -----------------------------------------------------------

    #[test]
    fn test_model_router_debug_does_not_panic() {
        let router = default_router();
        let _ = format!("{router:?}");
    }
}
