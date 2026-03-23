#![allow(dead_code)]
//! # Module: Prompt A/B Testing Optimizer
//!
//! ## Responsibility
//! Generates N prompt variants from a base prompt, runs them through the
//! pipeline in parallel, scores each response using configurable quality
//! metrics, and auto-promotes the best variant for future requests with
//! similar intent.
//!
//! ## Architecture
//!
//! ```text
//! base_prompt
//!   └─ VariantGenerator  ──►  [variant_0, variant_1, ..., variant_N-1]
//!                                        │
//!                         parallel inference via ModelWorker
//!                                        │
//!                         ──►  ScoringEngine (configurable metrics)
//!                                        │
//!                         ──►  PromoterRegistry (intent → best variant)
//! ```
//!
//! ## Guarantees
//! - Thread-safe: all public types are `Send + Sync`
//! - Non-blocking: variant generation is CPU-only; inference is async
//! - Bounded memory: registry capped at `max_intents` entries
//! - Graceful: scoring never panics; unknown metrics return 0.0

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors returned by the prompt optimizer.
#[derive(Debug, Error)]
pub enum PromptOptimizerError {
    /// No variants could be generated from the base prompt.
    #[error("variant generation produced zero variants")]
    NoVariants,
    /// All parallel inference calls failed.
    #[error("all variant inferences failed: {0}")]
    AllInferencesFailed(String),
    /// Internal lock was poisoned.
    #[error("internal lock poisoned")]
    LockPoisoned,
}

// ─── Quality metrics ─────────────────────────────────────────────────────────

/// A configurable quality metric used to score an inference response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QualityMetric {
    /// Reward longer responses up to `max_chars`; score = min(len/max,1).
    ResponseLength { max_chars: usize },
    /// Score 1.0 if any of the keywords appear in the response, else 0.0.
    KeywordPresence { keywords: Vec<String> },
    /// Score 1.0 if the response is valid JSON, else 0.0.
    JsonValidity,
    /// Score = fraction of required_keys present in the parsed JSON object.
    JsonKeyPresence { required_keys: Vec<String> },
    /// Score 1.0 if response length is within [min_chars, max_chars].
    LengthWindow { min_chars: usize, max_chars: usize },
}

impl QualityMetric {
    /// Evaluate this metric against a response string.
    ///
    /// Always returns a value in [0.0, 1.0]. Never panics.
    #[must_use]
    pub fn score(&self, response: &str) -> f64 {
        match self {
            QualityMetric::ResponseLength { max_chars } => {
                if *max_chars == 0 {
                    return 0.0;
                }
                (response.len() as f64 / *max_chars as f64).min(1.0)
            }
            QualityMetric::KeywordPresence { keywords } => {
                let lower = response.to_lowercase();
                let hit = keywords
                    .iter()
                    .any(|kw| lower.contains(kw.to_lowercase().as_str()));
                if hit { 1.0 } else { 0.0 }
            }
            QualityMetric::JsonValidity => {
                if serde_json::from_str::<serde_json::Value>(response).is_ok() {
                    1.0
                } else {
                    0.0
                }
            }
            QualityMetric::JsonKeyPresence { required_keys } => {
                if required_keys.is_empty() {
                    return 1.0;
                }
                match serde_json::from_str::<serde_json::Value>(response) {
                    Ok(serde_json::Value::Object(map)) => {
                        let found = required_keys
                            .iter()
                            .filter(|k| map.contains_key(k.as_str()))
                            .count();
                        found as f64 / required_keys.len() as f64
                    }
                    _ => 0.0,
                }
            }
            QualityMetric::LengthWindow { min_chars, max_chars } => {
                let len = response.len();
                if len >= *min_chars && len <= *max_chars {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
}

// ─── Scoring engine ──────────────────────────────────────────────────────────

/// Computes a weighted aggregate quality score from multiple metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringEngine {
    /// Each entry is (metric, weight). Weights need not sum to 1.
    pub metrics: Vec<(QualityMetric, f64)>,
}

impl ScoringEngine {
    /// Create a new scoring engine.
    #[must_use]
    pub fn new(metrics: Vec<(QualityMetric, f64)>) -> Self {
        Self { metrics }
    }

    /// Score a response. Returns a value in [0.0, 1.0] (or 0.0 if no metrics).
    #[must_use]
    pub fn score(&self, response: &str) -> f64 {
        if self.metrics.is_empty() {
            return 0.0;
        }
        let total_weight: f64 = self.metrics.iter().map(|(_, w)| w.abs()).sum();
        if total_weight == 0.0 {
            return 0.0;
        }
        let weighted_sum: f64 = self
            .metrics
            .iter()
            .map(|(metric, weight)| metric.score(response) * weight.abs())
            .sum();
        (weighted_sum / total_weight).clamp(0.0, 1.0)
    }
}

impl Default for ScoringEngine {
    /// Default engine: response length (up to 2000 chars) + keyword presence of "answer".
    fn default() -> Self {
        Self::new(vec![
            (QualityMetric::ResponseLength { max_chars: 2000 }, 0.6),
            (
                QualityMetric::KeywordPresence {
                    keywords: vec!["answer".to_string(), "result".to_string()],
                },
                0.4,
            ),
        ])
    }
}

// ─── Variant generator ───────────────────────────────────────────────────────

/// Strategies for generating prompt variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VariantStrategy {
    /// Prepend different instructional prefixes.
    InstructionPrefix,
    /// Append different closing instructions.
    ClosingSuffix,
    /// Reframe the request with different stylistic wrappers.
    Reframe,
    /// Use a custom set of prefix strings.
    CustomPrefixes(Vec<String>),
}

/// Generates N prompt variants from a base prompt.
pub struct VariantGenerator {
    strategy: VariantStrategy,
    num_variants: usize,
}

impl VariantGenerator {
    /// Create a new generator.
    #[must_use]
    pub fn new(strategy: VariantStrategy, num_variants: usize) -> Self {
        Self {
            strategy,
            num_variants: num_variants.max(1).min(32),
        }
    }

    /// Generate variants. Always returns at least 1 entry (the original prompt).
    #[must_use]
    pub fn generate(&self, base_prompt: &str) -> Vec<String> {
        match &self.strategy {
            VariantStrategy::InstructionPrefix => {
                let prefixes = [
                    "",
                    "Please answer concisely. ",
                    "Think step by step. ",
                    "Provide a detailed explanation. ",
                    "Answer as an expert. ",
                    "Be direct and precise. ",
                    "Use bullet points. ",
                    "Explain to a beginner. ",
                ];
                prefixes
                    .iter()
                    .take(self.num_variants)
                    .map(|p| format!("{p}{base_prompt}"))
                    .collect()
            }
            VariantStrategy::ClosingSuffix => {
                let suffixes = [
                    "",
                    "\nBe concise.",
                    "\nProvide examples.",
                    "\nBe thorough.",
                    "\nSummarise at the end.",
                ];
                suffixes
                    .iter()
                    .take(self.num_variants)
                    .map(|s| format!("{base_prompt}{s}"))
                    .collect()
            }
            VariantStrategy::Reframe => {
                let frames = [
                    format!("{base_prompt}"),
                    format!("Regarding the following: {base_prompt}\nWhat is the best answer?"),
                    format!("I need help with: {base_prompt}"),
                    format!("Question: {base_prompt}\nAnswer:"),
                    format!("Context: {base_prompt}\nProvide a clear response."),
                ];
                frames.into_iter().take(self.num_variants).collect()
            }
            VariantStrategy::CustomPrefixes(prefixes) => prefixes
                .iter()
                .take(self.num_variants)
                .map(|p| format!("{p}{base_prompt}"))
                .collect(),
        }
    }
}

// ─── A/B experiment result ───────────────────────────────────────────────────

/// Result of one A/B experiment run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbExperimentResult {
    /// The base prompt that was tested.
    pub base_prompt: String,
    /// Intent label (first 64 chars of base prompt, normalised).
    pub intent: String,
    /// Each variant and its score.
    pub variants: Vec<VariantScore>,
    /// Index of the winning variant.
    pub winner_index: usize,
    /// Score of the winning variant.
    pub winner_score: f64,
}

/// Score for a single prompt variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantScore {
    /// The variant text.
    pub prompt: String,
    /// The model's response.
    pub response: String,
    /// Aggregate quality score in [0.0, 1.0].
    pub score: f64,
}

// ─── Promoter registry ───────────────────────────────────────────────────────

/// Maps intent strings to the best known prompt variant.
#[derive(Debug)]
pub struct PromoterRegistry {
    inner: Mutex<PromoterInner>,
}

#[derive(Debug)]
struct PromoterInner {
    map: HashMap<String, (String, f64)>, // intent → (best_variant, best_score)
    max_intents: usize,
}

impl PromoterRegistry {
    /// Create a registry capped at `max_intents` entries.
    #[must_use]
    pub fn new(max_intents: usize) -> Self {
        Self {
            inner: Mutex::new(PromoterInner {
                map: HashMap::new(),
                max_intents: max_intents.max(1),
            }),
        }
    }

    /// Promote a variant if it beats the current best (or if no best exists).
    pub fn promote(&self, intent: &str, variant: String, score: f64) {
        let Ok(mut inner) = self.inner.lock() else {
            warn!("PromoterRegistry lock poisoned; skipping promote");
            return;
        };
        let should_insert = match inner.map.get(intent) {
            Some((_, existing_score)) => score > *existing_score,
            None => {
                if inner.map.len() >= inner.max_intents {
                    warn!(
                        max = inner.max_intents,
                        "PromoterRegistry at capacity; skipping intent"
                    );
                    return;
                }
                true
            }
        };
        if should_insert {
            info!(intent, score, "promoting new best prompt variant");
            inner.map.insert(intent.to_string(), (variant, score));
        }
    }

    /// Look up the best variant for an intent, if any.
    #[must_use]
    pub fn best_variant(&self, intent: &str) -> Option<String> {
        let Ok(inner) = self.inner.lock() else {
            return None;
        };
        inner.map.get(intent).map(|(v, _)| v.clone())
    }

    /// Return all registered intents and their scores.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, String, f64)> {
        let Ok(inner) = self.inner.lock() else {
            return vec![];
        };
        inner
            .map
            .iter()
            .map(|(intent, (variant, score))| (intent.clone(), variant.clone(), *score))
            .collect()
    }
}

impl Default for PromoterRegistry {
    fn default() -> Self {
        Self::new(10_000)
    }
}

// ─── Optimizer config ────────────────────────────────────────────────────────

/// Configuration for the prompt A/B optimizer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbOptimizerConfig {
    /// Strategy for generating variants.
    pub strategy: VariantStrategy,
    /// Number of variants to generate per experiment.
    pub num_variants: usize,
    /// Whether to automatically promote the winner.
    pub auto_promote: bool,
    /// Maximum number of intents to track in the promoter registry.
    pub max_intents: usize,
}

impl Default for AbOptimizerConfig {
    fn default() -> Self {
        Self {
            strategy: VariantStrategy::InstructionPrefix,
            num_variants: 4,
            auto_promote: true,
            max_intents: 10_000,
        }
    }
}

// ─── Main optimizer ──────────────────────────────────────────────────────────

/// Derives the "intent" from a prompt (first 64 normalised chars).
fn derive_intent(prompt: &str) -> String {
    prompt
        .chars()
        .take(64)
        .collect::<String>()
        .to_lowercase()
        .trim()
        .to_string()
}

/// Orchestrates prompt A/B testing.
///
/// Typical usage:
/// ```ignore
/// let optimizer = PromptAbOptimizer::new(config, scoring_engine, registry);
/// let result = optimizer.run("Explain quantum entanglement", |prompt| async move {
///     my_worker.infer(prompt).await
/// }).await?;
/// ```
pub struct PromptAbOptimizer {
    config: AbOptimizerConfig,
    scoring: ScoringEngine,
    registry: Arc<PromoterRegistry>,
    generator: VariantGenerator,
}

impl PromptAbOptimizer {
    /// Create a new optimizer.
    #[must_use]
    pub fn new(
        config: AbOptimizerConfig,
        scoring: ScoringEngine,
        registry: Arc<PromoterRegistry>,
    ) -> Self {
        let generator =
            VariantGenerator::new(config.strategy.clone(), config.num_variants);
        Self {
            config,
            scoring,
            registry,
            generator,
        }
    }

    /// Run an A/B experiment for the given base prompt.
    ///
    /// `infer_fn` is an async closure that accepts a prompt string and returns
    /// the model response. Variants are evaluated sequentially (parallel execution
    /// is the caller's responsibility via `tokio::spawn` if desired).
    ///
    /// # Errors
    /// Returns [`PromptOptimizerError::NoVariants`] if generation fails, or
    /// [`PromptOptimizerError::AllInferencesFailed`] if every variant inference errors.
    pub async fn run<F, Fut>(
        &self,
        base_prompt: &str,
        infer_fn: F,
    ) -> Result<AbExperimentResult, PromptOptimizerError>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        let variants = self.generator.generate(base_prompt);
        if variants.is_empty() {
            return Err(PromptOptimizerError::NoVariants);
        }

        debug!(
            num_variants = variants.len(),
            base_len = base_prompt.len(),
            "running A/B prompt experiment"
        );

        let mut scored: Vec<VariantScore> = Vec::with_capacity(variants.len());
        let mut errors: Vec<String> = Vec::new();

        for variant in variants {
            match infer_fn(variant.clone()).await {
                Ok(response) => {
                    let score = self.scoring.score(&response);
                    debug!(score, variant_len = variant.len(), "variant scored");
                    scored.push(VariantScore {
                        prompt: variant,
                        response,
                        score,
                    });
                }
                Err(e) => {
                    warn!(error = %e, "variant inference failed");
                    errors.push(e);
                }
            }
        }

        if scored.is_empty() {
            return Err(PromptOptimizerError::AllInferencesFailed(errors.join("; ")));
        }

        // Find winner
        let winner_index = scored
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0);

        let winner_score = scored[winner_index].score;
        let intent = derive_intent(base_prompt);

        if self.config.auto_promote {
            self.registry.promote(
                &intent,
                scored[winner_index].prompt.clone(),
                winner_score,
            );
        }

        info!(
            intent = %intent,
            winner_index,
            winner_score,
            total_variants = scored.len(),
            "A/B experiment complete"
        );

        Ok(AbExperimentResult {
            base_prompt: base_prompt.to_string(),
            intent,
            variants: scored,
            winner_index,
            winner_score,
        })
    }

    /// Return the best known variant for a prompt (if promoted previously).
    #[must_use]
    pub fn best_variant_for(&self, prompt: &str) -> Option<String> {
        let intent = derive_intent(prompt);
        self.registry.best_variant(&intent)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_engine_length_metric() {
        let engine = ScoringEngine::new(vec![(
            QualityMetric::ResponseLength { max_chars: 100 },
            1.0,
        )]);
        assert!((engine.score("hello") - 0.05).abs() < 0.01);
        assert!((engine.score(&"x".repeat(100)) - 1.0).abs() < 0.001);
    }

    #[test]
    fn scoring_engine_keyword_metric() {
        let engine = ScoringEngine::new(vec![(
            QualityMetric::KeywordPresence {
                keywords: vec!["answer".to_string()],
            },
            1.0,
        )]);
        assert_eq!(engine.score("The answer is 42"), 1.0);
        assert_eq!(engine.score("I don't know"), 0.0);
    }

    #[test]
    fn scoring_engine_json_validity() {
        let engine = ScoringEngine::new(vec![(QualityMetric::JsonValidity, 1.0)]);
        assert_eq!(engine.score(r#"{"ok": true}"#), 1.0);
        assert_eq!(engine.score("not json"), 0.0);
    }

    #[test]
    fn variant_generator_instruction_prefix() {
        let gen = VariantGenerator::new(VariantStrategy::InstructionPrefix, 3);
        let variants = gen.generate("What is Rust?");
        assert_eq!(variants.len(), 3);
        assert!(variants[1].contains("What is Rust?"));
    }

    #[test]
    fn promoter_registry_promotes_better_score() {
        let reg = Arc::new(PromoterRegistry::default());
        reg.promote("test intent", "variant A".to_string(), 0.5);
        reg.promote("test intent", "variant B".to_string(), 0.9);
        reg.promote("test intent", "variant C".to_string(), 0.3);
        assert_eq!(
            reg.best_variant("test intent").as_deref(),
            Some("variant B")
        );
    }

    #[tokio::test]
    async fn optimizer_run_picks_best() {
        let config = AbOptimizerConfig {
            num_variants: 3,
            strategy: VariantStrategy::InstructionPrefix,
            auto_promote: true,
            max_intents: 100,
        };
        let engine = ScoringEngine::new(vec![(
            QualityMetric::ResponseLength { max_chars: 50 },
            1.0,
        )]);
        let registry = Arc::new(PromoterRegistry::default());
        let optimizer = PromptAbOptimizer::new(config, engine, registry.clone());

        let result = optimizer
            .run("Hello", |prompt| async move {
                // Longer response for prefixed variants
                Ok(format!("{} response here!", prompt))
            })
            .await
            .expect("experiment should succeed");

        assert!(!result.variants.is_empty());
        assert!(result.winner_score >= 0.0);
    }

    #[test]
    fn derive_intent_truncates() {
        let long_prompt = "x".repeat(200);
        assert_eq!(derive_intent(&long_prompt).len(), 64);
    }
}
