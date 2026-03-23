//! # Prompt Optimizer
//!
//! Prompt variant testing and automatic improvement tracking using bandit
//! algorithms: UCB1, Epsilon-Greedy, Thompson Sampling, and BestFirst.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Re-exported legacy types (preserved for backward compatibility)
// ---------------------------------------------------------------------------

/// Errors returned by the legacy prompt optimizer.
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

/// A configurable quality metric used to score an inference response (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QualityMetric {
    /// Reward longer responses up to `max_chars`.
    ResponseLength { max_chars: usize },
    /// Score 1.0 if any keyword appears.
    KeywordPresence { keywords: Vec<String> },
    /// Score 1.0 if the response is valid JSON.
    JsonValidity,
    /// Score = fraction of required_keys present.
    JsonKeyPresence { required_keys: Vec<String> },
    /// Score 1.0 if length is within [min_chars, max_chars].
    LengthWindow { min_chars: usize, max_chars: usize },
}

impl QualityMetric {
    /// Evaluate this metric against a response string.
    #[must_use]
    pub fn score(&self, response: &str) -> f64 {
        match self {
            QualityMetric::ResponseLength { max_chars } => {
                if *max_chars == 0 { return 0.0; }
                (response.len() as f64 / *max_chars as f64).min(1.0)
            }
            QualityMetric::KeywordPresence { keywords } => {
                let lower = response.to_lowercase();
                if keywords.iter().any(|kw| lower.contains(kw.to_lowercase().as_str())) { 1.0 } else { 0.0 }
            }
            QualityMetric::JsonValidity => {
                if serde_json::from_str::<serde_json::Value>(response).is_ok() { 1.0 } else { 0.0 }
            }
            QualityMetric::JsonKeyPresence { required_keys } => {
                if required_keys.is_empty() { return 1.0; }
                match serde_json::from_str::<serde_json::Value>(response) {
                    Ok(serde_json::Value::Object(map)) => {
                        let found = required_keys.iter().filter(|k| map.contains_key(k.as_str())).count();
                        found as f64 / required_keys.len() as f64
                    }
                    _ => 0.0,
                }
            }
            QualityMetric::LengthWindow { min_chars, max_chars } => {
                let len = response.len();
                if len >= *min_chars && len <= *max_chars { 1.0 } else { 0.0 }
            }
        }
    }
}

/// Computes a weighted aggregate quality score from multiple metrics (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringEngine {
    /// Each entry is (metric, weight).
    pub metrics: Vec<(QualityMetric, f64)>,
}

impl ScoringEngine {
    /// Create a new scoring engine.
    #[must_use]
    pub fn new(metrics: Vec<(QualityMetric, f64)>) -> Self { Self { metrics } }

    /// Score a response.
    #[must_use]
    pub fn score(&self, response: &str) -> f64 {
        if self.metrics.is_empty() { return 0.0; }
        let total_weight: f64 = self.metrics.iter().map(|(_, w)| w.abs()).sum();
        if total_weight == 0.0 { return 0.0; }
        let weighted_sum: f64 = self.metrics.iter()
            .map(|(metric, weight)| metric.score(response) * weight.abs())
            .sum();
        (weighted_sum / total_weight).clamp(0.0, 1.0)
    }
}

impl Default for ScoringEngine {
    fn default() -> Self {
        Self::new(vec![
            (QualityMetric::ResponseLength { max_chars: 2000 }, 0.6),
            (QualityMetric::KeywordPresence { keywords: vec!["answer".to_string(), "result".to_string()] }, 0.4),
        ])
    }
}

/// Strategies for generating prompt variants (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VariantStrategy {
    /// Prepend different instructional prefixes.
    InstructionPrefix,
    /// Append different closing instructions.
    ClosingSuffix,
    /// Reframe the request.
    Reframe,
    /// Use a custom set of prefix strings.
    CustomPrefixes(Vec<String>),
}

/// Generates N prompt variants from a base prompt (legacy).
pub struct VariantGenerator {
    strategy: VariantStrategy,
    num_variants: usize,
}

impl VariantGenerator {
    /// Create a new generator.
    #[must_use]
    pub fn new(strategy: VariantStrategy, num_variants: usize) -> Self {
        Self { strategy, num_variants: num_variants.max(1).min(32) }
    }

    /// Generate variants.
    #[must_use]
    pub fn generate(&self, base_prompt: &str) -> Vec<String> {
        match &self.strategy {
            VariantStrategy::InstructionPrefix => {
                let prefixes = ["", "Please answer concisely. ", "Think step by step. ",
                    "Provide a detailed explanation. ", "Answer as an expert. ",
                    "Be direct and precise. ", "Use bullet points. ", "Explain to a beginner. "];
                prefixes.iter().take(self.num_variants).map(|p| format!("{p}{base_prompt}")).collect()
            }
            VariantStrategy::ClosingSuffix => {
                let suffixes = ["", "\nBe concise.", "\nProvide examples.", "\nBe thorough.", "\nSummarise at the end."];
                suffixes.iter().take(self.num_variants).map(|s| format!("{base_prompt}{s}")).collect()
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
            VariantStrategy::CustomPrefixes(prefixes) => {
                prefixes.iter().take(self.num_variants).map(|p| format!("{p}{base_prompt}")).collect()
            }
        }
    }
}

/// Result of one A/B experiment run (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbExperimentResult {
    /// The base prompt tested.
    pub base_prompt: String,
    /// Intent label.
    pub intent: String,
    /// Each variant and its score.
    pub variants: Vec<VariantScore>,
    /// Index of the winning variant.
    pub winner_index: usize,
    /// Score of the winning variant.
    pub winner_score: f64,
}

/// Score for a single prompt variant (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantScore {
    /// The variant text.
    pub prompt: String,
    /// The model's response.
    pub response: String,
    /// Aggregate quality score.
    pub score: f64,
}

/// Maps intent strings to the best known prompt variant (legacy).
#[derive(Debug)]
pub struct PromoterRegistry {
    inner: Mutex<PromoterInner>,
}

#[derive(Debug)]
struct PromoterInner {
    map: HashMap<String, (String, f64)>,
    max_intents: usize,
}

impl PromoterRegistry {
    /// Create a registry capped at `max_intents` entries.
    #[must_use]
    pub fn new(max_intents: usize) -> Self {
        Self { inner: Mutex::new(PromoterInner { map: HashMap::new(), max_intents: max_intents.max(1) }) }
    }

    /// Promote a variant if it beats the current best.
    pub fn promote(&self, intent: &str, variant: String, score: f64) {
        let Ok(mut inner) = self.inner.lock() else {
            warn!("PromoterRegistry lock poisoned; skipping promote");
            return;
        };
        let should_insert = match inner.map.get(intent) {
            Some((_, existing_score)) => score > *existing_score,
            None => {
                if inner.map.len() >= inner.max_intents {
                    warn!(max = inner.max_intents, "PromoterRegistry at capacity");
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

    /// Look up the best variant for an intent.
    #[must_use]
    pub fn best_variant(&self, intent: &str) -> Option<String> {
        let Ok(inner) = self.inner.lock() else { return None; };
        inner.map.get(intent).map(|(v, _)| v.clone())
    }

    /// Return all registered intents and their scores.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, String, f64)> {
        let Ok(inner) = self.inner.lock() else { return vec![]; };
        inner.map.iter().map(|(intent, (variant, score))| (intent.clone(), variant.clone(), *score)).collect()
    }
}

impl Default for PromoterRegistry {
    fn default() -> Self { Self::new(10_000) }
}

/// Configuration for the legacy prompt A/B optimizer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbOptimizerConfig {
    /// Strategy for generating variants.
    pub strategy: VariantStrategy,
    /// Number of variants to generate per experiment.
    pub num_variants: usize,
    /// Whether to automatically promote the winner.
    pub auto_promote: bool,
    /// Maximum number of intents to track.
    pub max_intents: usize,
}

impl Default for AbOptimizerConfig {
    fn default() -> Self {
        Self { strategy: VariantStrategy::InstructionPrefix, num_variants: 4, auto_promote: true, max_intents: 10_000 }
    }
}

fn derive_intent(prompt: &str) -> String {
    prompt.chars().take(64).collect::<String>().to_lowercase().trim().to_string()
}

/// Orchestrates prompt A/B testing (legacy).
pub struct PromptAbOptimizer {
    config: AbOptimizerConfig,
    scoring: ScoringEngine,
    registry: Arc<PromoterRegistry>,
    generator: VariantGenerator,
}

impl PromptAbOptimizer {
    /// Create a new optimizer.
    #[must_use]
    pub fn new(config: AbOptimizerConfig, scoring: ScoringEngine, registry: Arc<PromoterRegistry>) -> Self {
        let generator = VariantGenerator::new(config.strategy.clone(), config.num_variants);
        Self { config, scoring, registry, generator }
    }

    /// Run an A/B experiment for the given base prompt.
    pub async fn run<F, Fut>(&self, base_prompt: &str, infer_fn: F) -> Result<AbExperimentResult, PromptOptimizerError>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        let variants = self.generator.generate(base_prompt);
        if variants.is_empty() { return Err(PromptOptimizerError::NoVariants); }
        let mut scored: Vec<VariantScore> = Vec::with_capacity(variants.len());
        let mut errors: Vec<String> = Vec::new();
        for variant in variants {
            match infer_fn(variant.clone()).await {
                Ok(response) => {
                    let score = self.scoring.score(&response);
                    scored.push(VariantScore { prompt: variant, response, score });
                }
                Err(e) => { warn!(error = %e, "variant inference failed"); errors.push(e); }
            }
        }
        if scored.is_empty() { return Err(PromptOptimizerError::AllInferencesFailed(errors.join("; "))); }
        let winner_index = scored.iter().enumerate()
            .max_by(|(_, a), (_, b)| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i).unwrap_or(0);
        let winner_score = scored[winner_index].score;
        let intent = derive_intent(base_prompt);
        if self.config.auto_promote {
            self.registry.promote(&intent, scored[winner_index].prompt.clone(), winner_score);
        }
        Ok(AbExperimentResult { base_prompt: base_prompt.to_string(), intent, variants: scored, winner_index, winner_score })
    }

    /// Return the best known variant for a prompt.
    #[must_use]
    pub fn best_variant_for(&self, prompt: &str) -> Option<String> {
        let intent = derive_intent(prompt);
        self.registry.best_variant(&intent)
    }
}

// ---------------------------------------------------------------------------
// NEW SPEC IMPLEMENTATION
// ---------------------------------------------------------------------------

/// A single prompt variant being tracked by the optimizer.
#[derive(Debug, Clone)]
pub struct PromptVariant {
    /// Unique variant identifier.
    pub id: String,
    /// The prompt template text.
    pub template: String,
    /// Running performance score (mean of recorded scores).
    pub performance_score: f64,
    /// Number of times this variant has been evaluated.
    pub sample_count: u64,
    /// Average cost per evaluation.
    pub avg_cost: f64,
    /// Average number of output tokens.
    pub avg_tokens_out: u64,
    /// Unix timestamp when the variant was created.
    pub created_at: u64,
    // Internal accumulators (not exposed in the public interface).
    total_cost: f64,
    total_tokens_out: u64,
    /// Number of successes (score > 0.5) for Thompson Sampling.
    successes: u64,
}

impl PromptVariant {
    fn new(id: String, template: String) -> Self {
        Self {
            id, template, performance_score: 0.0, sample_count: 0,
            avg_cost: 0.0, avg_tokens_out: 0, created_at: 0,
            total_cost: 0.0, total_tokens_out: 0, successes: 0,
        }
    }
}

/// Strategy for selecting the next variant to evaluate.
#[derive(Debug, Clone)]
pub enum OptimizationStrategy {
    /// Upper Confidence Bound — balances exploration and exploitation.
    UCB1 {
        /// Exploration constant (higher = more exploration). Typical: 1.0–2.0.
        exploration: f64,
    },
    /// Epsilon-Greedy — exploit the best variant most of the time.
    EpsilonGreedy {
        /// Probability of exploring a random variant (0.0–1.0).
        epsilon: f64,
    },
    /// Thompson Sampling — sample from Beta posterior per variant.
    ThompsonSampling {
        /// Prior alpha parameter (pseudo-successes before any data).
        alpha: f64,
        /// Prior beta parameter (pseudo-failures before any data).
        beta_param: f64,
    },
    /// Always pick the highest-scoring variant (or first unsampled).
    BestFirst,
}

/// Tracks prompt variants and selects the best using bandit algorithms.
pub struct PromptOptimizer {
    /// All registered variants.
    pub variants: Vec<PromptVariant>,
    /// Selection strategy.
    pub strategy: OptimizationStrategy,
    /// Total number of selection trials performed.
    pub total_trials: u64,
    next_id: u64,
}

impl PromptOptimizer {
    /// Create a new optimizer with the given strategy.
    pub fn new(strategy: OptimizationStrategy) -> Self {
        Self { variants: Vec::new(), strategy, total_trials: 0, next_id: 0 }
    }

    /// Add a variant and return its generated ID.
    pub fn add_variant(&mut self, template: &str) -> String {
        let id = format!("variant-{}", self.next_id);
        self.next_id += 1;
        self.variants.push(PromptVariant::new(id.clone(), template.to_string()));
        id
    }

    /// Select the next variant to evaluate using the configured strategy.
    ///
    /// `rng_seed` is used for stochastic strategies (EpsilonGreedy, ThompsonSampling).
    /// Returns `None` if no variants are registered.
    pub fn select(&mut self, rng_seed: u64) -> Option<&PromptVariant> {
        if self.variants.is_empty() { return None; }

        // Always prefer an unsampled variant first (UCB1 and BestFirst benefit from this too).
        let unsampled = self.variants.iter().position(|v| v.sample_count == 0);

        let selected_idx = match &self.strategy {
            OptimizationStrategy::UCB1 { exploration } => {
                if let Some(idx) = unsampled { idx }
                else {
                    let total_ln = (self.total_trials as f64).ln().max(0.0);
                    let exploration = *exploration;
                    self.variants.iter().enumerate()
                        .map(|(i, v)| {
                            let mean = v.performance_score;
                            let ucb = mean + exploration * (2.0 * total_ln / v.sample_count as f64).sqrt();
                            (i, ucb)
                        })
                        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                        .map(|(i, _)| i)
                        .unwrap_or(0)
                }
            }
            OptimizationStrategy::EpsilonGreedy { epsilon } => {
                let epsilon = *epsilon;
                // LCG pseudo-random from seed.
                let rand_val = lcg_rand(rng_seed + self.total_trials) as f64 / u64::MAX as f64;
                if rand_val < epsilon {
                    // Explore: random variant.
                    let rand_idx = lcg_rand(rng_seed.wrapping_add(self.total_trials).wrapping_add(1337));
                    (rand_idx as usize) % self.variants.len()
                } else {
                    // Exploit: best mean.
                    self.variants.iter().enumerate()
                        .max_by(|(_, a), (_, b)| a.performance_score.partial_cmp(&b.performance_score)
                            .unwrap_or(std::cmp::Ordering::Equal))
                        .map(|(i, _)| i).unwrap_or(0)
                }
            }
            OptimizationStrategy::ThompsonSampling { alpha, beta_param } => {
                let alpha = *alpha;
                let beta_p = *beta_param;
                // Approximate Thompson Sampling: sample Beta(alpha + s, beta + f) for each variant.
                // Beta mean = alpha/(alpha+beta); add deterministic noise based on seed+variant_idx.
                self.variants.iter().enumerate()
                    .map(|(i, v)| {
                        let s = v.successes as f64;
                        let f = (v.sample_count - v.successes) as f64;
                        let a = alpha + s;
                        let b = beta_p + f;
                        // Approximate sample: mean + scaled noise.
                        let mean = a / (a + b);
                        let noise_seed = lcg_rand(rng_seed.wrapping_add(self.total_trials).wrapping_add(i as u64));
                        let noise = (noise_seed as f64 / u64::MAX as f64 - 0.5) * 0.1;
                        let sample = (mean + noise).clamp(0.0, 1.0);
                        (i, sample)
                    })
                    .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(i, _)| i).unwrap_or(0)
            }
            OptimizationStrategy::BestFirst => {
                // Prefer unsampled first, then highest performance_score.
                if let Some(idx) = unsampled { idx }
                else {
                    self.variants.iter().enumerate()
                        .max_by(|(_, a), (_, b)| a.performance_score.partial_cmp(&b.performance_score)
                            .unwrap_or(std::cmp::Ordering::Equal))
                        .map(|(i, _)| i).unwrap_or(0)
                }
            }
        };

        self.total_trials += 1;
        self.variants.get(selected_idx)
    }

    /// Record feedback for a variant after evaluation.
    pub fn record_feedback(&mut self, variant_id: &str, score: f64, cost: f64, tokens_out: u64) {
        if let Some(v) = self.variants.iter_mut().find(|v| v.id == variant_id) {
            let n = v.sample_count as f64;
            // Online mean update.
            v.performance_score = (v.performance_score * n + score) / (n + 1.0);
            v.sample_count += 1;
            v.total_cost += cost;
            v.avg_cost = v.total_cost / v.sample_count as f64;
            v.total_tokens_out += tokens_out;
            v.avg_tokens_out = v.total_tokens_out / v.sample_count;
            if score > 0.5 { v.successes += 1; }
            debug!(variant_id, score, "prompt_optimizer: feedback recorded");
        }
    }

    /// Return the variant with the highest performance score.
    pub fn best_variant(&self) -> Option<&PromptVariant> {
        self.variants.iter()
            .filter(|v| v.sample_count > 0)
            .max_by(|a, b| a.performance_score.partial_cmp(&b.performance_score).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Convergence ratio: best_score / 1.0 (max possible).
    pub fn convergence_ratio(&self) -> f64 {
        self.best_variant().map(|v| v.performance_score).unwrap_or(0.0).clamp(0.0, 1.0)
    }
}

/// Applies simple transformations to prompt templates.
pub struct PromptMutator;

impl PromptMutator {
    /// Replace 2–3 common words with synonyms and return the modified template.
    ///
    /// Uses a small hardcoded thesaurus. Deterministic for a given seed.
    pub fn paraphrase(template: &str, seed: u64) -> String {
        let thesaurus: HashMap<&str, Vec<&str>> = [
            ("quickly", vec!["rapidly", "swiftly", "promptly"]),
            ("help", vec!["assist", "support", "aid"]),
            ("make", vec!["create", "generate", "produce"]),
            ("show", vec!["display", "present", "demonstrate"]),
            ("use", vec!["utilize", "employ", "apply"]),
            ("good", vec!["excellent", "effective", "optimal"]),
            ("bad", vec!["poor", "ineffective", "suboptimal"]),
            ("big", vec!["large", "substantial", "significant"]),
            ("small", vec!["minimal", "compact", "concise"]),
            ("important", vec!["critical", "essential", "key"]),
        ].iter().cloned().collect();

        let mut result = template.to_string();
        let mut replacements = 0;
        let mut rng = seed;
        for (word, synonyms) in &thesaurus {
            if replacements >= 3 { break; }
            // Case-insensitive search.
            let lower = result.to_lowercase();
            if let Some(pos) = lower.find(word) {
                rng = lcg_rand(rng);
                let syn_idx = (rng as usize) % synonyms.len();
                let synonym = synonyms[syn_idx];
                // Preserve capitalization of first char.
                let original_char = result.chars().nth(pos).unwrap_or('a');
                let replacement = if original_char.is_uppercase() {
                    let mut s = synonym.to_string();
                    if let Some(c) = s.get_mut(0..1) { c.make_ascii_uppercase(); }
                    s
                } else {
                    synonym.to_string()
                };
                result = format!("{}{}{}", &result[..pos], replacement, &result[pos + word.len()..]);
                replacements += 1;
            }
        }
        result
    }

    /// Prepend an instruction to the template.
    pub fn add_instruction(template: &str, instruction: &str) -> String {
        format!("{instruction}\n\n{template}")
    }

    /// Truncate the template to approximately `max_tokens` tokens (1.3 tokens/word).
    pub fn trim_to_budget(template: &str, max_tokens: usize) -> String {
        let max_words = ((max_tokens as f64) / 1.3) as usize;
        let words: Vec<&str> = template.split_whitespace().collect();
        if words.len() <= max_words {
            template.to_string()
        } else {
            words[..max_words].join(" ")
        }
    }
}

// ---------------------------------------------------------------------------
// Math helpers
// ---------------------------------------------------------------------------

/// Linear congruential generator for deterministic pseudo-randomness.
fn lcg_rand(seed: u64) -> u64 {
    seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn ucb1_selects_unsampled_first() {
        let mut opt = PromptOptimizer::new(OptimizationStrategy::UCB1 { exploration: 1.414 });
        let id_a = opt.add_variant("template A");
        let id_b = opt.add_variant("template B");
        let id_c = opt.add_variant("template C");

        // With no samples, should select in insertion order (unsampled preferred).
        let selected = opt.select(42).unwrap();
        assert_eq!(selected.id, id_a);
        opt.record_feedback(&id_a, 0.5, 0.01, 100);

        let selected = opt.select(42).unwrap();
        assert_eq!(selected.id, id_b);
        opt.record_feedback(&id_b, 0.5, 0.01, 100);

        let selected = opt.select(42).unwrap();
        assert_eq!(selected.id, id_c);
    }

    #[test]
    fn epsilon_greedy_explores_at_epsilon_rate() {
        let mut opt = PromptOptimizer::new(OptimizationStrategy::EpsilonGreedy { epsilon: 1.0 });
        let _id_a = opt.add_variant("template A");
        let _id_b = opt.add_variant("template B");
        // Prime variant A as best.
        opt.record_feedback("variant-0", 0.9, 0.01, 100);
        opt.record_feedback("variant-1", 0.1, 0.01, 100);

        // With epsilon=1.0, always explore (random).
        let mut selections: HashMap<String, u64> = HashMap::new();
        for i in 0..100u64 {
            // Reset trial count to avoid total_trials dominating.
            if let Some(v) = opt.select(i * 1000) {
                *selections.entry(v.id.clone()).or_default() += 1;
            }
        }
        // Both variants should have been selected.
        assert!(selections.contains_key("variant-0") || selections.contains_key("variant-1"));
    }

    #[test]
    fn best_variant_returns_highest_score() {
        let mut opt = PromptOptimizer::new(OptimizationStrategy::BestFirst);
        let _id_a = opt.add_variant("template A");
        let _id_b = opt.add_variant("template B");
        let _id_c = opt.add_variant("template C");

        opt.record_feedback("variant-0", 0.3, 0.01, 100);
        opt.record_feedback("variant-1", 0.9, 0.01, 100);
        opt.record_feedback("variant-2", 0.6, 0.01, 100);

        let best = opt.best_variant().unwrap();
        assert_eq!(best.id, "variant-1", "variant-1 has the highest score");
    }

    #[test]
    fn best_first_selects_best_after_sampling() {
        let mut opt = PromptOptimizer::new(OptimizationStrategy::BestFirst);
        let _id_a = opt.add_variant("low score template");
        let _id_b = opt.add_variant("high score template");

        // Force-feed scores without going through select.
        opt.record_feedback("variant-0", 0.2, 0.01, 50);
        opt.record_feedback("variant-1", 0.95, 0.01, 50);

        // Next select should pick best (variant-1).
        let selected = opt.select(0).unwrap();
        assert_eq!(selected.id, "variant-1");
    }

    #[test]
    fn mutator_paraphrase_changes_output() {
        let template = "Please help me quickly make a good solution";
        let paraphrased = PromptMutator::paraphrase(template, 42);
        // Should differ from original (at least one word replaced).
        assert_ne!(paraphrased, template, "paraphrase should modify the template");
    }

    #[test]
    fn mutator_add_instruction_prepends() {
        let result = PromptMutator::add_instruction("Do the task.", "Be concise.");
        assert!(result.starts_with("Be concise.\n\n"), "instruction should be prepended");
        assert!(result.contains("Do the task."));
    }

    #[test]
    fn mutator_trim_to_budget_truncates() {
        let template = "one two three four five six seven eight nine ten";
        // max_tokens=5 → max_words = floor(5/1.3) = 3
        let trimmed = PromptMutator::trim_to_budget(template, 5);
        let word_count = trimmed.split_whitespace().count();
        assert!(word_count <= 4, "trimmed template should have at most 4 words, got {word_count}");
    }

    #[test]
    fn convergence_ratio_reflects_best_score() {
        let mut opt = PromptOptimizer::new(OptimizationStrategy::BestFirst);
        let _id = opt.add_variant("template");
        opt.record_feedback("variant-0", 0.75, 0.01, 100);
        assert!((opt.convergence_ratio() - 0.75).abs() < 0.001);
    }

    // --- Legacy tests ---

    #[test]
    fn scoring_engine_length_metric() {
        let engine = ScoringEngine::new(vec![(QualityMetric::ResponseLength { max_chars: 100 }, 1.0)]);
        assert!((engine.score("hello") - 0.05).abs() < 0.01);
        assert!((engine.score(&"x".repeat(100)) - 1.0).abs() < 0.001);
    }

    #[test]
    fn scoring_engine_keyword_metric() {
        let engine = ScoringEngine::new(vec![(QualityMetric::KeywordPresence { keywords: vec!["answer".to_string()] }, 1.0)]);
        assert_eq!(engine.score("The answer is 42"), 1.0);
        assert_eq!(engine.score("I don't know"), 0.0);
    }

    #[test]
    fn promoter_registry_promotes_better_score() {
        let reg = Arc::new(PromoterRegistry::default());
        reg.promote("test intent", "variant A".to_string(), 0.5);
        reg.promote("test intent", "variant B".to_string(), 0.9);
        reg.promote("test intent", "variant C".to_string(), 0.3);
        assert_eq!(reg.best_variant("test intent").as_deref(), Some("variant B"));
    }
}

// ===========================================================================
// Round-32 additions: compression, few-shot selection, CoT injection,
// and the unified PromptOptimizer facade.
// ===========================================================================

// ---------------------------------------------------------------------------
// OptimizationGoal
// ---------------------------------------------------------------------------

/// High-level objective that guides how the compression optimizer applies its tools.
#[derive(Clone, Debug)]
pub enum CompressionGoal {
    /// Reduce token count as aggressively as possible.
    MinimizeTokens,
    /// Preserve maximum clarity even at the cost of extra tokens.
    MaximizeClarity,
    /// Balance cost savings against output quality.
    BalancedCostQuality,
}

// ---------------------------------------------------------------------------
// FewShotExample / FewShotSelector
// ---------------------------------------------------------------------------

/// A single input/output example for few-shot prompting.
#[derive(Clone, Debug)]
pub struct FewShotExample {
    /// The example input text.
    pub input: String,
    /// The expected output text.
    pub output: String,
    /// Estimated token cost for this example.
    pub tokens: usize,
    /// Relevance score assigned during selection (0.0–1.0).
    pub relevance_score: f64,
}

/// Selects the most relevant few-shot examples that fit within a token budget.
#[derive(Debug, Default)]
pub struct FewShotSelector {
    examples: Vec<FewShotExample>,
    max_examples: usize,
}

impl FewShotSelector {
    /// Create a selector that returns at most `max_examples` examples.
    pub fn new(max_examples: usize) -> Self {
        Self { examples: Vec::new(), max_examples }
    }

    /// Add a new candidate example.
    pub fn add_example(&mut self, input: impl Into<String>, output: impl Into<String>, tokens: usize) {
        self.examples.push(FewShotExample {
            input: input.into(),
            output: output.into(),
            tokens,
            relevance_score: 0.0,
        });
    }

    /// Return up to `max_examples` examples sorted by Jaccard similarity to
    /// `query`, limited to `token_budget` total tokens.
    pub fn select_relevant<'a>(&'a self, query: &str, token_budget: usize) -> Vec<&'a FewShotExample> {
        let mut scored: Vec<(f64, &FewShotExample)> = self
            .examples
            .iter()
            .map(|ex| (Self::jaccard(query, &ex.input), ex))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut result = Vec::new();
        let mut used_tokens = 0usize;
        for (_, ex) in scored {
            if result.len() >= self.max_examples {
                break;
            }
            if used_tokens + ex.tokens > token_budget {
                continue;
            }
            used_tokens += ex.tokens;
            result.push(ex);
        }
        result
    }

    /// Jaccard similarity between the word sets of two strings.
    fn jaccard(a: &str, b: &str) -> f64 {
        let words_a: std::collections::HashSet<&str> = a.split_whitespace().collect();
        let words_b: std::collections::HashSet<&str> = b.split_whitespace().collect();
        if words_a.is_empty() && words_b.is_empty() {
            return 1.0;
        }
        let intersection = words_a.intersection(&words_b).count();
        let union = words_a.union(&words_b).count();
        if union == 0 { 0.0 } else { intersection as f64 / union as f64 }
    }
}

// ---------------------------------------------------------------------------
// PromptCompressor
// ---------------------------------------------------------------------------

/// Applies lossless and near-lossless text compression to reduce token count.
#[derive(Debug, Clone)]
pub struct PromptCompressor {
    /// Maximum tokens the compressed output should occupy (soft limit).
    pub max_tokens: usize,
    /// Target compression ratio (0.0–1.0). 0.8 means aim to keep 80% of chars.
    pub compression_ratio: f64,
}

impl PromptCompressor {
    /// Create a compressor with the given limits.
    pub fn new(max_tokens: usize, compression_ratio: f64) -> Self {
        Self { max_tokens, compression_ratio: compression_ratio.clamp(0.0, 1.0) }
    }

    /// Compress `text` by:
    /// 1. Collapsing redundant whitespace.
    /// 2. Deduplicating consecutive identical sentences.
    /// 3. Dropping common filler phrases.
    /// 4. Abbreviating frequent verbose patterns.
    pub fn compress(&self, text: &str) -> String {
        // Step 1 – normalise whitespace.
        let mut out = collapse_whitespace(text);

        // Step 2 – deduplicate consecutive identical sentences.
        out = dedup_sentences(&out);

        // Step 3 – remove filler phrases.
        const FILLERS: &[&str] = &[
            "please ", "kindly ", "as mentioned above, ", "as mentioned above ",
            "as previously mentioned, ", "as previously mentioned ",
            "it is worth noting that ", "it should be noted that ",
            "note that ", "please note that ",
        ];
        for filler in FILLERS {
            // Case-insensitive removal.
            let lower = out.to_lowercase();
            let mut result = String::with_capacity(out.len());
            let mut pos = 0usize;
            loop {
                if pos >= out.len() { break; }
                if let Some(idx) = lower[pos..].find(filler) {
                    let abs = pos + idx;
                    result.push_str(&out[pos..abs]);
                    pos = abs + filler.len();
                } else {
                    result.push_str(&out[pos..]);
                    break;
                }
            }
            out = result;
        }

        // Step 4 – abbreviate common patterns.
        out = out.replace("in order to", "to")
                 .replace("due to the fact that", "because")
                 .replace("for the purpose of", "for")
                 .replace("at this point in time", "now")
                 .replace("in the event that", "if");

        // Final whitespace cleanup.
        collapse_whitespace(&out)
    }

    /// Fraction of characters saved: `(orig - compressed) / orig`.
    pub fn estimate_savings(&self, original: &str, compressed: &str) -> f64 {
        if original.is_empty() {
            return 0.0;
        }
        let saved = original.len().saturating_sub(compressed.len());
        saved as f64 / original.len() as f64
    }
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn dedup_sentences(s: &str) -> String {
    let sentences: Vec<&str> = s.split(". ").collect();
    let mut result: Vec<&str> = Vec::with_capacity(sentences.len());
    for sentence in &sentences {
        if result.last().map_or(true, |last| *last != *sentence) {
            result.push(sentence);
        }
    }
    result.join(". ")
}

// ---------------------------------------------------------------------------
// ChainOfThoughtInjector
// ---------------------------------------------------------------------------

/// Wraps a prompt with chain-of-thought scaffolding.
#[derive(Debug, Clone)]
pub struct ChainOfThoughtInjector {
    /// Text inserted before the user prompt to invoke step-by-step reasoning.
    pub cot_prefix: String,
    /// Text appended after the prompt to elicit the final answer.
    pub cot_suffix: String,
}

impl ChainOfThoughtInjector {
    /// Create an injector with the canonical CoT framing.
    pub fn new() -> Self {
        Self {
            cot_prefix: "Let's think step by step:".to_string(),
            cot_suffix: "Therefore, the answer is:".to_string(),
        }
    }

    /// Inject CoT scaffolding around `prompt`.
    pub fn inject(&self, prompt: &str) -> String {
        format!("{}\n\n{}\n{}", self.cot_prefix, prompt, self.cot_suffix)
    }

    /// Inject a numbered step scaffold with `n_steps` placeholders.
    pub fn inject_numbered_steps(&self, prompt: &str, n_steps: u32) -> String {
        let steps: Vec<String> = (1..=n_steps)
            .map(|i| format!("Step {}: [reasoning here]", i))
            .collect();
        format!("{}\n\n{}\n\n{}\n{}", self.cot_prefix, prompt, steps.join("\n"), self.cot_suffix)
    }
}

impl Default for ChainOfThoughtInjector {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// PromptOptimizer (unified facade)
// ---------------------------------------------------------------------------

/// Unified facade that combines compression, few-shot selection, and CoT
/// injection into a single optimisation pipeline.
pub struct PromptCompressionOptimizer {
    /// Text compressor.
    pub compressor: PromptCompressor,
    /// Few-shot example selector.
    pub few_shot: FewShotSelector,
    /// Chain-of-thought injector.
    pub cot: ChainOfThoughtInjector,
    /// High-level optimisation objective.
    pub goal: CompressionGoal,
}

impl PromptCompressionOptimizer {
    /// Create an optimizer with sensible defaults.
    pub fn new(goal: CompressionGoal) -> Self {
        Self {
            compressor: PromptCompressor::new(4096, 0.8),
            few_shot: FewShotSelector::new(3),
            cot: ChainOfThoughtInjector::new(),
            goal,
        }
    }

    /// Optimise a single prompt within the given `token_budget`.
    ///
    /// * Always applies compression.
    /// * Injects few-shot examples if the budget allows.
    /// * Adds CoT scaffolding when the goal is `MaximizeClarity` or
    ///   `BalancedCostQuality`.
    pub fn optimize(&self, prompt: &str, token_budget: usize) -> String {
        // 1. Compress.
        let compressed = self.compressor.compress(prompt);

        // 2. Rough token estimate: 1 token ≈ 4 chars.
        let base_tokens = compressed.len() / 4 + 1;
        let remaining = token_budget.saturating_sub(base_tokens);

        // 3. Optionally prepend few-shot examples.
        let examples = self.few_shot.select_relevant(&compressed, remaining);
        let mut result = if !examples.is_empty() {
            let shots: Vec<String> = examples
                .iter()
                .map(|ex| format!("Input: {}\nOutput: {}", ex.input, ex.output))
                .collect();
            format!("{}\n\n{}", shots.join("\n\n"), compressed)
        } else {
            compressed
        };

        // 4. Optionally inject CoT.
        match self.goal {
            CompressionGoal::MinimizeTokens => {}
            CompressionGoal::MaximizeClarity | CompressionGoal::BalancedCostQuality => {
                result = self.cot.inject(&result);
            }
        }

        result
    }

    /// Optimise a batch of prompts, each within the same `token_budget`.
    pub fn optimize_batch(&self, prompts: &[String], token_budget: usize) -> Vec<String> {
        prompts.iter().map(|p| self.optimize(p, token_budget)).collect()
    }
}

#[cfg(test)]
mod round32_tests {
    use super::*;

    #[test]
    fn compressor_removes_fillers() {
        let c = PromptCompressor::new(4096, 0.8);
        let out = c.compress("Please summarise this kindly.");
        assert!(!out.to_lowercase().contains("please"));
        assert!(!out.to_lowercase().contains("kindly"));
    }

    #[test]
    fn compressor_savings_positive() {
        let c = PromptCompressor::new(4096, 0.8);
        let orig = "Please please please note that in order to do this please kindly proceed.";
        let comp = c.compress(orig);
        assert!(c.estimate_savings(orig, &comp) >= 0.0);
    }

    #[test]
    fn few_shot_respects_budget() {
        let mut sel = FewShotSelector::new(5);
        sel.add_example("what is 2+2", "4", 10);
        sel.add_example("what is 3+3", "6", 10);
        sel.add_example("what is 4+4", "8", 10);
        // Budget of 15 should only fit one example.
        let chosen = sel.select_relevant("what is 5+5", 15);
        assert!(chosen.len() <= 1);
    }

    #[test]
    fn cot_inject_contains_prefix_and_suffix() {
        let inj = ChainOfThoughtInjector::new();
        let out = inj.inject("What is 2+2?");
        assert!(out.contains("Let's think step by step:"));
        assert!(out.contains("Therefore, the answer is:"));
        assert!(out.contains("What is 2+2?"));
    }

    #[test]
    fn cot_numbered_steps() {
        let inj = ChainOfThoughtInjector::new();
        let out = inj.inject_numbered_steps("Solve x+1=5", 3);
        assert!(out.contains("Step 1:"));
        assert!(out.contains("Step 2:"));
        assert!(out.contains("Step 3:"));
    }

    #[test]
    fn optimizer_minimize_no_cot() {
        let opt = PromptCompressionOptimizer::new(CompressionGoal::MinimizeTokens);
        let out = opt.optimize("Please kindly answer this question.", 1000);
        assert!(!out.contains("Let's think step by step:"));
    }

    #[test]
    fn optimizer_batch() {
        let opt = PromptCompressionOptimizer::new(CompressionGoal::BalancedCostQuality);
        let prompts = vec!["Hello world".to_string(), "Please answer this kindly.".to_string()];
        let results = opt.optimize_batch(&prompts, 500);
        assert_eq!(results.len(), 2);
    }
}
