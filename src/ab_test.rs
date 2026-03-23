//! # A/B Testing Framework
//!
//! Statistically rigorous A/B testing for LLM model and prompt experiments.
//! Supports weighted traffic splits, deterministic user assignment via FNV hash,
//! two-proportion z-test significance testing, and automatic experiment completion.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use tracing::{debug, info, warn};

use crate::templates::PromptTemplate;

// ---------------------------------------------------------------------------
// Re-export legacy types so existing lib.rs re-exports keep compiling
// ---------------------------------------------------------------------------

/// Which A/B variant a request is assigned to (legacy two-variant API).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// The control variant.
    A,
    /// The treatment variant.
    B,
}

/// The metric used to evaluate variant quality (legacy API).
#[derive(Clone)]
pub enum SuccessMetric {
    /// Higher output length is better.
    OutputLength,
    /// Lower latency is better.
    Latency,
    /// A caller-supplied scoring function.
    CustomFn(Arc<dyn Fn(&str) -> f64 + Send + Sync>),
    /// Explicit numeric rating supplied by the caller.
    UserRating,
}

impl std::fmt::Debug for SuccessMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutputLength => write!(f, "OutputLength"),
            Self::Latency => write!(f, "Latency"),
            Self::CustomFn(_) => write!(f, "CustomFn(...)"),
            Self::UserRating => write!(f, "UserRating"),
        }
    }
}

/// Configuration for a single legacy two-variant A/B experiment.
#[derive(Debug, Clone)]
pub struct AbTestConfig {
    /// Unique experiment name.
    pub name: String,
    /// The control prompt template.
    pub variant_a: PromptTemplate,
    /// The treatment prompt template.
    pub variant_b: PromptTemplate,
    /// Fraction of traffic routed to variant A (0.0–1.0).
    pub traffic_split: f64,
    /// Metric used to score each response.
    pub success_metric: SuccessMetric,
    /// Minimum observations per variant before analysis.
    pub min_samples: usize,
}

/// Statistical result of a legacy two-variant A/B experiment.
#[derive(Debug, Clone)]
pub struct AbTestResult {
    /// The winning variant, or `None` when not yet significant.
    pub winner: Option<Variant>,
    /// Confidence level derived from the p-value.
    pub confidence: f64,
    /// Two-tailed p-value.
    pub p_value: f64,
    /// Effect size (Cohen's d).
    pub effect_size: f64,
    /// Number of observations for variant A.
    pub samples_a: usize,
    /// Number of observations for variant B.
    pub samples_b: usize,
    /// Sample mean score for variant A.
    pub mean_a: f64,
    /// Sample mean score for variant B.
    pub mean_b: f64,
}

/// Per-variant running statistics.
#[derive(Debug, Default)]
struct LegacyVariantStats {
    scores: Vec<f64>,
}

impl LegacyVariantStats {
    fn push(&mut self, score: f64) { self.scores.push(score); }
    fn n(&self) -> usize { self.scores.len() }
    fn mean(&self) -> f64 {
        if self.scores.is_empty() { return 0.0; }
        self.scores.iter().sum::<f64>() / self.scores.len() as f64
    }
    fn variance(&self) -> f64 {
        let n = self.scores.len();
        if n < 2 { return 0.0; }
        let mean = self.mean();
        let ss: f64 = self.scores.iter().map(|x| (x - mean).powi(2)).sum();
        ss / (n - 1) as f64
    }
}

struct LegacyExperiment {
    config: AbTestConfig,
    stats_a: LegacyVariantStats,
    stats_b: LegacyVariantStats,
}

/// Thread-safe legacy two-variant A/B test runner.
#[derive(Clone, Default)]
pub struct AbTestRunner {
    experiments: Arc<DashMap<String, parking_lot::Mutex<LegacyExperiment>>>,
}

impl AbTestRunner {
    /// Create an empty runner.
    pub fn new() -> Self { Self::default() }

    /// Register a new experiment.
    pub fn register(&self, config: AbTestConfig) {
        info!(name = %config.name, "ab_test: registering experiment");
        let name = config.name.clone();
        self.experiments.insert(name, parking_lot::Mutex::new(LegacyExperiment {
            config,
            stats_a: LegacyVariantStats::default(),
            stats_b: LegacyVariantStats::default(),
        }));
    }

    /// Remove an experiment.
    pub fn delete(&self, name: &str) -> bool {
        let removed = self.experiments.remove(name).is_some();
        if removed { info!(name, "ab_test: experiment deleted"); }
        else { warn!(name, "ab_test: delete requested for unknown experiment"); }
        removed
    }

    /// Assign a user deterministically to a variant.
    pub fn assign(&self, name: &str, user_id: &str) -> Option<Variant> {
        let exp = self.experiments.get(name)?;
        let guard = exp.lock();
        let split = guard.config.traffic_split.clamp(0.0, 1.0);
        let hash = fnv1a_64_pair(name.as_bytes(), user_id.as_bytes());
        let bucket = (hash as f64) / (u64::MAX as f64);
        let variant = if bucket < split { Variant::A } else { Variant::B };
        debug!(experiment = name, user = user_id, bucket, split, ?variant, "ab_test: assignment");
        Some(variant)
    }

    /// Record a metric observation for the given variant.
    pub fn record_observation(&self, name: &str, variant: Variant, score: f64) {
        if let Some(exp) = self.experiments.get(name) {
            let mut guard = exp.lock();
            match variant {
                Variant::A => guard.stats_a.push(score),
                Variant::B => guard.stats_b.push(score),
            }
        }
    }

    /// Run Welch's t-test on the accumulated observations.
    pub fn analyse(&self, name: &str) -> Option<AbTestResult> {
        let exp = self.experiments.get(name)?;
        let guard = exp.lock();
        let min = guard.config.min_samples;
        if guard.stats_a.n() < min || guard.stats_b.n() < min { return None; }
        let result = welch_t_test(&guard.stats_a, &guard.stats_b);
        info!(experiment = name, ?result.winner, p_value = result.p_value, "ab_test: analysis complete");
        Some(result)
    }

    /// Return sample counts for both variants.
    pub fn sample_counts(&self, name: &str) -> Option<(usize, usize)> {
        let exp = self.experiments.get(name)?;
        let guard = exp.lock();
        Some((guard.stats_a.n(), guard.stats_b.n()))
    }

    /// Return names of all registered experiments.
    pub fn experiment_names(&self) -> Vec<String> {
        self.experiments.iter().map(|e| e.key().clone()).collect()
    }

    /// Return current means for both variants.
    pub fn current_means(&self, name: &str) -> Option<(f64, f64)> {
        let exp = self.experiments.get(name)?;
        let guard = exp.lock();
        Some((guard.stats_a.mean(), guard.stats_b.mean()))
    }
}

fn welch_t_test(a: &LegacyVariantStats, b: &LegacyVariantStats) -> AbTestResult {
    let na = a.n() as f64;
    let nb = b.n() as f64;
    let mean_a = a.mean();
    let mean_b = b.mean();
    let var_a = a.variance();
    let var_b = b.variance();
    let se_sq_a = var_a / na;
    let se_sq_b = var_b / nb;
    let se = (se_sq_a + se_sq_b).sqrt();
    let (p_value, winner) = if se < f64::EPSILON {
        (1.0_f64, None)
    } else {
        let t = (mean_a - mean_b) / se;
        let df_num = (se_sq_a + se_sq_b).powi(2);
        let df_den = se_sq_a.powi(2) / (na - 1.0) + se_sq_b.powi(2) / (nb - 1.0);
        let df = if df_den < f64::EPSILON { 1.0 } else { df_num / df_den };
        let p = two_tailed_p_legacy(t, df);
        let w = if p <= 0.05 {
            if mean_a > mean_b { Some(Variant::A) } else { Some(Variant::B) }
        } else { None };
        (p, w)
    };
    let pooled_var = ((na - 1.0) * var_a + (nb - 1.0) * var_b) / (na + nb - 2.0);
    let pooled_sd = pooled_var.sqrt();
    let effect_size = if pooled_sd < f64::EPSILON { 0.0 } else { (mean_a - mean_b) / pooled_sd };
    let confidence = (1.0 - p_value).clamp(0.0, 1.0);
    AbTestResult { winner, confidence, p_value, effect_size, samples_a: a.n(), samples_b: b.n(), mean_a, mean_b }
}

fn two_tailed_p_legacy(t: f64, df: f64) -> f64 {
    let t2 = t * t;
    let x = df / (df + t2);
    let p_one_tail = regularised_incomplete_beta_legacy(x, df / 2.0, 0.5) / 2.0;
    (2.0 * p_one_tail).clamp(0.0, 1.0)
}

fn regularised_incomplete_beta_legacy(x: f64, a: f64, b: f64) -> f64 {
    if x <= 0.0 { return 0.0; }
    if x >= 1.0 { return 1.0; }
    if x > (a + 1.0) / (a + b + 2.0) {
        return 1.0 - regularised_incomplete_beta_legacy(1.0 - x, b, a);
    }
    let ln_beta_val = ln_gamma_legacy(a) + ln_gamma_legacy(b) - ln_gamma_legacy(a + b);
    let front = (x.ln() * a + (1.0 - x).ln() * b - ln_beta_val).exp() / a;
    let mut c = 1.0_f64;
    let mut d = 1.0 - (a + b) * x / (a + 1.0);
    d = if d.abs() < 1e-30 { 1e-30 } else { 1.0 / d };
    let mut f = d;
    for m in 1_u32..=200 {
        let mf = m as f64;
        let num_even = mf * (b - mf) * x / ((a + 2.0 * mf - 1.0) * (a + 2.0 * mf));
        d = 1.0 + num_even * d;
        d = if d.abs() < 1e-30 { 1e-30 } else { 1.0 / d };
        c = 1.0 + num_even / c;
        c = if c.abs() < 1e-30 { 1e-30 } else { c };
        f *= c * d;
        let num_odd = -(a + mf) * (a + b + mf) * x / ((a + 2.0 * mf) * (a + 2.0 * mf + 1.0));
        d = 1.0 + num_odd * d;
        d = if d.abs() < 1e-30 { 1e-30 } else { 1.0 / d };
        c = 1.0 + num_odd / c;
        c = if c.abs() < 1e-30 { 1e-30 } else { c };
        let delta = c * d;
        f *= delta;
        if (delta - 1.0).abs() < 1e-10 { break; }
    }
    front * f
}

fn ln_gamma_legacy(x: f64) -> f64 {
    const G: f64 = 7.0;
    const C: [f64; 9] = [
        0.999_999_999_999_809_9, 676.520_368_121_885_1, -1_259.139_216_722_402_8,
        771.323_428_777_653_1, -176.615_029_162_140_6, 12.507_343_278_686_905,
        -0.138_571_095_265_720_1, 9.984_369_578_019_572e-6, 1.505_632_735_149_312_4e-7,
    ];
    if x < 0.5 {
        return std::f64::consts::PI.ln() - (std::f64::consts::PI * x).sin().ln() - ln_gamma_legacy(1.0 - x);
    }
    let z = x - 1.0;
    let mut sum = C[0];
    for (i, &ci) in C.iter().enumerate().skip(1) { sum += ci / (z + i as f64); }
    let t = z + G + 0.5;
    0.5 * std::f64::consts::TAU.ln() + (z + 0.5) * t.ln() - t + sum.ln()
}

// ---------------------------------------------------------------------------
// NEW SPEC IMPLEMENTATION
// ---------------------------------------------------------------------------

/// A single variant in an experiment.
#[derive(Debug, Clone)]
pub struct ExperimentVariantSpec {
    /// Unique variant identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Relative weight for traffic split (e.g. 1.0 = equal share).
    pub weight: f64,
    /// Optional model override for this variant.
    pub model_id: Option<String>,
    /// Optional prompt template override.
    pub prompt_template: Option<String>,
    /// Arbitrary config key-value pairs.
    pub config: HashMap<String, String>,
}

/// Status of an experiment lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExperimentStatus {
    /// Not yet started.
    Draft,
    /// Currently collecting samples.
    Running,
    /// Temporarily paused.
    Paused,
    /// Finished normally.
    Completed,
    /// Stopped early.
    Aborted,
}

/// Condition that triggers automatic experiment completion.
#[derive(Debug, Clone)]
pub enum EndCondition {
    /// Stop after a fixed number of total samples.
    FixedSamples(u64),
    /// Stop after a fixed duration in seconds.
    FixedDuration(u64),
    /// Never auto-stop — must be stopped manually.
    ManualStop,
    /// Stop when statistical significance is reached.
    StatisticalSignificance {
        /// Minimum samples per variant before checking.
        min_samples: u64,
        /// Required confidence level (e.g. 0.95).
        confidence: f64,
    },
}

/// A multi-variant experiment definition.
#[derive(Debug, Clone)]
pub struct Experiment {
    /// Unique experiment identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// The variants to test.
    pub variants: Vec<ExperimentVariantSpec>,
    /// Current lifecycle status.
    pub status: ExperimentStatus,
    /// Unix timestamp when the experiment was created.
    pub created_at: u64,
    /// Condition that triggers automatic completion.
    pub end_condition: EndCondition,
}

/// Records that a user was assigned to a specific variant.
#[derive(Debug, Clone)]
pub struct Assignment {
    /// Experiment this assignment belongs to.
    pub experiment_id: String,
    /// Variant the user was assigned to.
    pub variant_id: String,
    /// User identifier.
    pub user_id: String,
    /// Unix timestamp of assignment.
    pub assigned_at: u64,
}

/// Aggregated results for one variant.
#[derive(Debug, Clone)]
pub struct ExperimentResult {
    /// Variant this result belongs to.
    pub variant_id: String,
    /// Total number of observations.
    pub samples: u64,
    /// Number of successful outcomes.
    pub successes: u64,
    /// Total cost accumulated.
    pub total_cost: f64,
    /// Average latency in milliseconds.
    pub avg_latency_ms: f64,
    /// Fraction of samples that were successful.
    pub conversion_rate: f64,
}

/// Per-variant mutable state stored inside `AbTestManager`.
#[derive(Debug, Default, Clone)]
struct VariantState {
    samples: u64,
    successes: u64,
    total_cost: f64,
    total_latency_ms: u64,
}

struct ExperimentState {
    experiment: Experiment,
    variants: HashMap<String, VariantState>,
    /// Total samples across all variants (for FixedSamples end condition).
    total_samples: u64,
}

/// Manages multi-variant A/B experiments.
pub struct AbTestManager {
    experiments: HashMap<String, ExperimentState>,
}

impl AbTestManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self { experiments: HashMap::new() }
    }

    /// Register a new experiment.
    pub fn create_experiment(&mut self, experiment: Experiment) {
        let mut variants: HashMap<String, VariantState> = HashMap::new();
        for v in &experiment.variants {
            variants.insert(v.id.clone(), VariantState::default());
        }
        self.experiments.insert(experiment.id.clone(), ExperimentState {
            experiment,
            variants,
            total_samples: 0,
        });
    }

    /// Deterministically assign a user to a variant using weighted FNV hash.
    ///
    /// Returns `None` if the experiment doesn't exist or is not Running.
    pub fn assign(&self, experiment_id: &str, user_id: &str) -> Option<Assignment> {
        let state = self.experiments.get(experiment_id)?;
        if state.experiment.status != ExperimentStatus::Running {
            return None;
        }
        let variants = &state.experiment.variants;
        if variants.is_empty() {
            return None;
        }
        let total_weight: f64 = variants.iter().map(|v| v.weight.max(0.0)).sum();
        if total_weight <= 0.0 {
            return None;
        }

        // FNV-1a hash of user_id + experiment_id
        let hash = fnv1a_64_pair(user_id.as_bytes(), experiment_id.as_bytes());
        // Map to [0.0, total_weight)
        let bucket = (hash as f64 / u64::MAX as f64) * total_weight;

        let mut cumulative = 0.0;
        let mut selected_variant_id = &variants[0].id;
        for v in variants {
            cumulative += v.weight.max(0.0);
            if bucket < cumulative {
                selected_variant_id = &v.id;
                break;
            }
        }

        Some(Assignment {
            experiment_id: experiment_id.to_string(),
            variant_id: selected_variant_id.clone(),
            user_id: user_id.to_string(),
            assigned_at: 0, // caller fills in timestamp
        })
    }

    /// Record an outcome for a previously made assignment.
    pub fn record_outcome(&mut self, assignment: &Assignment, success: bool, cost: f64, latency_ms: u64) {
        if let Some(state) = self.experiments.get_mut(&assignment.experiment_id) {
            if let Some(vs) = state.variants.get_mut(&assignment.variant_id) {
                vs.samples += 1;
                if success { vs.successes += 1; }
                vs.total_cost += cost;
                vs.total_latency_ms += latency_ms;
                state.total_samples += 1;
            }
        }
    }

    /// Return aggregated results for all variants in an experiment.
    pub fn results(&self, experiment_id: &str) -> Vec<ExperimentResult> {
        let Some(state) = self.experiments.get(experiment_id) else { return vec![]; };
        state.experiment.variants.iter().map(|v| {
            let vs = state.variants.get(&v.id).cloned().unwrap_or_default();
            let conversion_rate = if vs.samples > 0 { vs.successes as f64 / vs.samples as f64 } else { 0.0 };
            let avg_latency_ms = if vs.samples > 0 { vs.total_latency_ms as f64 / vs.samples as f64 } else { 0.0 };
            ExperimentResult {
                variant_id: v.id.clone(),
                samples: vs.samples,
                successes: vs.successes,
                total_cost: vs.total_cost,
                avg_latency_ms,
                conversion_rate,
            }
        }).collect()
    }

    /// Two-proportion z-test significance.
    ///
    /// Returns p-value approximation using erfc: p = erfc(|z| / sqrt(2)).
    pub fn statistical_significance(r_a: &ExperimentResult, r_b: &ExperimentResult) -> f64 {
        let n_a = r_a.samples as f64;
        let n_b = r_b.samples as f64;
        if n_a < 1.0 || n_b < 1.0 { return 1.0; }
        let s_a = r_a.successes as f64;
        let s_b = r_b.successes as f64;
        let p_a = s_a / n_a;
        let p_b = s_b / n_b;
        let p_pool = (s_a + s_b) / (n_a + n_b);
        let denom = (p_pool * (1.0 - p_pool) * (1.0 / n_a + 1.0 / n_b)).sqrt();
        if denom < f64::EPSILON { return 1.0; }
        let z = (p_a - p_b) / denom;
        erfc_approx(z.abs() / std::f64::consts::SQRT_2)
    }

    /// Return the variant ID with the highest conversion rate if statistically
    /// significant (z > 1.96 for p < 0.05), or None.
    pub fn winner(&self, experiment_id: &str) -> Option<String> {
        let results = self.results(experiment_id);
        if results.len() < 2 { return None; }

        // Find variant with highest conversion rate.
        let best = results.iter()
            .max_by(|a, b| a.conversion_rate.partial_cmp(&b.conversion_rate).unwrap_or(std::cmp::Ordering::Equal))?;

        // Check significance vs second-best.
        let second = results.iter()
            .filter(|r| r.variant_id != best.variant_id)
            .max_by(|a, b| a.conversion_rate.partial_cmp(&b.conversion_rate).unwrap_or(std::cmp::Ordering::Equal))?;

        let p = Self::statistical_significance(best, second);
        if p < 0.05 {
            Some(best.variant_id.clone())
        } else {
            None
        }
    }

    /// Check end condition and mark experiment completed if met. Returns true if completed.
    pub fn auto_complete(&mut self, experiment_id: &str, now: u64) -> bool {
        let state = match self.experiments.get_mut(experiment_id) {
            Some(s) => s,
            None => return false,
        };
        if state.experiment.status != ExperimentStatus::Running {
            return false;
        }
        let should_complete = match &state.experiment.end_condition {
            EndCondition::FixedSamples(n) => state.total_samples >= *n,
            EndCondition::FixedDuration(secs) => now >= state.experiment.created_at + secs,
            EndCondition::ManualStop => false,
            EndCondition::StatisticalSignificance { min_samples, confidence } => {
                let min = *min_samples;
                let required_confidence = *confidence;
                // Check if all variants have min samples and any pair is significant.
                let all_have_min = state.variants.values().all(|v| v.samples >= min);
                if !all_have_min { return false; }
                // Build results inline to avoid borrow issues.
                let results: Vec<ExperimentResult> = state.experiment.variants.iter().map(|v| {
                    let vs = state.variants.get(&v.id).cloned().unwrap_or_default();
                    let conversion_rate = if vs.samples > 0 { vs.successes as f64 / vs.samples as f64 } else { 0.0 };
                    let avg_latency_ms = if vs.samples > 0 { vs.total_latency_ms as f64 / vs.samples as f64 } else { 0.0 };
                    ExperimentResult {
                        variant_id: v.id.clone(),
                        samples: vs.samples,
                        successes: vs.successes,
                        total_cost: vs.total_cost,
                        avg_latency_ms,
                        conversion_rate,
                    }
                }).collect();
                // Check any pair for significance.
                let mut found = false;
                'outer: for i in 0..results.len() {
                    for j in (i + 1)..results.len() {
                        let p = Self::statistical_significance(&results[i], &results[j]);
                        if 1.0 - p >= required_confidence {
                            found = true;
                            break 'outer;
                        }
                    }
                }
                found
            }
        };
        if should_complete {
            state.experiment.status = ExperimentStatus::Completed;
            info!(experiment_id, "ab_test: experiment auto-completed");
        }
        should_complete
    }
}

impl Default for AbTestManager {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Math helpers
// ---------------------------------------------------------------------------

/// erfc approximation using Horner's method (Abramowitz & Stegun 7.1.26).
/// Accurate to ~1.5e-7.
fn erfc_approx(x: f64) -> f64 {
    if x < 0.0 { return 2.0 - erfc_approx(-x); }
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let poly = t * (0.254_829_592
        + t * (-0.284_496_736
        + t * (1.421_413_741
        + t * (-1.453_152_027
        + t * 1.061_405_429))));
    poly * (-x * x).exp()
}

/// FNV-1a 64-bit hash of two byte slices separated by a null byte.
fn fnv1a_64_pair(a: &[u8], b: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    let mut hash = OFFSET_BASIS;
    for &byte in a.iter().chain(&[0u8]).chain(b.iter()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn make_running_experiment(variants_weights: &[(&str, f64)]) -> Experiment {
        let variants = variants_weights.iter().map(|(id, w)| ExperimentVariantSpec {
            id: id.to_string(),
            name: id.to_string(),
            weight: *w,
            model_id: None,
            prompt_template: None,
            config: HashMap::new(),
        }).collect();
        Experiment {
            id: "exp-1".to_string(),
            name: "Test Experiment".to_string(),
            variants,
            status: ExperimentStatus::Running,
            created_at: 1000,
            end_condition: EndCondition::ManualStop,
        }
    }

    #[test]
    fn deterministic_assignment() {
        let mut manager = AbTestManager::new();
        let exp = make_running_experiment(&[("a", 1.0), ("b", 1.0)]);
        manager.create_experiment(exp);

        let a1 = manager.assign("exp-1", "user-42").unwrap();
        let a2 = manager.assign("exp-1", "user-42").unwrap();
        assert_eq!(a1.variant_id, a2.variant_id, "assignment must be deterministic");
    }

    #[test]
    fn weighted_split_proportional() {
        let mut manager = AbTestManager::new();
        // 3:1 weight ratio — expect ~75% assigned to "heavy", ~25% to "light".
        let exp = make_running_experiment(&[("heavy", 3.0), ("light", 1.0)]);
        manager.create_experiment(exp);

        let mut counts: HashMap<String, u64> = HashMap::new();
        for i in 0..1000 {
            let a = manager.assign("exp-1", &format!("user-{i}")).unwrap();
            *counts.entry(a.variant_id).or_default() += 1;
        }
        let heavy = *counts.get("heavy").unwrap_or(&0) as f64;
        // Allow ±10% tolerance around 750.
        assert!(heavy > 650.0 && heavy < 850.0,
            "expected ~750 heavy assignments, got {heavy}");
    }

    #[test]
    fn z_test_significance_large_difference() {
        let r_a = ExperimentResult {
            variant_id: "a".into(), samples: 500, successes: 400,
            total_cost: 0.0, avg_latency_ms: 0.0, conversion_rate: 0.8,
        };
        let r_b = ExperimentResult {
            variant_id: "b".into(), samples: 500, successes: 200,
            total_cost: 0.0, avg_latency_ms: 0.0, conversion_rate: 0.4,
        };
        let p = AbTestManager::statistical_significance(&r_a, &r_b);
        assert!(p < 0.001, "large difference should be highly significant, got p={p}");
    }

    #[test]
    fn z_test_no_significance_equal_rates() {
        let r_a = ExperimentResult {
            variant_id: "a".into(), samples: 100, successes: 50,
            total_cost: 0.0, avg_latency_ms: 0.0, conversion_rate: 0.5,
        };
        let r_b = ExperimentResult {
            variant_id: "b".into(), samples: 100, successes: 50,
            total_cost: 0.0, avg_latency_ms: 0.0, conversion_rate: 0.5,
        };
        let p = AbTestManager::statistical_significance(&r_a, &r_b);
        assert!(p > 0.9, "equal rates should not be significant, got p={p}");
    }

    #[test]
    fn winner_detection() {
        let mut manager = AbTestManager::new();
        let exp = make_running_experiment(&[("a", 1.0), ("b", 1.0)]);
        manager.create_experiment(exp);

        // Record 500 outcomes per variant: a=80% success, b=40% success.
        let assign_a = Assignment { experiment_id: "exp-1".into(), variant_id: "a".into(), user_id: "u".into(), assigned_at: 0 };
        let assign_b = Assignment { experiment_id: "exp-1".into(), variant_id: "b".into(), user_id: "u".into(), assigned_at: 0 };
        for i in 0..500u64 {
            manager.record_outcome(&assign_a, i < 400, 0.01, 50);
            manager.record_outcome(&assign_b, i < 200, 0.01, 50);
        }
        let winner = manager.winner("exp-1");
        assert_eq!(winner.as_deref(), Some("a"), "variant a should win with 80% vs 40%");
    }

    #[test]
    fn auto_complete_on_fixed_samples() {
        let mut manager = AbTestManager::new();
        let mut exp = make_running_experiment(&[("a", 1.0), ("b", 1.0)]);
        exp.end_condition = EndCondition::FixedSamples(10);
        manager.create_experiment(exp);

        let assign_a = Assignment { experiment_id: "exp-1".into(), variant_id: "a".into(), user_id: "u".into(), assigned_at: 0 };
        for i in 0..9u64 {
            manager.record_outcome(&assign_a, i % 2 == 0, 0.01, 50);
        }
        // 9 samples — not yet complete.
        assert!(!manager.auto_complete("exp-1", 2000));

        manager.record_outcome(&assign_a, true, 0.01, 50);
        // 10 samples — should complete.
        assert!(manager.auto_complete("exp-1", 2000));
        assert_eq!(manager.experiments["exp-1"].experiment.status, ExperimentStatus::Completed);
    }

    // --- Legacy AbTestRunner tests kept for regression ---

    fn make_runner_with_experiment(split: f64, min_samples: usize) -> AbTestRunner {
        let runner = AbTestRunner::new();
        runner.register(AbTestConfig {
            name: "test-exp".into(),
            variant_a: PromptTemplate::builder("a").body("A").build(),
            variant_b: PromptTemplate::builder("b").body("B").build(),
            traffic_split: split,
            success_metric: SuccessMetric::OutputLength,
            min_samples,
        });
        runner
    }

    #[test]
    fn legacy_assignment_is_deterministic() {
        let runner = make_runner_with_experiment(0.5, 1);
        let v1 = runner.assign("test-exp", "user-123").unwrap();
        let v2 = runner.assign("test-exp", "user-123").unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn legacy_split_zero_always_gives_b() {
        let runner = make_runner_with_experiment(0.0, 1);
        for i in 0..20 {
            let v = runner.assign("test-exp", &format!("u{i}")).unwrap();
            assert_eq!(v, Variant::B);
        }
    }

    #[test]
    fn legacy_analyse_detects_significant_difference() {
        let runner = make_runner_with_experiment(0.5, 20);
        for i in 0..50 {
            let jitter = (i as f64) * 0.001;
            runner.record_observation("test-exp", Variant::A, 10.0 + jitter);
            runner.record_observation("test-exp", Variant::B, 1.0 + jitter);
        }
        let result = runner.analyse("test-exp").unwrap();
        assert_eq!(result.winner, Some(Variant::A));
        assert!(result.p_value < 0.05);
    }
}
