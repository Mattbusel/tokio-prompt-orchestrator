//! Pre-flight cost estimation for LLM requests.
//!
//! Provides token-count heuristics, per-model pricing, and budget-checking
//! before a request is dispatched so that callers can decide whether to
//! proceed, choose a cheaper model, or reject the request entirely.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Task type
// ---------------------------------------------------------------------------

/// Characterises the kind of work being requested, which influences the
/// expected output token count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskType {
    /// Translate text from one language to another.
    Translation,
    /// Condense a longer document into a short summary.
    Summarization,
    /// Generate source code from a natural-language description.
    CodeGeneration,
    /// Answer a factual or reasoning question.
    QuestionAnswering,
    /// Categorise input into one of several labels.
    Classification,
    /// Open-ended creative writing.
    Creative,
}

// ---------------------------------------------------------------------------
// Token / cost value types
// ---------------------------------------------------------------------------

/// Input/output token count estimate for a request.
#[derive(Debug, Clone)]
pub struct TokenEstimate {
    /// Estimated number of input (prompt) tokens.
    pub input_tokens: usize,
    /// Estimated number of output (completion) tokens.
    pub output_tokens: usize,
    /// Model the estimate is scoped to.
    pub model: String,
}

/// Full cost estimate including budget compliance.
#[derive(Debug, Clone)]
pub struct CostEstimate {
    /// Estimated cost in US dollars.
    pub estimated_cost_usd: f64,
    /// Underlying token counts.
    pub token_estimate: TokenEstimate,
    /// Confidence in the estimate (0.0–1.0).  Lower when the model is
    /// unknown or the text is very short.
    pub confidence: f64,
    /// Whether the estimate fits within the per-request budget limit.
    pub within_budget: bool,
}

// ---------------------------------------------------------------------------
// Budget configuration
// ---------------------------------------------------------------------------

/// Spending limits used to check whether a request is within budget.
#[derive(Debug, Clone)]
pub struct BudgetConfig {
    /// Maximum cumulative spend per calendar day (USD).
    pub daily_limit_usd: f64,
    /// Maximum cost for a single request (USD).
    pub per_request_limit_usd: f64,
    /// Maximum cumulative spend per calendar month (USD).
    pub monthly_limit_usd: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_limit_usd: 10.0,
            per_request_limit_usd: 0.50,
            monthly_limit_usd: 200.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal pricing table
// ---------------------------------------------------------------------------

/// Per-1 M token pricing (input, output) in USD.
struct ModelPrice {
    input_per_1m: f64,
    output_per_1m: f64,
}

fn pricing_table() -> HashMap<&'static str, ModelPrice> {
    let mut m = HashMap::new();
    m.insert(
        "gpt-4o",
        ModelPrice {
            input_per_1m: 5.00,
            output_per_1m: 15.00,
        },
    );
    m.insert(
        "gpt-4o-mini",
        ModelPrice {
            input_per_1m: 0.15,
            output_per_1m: 0.60,
        },
    );
    m.insert(
        "claude-3-5-sonnet",
        ModelPrice {
            input_per_1m: 3.00,
            output_per_1m: 15.00,
        },
    );
    m.insert(
        "claude-3-haiku",
        ModelPrice {
            input_per_1m: 0.25,
            output_per_1m: 1.25,
        },
    );
    m.insert(
        "gemini-1.5-pro",
        ModelPrice {
            input_per_1m: 3.50,
            output_per_1m: 10.50,
        },
    );
    m
}

/// Canonical model names in ascending cost order (input price).
const MODELS_BY_COST: &[&str] = &[
    "gpt-4o-mini",
    "claude-3-haiku",
    "gemini-1.5-pro",
    "claude-3-5-sonnet",
    "gpt-4o",
];

// ---------------------------------------------------------------------------
// CostEstimator
// ---------------------------------------------------------------------------

/// Pre-flight cost estimation engine.
///
/// Thread-safe — all methods take `&self` and the pricing table is built once
/// on construction.
pub struct CostEstimator {
    prices: HashMap<&'static str, ModelPrice>,
}

impl CostEstimator {
    /// Create a new estimator with the built-in pricing table.
    pub fn new() -> Self {
        Self {
            prices: pricing_table(),
        }
    }

    // -----------------------------------------------------------------------
    // Token counting heuristics
    // -----------------------------------------------------------------------

    /// Estimate the number of tokens in `text` using the ~4 chars/token rule.
    pub fn estimate_tokens(text: &str) -> usize {
        let chars = text.chars().count();
        (chars / 4).max(1)
    }

    /// Estimate the expected number of output tokens given the input count and
    /// the nature of the task.
    pub fn estimate_output_tokens(input_tokens: usize, task_type: TaskType) -> usize {
        match task_type {
            TaskType::Classification => 10.min(input_tokens),
            TaskType::Summarization => (input_tokens / 4).max(50),
            TaskType::QuestionAnswering => (input_tokens / 3).max(30).min(500),
            TaskType::Translation => input_tokens,
            TaskType::CodeGeneration => (input_tokens * 2).max(100),
            TaskType::Creative => (input_tokens * 3).max(200),
        }
    }

    // -----------------------------------------------------------------------
    // Cost computation helpers
    // -----------------------------------------------------------------------

    fn compute_cost(&self, input_tokens: usize, output_tokens: usize, model: &str) -> (f64, f64) {
        // Look up by exact string match against the static key.
        let price = self.prices.iter().find(|(k, _)| **k == model);
        match price {
            Some((_, p)) => {
                let cost = (input_tokens as f64 / 1_000_000.0) * p.input_per_1m
                    + (output_tokens as f64 / 1_000_000.0) * p.output_per_1m;
                (cost, 0.90) // confidence high for known model
            }
            None => {
                // Unknown model: fall back to gpt-4o pricing, lower confidence.
                let fallback = self.prices.get("gpt-4o").expect("gpt-4o always present");
                let cost = (input_tokens as f64 / 1_000_000.0) * fallback.input_per_1m
                    + (output_tokens as f64 / 1_000_000.0) * fallback.output_per_1m;
                (cost, 0.40)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Estimate the cost of a single prompt/model/task combination.
    pub fn estimate_cost(
        &self,
        prompt: &str,
        model: &str,
        task_type: TaskType,
        budget: &BudgetConfig,
    ) -> CostEstimate {
        let input_tokens = Self::estimate_tokens(prompt);
        let output_tokens = Self::estimate_output_tokens(input_tokens, task_type);
        let (cost, confidence) = self.compute_cost(input_tokens, output_tokens, model);

        CostEstimate {
            estimated_cost_usd: cost,
            token_estimate: TokenEstimate {
                input_tokens,
                output_tokens,
                model: model.to_string(),
            },
            confidence,
            within_budget: cost <= budget.per_request_limit_usd,
        }
    }

    /// Estimate costs for a batch of prompts with a shared model and task type.
    ///
    /// Each prompt is estimated independently; no budget is enforced here since
    /// the caller typically aggregates totals and applies limits externally.
    pub fn batch_estimate(
        &self,
        prompts: &[&str],
        model: &str,
        task_type: TaskType,
    ) -> Vec<CostEstimate> {
        let budget = BudgetConfig {
            per_request_limit_usd: f64::MAX,
            ..BudgetConfig::default()
        };
        prompts
            .iter()
            .map(|p| self.estimate_cost(p, model, task_type, &budget))
            .collect()
    }

    /// Return the name of the cheapest known model whose per-request cost for
    /// `prompt` fits within `budget_usd`, or `None` if none qualifies.
    pub fn cheapest_model_for_budget(&self, prompt: &str, budget_usd: f64) -> Option<String> {
        let input_tokens = Self::estimate_tokens(prompt);
        // Use QuestionAnswering as a neutral default for model selection.
        let output_tokens = Self::estimate_output_tokens(input_tokens, TaskType::QuestionAnswering);

        for model_name in MODELS_BY_COST {
            let (cost, _) = self.compute_cost(input_tokens, output_tokens, model_name);
            if cost <= budget_usd {
                return Some(model_name.to_string());
            }
        }
        None
    }
}

impl Default for CostEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(CostEstimator::estimate_tokens("hello world"), 2);
        assert_eq!(CostEstimator::estimate_tokens(""), 1); // minimum 1
    }

    #[test]
    fn test_estimate_cost_known_model() {
        let est = CostEstimator::new();
        let budget = BudgetConfig::default();
        let result = est.estimate_cost("Some prompt text here.", "gpt-4o-mini", TaskType::Classification, &budget);
        assert!(result.estimated_cost_usd >= 0.0);
        assert!(result.confidence > 0.5);
    }

    #[test]
    fn test_estimate_cost_unknown_model() {
        let est = CostEstimator::new();
        let budget = BudgetConfig::default();
        let result = est.estimate_cost("Some text", "unknown-model-xyz", TaskType::Summarization, &budget);
        assert!(result.confidence < 0.5);
    }

    #[test]
    fn test_batch_estimate_length() {
        let est = CostEstimator::new();
        let prompts = vec!["hello", "world", "foo"];
        let results = est.batch_estimate(&prompts, "gpt-4o", TaskType::QuestionAnswering);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_cheapest_model() {
        let est = CostEstimator::new();
        // Any small prompt should have a cheapest model.
        let model = est.cheapest_model_for_budget("hello", 1.0);
        assert!(model.is_some());
    }

    #[test]
    fn test_within_budget_flag() {
        let est = CostEstimator::new();
        let tight = BudgetConfig {
            per_request_limit_usd: 0.0,
            ..BudgetConfig::default()
        };
        let result = est.estimate_cost("test", "gpt-4o", TaskType::CodeGeneration, &tight);
        assert!(!result.within_budget);
    }
}
