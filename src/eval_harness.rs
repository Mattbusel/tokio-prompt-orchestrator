//! # Evaluation Harness
//!
//! Compare prompt strategies across a shared set of [`EvalCase`] items.
//! Supports multiple scoring metrics, composite ranking, tag filtering,
//! and per-difficulty pass-rate breakdowns.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::eval_harness::{
//!     EvalCase, EvalHarness, EvalMetric,
//! };
//!
//! let mut harness = EvalHarness::new();
//! harness.add_case(EvalCase {
//!     id: "q1".into(),
//!     prompt: "What is 2+2?".into(),
//!     reference_answer: Some("4".into()),
//!     tags: vec!["math".into()],
//!     difficulty: 1,
//! });
//! let report = harness.run_eval(
//!     "baseline",
//!     vec![("4".to_string(), 120, 0.001)],
//! );
//! assert!(report.pass_rate > 0.99);
//! ```

use std::collections::HashMap;

// ── EvalCase ──────────────────────────────────────────────────────────────────

/// A single evaluation case.
#[derive(Debug, Clone)]
pub struct EvalCase {
    /// Unique identifier.
    pub id: String,
    /// The prompt sent to the strategy under test.
    pub prompt: String,
    /// Optional gold-standard answer for scoring.
    pub reference_answer: Option<String>,
    /// Arbitrary tags for filtering (e.g. `"math"`, `"hard"`).
    pub tags: Vec<String>,
    /// Difficulty level 1 (easy) – 5 (very hard).
    pub difficulty: u8,
}

// ── EvalMetric ────────────────────────────────────────────────────────────────

/// Scoring metric applied to a strategy response.
#[derive(Debug, Clone)]
pub enum EvalMetric {
    /// Score is 1.0 iff response equals the reference answer (trimmed, case-insensitive).
    ExactMatch,
    /// Score is 1.0 iff the reference answer appears as a substring of the response.
    ContainsAnswer,
    /// Score is the Jaccard word-overlap coefficient; threshold ignored in scoring.
    WordOverlap(f64),
    /// Score is 1.0 iff `len(response) / len(reference)` is within `[min, max]`.
    LengthRatio { min: f64, max: f64 },
    /// A named custom metric (always scores 0.5 unless overridden externally).
    Custom(String),
}

// ── EvalResult ────────────────────────────────────────────────────────────────

/// Result for a single case in a strategy evaluation.
#[derive(Debug, Clone)]
pub struct EvalResult {
    /// ID of the [`EvalCase`] this result corresponds to.
    pub case_id: String,
    /// The response produced by the strategy.
    pub response: String,
    /// Named metric scores.
    pub metrics: HashMap<String, f64>,
    /// `true` if the primary metric score exceeds the pass threshold (0.5).
    pub passed: bool,
    /// Wall-clock latency in milliseconds.
    pub latency_ms: u64,
    /// API cost in USD.
    pub cost_usd: f64,
}

// ── EvalReport ────────────────────────────────────────────────────────────────

/// Aggregated results for one strategy across all cases.
#[derive(Debug, Clone)]
pub struct EvalReport {
    /// Name of the strategy evaluated.
    pub strategy_name: String,
    /// Per-case results.
    pub results: Vec<EvalResult>,
    /// Fraction of cases that passed (0.0 – 1.0).
    pub pass_rate: f64,
    /// Mean latency across all cases.
    pub avg_latency_ms: f64,
    /// Mean per-case cost.
    pub avg_cost_usd: f64,
    /// Sum of all per-case costs.
    pub total_cost_usd: f64,
}

// ── EvalHarness ───────────────────────────────────────────────────────────────

/// Evaluation harness for comparing prompt strategies.
pub struct EvalHarness {
    cases: Vec<EvalCase>,
    /// Primary metric used for pass/fail determination (default: ExactMatch).
    primary_metric: EvalMetric,
}

impl EvalHarness {
    /// Create a new harness with [`EvalMetric::ExactMatch`] as the primary metric.
    pub fn new() -> Self {
        Self {
            cases: Vec::new(),
            primary_metric: EvalMetric::ExactMatch,
        }
    }

    /// Create a harness with a custom primary metric.
    pub fn with_metric(metric: EvalMetric) -> Self {
        Self {
            cases: Vec::new(),
            primary_metric: metric,
        }
    }

    /// Register a new evaluation case.
    pub fn add_case(&mut self, case: EvalCase) {
        self.cases.push(case);
    }

    /// Run an evaluation for `strategy_name`.
    ///
    /// `responses` must be in the same order as cases were added.  Each element
    /// is `(response_text, latency_ms, cost_usd)`.
    ///
    /// # Panics
    ///
    /// Does not panic — if `responses` is shorter than `cases`, remaining cases
    /// are skipped.
    pub fn run_eval(
        &self,
        strategy_name: &str,
        responses: Vec<(String, u64, f64)>,
    ) -> EvalReport {
        let mut results: Vec<EvalResult> = Vec::new();

        for (case, (response, latency_ms, cost_usd)) in
            self.cases.iter().zip(responses.into_iter())
        {
            let primary_score = self.score(&response, case, &self.primary_metric);
            let passed = primary_score >= 0.5;

            let mut metrics = HashMap::new();
            metrics.insert(metric_name(&self.primary_metric), primary_score);

            results.push(EvalResult {
                case_id: case.id.clone(),
                response,
                metrics,
                passed,
                latency_ms,
                cost_usd,
            });
        }

        let n = results.len() as f64;
        let pass_rate = if n == 0.0 {
            0.0
        } else {
            results.iter().filter(|r| r.passed).count() as f64 / n
        };
        let avg_latency_ms = if n == 0.0 {
            0.0
        } else {
            results.iter().map(|r| r.latency_ms as f64).sum::<f64>() / n
        };
        let total_cost_usd = results.iter().map(|r| r.cost_usd).sum::<f64>();
        let avg_cost_usd = if n == 0.0 { 0.0 } else { total_cost_usd / n };

        EvalReport {
            strategy_name: strategy_name.to_string(),
            results,
            pass_rate,
            avg_latency_ms,
            avg_cost_usd,
            total_cost_usd,
        }
    }

    /// Score a single response against a case using the supplied metric.
    pub fn score(&self, response: &str, case: &EvalCase, metric: &EvalMetric) -> f64 {
        match metric {
            EvalMetric::ExactMatch => {
                let reference = case
                    .reference_answer
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                let resp = response.trim().to_lowercase();
                if resp == reference { 1.0 } else { 0.0 }
            }
            EvalMetric::ContainsAnswer => {
                let reference = case
                    .reference_answer
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                if reference.is_empty() {
                    return 0.0;
                }
                if response.to_lowercase().contains(&reference) {
                    1.0
                } else {
                    0.0
                }
            }
            EvalMetric::WordOverlap(_threshold) => {
                let ref_words = word_set(case.reference_answer.as_deref().unwrap_or(""));
                let resp_words = word_set(response);
                if ref_words.is_empty() && resp_words.is_empty() {
                    return 1.0;
                }
                let intersection = ref_words.iter().filter(|w| resp_words.contains(*w)).count();
                let union = ref_words.len() + resp_words.len() - intersection;
                if union == 0 { 0.0 } else { intersection as f64 / union as f64 }
            }
            EvalMetric::LengthRatio { min, max } => {
                let ref_len = case.reference_answer.as_deref().unwrap_or("").len();
                if ref_len == 0 {
                    return 0.0;
                }
                let ratio = response.len() as f64 / ref_len as f64;
                if ratio >= *min && ratio <= *max { 1.0 } else { 0.0 }
            }
            EvalMetric::Custom(_name) => 0.5,
        }
    }

    /// Rank strategies by composite score: `pass_rate * 0.6 - avg_cost_usd_norm * 0.2 - avg_latency_norm * 0.2`.
    ///
    /// Returns references to reports in descending composite score order.
    pub fn compare_strategies<'a>(
        reports: &'a [EvalReport],
    ) -> Vec<(&'a EvalReport, f64)> {
        if reports.is_empty() {
            return Vec::new();
        }
        let max_cost = reports
            .iter()
            .map(|r| r.avg_cost_usd)
            .fold(0.0_f64, f64::max);
        let max_lat = reports
            .iter()
            .map(|r| r.avg_latency_ms)
            .fold(0.0_f64, f64::max);

        let mut scored: Vec<(&EvalReport, f64)> = reports
            .iter()
            .map(|r| {
                let cost_norm = if max_cost == 0.0 {
                    0.0
                } else {
                    r.avg_cost_usd / max_cost
                };
                let lat_norm = if max_lat == 0.0 {
                    0.0
                } else {
                    r.avg_latency_ms / max_lat
                };
                let composite = r.pass_rate * 0.6 - cost_norm * 0.2 - lat_norm * 0.2;
                (r, composite)
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }

    /// Return all cases tagged with `tag`.
    pub fn filter_by_tag(&self, tag: &str) -> Vec<&EvalCase> {
        self.cases
            .iter()
            .filter(|c| c.tags.iter().any(|t| t == tag))
            .collect()
    }

    /// Compute per-difficulty pass rate from a report.
    ///
    /// Keys are difficulty levels (1–5); values are pass rates (0.0 – 1.0).
    pub fn difficulty_breakdown(&self, report: &EvalReport) -> HashMap<u8, f64> {
        // Build a lookup from case_id to difficulty.
        let difficulty_map: HashMap<&str, u8> = self
            .cases
            .iter()
            .map(|c| (c.id.as_str(), c.difficulty))
            .collect();

        let mut totals: HashMap<u8, (u64, u64)> = HashMap::new(); // (passed, total)
        for result in &report.results {
            if let Some(&diff) = difficulty_map.get(result.case_id.as_str()) {
                let entry = totals.entry(diff).or_insert((0, 0));
                entry.1 += 1;
                if result.passed {
                    entry.0 += 1;
                }
            }
        }

        totals
            .into_iter()
            .map(|(diff, (passed, total))| {
                (diff, if total == 0 { 0.0 } else { passed as f64 / total as f64 })
            })
            .collect()
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn metric_name(metric: &EvalMetric) -> String {
    match metric {
        EvalMetric::ExactMatch => "exact_match".into(),
        EvalMetric::ContainsAnswer => "contains_answer".into(),
        EvalMetric::WordOverlap(_) => "word_overlap".into(),
        EvalMetric::LengthRatio { .. } => "length_ratio".into(),
        EvalMetric::Custom(name) => name.clone(),
    }
}

fn word_set(text: &str) -> std::collections::HashSet<String> {
    text.to_lowercase()
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_case(id: &str, reference: &str, tags: &[&str], difficulty: u8) -> EvalCase {
        EvalCase {
            id: id.into(),
            prompt: format!("prompt for {}", id),
            reference_answer: Some(reference.into()),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            difficulty,
        }
    }

    #[test]
    fn test_exact_match_pass() {
        let harness = EvalHarness::new();
        let case = make_case("c1", "Paris", &[], 1);
        assert_eq!(harness.score("Paris", &case, &EvalMetric::ExactMatch), 1.0);
    }

    #[test]
    fn test_exact_match_fail() {
        let harness = EvalHarness::new();
        let case = make_case("c1", "Paris", &[], 1);
        assert_eq!(harness.score("London", &case, &EvalMetric::ExactMatch), 0.0);
    }

    #[test]
    fn test_exact_match_case_insensitive() {
        let harness = EvalHarness::new();
        let case = make_case("c1", "Paris", &[], 1);
        assert_eq!(harness.score("paris", &case, &EvalMetric::ExactMatch), 1.0);
    }

    #[test]
    fn test_contains_answer_pass() {
        let harness = EvalHarness::new();
        let case = make_case("c1", "42", &[], 1);
        assert_eq!(
            harness.score("The answer is 42.", &case, &EvalMetric::ContainsAnswer),
            1.0
        );
    }

    #[test]
    fn test_contains_answer_fail() {
        let harness = EvalHarness::new();
        let case = make_case("c1", "42", &[], 1);
        assert_eq!(
            harness.score("The answer is 43.", &case, &EvalMetric::ContainsAnswer),
            0.0
        );
    }

    #[test]
    fn test_word_overlap() {
        let harness = EvalHarness::new();
        let case = make_case("c1", "the quick brown fox", &[], 1);
        let score = harness.score(
            "the quick brown dog",
            &case,
            &EvalMetric::WordOverlap(0.5),
        );
        // intersection={the,quick,brown}=3, union={the,quick,brown,fox,dog}=5, jaccard=0.6
        assert!((score - 0.6).abs() < 0.01);
    }

    #[test]
    fn test_length_ratio_pass() {
        let harness = EvalHarness::new();
        let case = make_case("c1", "hello", &[], 1); // len=5
        // response len=5, ratio=1.0, within [0.8, 1.2]
        assert_eq!(
            harness.score("world", &case, &EvalMetric::LengthRatio { min: 0.8, max: 1.2 }),
            1.0
        );
    }

    #[test]
    fn test_length_ratio_fail() {
        let harness = EvalHarness::new();
        let case = make_case("c1", "hi", &[], 1); // len=2
        // response len=100, ratio=50.0, outside [0.8, 1.2]
        let long_resp: String = "x".repeat(100);
        assert_eq!(
            harness.score(&long_resp, &case, &EvalMetric::LengthRatio { min: 0.8, max: 1.2 }),
            0.0
        );
    }

    #[test]
    fn test_run_eval_pass_rate() {
        let mut harness = EvalHarness::new();
        harness.add_case(make_case("c1", "Paris", &[], 1));
        harness.add_case(make_case("c2", "Berlin", &[], 2));
        harness.add_case(make_case("c3", "Rome", &[], 3));

        let responses = vec![
            ("Paris".into(), 100, 0.001),  // pass
            ("Madrid".into(), 200, 0.001), // fail
            ("Rome".into(), 150, 0.001),   // pass
        ];
        let report = harness.run_eval("strategy-a", responses);
        assert!((report.pass_rate - 2.0 / 3.0).abs() < 0.01);
        assert_eq!(report.strategy_name, "strategy-a");
        assert_eq!(report.results.len(), 3);
    }

    #[test]
    fn test_avg_latency_and_cost() {
        let mut harness = EvalHarness::new();
        harness.add_case(make_case("c1", "x", &[], 1));
        harness.add_case(make_case("c2", "y", &[], 1));

        let report = harness.run_eval(
            "s",
            vec![("x".into(), 100, 0.01), ("y".into(), 200, 0.02)],
        );
        assert!((report.avg_latency_ms - 150.0).abs() < 0.01);
        assert!((report.total_cost_usd - 0.03).abs() < 0.0001);
        assert!((report.avg_cost_usd - 0.015).abs() < 0.0001);
    }

    #[test]
    fn test_filter_by_tag() {
        let mut harness = EvalHarness::new();
        harness.add_case(make_case("c1", "x", &["math"], 1));
        harness.add_case(make_case("c2", "y", &["science"], 2));
        harness.add_case(make_case("c3", "z", &["math", "hard"], 3));

        let math_cases = harness.filter_by_tag("math");
        assert_eq!(math_cases.len(), 2);
    }

    #[test]
    fn test_difficulty_breakdown() {
        let mut harness = EvalHarness::new();
        harness.add_case(make_case("c1", "Paris", &[], 1));
        harness.add_case(make_case("c2", "Berlin", &[], 1));
        harness.add_case(make_case("c3", "Rome", &[], 2));

        let responses = vec![
            ("Paris".into(), 100, 0.0),  // diff=1, pass
            ("Madrid".into(), 100, 0.0), // diff=1, fail
            ("Rome".into(), 100, 0.0),   // diff=2, pass
        ];
        let report = harness.run_eval("s", responses);
        let breakdown = harness.difficulty_breakdown(&report);
        assert!((breakdown[&1] - 0.5).abs() < 0.01);
        assert!((breakdown[&2] - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_compare_strategies() {
        let mut harness = EvalHarness::new();
        harness.add_case(make_case("c1", "Paris", &[], 1));

        let r1 = harness.run_eval("good", vec![("Paris".into(), 50, 0.001)]);
        let r2 = harness.run_eval("bad", vec![("London".into(), 200, 0.01)]);

        let reports = vec![r1, r2];
        let ranked = EvalHarness::compare_strategies(&reports);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].0.strategy_name, "good");
    }

    #[test]
    fn test_custom_metric_returns_half() {
        let harness = EvalHarness::new();
        let case = make_case("c1", "any", &[], 1);
        let score = harness.score("anything", &case, &EvalMetric::Custom("my_metric".into()));
        assert!((score - 0.5).abs() < 0.01);
    }
}
