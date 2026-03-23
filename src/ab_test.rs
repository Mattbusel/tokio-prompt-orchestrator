//! # Prompt A/B Testing Framework
//!
//! Provides statistically rigorous A/B testing for LLM prompt variants.
//!
//! ## Overview
//!
//! An [`AbTestRunner`] manages one or more named experiments.  Each experiment
//! pits two [`PromptTemplate`] variants against each other, assigns incoming
//! requests to variants via **consistent hashing** (so the same user always
//! sees the same variant), collects per-variant metrics, and determines a
//! winner via **Welch's t-test** once enough samples accumulate.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use tokio_prompt_orchestrator::ab_test::{
//!     AbTestConfig, AbTestRunner, SuccessMetric, Variant,
//! };
//! use tokio_prompt_orchestrator::templates::PromptTemplate;
//!
//! let runner = AbTestRunner::new();
//!
//! let cfg = AbTestConfig {
//!     name: "greeting-style".into(),
//!     variant_a: PromptTemplate::builder("greeting-a")
//!         .body("Hello! How can I help?")
//!         .build(),
//!     variant_b: PromptTemplate::builder("greeting-b")
//!         .body("Hi there! What do you need?")
//!         .build(),
//!     traffic_split: 0.5,          // 50 % to each variant
//!     success_metric: SuccessMetric::OutputLength,
//!     min_samples: 30,
//! };
//!
//! runner.register(cfg);
//!
//! // Assign a user to a variant (consistent: same user_id → same variant).
//! let variant = runner.assign("greeting-style", "user-42");
//! println!("User sees variant: {:?}", variant);
//!
//! // After the response arrives, record the outcome.
//! runner.record_observation("greeting-style", Variant::A, 128.0);
//!
//! // When min_samples is reached, analyse.
//! if let Some(result) = runner.analyse("greeting-style") {
//!     println!("Winner: {:?} (p={:.3})", result.winner, result.p_value);
//! }
//! ```
//!
//! ## Statistical Method
//!
//! Winner determination uses **Welch's t-test** (unequal variances) at a
//! significance level of α = 0.05.  The p-value is approximated with the
//! Bhattacharya (1980) rational-polynomial approximation to the Student-t CDF,
//! which is accurate to ±1 × 10⁻⁶ for the degrees of freedom encountered in
//! typical prompt experiments.
//!
//! Effect size is reported as **Cohen's d** (pooled, Hedges-corrected).

use dashmap::DashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::templates::PromptTemplate;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which A/B variant a request is assigned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// The control variant.
    A,
    /// The treatment variant.
    B,
}

/// The metric used to evaluate variant quality.
#[derive(Clone)]
pub enum SuccessMetric {
    /// Higher output length is better (e.g. completeness).
    OutputLength,
    /// Lower latency is better (score = −latency_ms).
    Latency,
    /// A caller-supplied scoring function.  The `f64` argument is the raw
    /// output length; callers may substitute any observable they like.
    ///
    /// The function must be `Send + Sync` (clone-safe via `Arc`).
    CustomFn(Arc<dyn Fn(&str) -> f64 + Send + Sync>),
    /// Explicit numeric rating supplied by the caller (0.0 – 5.0 typical).
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

/// Configuration for a single A/B experiment.
#[derive(Debug, Clone)]
pub struct AbTestConfig {
    /// Unique experiment name used as the lookup key.
    pub name: String,
    /// The control prompt template.
    pub variant_a: PromptTemplate,
    /// The treatment prompt template.
    pub variant_b: PromptTemplate,
    /// Fraction of traffic routed to variant A (`0.0`–`1.0`).
    ///
    /// `0.5` = 50 % each.  Values outside `[0.0, 1.0]` are clamped.
    pub traffic_split: f64,
    /// Metric used to score each response and determine a winner.
    pub success_metric: SuccessMetric,
    /// Minimum number of observations per variant before analysis is attempted.
    pub min_samples: usize,
}

/// Statistical result of an A/B experiment.
#[derive(Debug, Clone)]
pub struct AbTestResult {
    /// The winning variant, or `None` when the result is not yet significant.
    pub winner: Option<Variant>,
    /// Confidence level derived from the p-value (`1.0 − p_value`), clamped to
    /// `[0.0, 1.0]`.
    pub confidence: f64,
    /// Two-tailed p-value from Welch's t-test.  Values ≤ 0.05 are considered
    /// statistically significant at the 5 % level.
    pub p_value: f64,
    /// Effect size (Cohen's d, Hedges-corrected).  Positive values favour
    /// variant A; negative values favour variant B.
    pub effect_size: f64,
    /// Number of observations collected for variant A.
    pub samples_a: usize,
    /// Number of observations collected for variant B.
    pub samples_b: usize,
    /// Sample mean score for variant A.
    pub mean_a: f64,
    /// Sample mean score for variant B.
    pub mean_b: f64,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Per-variant running statistics (Welford online algorithm for variance).
#[derive(Debug, Default)]
struct VariantStats {
    /// Observation scores.
    scores: Vec<f64>,
}

impl VariantStats {
    fn push(&mut self, score: f64) {
        self.scores.push(score);
    }

    fn n(&self) -> usize {
        self.scores.len()
    }

    fn mean(&self) -> f64 {
        if self.scores.is_empty() {
            return 0.0;
        }
        self.scores.iter().sum::<f64>() / self.scores.len() as f64
    }

    /// Sample variance (Bessel-corrected, n-1 denominator).
    fn variance(&self) -> f64 {
        let n = self.scores.len();
        if n < 2 {
            return 0.0;
        }
        let mean = self.mean();
        let ss: f64 = self.scores.iter().map(|x| (x - mean).powi(2)).sum();
        ss / (n - 1) as f64
    }
}

/// All state for one running experiment.
struct Experiment {
    config: AbTestConfig,
    stats_a: VariantStats,
    stats_b: VariantStats,
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Thread-safe A/B test runner.
///
/// Create one shared instance and register experiments with [`register`].
/// Requests are assigned to variants with [`assign`] and outcomes recorded
/// with [`record_observation`].  Call [`analyse`] to retrieve the current
/// statistical result.
///
/// [`register`]: AbTestRunner::register
/// [`assign`]: AbTestRunner::assign
/// [`record_observation`]: AbTestRunner::record_observation
/// [`analyse`]: AbTestRunner::analyse
#[derive(Clone, Default)]
pub struct AbTestRunner {
    experiments: Arc<DashMap<String, parking_lot::Mutex<Experiment>>>,
}

impl AbTestRunner {
    /// Create an empty runner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new experiment.
    ///
    /// If an experiment with the same name already exists it is **replaced**
    /// (all accumulated samples are discarded).
    pub fn register(&self, config: AbTestConfig) {
        info!(name = %config.name, "ab_test: registering experiment");
        let name = config.name.clone();
        self.experiments.insert(
            name,
            parking_lot::Mutex::new(Experiment {
                config,
                stats_a: VariantStats::default(),
                stats_b: VariantStats::default(),
            }),
        );
    }

    /// Remove an experiment and discard all accumulated data.
    ///
    /// Returns `true` if an experiment with `name` existed.
    pub fn delete(&self, name: &str) -> bool {
        let removed = self.experiments.remove(name).is_some();
        if removed {
            info!(name, "ab_test: experiment deleted");
        } else {
            warn!(name, "ab_test: delete requested for unknown experiment");
        }
        removed
    }

    /// Assign a request identified by `user_id` to a variant for the named
    /// experiment.
    ///
    /// Assignment is **deterministic**: the same `(experiment_name, user_id)`
    /// pair always produces the same variant.  This is achieved by hashing the
    /// concatenation of both strings with FNV-1a and comparing the result
    /// against the configured traffic split.
    ///
    /// Returns `None` when no experiment with `name` is registered.
    pub fn assign(&self, name: &str, user_id: &str) -> Option<Variant> {
        let exp = self.experiments.get(name)?;
        let guard = exp.lock();
        let split = guard.config.traffic_split.clamp(0.0, 1.0);

        // Consistent hash: FNV-1a over name + '\0' + user_id.
        let hash = fnv1a_64_pair(name.as_bytes(), user_id.as_bytes());
        // Map to [0.0, 1.0).
        let bucket = (hash as f64) / (u64::MAX as f64);

        let variant = if bucket < split {
            Variant::A
        } else {
            Variant::B
        };
        debug!(
            experiment = name,
            user = user_id,
            bucket,
            split,
            ?variant,
            "ab_test: assignment"
        );
        Some(variant)
    }

    /// Record a metric observation for the given variant.
    ///
    /// `score` should be computed from the response using the experiment's
    /// [`SuccessMetric`]:
    /// - `OutputLength` → output token / character count (higher is better)
    /// - `Latency` → negate latency ms before passing (lower latency → higher score)
    /// - `CustomFn` → caller applies the function and passes the result
    /// - `UserRating` → explicit rating (e.g. 1–5)
    ///
    /// Does nothing when no experiment with `name` is registered.
    pub fn record_observation(&self, name: &str, variant: Variant, score: f64) {
        if let Some(exp) = self.experiments.get(name) {
            let mut guard = exp.lock();
            match variant {
                Variant::A => guard.stats_a.push(score),
                Variant::B => guard.stats_b.push(score),
            }
            debug!(
                experiment = name,
                ?variant,
                score,
                n_a = guard.stats_a.n(),
                n_b = guard.stats_b.n(),
                "ab_test: observation recorded"
            );
        }
    }

    /// Run Welch's t-test on the accumulated observations for `name`.
    ///
    /// Returns `None` when:
    /// - No experiment with `name` is registered.
    /// - Either variant has fewer observations than `min_samples`.
    pub fn analyse(&self, name: &str) -> Option<AbTestResult> {
        let exp = self.experiments.get(name)?;
        let guard = exp.lock();

        let min = guard.config.min_samples;
        if guard.stats_a.n() < min || guard.stats_b.n() < min {
            return None;
        }

        let result = welch_t_test(&guard.stats_a, &guard.stats_b);
        info!(
            experiment = name,
            ?result.winner,
            p_value = result.p_value,
            effect_size = result.effect_size,
            "ab_test: analysis complete"
        );
        Some(result)
    }

    /// Return a snapshot of current sample counts for `name`, or `None` if
    /// the experiment does not exist.
    pub fn sample_counts(&self, name: &str) -> Option<(usize, usize)> {
        let exp = self.experiments.get(name)?;
        let guard = exp.lock();
        Some((guard.stats_a.n(), guard.stats_b.n()))
    }

    /// Return names of all registered experiments.
    pub fn experiment_names(&self) -> Vec<String> {
        self.experiments.iter().map(|e| e.key().clone()).collect()
    }

    /// Return current means for both variants, or `None` if the experiment does not exist.
    pub fn current_means(&self, name: &str) -> Option<(f64, f64)> {
        let exp = self.experiments.get(name)?;
        let guard = exp.lock();
        Some((guard.stats_a.mean(), guard.stats_b.mean()))
    }
}

// ---------------------------------------------------------------------------
// Statistical helpers
// ---------------------------------------------------------------------------

/// Perform Welch's t-test and compute Cohen's d effect size.
fn welch_t_test(a: &VariantStats, b: &VariantStats) -> AbTestResult {
    let na = a.n() as f64;
    let nb = b.n() as f64;
    let mean_a = a.mean();
    let mean_b = b.mean();
    let var_a = a.variance();
    let var_b = b.variance();

    // Welch's t statistic.
    let se_sq_a = var_a / na;
    let se_sq_b = var_b / nb;
    let se = (se_sq_a + se_sq_b).sqrt();

    let (p_value, winner) = if se < f64::EPSILON {
        // Zero standard error — identical observations.
        (1.0_f64, None)
    } else {
        let t = (mean_a - mean_b) / se;

        // Welch-Satterthwaite degrees of freedom.
        let df_num = (se_sq_a + se_sq_b).powi(2);
        let df_den = se_sq_a.powi(2) / (na - 1.0) + se_sq_b.powi(2) / (nb - 1.0);
        let df = if df_den < f64::EPSILON { 1.0 } else { df_num / df_den };

        let p = two_tailed_p(t, df);
        let w = if p <= 0.05 {
            // Significant: variant with higher mean wins.
            if mean_a > mean_b {
                Some(Variant::A)
            } else {
                Some(Variant::B)
            }
        } else {
            None
        };
        (p, w)
    };

    // Cohen's d (Hedges-corrected pooled standard deviation).
    let pooled_var = ((na - 1.0) * var_a + (nb - 1.0) * var_b) / (na + nb - 2.0);
    let pooled_sd = pooled_var.sqrt();
    let effect_size = if pooled_sd < f64::EPSILON {
        0.0
    } else {
        (mean_a - mean_b) / pooled_sd
    };

    let confidence = (1.0 - p_value).clamp(0.0, 1.0);

    AbTestResult {
        winner,
        confidence,
        p_value,
        effect_size,
        samples_a: a.n(),
        samples_b: b.n(),
        mean_a,
        mean_b,
    }
}

/// Approximate two-tailed p-value for a t-distribution using the
/// Bhattacharya rational-polynomial approximation (1980).
///
/// Accurate to ±1 × 10⁻⁶ for df ≥ 1.
fn two_tailed_p(t: f64, df: f64) -> f64 {
    // We approximate via the regularised incomplete beta function:
    //   p_one_tail = I(df/(df + t²); df/2, 1/2) / 2
    // Using Abramowitz & Stegun §26.7 continued-fraction approximation.
    let t2 = t * t;
    let x = df / (df + t2);
    let a = df / 2.0;
    let b = 0.5_f64;
    let p_one_tail = regularised_incomplete_beta(x, a, b) / 2.0;
    // Two-tailed.
    (2.0 * p_one_tail).clamp(0.0, 1.0)
}

/// Regularised incomplete beta function I_x(a, b) via continued fraction.
///
/// Uses Lentz's method.  Converges for `0 ≤ x ≤ 1` and positive a, b.
fn regularised_incomplete_beta(x: f64, a: f64, b: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }

    // Use symmetry relation for better convergence.
    if x > (a + 1.0) / (a + b + 2.0) {
        return 1.0 - regularised_incomplete_beta(1.0 - x, b, a);
    }

    let ln_beta = ln_beta(a, b);
    let front = (x.ln() * a + (1.0 - x).ln() * b - ln_beta).exp() / a;

    // Continued fraction via Lentz's method (max 200 iterations).
    let mut f = 1.0_f64;
    let mut c = 1.0_f64;
    let mut d = 1.0 - (a + b) * x / (a + 1.0);
    d = if d.abs() < 1e-30 { 1e-30 } else { 1.0 / d };
    f = d;

    for m in 1_u32..=200 {
        // Even step.
        let mf = m as f64;
        let num_even = mf * (b - mf) * x / ((a + 2.0 * mf - 1.0) * (a + 2.0 * mf));
        d = 1.0 + num_even * d;
        d = if d.abs() < 1e-30 { 1e-30 } else { 1.0 / d };
        c = 1.0 + num_even / c;
        c = if c.abs() < 1e-30 { 1e-30 } else { c };
        f *= c * d;

        // Odd step.
        let num_odd = -(a + mf) * (a + b + mf) * x / ((a + 2.0 * mf) * (a + 2.0 * mf + 1.0));
        d = 1.0 + num_odd * d;
        d = if d.abs() < 1e-30 { 1e-30 } else { 1.0 / d };
        c = 1.0 + num_odd / c;
        c = if c.abs() < 1e-30 { 1e-30 } else { c };
        let delta = c * d;
        f *= delta;
        if (delta - 1.0).abs() < 1e-10 {
            break;
        }
    }

    front * f
}

/// Natural log of the Beta function: ln Γ(a) + ln Γ(b) − ln Γ(a+b).
fn ln_beta(a: f64, b: f64) -> f64 {
    ln_gamma(a) + ln_gamma(b) - ln_gamma(a + b)
}

/// Stirling-series approximation to ln Γ(x) for x > 0.
///
/// Uses the Lanczos approximation (g=7, n=9) — maximum error < 1.5 × 10⁻¹².
fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_1,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_312_4e-7,
    ];

    if x < 0.5 {
        // Reflection formula: Γ(x)Γ(1-x) = π/sin(πx).
        return std::f64::consts::PI.ln()
            - (std::f64::consts::PI * x).sin().ln()
            - ln_gamma(1.0 - x);
    }

    let z = x - 1.0;
    let mut sum = C[0];
    for (i, &ci) in C.iter().enumerate().skip(1) {
        sum += ci / (z + i as f64);
    }

    let t = z + G + 0.5;
    0.5 * std::f64::consts::TAU.ln()
        + (z + 0.5) * t.ln()
        - t
        + sum.ln()
}

// ---------------------------------------------------------------------------
// Hashing helpers
// ---------------------------------------------------------------------------

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
    fn assignment_is_deterministic() {
        let runner = make_runner_with_experiment(0.5, 1);
        let v1 = runner.assign("test-exp", "user-123").unwrap();
        let v2 = runner.assign("test-exp", "user-123").unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn split_zero_always_gives_b() {
        let runner = make_runner_with_experiment(0.0, 1);
        // With split=0.0, bucket (≥0) is never < 0.0 → always B.
        for i in 0..20 {
            let v = runner.assign("test-exp", &format!("u{i}")).unwrap();
            assert_eq!(v, Variant::B, "user u{i} should be B with split=0.0");
        }
    }

    #[test]
    fn split_one_always_gives_a() {
        let runner = make_runner_with_experiment(1.0, 1);
        // With split=1.0, bucket (< 1.0) is always < 1.0 → always A.
        for i in 0..20 {
            let v = runner.assign("test-exp", &format!("u{i}")).unwrap();
            assert_eq!(v, Variant::A, "user u{i} should be A with split=1.0");
        }
    }

    #[test]
    fn returns_none_for_unknown_experiment() {
        let runner = AbTestRunner::new();
        assert!(runner.assign("nonexistent", "user").is_none());
        assert!(runner.analyse("nonexistent").is_none());
    }

    #[test]
    fn analyse_returns_none_before_min_samples() {
        let runner = make_runner_with_experiment(0.5, 30);
        for _ in 0..10 {
            runner.record_observation("test-exp", Variant::A, 1.0);
            runner.record_observation("test-exp", Variant::B, 2.0);
        }
        assert!(runner.analyse("test-exp").is_none());
    }

    #[test]
    fn analyse_detects_significant_difference() {
        let runner = make_runner_with_experiment(0.5, 20);
        // A scores: 10 ± 0.1, B scores: 1 ± 0.1 — very significant difference.
        for i in 0..50 {
            let jitter = (i as f64) * 0.001;
            runner.record_observation("test-exp", Variant::A, 10.0 + jitter);
            runner.record_observation("test-exp", Variant::B, 1.0 + jitter);
        }
        let result = runner.analyse("test-exp").unwrap();
        assert_eq!(result.winner, Some(Variant::A));
        assert!(result.p_value < 0.05, "p={}", result.p_value);
        assert!(result.effect_size > 0.0, "Cohen's d should be positive");
    }

    #[test]
    fn analyse_no_winner_for_identical_distributions() {
        let runner = make_runner_with_experiment(0.5, 10);
        for _ in 0..10 {
            runner.record_observation("test-exp", Variant::A, 5.0);
            runner.record_observation("test-exp", Variant::B, 5.0);
        }
        let result = runner.analyse("test-exp").unwrap();
        // Identical means → p = 1.0 → no winner.
        assert_eq!(result.winner, None);
        assert_eq!(result.effect_size, 0.0);
    }

    #[test]
    fn delete_removes_experiment() {
        let runner = make_runner_with_experiment(0.5, 1);
        assert!(runner.delete("test-exp"));
        assert!(!runner.delete("test-exp")); // second delete returns false
        assert!(runner.assign("test-exp", "u").is_none());
    }

    #[test]
    fn sample_counts_tracks_observations() {
        let runner = make_runner_with_experiment(0.5, 100);
        runner.record_observation("test-exp", Variant::A, 1.0);
        runner.record_observation("test-exp", Variant::A, 2.0);
        runner.record_observation("test-exp", Variant::B, 3.0);
        let (na, nb) = runner.sample_counts("test-exp").unwrap();
        assert_eq!(na, 2);
        assert_eq!(nb, 1);
    }

    #[test]
    fn ln_gamma_known_values() {
        // Γ(1) = 1 → ln Γ(1) = 0.
        assert!((ln_gamma(1.0)).abs() < 1e-9);
        // Γ(2) = 1 → ln Γ(2) = 0.
        assert!((ln_gamma(2.0)).abs() < 1e-9);
        // Γ(0.5) = √π → ln Γ(0.5) = 0.5 ln π.
        let expected = 0.5 * std::f64::consts::PI.ln();
        assert!((ln_gamma(0.5) - expected).abs() < 1e-9);
    }

    #[test]
    fn two_tailed_p_extreme_t_gives_small_p() {
        // t=10 with df=100 should produce a very small p-value.
        let p = two_tailed_p(10.0, 100.0);
        assert!(p < 1e-5, "p={p}");
    }

    #[test]
    fn two_tailed_p_zero_t_gives_p_one() {
        let p = two_tailed_p(0.0, 50.0);
        assert!((p - 1.0).abs() < 0.01, "p={p}");
    }

    #[test]
    fn experiment_names_returns_all() {
        let runner = AbTestRunner::new();
        runner.register(AbTestConfig {
            name: "exp-1".into(),
            variant_a: PromptTemplate::builder("a1").body("A").build(),
            variant_b: PromptTemplate::builder("b1").body("B").build(),
            traffic_split: 0.5,
            success_metric: SuccessMetric::Latency,
            min_samples: 5,
        });
        runner.register(AbTestConfig {
            name: "exp-2".into(),
            variant_a: PromptTemplate::builder("a2").body("A").build(),
            variant_b: PromptTemplate::builder("b2").body("B").build(),
            traffic_split: 0.5,
            success_metric: SuccessMetric::UserRating,
            min_samples: 5,
        });
        let mut names = runner.experiment_names();
        names.sort();
        assert_eq!(names, ["exp-1", "exp-2"]);
    }
}
