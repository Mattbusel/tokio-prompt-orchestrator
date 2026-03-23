//! A/B test runner with statistical significance testing.

use std::collections::HashMap;

/// A single variant in an experiment.
#[derive(Debug, Clone)]
pub struct ExperimentVariant {
    pub name: String,
    pub prompt_template: String,
    pub weight: f64,
}

/// Configuration for an experiment.
#[derive(Debug, Clone)]
pub struct ExperimentConfig {
    pub name: String,
    pub variants: Vec<ExperimentVariant>,
    pub min_samples: usize,
    pub confidence_level: f64,
}

/// Recorded results for a single variant.
#[derive(Debug, Clone)]
pub struct VariantResult {
    pub variant_name: String,
    pub samples: Vec<f64>,
    pub mean: f64,
    pub std_dev: f64,
    pub sample_count: usize,
}

impl VariantResult {
    fn new(name: &str) -> Self {
        Self {
            variant_name: name.to_string(),
            samples: Vec::new(),
            mean: 0.0,
            std_dev: 0.0,
            sample_count: 0,
        }
    }

    fn push(&mut self, value: f64) {
        self.samples.push(value);
        self.sample_count = self.samples.len();
        self.mean = self.samples.iter().sum::<f64>() / self.sample_count as f64;
        if self.sample_count > 1 {
            let variance = self.samples.iter()
                .map(|x| (x - self.mean).powi(2))
                .sum::<f64>()
                / (self.sample_count - 1) as f64;
            self.std_dev = variance.sqrt();
        } else {
            self.std_dev = 0.0;
        }
    }
}

/// Result of a statistical significance test.
#[derive(Debug, Clone)]
pub struct SignificanceTest {
    pub p_value: f64,
    pub is_significant: bool,
    pub effect_size: f64,
    pub winner: Option<String>,
}

/// State of a single experiment.
#[derive(Debug)]
pub struct ExperimentState {
    pub config: ExperimentConfig,
    pub results: HashMap<String, VariantResult>,
    pub started_at_ms: u64,
}

/// Manages multiple A/B experiments.
pub struct ExperimentRunner {
    pub experiments: HashMap<String, ExperimentState>,
    next_id: u64,
}

impl ExperimentRunner {
    /// Create a new `ExperimentRunner`.
    pub fn new() -> Self {
        Self {
            experiments: HashMap::new(),
            next_id: 1,
        }
    }

    /// Register an experiment and return its generated ID.
    pub fn create_experiment(&mut self, config: ExperimentConfig) -> String {
        let id = format!("exp-{}", self.next_id);
        self.next_id += 1;
        let mut results = HashMap::new();
        for v in &config.variants {
            results.insert(v.name.clone(), VariantResult::new(&v.name));
        }
        let state = ExperimentState {
            config,
            results,
            started_at_ms: current_time_ms(),
        };
        self.experiments.insert(id.clone(), state);
        id
    }

    /// Consistently assign a variant to a user using FNV1a hashing.
    pub fn assign_variant<'a>(&'a self, experiment_id: &str, user_id: &str) -> Option<&'a ExperimentVariant> {
        let state = self.experiments.get(experiment_id)?;
        if state.config.variants.is_empty() {
            return None;
        }
        let total_weight: f64 = state.config.variants.iter().map(|v| v.weight).sum();
        if total_weight <= 0.0 {
            return None;
        }
        let hash = fnv1a(user_id);
        // Map hash to [0, total_weight)
        let position = (hash as f64 / u64::MAX as f64) * total_weight;
        let mut cumulative = 0.0;
        for variant in &state.config.variants {
            cumulative += variant.weight;
            if position < cumulative {
                return Some(variant);
            }
        }
        // Fallback to last variant
        state.config.variants.last()
    }

    /// Record a metric value for a variant.
    pub fn record_metric(&mut self, experiment_id: &str, variant_name: &str, value: f64) {
        if let Some(state) = self.experiments.get_mut(experiment_id) {
            let entry = state.results.entry(variant_name.to_string())
                .or_insert_with(|| VariantResult::new(variant_name));
            entry.push(value);
        }
    }

    /// Run Welch's t-test between two samples. Returns the two-tailed p-value approximated
    /// via the normal CDF.
    pub fn welch_t_test(a: &[f64], b: &[f64]) -> f64 {
        if a.len() < 2 || b.len() < 2 {
            return 1.0;
        }
        let mean_a = a.iter().sum::<f64>() / a.len() as f64;
        let mean_b = b.iter().sum::<f64>() / b.len() as f64;
        let var_a = a.iter().map(|x| (x - mean_a).powi(2)).sum::<f64>() / (a.len() - 1) as f64;
        let var_b = b.iter().map(|x| (x - mean_b).powi(2)).sum::<f64>() / (b.len() - 1) as f64;
        let se = (var_a / a.len() as f64 + var_b / b.len() as f64).sqrt();
        if se == 0.0 {
            return if (mean_a - mean_b).abs() < 1e-12 { 1.0 } else { 0.0 };
        }
        let t = (mean_a - mean_b) / se;
        // Approximate p-value via standard normal CDF (two-tailed)
        let p = 2.0 * (1.0 - normal_cdf(t.abs()));
        p.clamp(0.0, 1.0)
    }

    /// Cohen's d effect size: (mean_a - mean_b) / pooled_std
    pub fn cohen_d(a: &[f64], b: &[f64]) -> f64 {
        if a.len() < 2 || b.len() < 2 {
            return 0.0;
        }
        let mean_a = a.iter().sum::<f64>() / a.len() as f64;
        let mean_b = b.iter().sum::<f64>() / b.len() as f64;
        let var_a = a.iter().map(|x| (x - mean_a).powi(2)).sum::<f64>() / (a.len() - 1) as f64;
        let var_b = b.iter().map(|x| (x - mean_b).powi(2)).sum::<f64>() / (b.len() - 1) as f64;
        let pooled_std = ((var_a + var_b) / 2.0).sqrt();
        if pooled_std == 0.0 {
            return 0.0;
        }
        (mean_a - mean_b) / pooled_std
    }

    /// Pairwise comparison of control (first variant) vs all others.
    pub fn analyze_experiment(&self, experiment_id: &str) -> Option<SignificanceTest> {
        let state = self.experiments.get(experiment_id)?;
        if state.config.variants.len() < 2 {
            return None;
        }
        let control_name = &state.config.variants[0].name;
        let control = state.results.get(control_name)?;
        if control.samples.len() < state.config.min_samples {
            return None;
        }

        let alpha = 1.0 - state.config.confidence_level;
        let mut best_p = 1.0_f64;
        let mut best_d = 0.0_f64;
        let mut winner: Option<String> = None;

        for variant in state.config.variants.iter().skip(1) {
            if let Some(vr) = state.results.get(&variant.name) {
                if vr.samples.len() < state.config.min_samples {
                    continue;
                }
                let p = Self::welch_t_test(&control.samples, &vr.samples);
                let d = Self::cohen_d(&control.samples, &vr.samples);
                if p < best_p {
                    best_p = p;
                    best_d = d;
                    if p < alpha {
                        // Positive d means control is better, negative means variant is better
                        winner = if d > 0.0 {
                            Some(control_name.clone())
                        } else {
                            Some(variant.name.clone())
                        };
                    }
                }
            }
        }

        Some(SignificanceTest {
            p_value: best_p,
            is_significant: best_p < alpha,
            effect_size: best_d,
            winner,
        })
    }

    /// Generate a text report for an experiment.
    pub fn experiment_report(&self, experiment_id: &str) -> Option<String> {
        let state = self.experiments.get(experiment_id)?;
        let mut out = format!("=== Experiment: {} (id={}) ===\n", state.config.name, experiment_id);
        out.push_str(&format!("Confidence level: {:.0}%\n", state.config.confidence_level * 100.0));
        out.push_str(&format!("Min samples required: {}\n\n", state.config.min_samples));

        for variant in &state.config.variants {
            if let Some(vr) = state.results.get(&variant.name) {
                out.push_str(&format!(
                    "Variant: {} | n={} | mean={:.4} | std_dev={:.4}\n",
                    vr.variant_name, vr.sample_count, vr.mean, vr.std_dev
                ));
            } else {
                out.push_str(&format!("Variant: {} | no data\n", variant.name));
            }
        }

        if let Some(sig) = self.analyze_experiment(experiment_id) {
            out.push_str(&format!(
                "\nSignificance test: p={:.4}, significant={}, effect_size={:.4}\n",
                sig.p_value, sig.is_significant, sig.effect_size
            ));
            if let Some(w) = &sig.winner {
                out.push_str(&format!("Winner: {}\n", w));
            } else {
                out.push_str("Winner: (none yet)\n");
            }
        } else {
            out.push_str("\nInsufficient data for significance test.\n");
        }

        Some(out)
    }
}

impl Default for ExperimentRunner {
    fn default() -> Self {
        Self::new()
    }
}

// --- helpers ---

fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

/// Approximate the normal CDF Φ(x) using the Abramowitz & Stegun rational approximation.
fn normal_cdf(x: f64) -> f64 {
    if x < 0.0 {
        return 1.0 - normal_cdf(-x);
    }
    let t = 1.0 / (1.0 + 0.2316419 * x);
    let poly = t * (0.319381530
        + t * (-0.356563782
        + t * (1.781477937
        + t * (-1.821255978
        + t * 1.330274429))));
    1.0 - ((-x * x / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt()) * poly
}

fn current_time_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(name: &str, weights: &[f64]) -> ExperimentConfig {
        let variants = weights.iter().enumerate().map(|(i, &w)| ExperimentVariant {
            name: format!("variant_{}", i),
            prompt_template: format!("template {}", i),
            weight: w,
        }).collect();
        ExperimentConfig {
            name: name.to_string(),
            variants,
            min_samples: 5,
            confidence_level: 0.95,
        }
    }

    #[test]
    fn test_variant_assignment_consistent() {
        let mut runner = ExperimentRunner::new();
        let config = make_config("test", &[0.5, 0.5]);
        let eid = runner.create_experiment(config);
        let v1 = runner.assign_variant(&eid, "user-abc").map(|v| v.name.clone());
        let v2 = runner.assign_variant(&eid, "user-abc").map(|v| v.name.clone());
        assert_eq!(v1, v2, "same user should always get same variant");
    }

    #[test]
    fn test_record_metrics_grows_samples() {
        let mut runner = ExperimentRunner::new();
        let config = make_config("growth", &[0.5, 0.5]);
        let eid = runner.create_experiment(config);
        runner.record_metric(&eid, "variant_0", 1.0);
        runner.record_metric(&eid, "variant_0", 2.0);
        runner.record_metric(&eid, "variant_0", 3.0);
        let state = runner.experiments.get(&eid).unwrap();
        let vr = state.results.get("variant_0").unwrap();
        assert_eq!(vr.sample_count, 3);
        assert!((vr.mean - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_welch_t_test_different_distributions() {
        let a: Vec<f64> = (0..30).map(|i| i as f64 * 1.0).collect();
        let b: Vec<f64> = (0..30).map(|i| i as f64 * 1.0 + 100.0).collect();
        let p = ExperimentRunner::welch_t_test(&a, &b);
        assert!(p < 0.05, "clearly different distributions should yield p < 0.05, got {}", p);
    }

    #[test]
    fn test_cohen_d_direction() {
        let a = vec![10.0, 10.0, 10.0, 10.0, 10.0];
        let b = vec![5.0, 5.0, 5.0, 5.0, 5.0];
        let d = ExperimentRunner::cohen_d(&a, &b);
        // a > b so d should be positive... but if pooled_std is 0, d is 0
        // Use non-constant samples to get a meaningful pooled_std
        let a2 = vec![10.0, 11.0, 9.0, 10.5, 9.5];
        let b2 = vec![5.0, 6.0, 4.0, 5.5, 4.5];
        let d2 = ExperimentRunner::cohen_d(&a2, &b2);
        assert!(d2 > 0.0, "a > b so cohen_d should be positive, got {}", d2);
        let _ = d; // suppress unused warning
    }

    #[test]
    fn test_experiment_report_non_empty() {
        let mut runner = ExperimentRunner::new();
        let config = make_config("report_test", &[0.5, 0.5]);
        let eid = runner.create_experiment(config);
        // Add enough samples for significance test
        for i in 0..10 {
            runner.record_metric(&eid, "variant_0", i as f64);
            runner.record_metric(&eid, "variant_1", i as f64 + 50.0);
        }
        let report = runner.experiment_report(&eid);
        assert!(report.is_some());
        let r = report.unwrap();
        assert!(!r.is_empty());
        assert!(r.contains("Experiment"));
    }
}
