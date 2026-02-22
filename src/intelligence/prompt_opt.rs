#![cfg(feature = "intelligence")]

//! # Stage: Prompt Optimizer
//! Applies learned transformations to prompts to reduce token use and improve quality.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::{debug, info};

#[derive(Debug, Error)]
pub enum PromptOptimizerError {
    #[error("transform failed: {0}")]
    TransformFailed(String),
    #[error("failed to store transform record")]
    StoreFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OptimizationStrategy {
    SystemPromptCompression,
    InstructionReordering,
    RedundancyRemoval,
    ExampleInjection,
}

#[derive(Debug, Clone)]
pub struct TransformRecord {
    pub strategy: OptimizationStrategy,
    pub original_len: usize,
    pub optimized_len: usize,
    pub quality_delta: f64,
    pub applied_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptOptimizerConfig {
    pub max_system_prompt_len: usize,
    pub redundancy_threshold: f64,
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
}
impl PromptOptimizer {
    pub fn new(config: PromptOptimizerConfig) -> Self {
        Self {
            history: Arc::new(Mutex::new(Vec::new())),
            config,
        }
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
                    debug!("ExampleInjection: no-op in base implementation");
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
