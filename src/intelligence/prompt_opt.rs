#![allow(
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    clippy::redundant_closure,
    clippy::derivable_impls,
    clippy::unwrap_or_default,
    dead_code,
    private_interfaces
)]
#![cfg(feature = "intelligence")]

//! # Stage: Prompt Optimizer
//! Applies learned transformations to prompts to reduce token use and improve quality.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Errors returned by [`PromptOptimizer`] transformation methods.
#[derive(Debug, Error)]
pub enum PromptOptimizerError {
    /// A specific transformation step failed; the inner string describes the cause.
    #[error("transform failed: {0}")]
    TransformFailed(String),
    /// The history store could not persist a transform record.
    #[error("failed to store transform record")]
    StoreFailed,
}

/// Learned prompt transformation strategies.
///
/// Each variant describes a distinct rewrite that the [`PromptOptimizer`] can
/// apply to reduce token count or improve response quality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OptimizationStrategy {
    /// Truncate or compress the system prompt to stay under a token budget.
    SystemPromptCompression,
    /// Reorder instructions to improve model instruction-following.
    InstructionReordering,
    /// Remove semantically redundant sentences from the user prompt.
    RedundancyRemoval,
    /// Placeholder: returns input unchanged. TODO: implement few-shot example retrieval.
    ///
    /// This variant is a placeholder. Enabling it has no effect on the prompt.
    // TODO: implement ExampleInjection by selecting and prepending relevant
    // few-shot examples from a retrieval store.
    #[doc = "Not yet implemented — returns input unchanged"]
    ExampleInjection,
}

/// A record of a single prompt transformation, used to track effectiveness.
#[derive(Debug, Clone)]
pub struct TransformRecord {
    /// Which strategy produced this record.
    pub strategy: OptimizationStrategy,
    /// Byte length of the prompt before transformation.
    pub original_len: usize,
    /// Byte length of the prompt after transformation.
    pub optimized_len: usize,
    /// Estimated quality change (positive = better, negative = worse).
    pub quality_delta: f64,
    /// Number of times this strategy has been applied in this session.
    pub applied_count: u64,
}

/// Configuration for the [`PromptOptimizer`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptOptimizerConfig {
    /// Maximum byte length of the system prompt before compression is applied.
    pub max_system_prompt_len: usize,
    /// Similarity score above which a sentence is considered redundant (0.0–1.0).
    pub redundancy_threshold: f64,
    /// Which strategies are active for this optimizer instance.
    pub enabled_strategies: Vec<OptimizationStrategy>,
}
impl Default for PromptOptimizerConfig {
    fn default() -> Self {
        Self {
            max_system_prompt_len: 2000,
            redundancy_threshold: 0.85,
            enabled_strategies: vec![
                OptimizationStrategy::SystemPromptCompression,
                OptimizationStrategy::RedundancyRemoval,
            ],
        }
    }
}

pub struct PromptOptimizer {
    history: Arc<Mutex<Vec<TransformRecord>>>,
    config: PromptOptimizerConfig,
    /// Few-shot example store: (input, output) pairs used by ExampleInjection.
    examples: Arc<Mutex<Vec<(String, String)>>>,
}
impl PromptOptimizer {
    pub fn new(config: PromptOptimizerConfig) -> Self {
        Self {
            history: Arc::new(Mutex::new(Vec::new())),
            config,
            examples: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Add a (input, output) few-shot example to the retrieval store.
    pub fn add_example(&self, input: impl Into<String>, output: impl Into<String>) {
        if let Ok(mut guard) = self.examples.lock() {
            guard.push((input.into(), output.into()));
        }
    }

    /// Word-set Jaccard similarity between two strings.
    ///
    /// Splits both strings on whitespace, builds word-sets, then returns
    /// |intersection| / |union|.  Returns 0.0 when both sets are empty.
    fn jaccard_similarity(a: &str, b: &str) -> f64 {
        use std::collections::HashSet;
        let set_a: HashSet<&str> = a.split_whitespace().collect();
        let set_b: HashSet<&str> = b.split_whitespace().collect();
        if set_a.is_empty() && set_b.is_empty() {
            return 0.0;
        }
        let intersection = set_a.intersection(&set_b).count() as f64;
        let union = set_a.union(&set_b).count() as f64;
        if union == 0.0 {
            0.0
        } else {
            intersection / union
        }
    }

    /// Retrieve the top-`k` most similar few-shot examples for `query`.
    ///
    /// Returns a `Vec` of `(input, output)` pairs sorted by descending Jaccard
    /// similarity.  If `k` exceeds the number of stored examples the full store
    /// is returned.
    pub fn retrieve_examples(&self, query: &str, k: usize) -> Vec<(String, String)> {
        let guard = match self.examples.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        if guard.is_empty() || k == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(f64, &(String, String))> = guard
            .iter()
            .map(|ex| (Self::jaccard_similarity(query, &ex.0), ex))
            .collect();
        // Sort descending by score; stable sort for reproducibility.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(k)
            .map(|(_, ex)| ex.clone())
            .collect()
    }

    /// Format retrieved examples as a preamble prepended to the system prompt.
    ///
    /// Each example becomes an "Input: … / Output: …" block separated by
    /// blank lines.  Returns an empty string when `examples` is empty.
    fn format_examples(examples: &[(String, String)]) -> String {
        if examples.is_empty() {
            return String::new();
        }
        let mut parts = vec!["### Few-shot examples".to_string()];
        for (i, (input, output)) in examples.iter().enumerate() {
            parts.push(format!(
                "Example {}:\nInput: {}\nOutput: {}",
                i + 1,
                input,
                output
            ));
        }
        parts.push("### End of examples".to_string());
        parts.join("\n\n")
    }

    fn compress_system_prompt(&self, prompt: &str) -> String {
        let limit = self.config.max_system_prompt_len;
        if prompt.len() <= limit {
            return prompt.to_string();
        }
        let truncated = &prompt[..limit];
        match truncated.rfind(|c: char| c.is_whitespace()) {
            Some(pos) => truncated[..pos].to_string(),
            None => truncated.to_string(),
        }
    }

    fn remove_redundancy(&self, prompt: &str) -> String {
        let sentences: Vec<&str> = prompt.split('.').collect();
        let mut result: Vec<&str> = Vec::new();
        for s in &sentences {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                result.push(s);
                continue;
            }
            let threshold = self.config.redundancy_threshold;
            let is_dup = result.iter().any(|prev| {
                let pt = prev.trim();
                if pt.is_empty() {
                    return false;
                }
                let shorter = pt.len().min(trimmed.len()) as f64;
                let common = pt
                    .chars()
                    .zip(trimmed.chars())
                    .filter(|(a, b)| a == b)
                    .count() as f64;
                common / shorter >= threshold
            });
            if !is_dup {
                result.push(s);
            }
        }
        result.join(".")
    }

    fn reorder_instructions(&self, prompt: &str) -> String {
        let mut imperatives: Vec<&str> = Vec::new();
        let mut others: Vec<&str> = Vec::new();
        for line in prompt.lines() {
            let lower = line.to_lowercase();
            if lower.starts_with("always")
                || lower.starts_with("never")
                || lower.starts_with("do ")
                || lower.starts_with("make ")
                || lower.starts_with("ensure")
                || lower.starts_with("you must")
            {
                imperatives.push(line);
            } else {
                others.push(line);
            }
        }
        imperatives.extend(others);
        imperatives.join(
            "
",
        )
    }

    pub fn optimize(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<(String, String), PromptOptimizerError> {
        let mut sys = system_prompt.to_string();
        let usr = user_prompt.to_string();
        for strategy in &self.config.enabled_strategies {
            match strategy {
                OptimizationStrategy::SystemPromptCompression => {
                    let orig = sys.len();
                    sys = self.compress_system_prompt(&sys);
                    debug!(orig, new = sys.len(), "SystemPromptCompression applied");
                    self.record_transform(strategy.clone(), orig, sys.len());
                }
                OptimizationStrategy::RedundancyRemoval => {
                    let orig = sys.len();
                    sys = self.remove_redundancy(&sys);
                    debug!(orig, new = sys.len(), "RedundancyRemoval applied");
                    self.record_transform(strategy.clone(), orig, sys.len());
                }
                OptimizationStrategy::InstructionReordering => {
                    let orig = sys.len();
                    sys = self.reorder_instructions(&sys);
                    debug!(orig, new = sys.len(), "InstructionReordering applied");
                    self.record_transform(strategy.clone(), orig, sys.len());
                }
                OptimizationStrategy::ExampleInjection => {
                    // Retrieve the top-3 most relevant few-shot examples by
                    // Jaccard word-overlap similarity to the user prompt, then
                    // prepend them to the system prompt as a preamble.
                    let top_k = self.retrieve_examples(&usr, 3);
                    if !top_k.is_empty() {
                        let preamble = Self::format_examples(&top_k);
                        let orig = sys.len();
                        sys = format!("{}\n\n{}", preamble, sys);
                        debug!(
                            orig,
                            new = sys.len(),
                            examples = top_k.len(),
                            "ExampleInjection applied"
                        );
                        self.record_transform(strategy.clone(), orig, sys.len());
                    } else {
                        warn!(
                            "prompt_opt: ExampleInjection enabled but no examples in store \
                             (few-shot retrieval not yet implemented) — returning input unchanged"
                        );
                    }
                }
            }
        }
        info!(sys_len = sys.len(), usr_len = usr.len(), "prompt optimized");
        Ok((sys, usr))
    }

    fn record_transform(
        &self,
        strategy: OptimizationStrategy,
        original_len: usize,
        optimized_len: usize,
    ) {
        let mut guard = match self.history.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(rec) = guard.iter_mut().find(|r| r.strategy == strategy) {
            rec.applied_count += 1;
            rec.original_len = original_len;
            rec.optimized_len = optimized_len;
        } else {
            guard.push(TransformRecord {
                strategy,
                original_len,
                optimized_len,
                quality_delta: 0.0,
                applied_count: 1,
            });
        }
    }

    pub fn record_quality_delta(&self, strategy: OptimizationStrategy, delta: f64) {
        let mut guard = match self.history.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(rec) = guard.iter_mut().find(|r| r.strategy == strategy) {
            rec.quality_delta = delta;
        } else {
            guard.push(TransformRecord {
                strategy,
                original_len: 0,
                optimized_len: 0,
                quality_delta: delta,
                applied_count: 0,
            });
        }
    }

    pub fn best_strategy(&self) -> Option<OptimizationStrategy> {
        let guard = self.history.lock().ok()?;
        guard
            .iter()
            .max_by(|a, b| {
                a.quality_delta
                    .partial_cmp(&b.quality_delta)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|r| r.strategy.clone())
    }

    pub fn compression_ratio(&self) -> f64 {
        let guard = match self.history.lock() {
            Ok(g) => g,
            Err(_) => return 1.0,
        };
        let records: Vec<&TransformRecord> = guard.iter().filter(|r| r.original_len > 0).collect();
        if records.is_empty() {
            return 1.0;
        }
        let sum: f64 = records
            .iter()
            .map(|r| r.optimized_len as f64 / r.original_len as f64)
            .sum();
        sum / records.len() as f64
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn default_optimizer() -> PromptOptimizer {
        PromptOptimizer::new(PromptOptimizerConfig::default())
    }

    #[test]
    fn test_default_max_system_prompt_len() {
        assert_eq!(PromptOptimizerConfig::default().max_system_prompt_len, 2000);
    }

    #[test]
    fn test_optimize_short_prompt_unchanged() {
        let opt = default_optimizer();
        let sys = "short prompt";
        let (out_sys, _) = opt.optimize(sys, "user").unwrap();
        assert!(out_sys.contains("short"));
    }

    #[test]
    fn test_compress_truncates_at_word_boundary() {
        let config = PromptOptimizerConfig {
            max_system_prompt_len: 10,
            enabled_strategies: vec![OptimizationStrategy::SystemPromptCompression],
            ..PromptOptimizerConfig::default()
        };
        let opt = PromptOptimizer::new(config);
        let (out, _) = opt.optimize("hello world foo bar", "u").unwrap();
        assert!(out.len() <= 10);
    }

    #[test]
    fn test_compression_ratio_less_than_one_after_truncation() {
        let config = PromptOptimizerConfig {
            max_system_prompt_len: 5,
            enabled_strategies: vec![OptimizationStrategy::SystemPromptCompression],
            ..PromptOptimizerConfig::default()
        };
        let opt = PromptOptimizer::new(config);
        opt.optimize("hello world", "u").unwrap();
        assert!(opt.compression_ratio() < 1.0);
    }

    #[test]
    fn test_redundancy_removal_deduplicates_sentences() {
        let config = PromptOptimizerConfig {
            enabled_strategies: vec![OptimizationStrategy::RedundancyRemoval],
            ..PromptOptimizerConfig::default()
        };
        let opt = PromptOptimizer::new(config);
        let prompt = "Be helpful. Be helpful. Answer clearly.";
        let (out, _) = opt.optimize(prompt, "u").unwrap();
        assert!(out.len() <= prompt.len());
    }

    #[test]
    fn test_example_injection_is_noop() {
        let config = PromptOptimizerConfig {
            enabled_strategies: vec![OptimizationStrategy::ExampleInjection],
            ..PromptOptimizerConfig::default()
        };
        let opt = PromptOptimizer::new(config);
        let sys = "be helpful";
        let (out, _) = opt.optimize(sys, "u").unwrap();
        assert_eq!(out, sys);
    }

    #[test]
    fn test_instruction_reordering_puts_imperatives_first() {
        let config = PromptOptimizerConfig {
            enabled_strategies: vec![OptimizationStrategy::InstructionReordering],
            ..PromptOptimizerConfig::default()
        };
        let opt = PromptOptimizer::new(config);
        let sys = "context line
always be helpful
more context";
        let (out, _) = opt.optimize(sys, "u").unwrap();
        let first_line = out.lines().next().unwrap_or("");
        assert!(first_line.to_lowercase().starts_with("always"));
    }

    #[test]
    fn test_record_quality_delta_stores_value() {
        let opt = default_optimizer();
        opt.record_quality_delta(OptimizationStrategy::SystemPromptCompression, 0.8);
        let best = opt.best_strategy().unwrap();
        assert_eq!(best, OptimizationStrategy::SystemPromptCompression);
    }

    #[test]
    fn test_best_strategy_returns_highest_delta() {
        let opt = default_optimizer();
        opt.record_quality_delta(OptimizationStrategy::SystemPromptCompression, 0.3);
        opt.record_quality_delta(OptimizationStrategy::RedundancyRemoval, 0.7);
        let best = opt.best_strategy().unwrap();
        assert_eq!(best, OptimizationStrategy::RedundancyRemoval);
    }

    #[test]
    fn test_best_strategy_none_when_no_history() {
        let opt = PromptOptimizer::new(PromptOptimizerConfig {
            enabled_strategies: vec![],
            ..PromptOptimizerConfig::default()
        });
        assert!(opt.best_strategy().is_none());
    }

    #[test]
    fn test_compression_ratio_one_when_no_transforms() {
        let opt = PromptOptimizer::new(PromptOptimizerConfig {
            enabled_strategies: vec![],
            ..PromptOptimizerConfig::default()
        });
        assert!((opt.compression_ratio() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_optimize_returns_user_prompt_unchanged() {
        let opt = default_optimizer();
        let user = "what is the capital of France";
        let (_, out_usr) = opt.optimize("sys", user).unwrap();
        assert_eq!(out_usr, user);
    }

    #[test]
    fn test_multiple_strategies_applied_in_order() {
        let config = PromptOptimizerConfig {
            max_system_prompt_len: 100,
            enabled_strategies: vec![
                OptimizationStrategy::SystemPromptCompression,
                OptimizationStrategy::RedundancyRemoval,
            ],
            ..PromptOptimizerConfig::default()
        };
        let opt = PromptOptimizer::new(config);
        let (out, _) = opt
            .optimize("A long system prompt with content.", "user")
            .unwrap();
        assert!(!out.is_empty());
    }
}
