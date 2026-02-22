#![cfg(feature = "self-tune")]

//! # Experiment Engine (Task 1.3)
//!
//! ## Responsibility
//! Run controlled A/B experiments on pipeline parameters. Uses Welch's t-test
//! for statistical significance testing. Auto-promotes winners and kills losers
//! when significance is reached.
//!
//! ## Guarantees
//! - Thread-safe: all operations safe under concurrent access
//! - Statistically sound: requires minimum sample size before concluding
//! - Bounded: experiment history capped at configurable limit
//! - Non-blocking: significance checks run in O(n) time
//!
//! ## NOT Responsible For
//! - Applying parameter changes (that's the controller's job)
//! - Multi-armed bandit optimization (see intelligence/router.rs)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced by the experiment engine.
#[derive(Debug, Error)]
pub enum ExperimentError {
    /// Internal lock was poisoned by a panicking thread.
    #[error("experiment engine lock poisoned")]
    LockPoisoned,

    /// The named experiment does not exist.
    #[error("experiment '{0}' not found")]
    ExperimentNotFound(String),

    /// An experiment with this name already exists.
    #[error("experiment '{0}' already exists")]
    ExperimentAlreadyExists(String),

    /// Not enough samples to perform significance testing.
    #[error("insufficient samples for experiment '{experiment_id}': have {have}, need {need}")]
    InsufficientSamples {
        /// The experiment identifier.
        experiment_id: String,
        /// Number of samples currently collected in the smaller arm.
        have: usize,
        /// Minimum samples required per arm.
        need: usize,
    },

    /// Cannot add samples to an experiment that has already concluded.
    #[error("experiment '{0}' has already concluded")]
    ExperimentConcluded(String),
}

// ---------------------------------------------------------------------------
// ExperimentStatus
// ---------------------------------------------------------------------------

/// Lifecycle status of an experiment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExperimentStatus {
    /// The experiment is actively collecting samples.
    Running,
    /// The experiment has reached a conclusion.
    Concluded(ExperimentConclusion),
}

// ---------------------------------------------------------------------------
// ExperimentConclusion
// ---------------------------------------------------------------------------

/// The outcome of a concluded experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExperimentConclusion {
    /// Control arm is statistically better; discard treatment.
    ControlWins,
    /// Treatment arm is statistically better; promote it.
    TreatmentWins,
    /// No statistically significant difference detected.
    NoSignificantDifference,
    /// Maximum duration or sample count reached without significance.
    TimedOut,
}

// ---------------------------------------------------------------------------
// ExperimentConfig
// ---------------------------------------------------------------------------

/// Configuration parameters for the experiment engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentConfig {
    /// Minimum number of samples required in each arm before significance testing.
    pub min_samples_per_arm: usize,
    /// Maximum number of samples per arm before forcing a conclusion.
    pub max_samples_per_arm: usize,
    /// P-value threshold for statistical significance (e.g. 0.05).
    pub significance_level: f64,
    /// Maximum experiment duration in seconds before forcing a conclusion.
    pub max_duration_secs: u64,
}

impl Default for ExperimentConfig {
    fn default() -> Self {
        Self {
            min_samples_per_arm: 30,
            max_samples_per_arm: 1000,
            significance_level: 0.05,
            max_duration_secs: 3600,
        }
    }
}

// ---------------------------------------------------------------------------
// Experiment
// ---------------------------------------------------------------------------

/// A single A/B experiment tracking control and treatment observations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experiment {
    /// Unique identifier for this experiment.
    pub id: String,
    /// Human-readable description of what is being tested.
    pub description: String,
    /// Which pipeline parameter is being tested.
    pub parameter: String,
    /// Current (baseline) parameter value.
    pub control_value: f64,
    /// Proposed (candidate) parameter value.
    pub treatment_value: f64,
    /// Name of the metric being compared (e.g. "p95_latency_ms").
    pub metric_name: String,
    /// If `true`, a lower metric value is considered better.
    pub lower_is_better: bool,
    /// Metric observations collected under the control arm.
    pub control_samples: Vec<f64>,
    /// Metric observations collected under the treatment arm.
    pub treatment_samples: Vec<f64>,
    /// Current lifecycle status.
    pub status: ExperimentStatus,
    /// Unix timestamp (seconds) when the experiment was created.
    pub created_at_secs: u64,
    /// Unix timestamp (seconds) when the experiment concluded, if applicable.
    pub concluded_at_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// SignificanceResult
// ---------------------------------------------------------------------------

/// Result of a Welch's t-test significance check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignificanceResult {
    /// The computed t-statistic.
    pub t_statistic: f64,
    /// Welch-Satterthwaite degrees of freedom.
    pub degrees_of_freedom: f64,
    /// Approximate two-tailed p-value using normal CDF approximation.
    pub p_value_approx: f64,
    /// Whether the result is statistically significant at the configured level.
    pub significant: bool,
    /// Mean of the control arm samples.
    pub control_mean: f64,
    /// Mean of the treatment arm samples.
    pub treatment_mean: f64,
    /// Standard deviation of the control arm samples.
    pub control_stddev: f64,
    /// Standard deviation of the treatment arm samples.
    pub treatment_stddev: f64,
}

// ---------------------------------------------------------------------------
// ExperimentResult
// ---------------------------------------------------------------------------

/// Summary of a concluded experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentResult {
    /// The experiment identifier.
    pub experiment_id: String,
    /// The conclusion reached.
    pub conclusion: ExperimentConclusion,
    /// Significance test results, if a test was performed.
    pub significance: Option<SignificanceResult>,
    /// Number of control arm samples collected.
    pub control_sample_count: usize,
    /// Number of treatment arm samples collected.
    pub treatment_sample_count: usize,
}

// ---------------------------------------------------------------------------
// Statistical helpers
// ---------------------------------------------------------------------------

/// Abramowitz & Stegun approximation 26.2.17 for the standard normal CDF.
///
/// # Arguments
/// * `x` — The input value.
///
/// # Returns
/// An approximation of Phi(x), the cumulative distribution function of
/// the standard normal distribution.
///
/// # Panics
/// This function never panics.
fn normal_cdf(x: f64) -> f64 {
    if x < -8.0 {
        return 0.0;
    }
    if x > 8.0 {
        return 1.0;
    }
    let t = 1.0 / (1.0 + 0.2316419 * x.abs());
    let d = 0.3989422804014327; // 1/sqrt(2*pi)
    let p = d
        * (-x * x / 2.0).exp()
        * (t * (0.3193815 + t * (-0.3565638 + t * (1.781478 + t * (-1.8212560 + t * 1.3302744)))));
    if x > 0.0 {
        1.0 - p
    } else {
        p
    }
}

/// Compute the arithmetic mean of a slice of f64 values.
///
/// Returns `0.0` for an empty slice.
fn mean(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().sum::<f64>() / samples.len() as f64
}

/// Compute the unbiased sample variance (divides by n-1) of a slice.
///
/// Returns `0.0` for slices with fewer than 2 elements.
fn variance(samples: &[f64]) -> f64 {
    let n = samples.len();
    if n < 2 {
        return 0.0;
    }
    let m = mean(samples);
    let sum_sq: f64 = samples.iter().map(|x| (x - m) * (x - m)).sum();
    sum_sq / (n as f64 - 1.0)
}

/// Perform Welch's t-test on two sets of samples.
///
/// Returns a `SignificanceResult` with the t-statistic, degrees of freedom,
/// approximate p-value, and arm summary statistics.
///
/// # Arguments
/// * `control` — Samples from the control arm.
/// * `treatment` — Samples from the treatment arm.
/// * `significance_level` — The alpha threshold for significance determination.
///
/// # Panics
/// This function never panics.
fn welch_t_test(control: &[f64], treatment: &[f64], significance_level: f64) -> SignificanceResult {
    let mean1 = mean(control);
    let mean2 = mean(treatment);
    let var1 = variance(control);
    let var2 = variance(treatment);
    let n1 = control.len() as f64;
    let n2 = treatment.len() as f64;

    let se_sq = var1 / n1 + var2 / n2;
    let se = se_sq.sqrt();

    let t_statistic = if se == 0.0 { 0.0 } else { (mean1 - mean2) / se };

    // Welch-Satterthwaite degrees of freedom
    let df = if se_sq == 0.0 {
        0.0
    } else {
        let num = se_sq * se_sq;
        let denom = (var1 / n1).powi(2) / (n1 - 1.0) + (var2 / n2).powi(2) / (n2 - 1.0);
        if denom == 0.0 {
            0.0
        } else {
            num / denom
        }
    };

    // Approximate p-value using normal CDF for large df (>30)
    let p_value_approx = 2.0 * (1.0 - normal_cdf(t_statistic.abs()));

    SignificanceResult {
        t_statistic,
        degrees_of_freedom: df,
        p_value_approx,
        significant: p_value_approx < significance_level,
        control_mean: mean1,
        treatment_mean: mean2,
        control_stddev: var1.sqrt(),
        treatment_stddev: var2.sqrt(),
    }
}

// ---------------------------------------------------------------------------
// EngineInner
// ---------------------------------------------------------------------------

/// Internal mutable state of the experiment engine.
#[derive(Debug)]
struct EngineInner {
    /// Configuration parameters.
    config: ExperimentConfig,
    /// Currently active (running) experiments keyed by ID.
    experiments: HashMap<String, Experiment>,
    /// History of concluded experiment results.
    concluded: Vec<ExperimentResult>,
    /// Maximum number of concluded results to retain.
    max_history: usize,
}

// ---------------------------------------------------------------------------
// ExperimentEngine
// ---------------------------------------------------------------------------

/// Thread-safe engine for managing A/B experiments on pipeline parameters.
///
/// Uses `Arc<Mutex<EngineInner>>` for cheap `Clone` and shared state across
/// threads. All public methods acquire the lock, perform their operation,
/// and release it before returning.
///
/// # Example
/// ```rust,ignore
/// let engine = ExperimentEngine::with_defaults();
/// engine.create_experiment(
///     "latency_test".into(), "Lower p95".into(), "timeout_ms".into(),
///     500.0, 350.0, "p95_latency_ms".into(), true,
/// )?;
/// engine.add_control_sample("latency_test", 120.0)?;
/// engine.add_treatment_sample("latency_test", 95.0)?;
/// ```
#[derive(Debug, Clone)]
pub struct ExperimentEngine {
    inner: Arc<Mutex<EngineInner>>,
}

impl ExperimentEngine {
    /// Create a new experiment engine with the given configuration.
    ///
    /// # Arguments
    /// * `config` — Configuration parameters controlling sample sizes and significance levels.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: ExperimentConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(EngineInner {
                config,
                experiments: HashMap::new(),
                concluded: Vec::new(),
                max_history: 100,
            })),
        }
    }

    /// Create a new experiment engine with default configuration.
    ///
    /// # Panics
    /// This function never panics.
    pub fn with_defaults() -> Self {
        Self::new(ExperimentConfig::default())
    }

    /// Register a new A/B experiment.
    ///
    /// # Arguments
    /// * `id` — Unique identifier for the experiment.
    /// * `description` — Human-readable description.
    /// * `parameter` — The pipeline parameter being tested.
    /// * `control_value` — Baseline value of the parameter.
    /// * `treatment_value` — Proposed new value of the parameter.
    /// * `metric_name` — Which metric to compare (e.g. "p95_latency_ms").
    /// * `lower_is_better` — If `true`, lower metric values are considered better.
    ///
    /// # Errors
    /// - [`ExperimentError::ExperimentAlreadyExists`] if an experiment with this ID is active.
    /// - [`ExperimentError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn create_experiment(
        &self,
        id: String,
        description: String,
        parameter: String,
        control_value: f64,
        treatment_value: f64,
        metric_name: String,
        lower_is_better: bool,
    ) -> Result<(), ExperimentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| ExperimentError::LockPoisoned)?;

        if inner.experiments.contains_key(&id) {
            return Err(ExperimentError::ExperimentAlreadyExists(id));
        }

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let experiment = Experiment {
            id: id.clone(),
            description,
            parameter,
            control_value,
            treatment_value,
            metric_name,
            lower_is_better,
            control_samples: Vec::new(),
            treatment_samples: Vec::new(),
            status: ExperimentStatus::Running,
            created_at_secs: now_secs,
            concluded_at_secs: None,
        };

        inner.experiments.insert(id, experiment);
        Ok(())
    }

    /// Add a metric observation to the control arm of an experiment.
    ///
    /// After adding the sample, checks whether the experiment should auto-conclude.
    /// Returns `Some(ExperimentResult)` if the experiment concluded, `None` otherwise.
    ///
    /// # Arguments
    /// * `experiment_id` — The experiment to add a sample to.
    /// * `value` — The observed metric value.
    ///
    /// # Errors
    /// - [`ExperimentError::ExperimentNotFound`] if no experiment has this ID.
    /// - [`ExperimentError::ExperimentConcluded`] if the experiment already concluded.
    /// - [`ExperimentError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn add_control_sample(
        &self,
        experiment_id: &str,
        value: f64,
    ) -> Result<Option<ExperimentResult>, ExperimentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| ExperimentError::LockPoisoned)?;
        let config = inner.config.clone();
        let max_history = inner.max_history;

        let experiment = inner
            .experiments
            .get_mut(experiment_id)
            .ok_or_else(|| ExperimentError::ExperimentNotFound(experiment_id.to_owned()))?;

        if experiment.status != ExperimentStatus::Running {
            return Err(ExperimentError::ExperimentConcluded(
                experiment_id.to_owned(),
            ));
        }

        experiment.control_samples.push(value);

        let result = Self::maybe_auto_conclude(experiment, &config)?;
        if let Some(ref r) = result {
            if inner.concluded.len() >= max_history {
                inner.concluded.remove(0);
            }
            inner.concluded.push(r.clone());
        }
        Ok(result)
    }

    /// Add a metric observation to the treatment arm of an experiment.
    ///
    /// After adding the sample, checks whether the experiment should auto-conclude.
    /// Returns `Some(ExperimentResult)` if the experiment concluded, `None` otherwise.
    ///
    /// # Arguments
    /// * `experiment_id` — The experiment to add a sample to.
    /// * `value` — The observed metric value.
    ///
    /// # Errors
    /// - [`ExperimentError::ExperimentNotFound`] if no experiment has this ID.
    /// - [`ExperimentError::ExperimentConcluded`] if the experiment already concluded.
    /// - [`ExperimentError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn add_treatment_sample(
        &self,
        experiment_id: &str,
        value: f64,
    ) -> Result<Option<ExperimentResult>, ExperimentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| ExperimentError::LockPoisoned)?;
        let config = inner.config.clone();
        let max_history = inner.max_history;

        let experiment = inner
            .experiments
            .get_mut(experiment_id)
            .ok_or_else(|| ExperimentError::ExperimentNotFound(experiment_id.to_owned()))?;

        if experiment.status != ExperimentStatus::Running {
            return Err(ExperimentError::ExperimentConcluded(
                experiment_id.to_owned(),
            ));
        }

        experiment.treatment_samples.push(value);

        let result = Self::maybe_auto_conclude(experiment, &config)?;
        if let Some(ref r) = result {
            if inner.concluded.len() >= max_history {
                inner.concluded.remove(0);
            }
            inner.concluded.push(r.clone());
        }
        Ok(result)
    }

    /// Check whether an experiment should auto-conclude and do so if appropriate.
    ///
    /// Called after each sample addition. Logic:
    /// 1. If both arms have >= min_samples, run significance test.
    /// 2. If p_value < significance_level, conclude with winner.
    /// 3. If either arm has >= max_samples, conclude as TimedOut.
    /// 4. Otherwise, return None (still running).
    fn maybe_auto_conclude(
        experiment: &mut Experiment,
        config: &ExperimentConfig,
    ) -> Result<Option<ExperimentResult>, ExperimentError> {
        let control_count = experiment.control_samples.len();
        let treatment_count = experiment.treatment_samples.len();
        let both_have_min = control_count >= config.min_samples_per_arm
            && treatment_count >= config.min_samples_per_arm;
        let either_at_max = control_count >= config.max_samples_per_arm
            || treatment_count >= config.max_samples_per_arm;

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if both_have_min {
            let sig = welch_t_test(
                &experiment.control_samples,
                &experiment.treatment_samples,
                config.significance_level,
            );

            if sig.significant {
                let conclusion = if experiment.lower_is_better {
                    if sig.treatment_mean < sig.control_mean {
                        ExperimentConclusion::TreatmentWins
                    } else {
                        ExperimentConclusion::ControlWins
                    }
                } else if sig.treatment_mean > sig.control_mean {
                    ExperimentConclusion::TreatmentWins
                } else {
                    ExperimentConclusion::ControlWins
                };

                experiment.status = ExperimentStatus::Concluded(conclusion);
                experiment.concluded_at_secs = Some(now_secs);

                return Ok(Some(ExperimentResult {
                    experiment_id: experiment.id.clone(),
                    conclusion,
                    significance: Some(sig),
                    control_sample_count: control_count,
                    treatment_sample_count: treatment_count,
                }));
            }

            if either_at_max {
                let conclusion = ExperimentConclusion::TimedOut;
                experiment.status = ExperimentStatus::Concluded(conclusion);
                experiment.concluded_at_secs = Some(now_secs);

                return Ok(Some(ExperimentResult {
                    experiment_id: experiment.id.clone(),
                    conclusion,
                    significance: Some(sig),
                    control_sample_count: control_count,
                    treatment_sample_count: treatment_count,
                }));
            }
        } else if either_at_max {
            let conclusion = ExperimentConclusion::TimedOut;
            experiment.status = ExperimentStatus::Concluded(conclusion);
            experiment.concluded_at_secs = Some(now_secs);

            return Ok(Some(ExperimentResult {
                experiment_id: experiment.id.clone(),
                conclusion,
                significance: None,
                control_sample_count: control_count,
                treatment_sample_count: treatment_count,
            }));
        }

        Ok(None)
    }

    /// Run Welch's t-test on the current samples of an experiment.
    ///
    /// # Arguments
    /// * `experiment_id` — The experiment to test.
    ///
    /// # Errors
    /// - [`ExperimentError::ExperimentNotFound`] if no experiment has this ID.
    /// - [`ExperimentError::InsufficientSamples`] if either arm has fewer than 2 samples.
    /// - [`ExperimentError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn check_significance(
        &self,
        experiment_id: &str,
    ) -> Result<SignificanceResult, ExperimentError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| ExperimentError::LockPoisoned)?;
        let config = &inner.config;

        let experiment = inner
            .experiments
            .get(experiment_id)
            .ok_or_else(|| ExperimentError::ExperimentNotFound(experiment_id.to_owned()))?;

        let min_needed = config.min_samples_per_arm;
        let have = experiment
            .control_samples
            .len()
            .min(experiment.treatment_samples.len());

        if have < min_needed {
            return Err(ExperimentError::InsufficientSamples {
                experiment_id: experiment_id.to_owned(),
                have,
                need: min_needed,
            });
        }

        Ok(welch_t_test(
            &experiment.control_samples,
            &experiment.treatment_samples,
            config.significance_level,
        ))
    }

    /// Force an experiment to conclude immediately.
    ///
    /// If the experiment has enough samples for a significance test, the test is
    /// performed and the conclusion is based on the result. Otherwise, the
    /// conclusion is `NoSignificantDifference`.
    ///
    /// # Arguments
    /// * `experiment_id` — The experiment to conclude.
    ///
    /// # Errors
    /// - [`ExperimentError::ExperimentNotFound`] if no experiment has this ID.
    /// - [`ExperimentError::ExperimentConcluded`] if already concluded.
    /// - [`ExperimentError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn conclude_experiment(
        &self,
        experiment_id: &str,
    ) -> Result<ExperimentResult, ExperimentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| ExperimentError::LockPoisoned)?;
        let config = inner.config.clone();
        let max_history = inner.max_history;

        let experiment = inner
            .experiments
            .get_mut(experiment_id)
            .ok_or_else(|| ExperimentError::ExperimentNotFound(experiment_id.to_owned()))?;

        if experiment.status != ExperimentStatus::Running {
            return Err(ExperimentError::ExperimentConcluded(
                experiment_id.to_owned(),
            ));
        }

        let control_count = experiment.control_samples.len();
        let treatment_count = experiment.treatment_samples.len();
        let have_enough = control_count >= config.min_samples_per_arm
            && treatment_count >= config.min_samples_per_arm;

        let (conclusion, significance) = if have_enough {
            let sig = welch_t_test(
                &experiment.control_samples,
                &experiment.treatment_samples,
                config.significance_level,
            );
            let conc = if sig.significant {
                if experiment.lower_is_better {
                    if sig.treatment_mean < sig.control_mean {
                        ExperimentConclusion::TreatmentWins
                    } else {
                        ExperimentConclusion::ControlWins
                    }
                } else if sig.treatment_mean > sig.control_mean {
                    ExperimentConclusion::TreatmentWins
                } else {
                    ExperimentConclusion::ControlWins
                }
            } else {
                ExperimentConclusion::NoSignificantDifference
            };
            (conc, Some(sig))
        } else {
            (ExperimentConclusion::NoSignificantDifference, None)
        };

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        experiment.status = ExperimentStatus::Concluded(conclusion);
        experiment.concluded_at_secs = Some(now_secs);

        let result = ExperimentResult {
            experiment_id: experiment_id.to_owned(),
            conclusion,
            significance,
            control_sample_count: control_count,
            treatment_sample_count: treatment_count,
        };

        inner.concluded.push(result.clone());
        if inner.concluded.len() > max_history {
            inner.concluded.remove(0);
        }

        Ok(result)
    }

    /// Retrieve a clone of an experiment by its ID.
    ///
    /// # Arguments
    /// * `experiment_id` — The experiment to retrieve.
    ///
    /// # Errors
    /// - [`ExperimentError::ExperimentNotFound`] if no experiment has this ID.
    /// - [`ExperimentError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn get_experiment(&self, experiment_id: &str) -> Result<Experiment, ExperimentError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| ExperimentError::LockPoisoned)?;
        inner
            .experiments
            .get(experiment_id)
            .cloned()
            .ok_or_else(|| ExperimentError::ExperimentNotFound(experiment_id.to_owned()))
    }

    /// Return the IDs of all currently running experiments.
    ///
    /// # Panics
    /// This function never panics.
    pub fn active_experiments(&self) -> Vec<String> {
        let inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(_) => return Vec::new(),
        };
        inner
            .experiments
            .iter()
            .filter(|(_, exp)| exp.status == ExperimentStatus::Running)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Return a clone of all concluded experiment results.
    ///
    /// # Panics
    /// This function never panics.
    pub fn concluded_results(&self) -> Vec<ExperimentResult> {
        let inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(_) => return Vec::new(),
        };
        inner.concluded.clone()
    }

    /// Remove an experiment by its ID (whether running or concluded).
    ///
    /// # Arguments
    /// * `experiment_id` — The experiment to remove.
    ///
    /// # Errors
    /// - [`ExperimentError::ExperimentNotFound`] if no experiment has this ID.
    /// - [`ExperimentError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn remove_experiment(&self, experiment_id: &str) -> Result<(), ExperimentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| ExperimentError::LockPoisoned)?;
        if inner.experiments.remove(experiment_id).is_none() {
            return Err(ExperimentError::ExperimentNotFound(
                experiment_id.to_owned(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn make_engine(min: usize, max: usize) -> ExperimentEngine {
        ExperimentEngine::new(ExperimentConfig {
            min_samples_per_arm: min,
            max_samples_per_arm: max,
            significance_level: 0.05,
            max_duration_secs: 3600,
        })
    }

    fn create_basic(engine: &ExperimentEngine, id: &str) {
        engine
            .create_experiment(
                id.to_owned(),
                "Test experiment".to_owned(),
                "timeout_ms".to_owned(),
                500.0,
                350.0,
                "p95_latency_ms".to_owned(),
                true,
            )
            .unwrap();
    }

    // ------------------------------------------------------------------
    // test_create_experiment_success
    // ------------------------------------------------------------------

    #[test]
    fn test_create_experiment_success() {
        let engine = ExperimentEngine::with_defaults();
        let result = engine.create_experiment(
            "exp1".to_owned(),
            "Desc".to_owned(),
            "param".to_owned(),
            1.0,
            2.0,
            "metric".to_owned(),
            true,
        );
        assert!(result.is_ok());

        let exp = engine.get_experiment("exp1").unwrap();
        assert_eq!(exp.id, "exp1");
        assert_eq!(exp.status, ExperimentStatus::Running);
    }

    // ------------------------------------------------------------------
    // test_create_duplicate_fails
    // ------------------------------------------------------------------

    #[test]
    fn test_create_duplicate_fails() {
        let engine = ExperimentEngine::with_defaults();
        create_basic(&engine, "dup");
        let result = engine.create_experiment(
            "dup".to_owned(),
            "Dup".to_owned(),
            "p".to_owned(),
            1.0,
            2.0,
            "m".to_owned(),
            true,
        );
        assert!(matches!(
            result,
            Err(ExperimentError::ExperimentAlreadyExists(ref n)) if n == "dup"
        ));
    }

    // ------------------------------------------------------------------
    // test_add_control_sample
    // ------------------------------------------------------------------

    #[test]
    fn test_add_control_sample() {
        let engine = make_engine(100, 1000);
        create_basic(&engine, "cs");
        let result = engine.add_control_sample("cs", 42.0).unwrap();
        assert!(result.is_none()); // not enough samples to conclude

        let exp = engine.get_experiment("cs").unwrap();
        assert_eq!(exp.control_samples.len(), 1);
        assert!((exp.control_samples[0] - 42.0).abs() < f64::EPSILON);
    }

    // ------------------------------------------------------------------
    // test_add_treatment_sample
    // ------------------------------------------------------------------

    #[test]
    fn test_add_treatment_sample() {
        let engine = make_engine(100, 1000);
        create_basic(&engine, "ts");
        let result = engine.add_treatment_sample("ts", 99.0).unwrap();
        assert!(result.is_none());

        let exp = engine.get_experiment("ts").unwrap();
        assert_eq!(exp.treatment_samples.len(), 1);
        assert!((exp.treatment_samples[0] - 99.0).abs() < f64::EPSILON);
    }

    // ------------------------------------------------------------------
    // test_add_sample_to_concluded_fails
    // ------------------------------------------------------------------

    #[test]
    fn test_add_sample_to_concluded_fails() {
        let engine = make_engine(5, 1000);
        create_basic(&engine, "concluded");

        // Force conclude
        engine.conclude_experiment("concluded").unwrap();

        let result = engine.add_control_sample("concluded", 1.0);
        assert!(matches!(
            result,
            Err(ExperimentError::ExperimentConcluded(_))
        ));

        let result = engine.add_treatment_sample("concluded", 1.0);
        assert!(matches!(
            result,
            Err(ExperimentError::ExperimentConcluded(_))
        ));
    }

    // ------------------------------------------------------------------
    // test_experiment_not_found
    // ------------------------------------------------------------------

    #[test]
    fn test_experiment_not_found() {
        let engine = ExperimentEngine::with_defaults();
        let result = engine.get_experiment("ghost");
        assert!(matches!(
            result,
            Err(ExperimentError::ExperimentNotFound(_))
        ));

        let result = engine.add_control_sample("ghost", 1.0);
        assert!(matches!(
            result,
            Err(ExperimentError::ExperimentNotFound(_))
        ));

        let result = engine.add_treatment_sample("ghost", 1.0);
        assert!(matches!(
            result,
            Err(ExperimentError::ExperimentNotFound(_))
        ));
    }

    // ------------------------------------------------------------------
    // test_check_significance_insufficient_samples
    // ------------------------------------------------------------------

    #[test]
    fn test_check_significance_insufficient_samples() {
        let engine = make_engine(30, 1000);
        create_basic(&engine, "insuff");

        // Add only a few samples
        for i in 0..5 {
            engine.add_control_sample("insuff", i as f64).unwrap();
            engine.add_treatment_sample("insuff", i as f64).unwrap();
        }

        let result = engine.check_significance("insuff");
        assert!(matches!(
            result,
            Err(ExperimentError::InsufficientSamples {
                have: 5,
                need: 30,
                ..
            })
        ));
    }

    // ------------------------------------------------------------------
    // test_check_significance_identical_samples
    // ------------------------------------------------------------------

    #[test]
    fn test_check_significance_identical_samples() {
        let engine = make_engine(5, 1000);
        create_basic(&engine, "identical");

        for _ in 0..10 {
            engine.add_control_sample("identical", 100.0).unwrap();
            engine.add_treatment_sample("identical", 100.0).unwrap();
        }

        let sig = engine.check_significance("identical").unwrap();
        assert!((sig.t_statistic).abs() < f64::EPSILON);
        assert!(!sig.significant);
    }

    // ------------------------------------------------------------------
    // test_check_significance_different_distributions
    // ------------------------------------------------------------------

    #[test]
    fn test_check_significance_different_distributions() {
        let engine = make_engine(5, 1000);
        create_basic(&engine, "diff");

        // Control: values around 200
        for i in 0..50 {
            engine
                .add_control_sample("diff", 195.0 + (i % 10) as f64)
                .unwrap();
        }
        // Treatment: values around 10
        for i in 0..50 {
            engine
                .add_treatment_sample("diff", 8.0 + (i % 4) as f64)
                .unwrap();
        }

        let sig = engine.check_significance("diff").unwrap();
        assert!(sig.significant, "p={}", sig.p_value_approx);
        assert!(sig.t_statistic > 0.0); // control mean > treatment mean
    }

    // ------------------------------------------------------------------
    // test_welch_t_test_known_values
    // ------------------------------------------------------------------

    #[test]
    fn test_welch_t_test_known_values() {
        // Two clearly different distributions
        let control = vec![10.0, 12.0, 11.0, 9.0, 10.5, 11.5, 10.0, 12.0, 11.0, 9.5];
        let treatment = vec![20.0, 22.0, 21.0, 19.0, 20.5, 21.5, 20.0, 22.0, 21.0, 19.5];

        let result = welch_t_test(&control, &treatment, 0.05);

        // t should be negative (control mean < treatment mean)
        assert!(result.t_statistic < 0.0, "t={}", result.t_statistic);
        assert!(result.p_value_approx < 0.01, "p={}", result.p_value_approx);
        assert!(result.significant);
        assert!(result.degrees_of_freedom > 0.0);
    }

    // ------------------------------------------------------------------
    // test_normal_cdf_symmetry
    // ------------------------------------------------------------------

    #[test]
    fn test_normal_cdf_symmetry() {
        // CDF(0) should be 0.5
        let mid = normal_cdf(0.0);
        assert!((mid - 0.5).abs() < 0.01, "normal_cdf(0) = {}", mid);

        // CDF(-x) + CDF(x) should be approximately 1.0
        for x in [0.5, 1.0, 1.5, 2.0, 2.5, 3.0] {
            let sum = normal_cdf(x) + normal_cdf(-x);
            assert!(
                (sum - 1.0).abs() < 0.02,
                "CDF({}) + CDF(-{}) = {}",
                x,
                x,
                sum
            );
        }
    }

    // ------------------------------------------------------------------
    // test_normal_cdf_known_values
    // ------------------------------------------------------------------

    #[test]
    fn test_normal_cdf_known_values() {
        // CDF(1.96) ≈ 0.975
        let val = normal_cdf(1.96);
        assert!((val - 0.975).abs() < 0.01, "normal_cdf(1.96) = {}", val);

        // CDF(-8.1) should be 0.0
        assert_eq!(normal_cdf(-8.1), 0.0);

        // CDF(8.1) should be 1.0
        assert_eq!(normal_cdf(8.1), 1.0);
    }

    // ------------------------------------------------------------------
    // test_auto_conclude_treatment_wins
    // ------------------------------------------------------------------

    #[test]
    fn test_auto_conclude_treatment_wins() {
        // lower_is_better=true, treatment has lower values → treatment wins
        let engine = make_engine(5, 1000);
        engine
            .create_experiment(
                "tw".to_owned(),
                "Treatment wins".to_owned(),
                "p".to_owned(),
                500.0,
                350.0,
                "latency".to_owned(),
                true, // lower is better
            )
            .unwrap();

        // Control: high values (~200)
        for i in 0..20 {
            engine
                .add_control_sample("tw", 195.0 + (i % 10) as f64)
                .unwrap();
        }

        // Treatment: low values (~10), last sample should trigger conclusion
        let mut concluded = false;
        for i in 0..20 {
            if let Ok(Some(result)) = engine.add_treatment_sample("tw", 8.0 + (i % 4) as f64) {
                assert_eq!(result.conclusion, ExperimentConclusion::TreatmentWins);
                concluded = true;
                break;
            }
        }
        assert!(concluded, "experiment should have auto-concluded");
    }

    // ------------------------------------------------------------------
    // test_auto_conclude_control_wins
    // ------------------------------------------------------------------

    #[test]
    fn test_auto_conclude_control_wins() {
        // lower_is_better=true, control has lower values → control wins
        let engine = make_engine(5, 1000);
        engine
            .create_experiment(
                "cw".to_owned(),
                "Control wins".to_owned(),
                "p".to_owned(),
                100.0,
                500.0,
                "latency".to_owned(),
                true,
            )
            .unwrap();

        // Control: low values (~10)
        for i in 0..20 {
            engine
                .add_control_sample("cw", 8.0 + (i % 4) as f64)
                .unwrap();
        }

        // Treatment: high values (~200)
        let mut concluded = false;
        for i in 0..20 {
            if let Ok(Some(result)) = engine.add_treatment_sample("cw", 195.0 + (i % 10) as f64) {
                assert_eq!(result.conclusion, ExperimentConclusion::ControlWins);
                concluded = true;
                break;
            }
        }
        assert!(concluded, "experiment should have auto-concluded");
    }

    // ------------------------------------------------------------------
    // test_auto_conclude_timed_out_at_max_samples
    // ------------------------------------------------------------------

    #[test]
    fn test_auto_conclude_timed_out_at_max_samples() {
        // Identical values → no significance → hits max_samples → TimedOut
        let engine = make_engine(3, 10);
        create_basic(&engine, "timeout");

        let mut concluded = false;
        for _ in 0..10 {
            engine.add_control_sample("timeout", 50.0).unwrap();
            if let Ok(Some(result)) = engine.add_treatment_sample("timeout", 50.0) {
                assert_eq!(result.conclusion, ExperimentConclusion::TimedOut);
                concluded = true;
                break;
            }
        }
        assert!(concluded, "experiment should have timed out at max samples");
    }

    // ------------------------------------------------------------------
    // test_lower_is_better_reverses_winner
    // ------------------------------------------------------------------

    #[test]
    fn test_lower_is_better_reverses_winner() {
        // lower_is_better=false: higher metric is better
        // Treatment has higher values → treatment wins
        let engine = make_engine(5, 1000);
        engine
            .create_experiment(
                "higher_better".to_owned(),
                "Higher is better".to_owned(),
                "p".to_owned(),
                1.0,
                2.0,
                "throughput".to_owned(),
                false, // higher is better
            )
            .unwrap();

        // Control: low values (~10)
        for i in 0..20 {
            engine
                .add_control_sample("higher_better", 8.0 + (i % 4) as f64)
                .unwrap();
        }

        // Treatment: high values (~200), treatment should win since higher is better
        let mut concluded = false;
        for i in 0..20 {
            if let Ok(Some(result)) =
                engine.add_treatment_sample("higher_better", 195.0 + (i % 10) as f64)
            {
                assert_eq!(result.conclusion, ExperimentConclusion::TreatmentWins);
                concluded = true;
                break;
            }
        }
        assert!(concluded, "experiment should have auto-concluded");
    }

    // ------------------------------------------------------------------
    // test_conclude_experiment_forced
    // ------------------------------------------------------------------

    #[test]
    fn test_conclude_experiment_forced() {
        let engine = make_engine(100, 1000);
        create_basic(&engine, "forced");

        // Add only a few samples (not enough for significance)
        for i in 0..5 {
            engine.add_control_sample("forced", i as f64).unwrap();
            engine.add_treatment_sample("forced", i as f64).unwrap();
        }

        let result = engine.conclude_experiment("forced").unwrap();
        assert_eq!(
            result.conclusion,
            ExperimentConclusion::NoSignificantDifference
        );
        assert!(result.significance.is_none()); // not enough samples for test

        // Verify experiment is now concluded
        let exp = engine.get_experiment("forced").unwrap();
        assert!(matches!(exp.status, ExperimentStatus::Concluded(_)));
    }

    // ------------------------------------------------------------------
    // test_get_experiment
    // ------------------------------------------------------------------

    #[test]
    fn test_get_experiment() {
        let engine = ExperimentEngine::with_defaults();
        create_basic(&engine, "getme");

        let exp = engine.get_experiment("getme").unwrap();
        assert_eq!(exp.id, "getme");
        assert_eq!(exp.parameter, "timeout_ms");
        assert_eq!(exp.status, ExperimentStatus::Running);
        assert!(exp.control_samples.is_empty());
        assert!(exp.treatment_samples.is_empty());
    }

    // ------------------------------------------------------------------
    // test_active_experiments_lists_running
    // ------------------------------------------------------------------

    #[test]
    fn test_active_experiments_lists_running() {
        let engine = make_engine(100, 1000);
        create_basic(&engine, "a");
        create_basic(&engine, "b");
        create_basic(&engine, "c");

        // Conclude one
        engine.conclude_experiment("b").unwrap();

        let active = engine.active_experiments();
        assert_eq!(active.len(), 2);
        assert!(active.contains(&"a".to_owned()));
        assert!(active.contains(&"c".to_owned()));
        assert!(!active.contains(&"b".to_owned()));
    }

    // ------------------------------------------------------------------
    // test_concluded_results_stores_history
    // ------------------------------------------------------------------

    #[test]
    fn test_concluded_results_stores_history() {
        let engine = make_engine(100, 1000);
        create_basic(&engine, "hist1");
        create_basic(&engine, "hist2");

        engine.conclude_experiment("hist1").unwrap();
        engine.conclude_experiment("hist2").unwrap();

        let results = engine.concluded_results();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].experiment_id, "hist1");
        assert_eq!(results[1].experiment_id, "hist2");
    }

    // ------------------------------------------------------------------
    // test_remove_experiment
    // ------------------------------------------------------------------

    #[test]
    fn test_remove_experiment() {
        let engine = ExperimentEngine::with_defaults();
        create_basic(&engine, "removeme");

        assert!(engine.get_experiment("removeme").is_ok());
        engine.remove_experiment("removeme").unwrap();
        assert!(matches!(
            engine.get_experiment("removeme"),
            Err(ExperimentError::ExperimentNotFound(_))
        ));
    }

    // ------------------------------------------------------------------
    // test_remove_nonexistent_fails
    // ------------------------------------------------------------------

    #[test]
    fn test_remove_nonexistent_fails() {
        let engine = ExperimentEngine::with_defaults();
        let result = engine.remove_experiment("nope");
        assert!(matches!(
            result,
            Err(ExperimentError::ExperimentNotFound(_))
        ));
    }

    // ------------------------------------------------------------------
    // test_config_defaults
    // ------------------------------------------------------------------

    #[test]
    fn test_config_defaults() {
        let config = ExperimentConfig::default();
        assert_eq!(config.min_samples_per_arm, 30);
        assert_eq!(config.max_samples_per_arm, 1000);
        assert!((config.significance_level - 0.05).abs() < f64::EPSILON);
        assert_eq!(config.max_duration_secs, 3600);
    }

    // ------------------------------------------------------------------
    // test_clone_shares_state
    // ------------------------------------------------------------------

    #[test]
    fn test_clone_shares_state() {
        let engine1 = ExperimentEngine::with_defaults();
        let engine2 = engine1.clone();

        create_basic(&engine1, "shared");

        // engine2 should see the experiment created via engine1
        let exp = engine2.get_experiment("shared");
        assert!(exp.is_ok(), "cloned engine should share state");
        assert_eq!(exp.unwrap().id, "shared");
    }

    // ------------------------------------------------------------------
    // test_experiment_serialization
    // ------------------------------------------------------------------

    #[test]
    fn test_experiment_serialization() {
        let engine = ExperimentEngine::with_defaults();
        create_basic(&engine, "serial");

        engine.add_control_sample("serial", 42.0).unwrap();
        engine.add_treatment_sample("serial", 37.0).unwrap();

        let exp = engine.get_experiment("serial").unwrap();

        // Serialize to JSON
        let json = serde_json::to_string(&exp).unwrap();
        assert!(json.contains("\"id\":\"serial\""));
        assert!(json.contains("\"status\":\"Running\""));

        // Deserialize back
        let deser: Experiment = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.id, "serial");
        assert_eq!(deser.control_samples.len(), 1);
        assert_eq!(deser.treatment_samples.len(), 1);
    }

    // ------------------------------------------------------------------
    // test_significance_result_fields
    // ------------------------------------------------------------------

    #[test]
    fn test_significance_result_fields() {
        let control = vec![10.0, 12.0, 11.0, 9.0, 10.5, 11.5, 10.0, 12.0, 11.0, 9.5];
        let treatment = vec![20.0, 22.0, 21.0, 19.0, 20.5, 21.5, 20.0, 22.0, 21.0, 19.5];

        let result = welch_t_test(&control, &treatment, 0.05);

        // Verify all fields are populated and sensible
        assert!(result.t_statistic.is_finite());
        assert!(result.degrees_of_freedom > 0.0);
        assert!(result.p_value_approx >= 0.0);
        assert!(result.p_value_approx <= 1.0);
        assert!((result.control_mean - 10.65).abs() < 0.1);
        assert!((result.treatment_mean - 20.65).abs() < 0.1);
        assert!(result.control_stddev > 0.0);
        assert!(result.treatment_stddev > 0.0);

        // Verify serialization round-trip
        let json = serde_json::to_string(&result).unwrap();
        let deser: SignificanceResult = serde_json::from_str(&json).unwrap();
        assert!((deser.t_statistic - result.t_statistic).abs() < f64::EPSILON);
    }
}
