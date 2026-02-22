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
//! # Cost Optimizer (Task 1.5)
//!
//! ## Responsibility
//! Track per-model inference costs and recommend routing shifts when the
//! budget nears its ceiling. Maintains a sliding window of cost observations
//! and projects future spend based on current trajectory.
//!
//! ## Guarantees
//! - Thread-safe: all operations safe under concurrent access
//! - Bounded memory: cost windows capped at configurable size
//! - Non-blocking: all computations are O(window_size)
//! - Graceful: recommendations are advisory, never forced
//!
//! ## NOT Responsible For
//! - Executing routing changes (that's the router's job)
//! - Token counting (expects pre-computed costs from workers)
//! - Billing integration (in-process tracking only)

#![cfg(feature = "self-tune")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors that can occur during cost optimization operations.
#[derive(Debug, Error)]
pub enum CostError {
    /// The internal mutex was poisoned by a panicking thread.
    #[error("internal lock poisoned")]
    LockPoisoned,

    /// The requested model was not found in the cost tracker.
    #[error("model not found: {0}")]
    ModelNotFound(String),

    /// The provided budget value is invalid (zero or negative).
    #[error("invalid budget: {0}")]
    InvalidBudget(String),
}

// ─── BudgetConfig ────────────────────────────────────────────────────────────

/// Configuration for the budget tracking period and alert thresholds.
///
/// Defines the total budget, the period over which it applies, and the
/// thresholds at which warning and critical alerts are raised.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Total budget in US dollars for the period.
    pub total_budget: f64,
    /// Budget period duration in seconds (default: 86400 = 1 day).
    pub period_secs: u64,
    /// Fraction of budget that triggers a warning alert (default: 0.80).
    pub warning_threshold: f64,
    /// Fraction of budget that triggers a critical alert (default: 0.95).
    pub critical_threshold: f64,
    /// Optional per-model spending caps in US dollars.
    pub per_model_limits: HashMap<String, f64>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            total_budget: 100.0,
            period_secs: 86400,
            warning_threshold: 0.80,
            critical_threshold: 0.95,
            per_model_limits: HashMap::new(),
        }
    }
}

// ─── CostEntry ───────────────────────────────────────────────────────────────

/// A single cost observation from an inference request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostEntry {
    /// The model identifier that served the request.
    pub model: String,
    /// The cost in US dollars for this request.
    pub cost_usd: f64,
    /// Number of input tokens consumed.
    pub tokens_in: u64,
    /// Number of output tokens produced.
    pub tokens_out: u64,
    /// Timestamp of the observation in seconds since an epoch.
    pub timestamp_secs: u64,
}

// ─── ModelCostSummary ────────────────────────────────────────────────────────

/// Aggregated cost summary for a single model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCostSummary {
    /// The model identifier.
    pub model: String,
    /// Total cost in US dollars across all requests.
    pub total_cost_usd: f64,
    /// Total number of requests served.
    pub total_requests: u64,
    /// Average cost per request in US dollars.
    pub avg_cost_per_request: f64,
    /// Cost per 1,000 input tokens in US dollars.
    pub cost_per_1k_tokens_in: f64,
    /// Cost per 1,000 output tokens in US dollars.
    pub cost_per_1k_tokens_out: f64,
    /// Fraction of the total budget consumed by this model.
    pub budget_fraction: f64,
}

// ─── BudgetAlert ─────────────────────────────────────────────────────────────

/// Alert level for the current budget status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetAlert {
    /// Spending is on track and within budget.
    Normal,
    /// Spending is approaching the budget limit.
    Warning,
    /// Spending is at or very near the budget limit.
    Critical,
    /// Spending has exceeded the budget.
    Exceeded,
}

// ─── BudgetStatus ────────────────────────────────────────────────────────────

/// Current budget status including spend totals, projections, and alerts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetStatus {
    /// Total amount spent in US dollars so far.
    pub total_spent: f64,
    /// Total budget in US dollars for the period.
    pub total_budget: f64,
    /// Fraction of budget spent (spent / budget).
    pub budget_fraction: f64,
    /// Projected total spend if the current rate continues.
    pub projected_spend: f64,
    /// Fraction of the budget period that has elapsed.
    pub period_elapsed_fraction: f64,
    /// Current alert level based on spending.
    pub status: BudgetAlert,
    /// Per-model cost breakdowns.
    pub per_model: Vec<ModelCostSummary>,
}

// ─── CostAction ──────────────────────────────────────────────────────────────

/// An actionable cost optimization step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostAction {
    /// Reduce usage of the named model.
    ReduceUsage(String),
    /// Shift traffic from an expensive model to a cheaper one.
    ShiftTraffic {
        /// The model to shift traffic away from.
        from: String,
        /// The model to shift traffic toward.
        to: String,
    },
    /// Reduce the overall request rate.
    ThrottleRequests,
    /// No action needed; budget is healthy.
    NoAction,
}

// ─── CostRecommendation ─────────────────────────────────────────────────────

/// A cost optimization recommendation with rationale and estimated savings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecommendation {
    /// The recommended action.
    pub action: CostAction,
    /// Human-readable reason for the recommendation.
    pub reason: String,
    /// Estimated savings in US dollars if this recommendation is followed.
    pub estimated_savings_usd: f64,
    /// The model to shift traffic away from, if applicable.
    pub model_from: Option<String>,
    /// The model to shift traffic toward, if applicable.
    pub model_to: Option<String>,
}

// ─── CostInner ───────────────────────────────────────────────────────────────

/// Internal mutable state for the cost optimizer, protected by a mutex.
struct CostInner {
    /// Budget configuration.
    config: BudgetConfig,
    /// Sliding window of cost entries.
    entries: Vec<CostEntry>,
    /// Maximum number of entries to retain.
    max_entries: usize,
    /// Start of the current budget period in seconds.
    period_start_secs: u64,
    /// Known pricing rates per model (USD per 1,000 tokens).
    model_rates: HashMap<String, f64>,
}

// ─── CostOptimizer ──────────────────────────────────────────────────────────

/// Budget-aware cost optimizer that tracks per-model inference costs and
/// recommends routing shifts when the budget nears its ceiling.
///
/// This type is cheaply cloneable and shares state across clones via
/// `Arc<Mutex<CostInner>>`.
#[derive(Clone)]
pub struct CostOptimizer {
    inner: Arc<Mutex<CostInner>>,
}

impl std::fmt::Debug for CostOptimizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CostOptimizer")
            .field("inner", &"Arc<Mutex<CostInner>>")
            .finish()
    }
}

impl CostOptimizer {
    /// Create a new cost optimizer with the given budget configuration.
    ///
    /// # Arguments
    /// * `config` — Budget configuration including thresholds and limits.
    ///
    /// # Returns
    /// - `Ok(CostOptimizer)` — if the budget is valid (positive).
    /// - `Err(CostError::InvalidBudget)` — if the budget is zero or negative.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: BudgetConfig) -> Result<Self, CostError> {
        if config.total_budget <= 0.0 {
            return Err(CostError::InvalidBudget(format!(
                "total_budget must be positive, got {}",
                config.total_budget
            )));
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(CostInner {
                config,
                entries: Vec::new(),
                max_entries: 10_000,
                period_start_secs: 0,
                model_rates: HashMap::new(),
            })),
        })
    }

    /// Create a cost optimizer with default budget configuration.
    ///
    /// Uses `BudgetConfig::default()` which sets a $100 daily budget.
    ///
    /// # Panics
    /// This function never panics.
    pub fn with_defaults() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CostInner {
                config: BudgetConfig::default(),
                entries: Vec::new(),
                max_entries: 10_000,
                period_start_secs: 0,
                model_rates: HashMap::new(),
            })),
        }
    }

    /// Record a cost entry from a completed inference request.
    ///
    /// If the number of entries exceeds `max_entries`, the oldest entry is
    /// removed to maintain bounded memory usage.
    ///
    /// # Arguments
    /// * `entry` — The cost observation to record.
    ///
    /// # Panics
    /// This function never panics.
    pub fn record_cost(&self, entry: CostEntry) -> Result<(), CostError> {
        let mut inner = self.inner.lock().map_err(|_| CostError::LockPoisoned)?;
        if inner.entries.len() >= inner.max_entries {
            inner.entries.remove(0);
        }
        inner.entries.push(entry);
        Ok(())
    }

    /// Set the known pricing rate for a model.
    ///
    /// # Arguments
    /// * `model` — The model identifier.
    /// * `rate_per_1k_tokens` — Cost in US dollars per 1,000 tokens.
    ///
    /// # Panics
    /// This function never panics.
    pub fn set_model_rate(&self, model: &str, rate_per_1k_tokens: f64) -> Result<(), CostError> {
        let mut inner = self.inner.lock().map_err(|_| CostError::LockPoisoned)?;
        inner
            .model_rates
            .insert(model.to_string(), rate_per_1k_tokens);
        Ok(())
    }

    /// Compute the current budget status including spend totals, projections,
    /// per-model breakdowns, and the current alert level.
    ///
    /// # Returns
    /// A `BudgetStatus` snapshot of the current spending state.
    ///
    /// # Panics
    /// This function never panics.
    pub fn budget_status(&self) -> Result<BudgetStatus, CostError> {
        let inner = self.inner.lock().map_err(|_| CostError::LockPoisoned)?;

        let total_spent: f64 = inner.entries.iter().map(|e| e.cost_usd).sum();
        let total_budget = inner.config.total_budget;
        let budget_fraction = if total_budget > 0.0 {
            total_spent / total_budget
        } else {
            1.0
        };

        // Compute elapsed time and projection
        let (period_elapsed_fraction, projected_spend) = Self::compute_projection(
            &inner.entries,
            inner.period_start_secs,
            inner.config.period_secs,
            total_spent,
        );

        let status = Self::classify_alert(budget_fraction, &inner.config);

        let per_model = Self::build_model_summaries(&inner.entries, total_budget);

        Ok(BudgetStatus {
            total_spent,
            total_budget,
            budget_fraction,
            projected_spend,
            period_elapsed_fraction,
            status,
            per_model,
        })
    }

    /// Generate cost optimization recommendations based on the current budget
    /// status and spending patterns.
    ///
    /// # Returns
    /// A vector of recommendations. At minimum contains one entry (which may
    /// be `NoAction` if the budget is healthy).
    ///
    /// # Panics
    /// This function never panics.
    pub fn recommendations(&self) -> Result<Vec<CostRecommendation>, CostError> {
        let status = self.budget_status()?;
        let inner = self.inner.lock().map_err(|_| CostError::LockPoisoned)?;
        let mut recs = Vec::new();

        match status.status {
            BudgetAlert::Normal => {
                recs.push(CostRecommendation {
                    action: CostAction::NoAction,
                    reason: "Budget is healthy; no action needed.".to_string(),
                    estimated_savings_usd: 0.0,
                    model_from: None,
                    model_to: None,
                });
            }
            BudgetAlert::Warning => {
                let most_expensive = Self::find_most_expensive_model(&inner.entries);
                let cheapest = Self::find_cheapest_model(&inner.entries);
                if let (Some(ref expensive), Some(ref cheap)) = (&most_expensive, &cheapest) {
                    if expensive != cheap {
                        let savings = Self::estimate_shift_savings(
                            &inner.entries,
                            expensive,
                            &inner.model_rates,
                        );
                        recs.push(CostRecommendation {
                            action: CostAction::ShiftTraffic {
                                from: expensive.clone(),
                                to: cheap.clone(),
                            },
                            reason: format!(
                                "Budget approaching limit ({:.0}% used). Shift traffic from {} to {}.",
                                status.budget_fraction * 100.0,
                                expensive,
                                cheap,
                            ),
                            estimated_savings_usd: savings,
                            model_from: Some(expensive.clone()),
                            model_to: Some(cheap.clone()),
                        });
                    }
                }
                if recs.is_empty() {
                    recs.push(CostRecommendation {
                        action: CostAction::NoAction,
                        reason: "Budget approaching limit but no alternative model available."
                            .to_string(),
                        estimated_savings_usd: 0.0,
                        model_from: None,
                        model_to: None,
                    });
                }
            }
            BudgetAlert::Critical | BudgetAlert::Exceeded => {
                recs.push(CostRecommendation {
                    action: CostAction::ThrottleRequests,
                    reason: format!(
                        "Budget {}: {:.0}% used. Throttle request rate immediately.",
                        if status.status == BudgetAlert::Exceeded {
                            "exceeded"
                        } else {
                            "critical"
                        },
                        status.budget_fraction * 100.0,
                    ),
                    estimated_savings_usd: 0.0,
                    model_from: None,
                    model_to: None,
                });

                let most_expensive = Self::find_most_expensive_model(&inner.entries);
                let cheapest = Self::find_cheapest_model(&inner.entries);
                if let (Some(ref expensive), Some(ref cheap)) = (&most_expensive, &cheapest) {
                    if expensive != cheap {
                        let savings = Self::estimate_shift_savings(
                            &inner.entries,
                            expensive,
                            &inner.model_rates,
                        );
                        recs.push(CostRecommendation {
                            action: CostAction::ShiftTraffic {
                                from: expensive.clone(),
                                to: cheap.clone(),
                            },
                            reason: format!(
                                "Additionally shift traffic from {} to {} to reduce costs.",
                                expensive, cheap,
                            ),
                            estimated_savings_usd: savings,
                            model_from: Some(expensive.clone()),
                            model_to: Some(cheap.clone()),
                        });
                    }
                }
            }
        }

        Ok(recs)
    }

    /// Return the total amount spent across all recorded entries.
    ///
    /// # Panics
    /// This function never panics.
    pub fn total_spent(&self) -> f64 {
        self.inner
            .lock()
            .map(|inner| inner.entries.iter().map(|e| e.cost_usd).sum())
            .unwrap_or(0.0)
    }

    /// Return the total amount spent for a specific model.
    ///
    /// # Arguments
    /// * `model` — The model identifier to filter by.
    ///
    /// # Panics
    /// This function never panics.
    pub fn spent_by_model(&self, model: &str) -> f64 {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .entries
                    .iter()
                    .filter(|e| e.model == model)
                    .map(|e| e.cost_usd)
                    .sum()
            })
            .unwrap_or(0.0)
    }

    /// Return the model with the lowest average cost per request, if any
    /// entries exist.
    ///
    /// # Panics
    /// This function never panics.
    pub fn cheapest_model(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| Self::find_cheapest_model(&inner.entries))
    }

    /// Return the model with the highest average cost per request, if any
    /// entries exist.
    ///
    /// # Panics
    /// This function never panics.
    pub fn most_expensive_model(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| Self::find_most_expensive_model(&inner.entries))
    }

    /// Clear all entries and start a new budget period.
    ///
    /// # Panics
    /// This function never panics.
    pub fn reset_period(&self) -> Result<(), CostError> {
        let mut inner = self.inner.lock().map_err(|_| CostError::LockPoisoned)?;
        inner.entries.clear();
        inner.period_start_secs = 0;
        Ok(())
    }

    /// Return the number of recorded cost entries.
    ///
    /// # Panics
    /// This function never panics.
    pub fn entry_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.entries.len())
            .unwrap_or(0)
    }

    /// Update the total budget to a new value.
    ///
    /// # Arguments
    /// * `new_budget` — The new budget in US dollars. Must be positive.
    ///
    /// # Panics
    /// This function never panics.
    pub fn update_budget(&self, new_budget: f64) -> Result<(), CostError> {
        if new_budget <= 0.0 {
            return Err(CostError::InvalidBudget(format!(
                "total_budget must be positive, got {}",
                new_budget
            )));
        }
        let mut inner = self.inner.lock().map_err(|_| CostError::LockPoisoned)?;
        inner.config.total_budget = new_budget;
        Ok(())
    }

    // ─── Private helpers ─────────────────────────────────────────────────

    /// Compute the budget period elapsed fraction and projected total spend.
    fn compute_projection(
        entries: &[CostEntry],
        period_start_secs: u64,
        period_secs: u64,
        total_spent: f64,
    ) -> (f64, f64) {
        let now = entries
            .iter()
            .map(|e| e.timestamp_secs)
            .max()
            .unwrap_or(period_start_secs);

        let elapsed = now.saturating_sub(period_start_secs);
        if elapsed == 0 || period_secs == 0 {
            return (0.0, total_spent);
        }

        let period_elapsed_fraction = elapsed as f64 / period_secs as f64;
        let rate_per_sec = total_spent / elapsed as f64;
        let projected_spend = rate_per_sec * period_secs as f64;

        (period_elapsed_fraction, projected_spend)
    }

    /// Classify the alert level based on budget fraction consumed.
    fn classify_alert(budget_fraction: f64, config: &BudgetConfig) -> BudgetAlert {
        if budget_fraction >= 1.0 {
            BudgetAlert::Exceeded
        } else if budget_fraction >= config.critical_threshold {
            BudgetAlert::Critical
        } else if budget_fraction >= config.warning_threshold {
            BudgetAlert::Warning
        } else {
            BudgetAlert::Normal
        }
    }

    /// Build per-model cost summaries from the entries.
    fn build_model_summaries(entries: &[CostEntry], total_budget: f64) -> Vec<ModelCostSummary> {
        let mut model_data: HashMap<&str, (f64, u64, u64, u64)> = HashMap::new();
        for entry in entries {
            let data = model_data.entry(&entry.model).or_insert((0.0, 0, 0, 0));
            data.0 += entry.cost_usd;
            data.1 += 1;
            data.2 += entry.tokens_in;
            data.3 += entry.tokens_out;
        }

        model_data
            .into_iter()
            .map(|(model, (cost, count, tokens_in, tokens_out))| {
                let avg_cost = if count > 0 { cost / count as f64 } else { 0.0 };
                let cost_per_1k_in = if tokens_in > 0 {
                    (cost / tokens_in as f64) * 1000.0
                } else {
                    0.0
                };
                let cost_per_1k_out = if tokens_out > 0 {
                    (cost / tokens_out as f64) * 1000.0
                } else {
                    0.0
                };
                let budget_fraction = if total_budget > 0.0 {
                    cost / total_budget
                } else {
                    0.0
                };

                ModelCostSummary {
                    model: model.to_string(),
                    total_cost_usd: cost,
                    total_requests: count,
                    avg_cost_per_request: avg_cost,
                    cost_per_1k_tokens_in: cost_per_1k_in,
                    cost_per_1k_tokens_out: cost_per_1k_out,
                    budget_fraction,
                }
            })
            .collect()
    }

    /// Find the model with the lowest average cost per request.
    fn find_cheapest_model(entries: &[CostEntry]) -> Option<String> {
        let mut model_costs: HashMap<&str, (f64, u64)> = HashMap::new();
        for entry in entries {
            let data = model_costs.entry(&entry.model).or_insert((0.0, 0));
            data.0 += entry.cost_usd;
            data.1 += 1;
        }

        model_costs
            .into_iter()
            .filter(|(_, (_, count))| *count > 0)
            .min_by(|(_, (cost_a, count_a)), (_, (cost_b, count_b))| {
                let avg_a = cost_a / *count_a as f64;
                let avg_b = cost_b / *count_b as f64;
                avg_a
                    .partial_cmp(&avg_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(model, _)| model.to_string())
    }

    /// Find the model with the highest average cost per request.
    fn find_most_expensive_model(entries: &[CostEntry]) -> Option<String> {
        let mut model_costs: HashMap<&str, (f64, u64)> = HashMap::new();
        for entry in entries {
            let data = model_costs.entry(&entry.model).or_insert((0.0, 0));
            data.0 += entry.cost_usd;
            data.1 += 1;
        }

        model_costs
            .into_iter()
            .filter(|(_, (_, count))| *count > 0)
            .max_by(|(_, (cost_a, count_a)), (_, (cost_b, count_b))| {
                let avg_a = cost_a / *count_a as f64;
                let avg_b = cost_b / *count_b as f64;
                avg_a
                    .partial_cmp(&avg_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(model, _)| model.to_string())
    }

    /// Estimate savings from shifting traffic away from the given model.
    fn estimate_shift_savings(
        entries: &[CostEntry],
        expensive_model: &str,
        model_rates: &HashMap<String, f64>,
    ) -> f64 {
        let expensive_spend: f64 = entries
            .iter()
            .filter(|e| e.model == expensive_model)
            .map(|e| e.cost_usd)
            .sum();

        // Estimate savings as a fraction of the expensive model's spend.
        // If we have rate data, use the rate differential; otherwise estimate 50%.
        if let Some(&expensive_rate) = model_rates.get(expensive_model) {
            let min_rate = model_rates
                .values()
                .copied()
                .filter(|r| *r < expensive_rate)
                .fold(f64::MAX, f64::min);
            if min_rate < f64::MAX && expensive_rate > 0.0 {
                let fraction_saved = 1.0 - (min_rate / expensive_rate);
                return expensive_spend * fraction_saved;
            }
        }

        expensive_spend * 0.5
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(model: &str, cost_usd: f64, tokens_in: u64, tokens_out: u64) -> CostEntry {
        CostEntry {
            model: model.to_string(),
            cost_usd,
            tokens_in,
            tokens_out,
            timestamp_secs: 100,
        }
    }

    fn make_entry_at(
        model: &str,
        cost_usd: f64,
        tokens_in: u64,
        tokens_out: u64,
        ts: u64,
    ) -> CostEntry {
        CostEntry {
            model: model.to_string(),
            cost_usd,
            tokens_in,
            tokens_out,
            timestamp_secs: ts,
        }
    }

    #[test]
    fn test_new_validates_budget() {
        let config = BudgetConfig {
            total_budget: 50.0,
            ..Default::default()
        };
        let result = CostOptimizer::new(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_new_rejects_zero_budget() {
        let config = BudgetConfig {
            total_budget: 0.0,
            ..Default::default()
        };
        let result = CostOptimizer::new(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, CostError::InvalidBudget(_)));
    }

    #[test]
    fn test_new_rejects_negative_budget() {
        let config = BudgetConfig {
            total_budget: -10.0,
            ..Default::default()
        };
        let result = CostOptimizer::new(config);
        assert!(result.is_err());
    }

    #[test]
    fn test_with_defaults() {
        let opt = CostOptimizer::with_defaults();
        assert_eq!(opt.entry_count(), 0);
        assert!((opt.total_spent() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_record_cost_success() {
        let opt = CostOptimizer::with_defaults();
        let entry = make_entry("claude", 0.05, 1000, 200);
        let result = opt.record_cost(entry);
        assert!(result.is_ok());
        assert_eq!(opt.entry_count(), 1);
    }

    #[test]
    fn test_total_spent() {
        let opt = CostOptimizer::with_defaults();
        opt.record_cost(make_entry("claude", 0.05, 1000, 200))
            .unwrap();
        opt.record_cost(make_entry("claude", 0.03, 500, 100))
            .unwrap();
        assert!((opt.total_spent() - 0.08).abs() < 1e-10);
    }

    #[test]
    fn test_spent_by_model() {
        let opt = CostOptimizer::with_defaults();
        opt.record_cost(make_entry("claude", 0.05, 1000, 200))
            .unwrap();
        opt.record_cost(make_entry("gpt4", 0.03, 500, 100)).unwrap();
        opt.record_cost(make_entry("claude", 0.02, 300, 50))
            .unwrap();
        assert!((opt.spent_by_model("claude") - 0.07).abs() < 1e-10);
        assert!((opt.spent_by_model("gpt4") - 0.03).abs() < 1e-10);
        assert!((opt.spent_by_model("nonexistent") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_budget_status_normal() {
        let config = BudgetConfig {
            total_budget: 100.0,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        opt.record_cost(make_entry("claude", 10.0, 1000, 200))
            .unwrap();
        let status = opt.budget_status().unwrap();
        assert_eq!(status.status, BudgetAlert::Normal);
        assert!((status.budget_fraction - 0.1).abs() < 1e-10);
    }

    #[test]
    fn test_budget_status_warning() {
        let config = BudgetConfig {
            total_budget: 100.0,
            warning_threshold: 0.80,
            critical_threshold: 0.95,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        opt.record_cost(make_entry("claude", 85.0, 1000, 200))
            .unwrap();
        let status = opt.budget_status().unwrap();
        assert_eq!(status.status, BudgetAlert::Warning);
    }

    #[test]
    fn test_budget_status_critical() {
        let config = BudgetConfig {
            total_budget: 100.0,
            warning_threshold: 0.80,
            critical_threshold: 0.95,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        opt.record_cost(make_entry("claude", 96.0, 1000, 200))
            .unwrap();
        let status = opt.budget_status().unwrap();
        assert_eq!(status.status, BudgetAlert::Critical);
    }

    #[test]
    fn test_budget_status_exceeded() {
        let config = BudgetConfig {
            total_budget: 100.0,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        opt.record_cost(make_entry("claude", 110.0, 1000, 200))
            .unwrap();
        let status = opt.budget_status().unwrap();
        assert_eq!(status.status, BudgetAlert::Exceeded);
    }

    #[test]
    fn test_recommendations_no_action_when_normal() {
        let opt = CostOptimizer::with_defaults();
        opt.record_cost(make_entry("claude", 5.0, 1000, 200))
            .unwrap();
        let recs = opt.recommendations().unwrap();
        assert!(!recs.is_empty());
        assert_eq!(recs[0].action, CostAction::NoAction);
    }

    #[test]
    fn test_recommendations_shift_traffic_when_warning() {
        let config = BudgetConfig {
            total_budget: 100.0,
            warning_threshold: 0.80,
            critical_threshold: 0.95,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        // Record expensive model
        for _ in 0..5 {
            opt.record_cost(make_entry("claude", 10.0, 1000, 200))
                .unwrap();
        }
        // Record cheap model
        for _ in 0..5 {
            opt.record_cost(make_entry("local", 0.01, 1000, 200))
                .unwrap();
        }
        // Total: 50.05 → 50.05% but we need > 80%
        // Add more expensive entries to push past 80%
        for _ in 0..4 {
            opt.record_cost(make_entry("claude", 10.0, 1000, 200))
                .unwrap();
        }
        // Total: 90.05 → 90.05%
        let recs = opt.recommendations().unwrap();
        let has_shift = recs.iter().any(|r| {
            matches!(
                &r.action,
                CostAction::ShiftTraffic { from, to }
                if from == "claude" && to == "local"
            )
        });
        assert!(
            has_shift,
            "Expected ShiftTraffic recommendation, got: {recs:?}"
        );
    }

    #[test]
    fn test_recommendations_throttle_when_critical() {
        let config = BudgetConfig {
            total_budget: 100.0,
            warning_threshold: 0.80,
            critical_threshold: 0.95,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        opt.record_cost(make_entry("claude", 97.0, 1000, 200))
            .unwrap();
        let recs = opt.recommendations().unwrap();
        let has_throttle = recs
            .iter()
            .any(|r| matches!(r.action, CostAction::ThrottleRequests));
        assert!(has_throttle, "Expected ThrottleRequests recommendation");
    }

    #[test]
    fn test_cheapest_model() {
        let opt = CostOptimizer::with_defaults();
        for _ in 0..3 {
            opt.record_cost(make_entry("claude", 0.10, 1000, 200))
                .unwrap();
        }
        for _ in 0..3 {
            opt.record_cost(make_entry("local", 0.001, 1000, 200))
                .unwrap();
        }
        assert_eq!(opt.cheapest_model(), Some("local".to_string()));
    }

    #[test]
    fn test_most_expensive_model() {
        let opt = CostOptimizer::with_defaults();
        for _ in 0..3 {
            opt.record_cost(make_entry("claude", 0.10, 1000, 200))
                .unwrap();
        }
        for _ in 0..3 {
            opt.record_cost(make_entry("local", 0.001, 1000, 200))
                .unwrap();
        }
        assert_eq!(opt.most_expensive_model(), Some("claude".to_string()));
    }

    #[test]
    fn test_cheapest_model_none_when_empty() {
        let opt = CostOptimizer::with_defaults();
        assert_eq!(opt.cheapest_model(), None);
    }

    #[test]
    fn test_set_model_rate() {
        let opt = CostOptimizer::with_defaults();
        let result = opt.set_model_rate("claude", 3.0);
        assert!(result.is_ok());

        // Verify the rate is stored by checking it affects savings estimation
        let inner = opt.inner.lock().unwrap();
        assert!((inner.model_rates["claude"] - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_reset_period_clears_entries() {
        let opt = CostOptimizer::with_defaults();
        opt.record_cost(make_entry("claude", 0.05, 1000, 200))
            .unwrap();
        opt.record_cost(make_entry("gpt4", 0.03, 500, 100)).unwrap();
        assert_eq!(opt.entry_count(), 2);

        opt.reset_period().unwrap();
        assert_eq!(opt.entry_count(), 0);
        assert!((opt.total_spent() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_update_budget_validates() {
        let opt = CostOptimizer::with_defaults();

        // Valid update
        assert!(opt.update_budget(200.0).is_ok());

        // Invalid: zero
        assert!(opt.update_budget(0.0).is_err());

        // Invalid: negative
        assert!(opt.update_budget(-50.0).is_err());
    }

    #[test]
    fn test_per_model_summary_in_status() {
        let opt = CostOptimizer::with_defaults();
        opt.record_cost(make_entry("claude", 5.0, 1000, 200))
            .unwrap();
        opt.record_cost(make_entry("gpt4", 3.0, 800, 150)).unwrap();
        opt.record_cost(make_entry("claude", 4.0, 900, 180))
            .unwrap();

        let status = opt.budget_status().unwrap();
        assert_eq!(status.per_model.len(), 2);

        let claude_summary = status
            .per_model
            .iter()
            .find(|s| s.model == "claude")
            .unwrap();
        assert_eq!(claude_summary.total_requests, 2);
        assert!((claude_summary.total_cost_usd - 9.0).abs() < 1e-10);
        assert!((claude_summary.avg_cost_per_request - 4.5).abs() < 1e-10);
    }

    #[test]
    fn test_entry_count() {
        let opt = CostOptimizer::with_defaults();
        assert_eq!(opt.entry_count(), 0);
        opt.record_cost(make_entry("claude", 0.05, 100, 20))
            .unwrap();
        assert_eq!(opt.entry_count(), 1);
        opt.record_cost(make_entry("gpt4", 0.03, 100, 20)).unwrap();
        assert_eq!(opt.entry_count(), 2);
    }

    #[test]
    fn test_clone_shares_state() {
        let opt = CostOptimizer::with_defaults();
        let clone = opt.clone();

        opt.record_cost(make_entry("claude", 0.05, 1000, 200))
            .unwrap();

        // Clone should see the same entry
        assert_eq!(clone.entry_count(), 1);
        assert!((clone.total_spent() - 0.05).abs() < 1e-10);
    }

    #[test]
    fn test_budget_alert_serialization() {
        let alert = BudgetAlert::Warning;
        let json = serde_json::to_string(&alert).unwrap();
        let deserialized: BudgetAlert = serde_json::from_str(&json).unwrap();
        assert_eq!(alert, deserialized);

        // Round-trip all variants
        for variant in &[
            BudgetAlert::Normal,
            BudgetAlert::Warning,
            BudgetAlert::Critical,
            BudgetAlert::Exceeded,
        ] {
            let json = serde_json::to_string(variant).unwrap();
            let back: BudgetAlert = serde_json::from_str(&json).unwrap();
            assert_eq!(*variant, back);
        }
    }

    #[test]
    fn test_cost_action_serialization() {
        let actions = vec![
            CostAction::NoAction,
            CostAction::ThrottleRequests,
            CostAction::ReduceUsage("claude".to_string()),
            CostAction::ShiftTraffic {
                from: "claude".to_string(),
                to: "local".to_string(),
            },
        ];
        for action in &actions {
            let json = serde_json::to_string(action).unwrap();
            let back: CostAction = serde_json::from_str(&json).unwrap();
            assert_eq!(*action, back);
        }
    }

    #[test]
    fn test_entries_capped_at_max() {
        let config = BudgetConfig {
            total_budget: 1_000_000.0,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();

        // Override max_entries to a small value for testing
        {
            let mut inner = opt.inner.lock().unwrap();
            inner.max_entries = 5;
        }

        for _ in 0..10 {
            opt.record_cost(make_entry("claude", 1.0, 100, 20)).unwrap();
        }

        assert_eq!(opt.entry_count(), 5);
    }

    #[test]
    fn test_budget_fraction_computed_correctly() {
        let config = BudgetConfig {
            total_budget: 200.0,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        opt.record_cost(make_entry("claude", 50.0, 1000, 200))
            .unwrap();

        let status = opt.budget_status().unwrap();
        assert!((status.budget_fraction - 0.25).abs() < 1e-10);
        assert!((status.total_budget - 200.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_projection_with_elapsed_time() {
        let config = BudgetConfig {
            total_budget: 100.0,
            period_secs: 1000,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();

        // Record entries with timestamps that create a time span
        // period_start_secs = 0, entries at t=100 and t=200
        opt.record_cost(make_entry_at("claude", 10.0, 1000, 200, 100))
            .unwrap();
        opt.record_cost(make_entry_at("claude", 10.0, 1000, 200, 200))
            .unwrap();

        let status = opt.budget_status().unwrap();
        // elapsed = 200 - 0 = 200, rate = 20/200 = 0.1/sec, projected = 0.1 * 1000 = 100
        assert!((status.projected_spend - 100.0).abs() < 1e-10);
        assert!((status.period_elapsed_fraction - 0.2).abs() < 1e-10);
    }

    #[test]
    fn test_model_cost_summary_tokens_computation() {
        let config = BudgetConfig {
            total_budget: 100.0,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        // 2 requests, total cost $2, total tokens_in 2000, total tokens_out 400
        opt.record_cost(make_entry("claude", 1.0, 1000, 200))
            .unwrap();
        opt.record_cost(make_entry("claude", 1.0, 1000, 200))
            .unwrap();

        let status = opt.budget_status().unwrap();
        let claude = status
            .per_model
            .iter()
            .find(|s| s.model == "claude")
            .unwrap();

        // cost_per_1k_tokens_in = (2.0 / 2000) * 1000 = 1.0
        assert!((claude.cost_per_1k_tokens_in - 1.0).abs() < 1e-10);
        // cost_per_1k_tokens_out = (2.0 / 400) * 1000 = 5.0
        assert!((claude.cost_per_1k_tokens_out - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_most_expensive_model_none_when_empty() {
        let opt = CostOptimizer::with_defaults();
        assert_eq!(opt.most_expensive_model(), None);
    }

    #[test]
    fn test_recommendations_exceeded_includes_throttle() {
        let config = BudgetConfig {
            total_budget: 100.0,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        opt.record_cost(make_entry("claude", 120.0, 1000, 200))
            .unwrap();

        let recs = opt.recommendations().unwrap();
        let has_throttle = recs
            .iter()
            .any(|r| matches!(r.action, CostAction::ThrottleRequests));
        assert!(has_throttle);
    }

    #[test]
    fn test_update_budget_changes_status() {
        let config = BudgetConfig {
            total_budget: 100.0,
            ..Default::default()
        };
        let opt = CostOptimizer::new(config).unwrap();
        opt.record_cost(make_entry("claude", 85.0, 1000, 200))
            .unwrap();

        // Should be Warning at $100 budget
        let status = opt.budget_status().unwrap();
        assert_eq!(status.status, BudgetAlert::Warning);

        // Update budget to $200 — now should be Normal
        opt.update_budget(200.0).unwrap();
        let status = opt.budget_status().unwrap();
        assert_eq!(status.status, BudgetAlert::Normal);
    }

    #[test]
    fn test_budget_config_default_values() {
        let config = BudgetConfig::default();
        assert!((config.total_budget - 100.0).abs() < f64::EPSILON);
        assert_eq!(config.period_secs, 86400);
        assert!((config.warning_threshold - 0.80).abs() < f64::EPSILON);
        assert!((config.critical_threshold - 0.95).abs() < f64::EPSILON);
        assert!(config.per_model_limits.is_empty());
    }
}
