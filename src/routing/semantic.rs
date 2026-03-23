//! Semantic prompt router using keyword-based heuristics.
//!
//! Routes prompts to specialised worker labels based on the prompt's
//! semantic category.  Categories are identified via keyword matching — no
//! external embedding API is required, keeping the hot path zero-I/O.
//!
//! ## Categories
//!
//! | Category | Representative keywords | Default worker label |
//! |----------|------------------------|----------------------|
//! | `Code`   | "function", "debug", "compile", … | `"code-specialist"` |
//! | `Math`   | "equation", "integral", "derivative", … | `"math-specialist"` |
//! | `Creative` | "poem", "story", "imagine", … | `"creative-specialist"` |
//! | `Data`   | "csv", "dataframe", "sql", "query", … | `"data-specialist"` |
//! | `General` | (catch-all) | `"general-worker"` |
//!
//! ## Usage
//!
//! ```rust
//! use tokio_prompt_orchestrator::routing::semantic::{SemanticRouter, RoutingCategory};
//!
//! let router = SemanticRouter::default();
//! let category = router.classify("Write a Python function to sort a list");
//! assert_eq!(category, RoutingCategory::Code);
//!
//! let target = router.route("Calculate the integral of x^2");
//! assert_eq!(target, "math-specialist");
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::debug;

// ============================================================================
// RoutingCategory
// ============================================================================

/// Semantic category assigned to an incoming prompt.
///
/// Each variant maps to a default worker label which operators can override
/// via [`SemanticRouterConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoutingCategory {
    /// Prompts primarily about programming, code review, debugging, or
    /// software architecture.
    Code,
    /// Prompts about mathematics: equations, proofs, statistics, calculus.
    Math,
    /// Creative writing: stories, poetry, brainstorming, world-building.
    Creative,
    /// Data analysis: SQL queries, CSV/DataFrame operations, charting.
    Data,
    /// Fallback for prompts that don't match any specialised category.
    General,
}

impl RoutingCategory {
    /// Return the default worker label for this category.
    ///
    /// Operators override this via [`SemanticRouterConfig::worker_labels`].
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn default_worker_label(self) -> &'static str {
        match self {
            Self::Code     => "code-specialist",
            Self::Math     => "math-specialist",
            Self::Creative => "creative-specialist",
            Self::Data     => "data-specialist",
            Self::General  => "general-worker",
        }
    }

    /// Human-readable name for logging and metrics labels.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn label(self) -> &'static str {
        match self {
            Self::Code     => "code",
            Self::Math     => "math",
            Self::Creative => "creative",
            Self::Data     => "data",
            Self::General  => "general",
        }
    }
}

impl std::fmt::Display for RoutingCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ============================================================================
// Keyword tables
// ============================================================================

/// Keywords that strongly indicate a **code** prompt.
static CODE_KEYWORDS: &[&str] = &[
    "function", "method", "class", "struct", "enum", "trait",
    "debug", "compile", "syntax", "algorithm", "implement",
    "refactor", "api", "library", "module", "import", "export",
    "variable", "loop", "recursion", "runtime", "stack", "heap",
    "async", "await", "thread", "mutex", "closure", "lambda",
    "regex", "parse", "serialize", "deserialize", "http", "endpoint",
    "sql", "query", "schema", "migration", "index",
    "python", "javascript", "typescript", "rust", "golang", "java",
    "kotlin", "swift", "c++", "c#", "ruby", "php", "bash", "shell",
    "dockerfile", "kubernetes", "terraform", "git", "ci/cd",
    "unit test", "integration test", "mock", "fixture",
    "dependency", "package", "cargo", "npm", "pip", "brew",
    "error", "exception", "panic", "crash", "segfault", "bug",
    "write a", "implement a", "create a function", "code to",
];

/// Keywords that strongly indicate a **math** prompt.
static MATH_KEYWORDS: &[&str] = &[
    "equation", "integral", "derivative", "differential",
    "matrix", "vector", "tensor", "eigenvalue", "determinant",
    "probability", "statistics", "hypothesis", "variance",
    "standard deviation", "distribution", "regression",
    "linear algebra", "calculus", "trigonometry", "geometry",
    "topology", "proof", "theorem", "lemma", "corollary",
    "fourier", "laplace", "transform", "series", "converge",
    "solve for", "calculate", "compute", "evaluate the",
    "simplify", "factor", "polynomial", "quadratic", "cubic",
    "logarithm", "exponential", "modulo", "prime", "fibonacci",
    "formula", "expression", "coefficient", "constant",
    "optimization", "minimize", "maximize", "gradient descent",
    "monte carlo", "numerical", "approximation",
];

/// Keywords that strongly indicate a **creative** prompt.
static CREATIVE_KEYWORDS: &[&str] = &[
    "poem", "poetry", "sonnet", "haiku", "limerick",
    "story", "short story", "narrative", "fiction", "novel",
    "character", "plot", "setting", "dialogue", "scene",
    "imagine", "write a story", "write a poem", "creative",
    "fantasy", "sci-fi", "science fiction", "dystopia", "utopia",
    "brainstorm", "ideas for", "come up with", "invent",
    "metaphor", "simile", "allegory", "fable", "myth",
    "screenplay", "script", "monologue", "soliloquy",
    "lyrics", "song", "rhyme", "verse", "stanza",
    "humour", "humor", "satire", "parody", "irony",
    "world-building", "lore", "mythology", "folklore",
];

/// Keywords that strongly indicate a **data** prompt.
static DATA_KEYWORDS: &[&str] = &[
    "csv", "dataframe", "pandas", "numpy", "matplotlib",
    "chart", "plot", "graph", "visualize", "visualization",
    "dataset", "data analysis", "analytics", "dashboard",
    "pivot", "aggregate", "group by", "join", "merge",
    "filter", "sort by", "order by", "select",
    "json", "parquet", "excel", "spreadsheet",
    "machine learning", "model", "training", "inference",
    "feature", "label", "classification", "clustering",
    "neural network", "deep learning", "transformer",
    "etl", "pipeline", "warehouse", "lake",
    "average", "median", "mean", "sum", "count",
    "correlation", "trend", "forecast", "prediction",
];

// ============================================================================
// SemanticRouterConfig
// ============================================================================

/// Configuration for the semantic router.
///
/// Override the default worker labels per category and tune the minimum
/// keyword match threshold.
#[derive(Debug, Clone)]
pub struct SemanticRouterConfig {
    /// Map from [`RoutingCategory`] to a worker label string.
    ///
    /// Categories absent from this map fall back to
    /// [`RoutingCategory::default_worker_label`].
    pub worker_labels: HashMap<RoutingCategory, String>,

    /// Minimum number of keyword matches required to assign a category.
    ///
    /// When fewer than `min_matches` keywords are found for a candidate
    /// category the prompt is classified as `General`.  Default: 1.
    pub min_matches: usize,

    /// When `true`, the router compares keywords case-insensitively.
    /// Default: `true`.
    pub case_insensitive: bool,
}

impl Default for SemanticRouterConfig {
    fn default() -> Self {
        Self {
            worker_labels: HashMap::new(),
            min_matches: 1,
            case_insensitive: true,
        }
    }
}

// ============================================================================
// ClassificationResult
// ============================================================================

/// Detailed result of a [`SemanticRouter::classify_detailed`] call.
#[derive(Debug, Clone)]
pub struct ClassificationResult {
    /// The winning category.
    pub category: RoutingCategory,
    /// The worker label to route to.
    pub worker_label: String,
    /// Match counts per category, for diagnostics.
    pub scores: HashMap<RoutingCategory, usize>,
    /// The keyword(s) that produced the winning match (up to 5).
    pub matched_keywords: Vec<String>,
}

// ============================================================================
// SemanticRouter
// ============================================================================

/// Routes prompts to specialised workers using keyword-based heuristics.
///
/// The router is stateless — all routing decisions are computed from the
/// prompt text alone, making it safe to share across threads via `Arc`.
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::routing::semantic::{SemanticRouter, RoutingCategory};
///
/// let router = SemanticRouter::default();
/// assert_eq!(
///     router.classify("Write a recursive Fibonacci function in Rust"),
///     RoutingCategory::Code,
/// );
/// ```
#[derive(Debug, Clone, Default)]
pub struct SemanticRouter {
    config: SemanticRouterConfig,
}

impl SemanticRouter {
    /// Create a router with default configuration.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a router with custom configuration.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn with_config(config: SemanticRouterConfig) -> Self {
        Self { config }
    }

    /// Classify a prompt into a [`RoutingCategory`].
    ///
    /// Returns `RoutingCategory::General` when no category reaches the
    /// configured `min_matches` threshold, or when the prompt is empty.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn classify(&self, prompt: &str) -> RoutingCategory {
        self.classify_detailed(prompt).category
    }

    /// Classify a prompt and return a full [`ClassificationResult`].
    ///
    /// Useful for diagnostics, logging, and metrics.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn classify_detailed(&self, prompt: &str) -> ClassificationResult {
        let haystack = if self.config.case_insensitive {
            prompt.to_lowercase()
        } else {
            prompt.to_string()
        };

        let (code_score, code_kw)     = count_matches(&haystack, CODE_KEYWORDS,     self.config.case_insensitive);
        let (math_score, math_kw)     = count_matches(&haystack, MATH_KEYWORDS,     self.config.case_insensitive);
        let (creative_score, cre_kw)  = count_matches(&haystack, CREATIVE_KEYWORDS, self.config.case_insensitive);
        let (data_score, data_kw)     = count_matches(&haystack, DATA_KEYWORDS,     self.config.case_insensitive);

        let mut scores = HashMap::with_capacity(5);
        scores.insert(RoutingCategory::Code,     code_score);
        scores.insert(RoutingCategory::Math,     math_score);
        scores.insert(RoutingCategory::Creative, creative_score);
        scores.insert(RoutingCategory::Data,     data_score);
        scores.insert(RoutingCategory::General,  0);

        // Pick the category with the highest score that meets `min_matches`.
        let candidates = [
            (RoutingCategory::Code,     code_score,     code_kw),
            (RoutingCategory::Math,     math_score,     math_kw),
            (RoutingCategory::Creative, creative_score, cre_kw),
            (RoutingCategory::Data,     data_score,     data_kw),
        ];

        let winner = candidates
            .iter()
            .filter(|(_, score, _)| *score >= self.config.min_matches)
            .max_by_key(|(_, score, _)| *score);

        let (category, matched_keywords) = match winner {
            Some((cat, _, kws)) => (*cat, kws.clone()),
            None => (RoutingCategory::General, vec![]),
        };

        let worker_label = self
            .config
            .worker_labels
            .get(&category)
            .cloned()
            .unwrap_or_else(|| category.default_worker_label().to_string());

        debug!(
            category = %category,
            worker_label = %worker_label,
            code_score,
            math_score,
            creative_score,
            data_score,
            "semantic router classification"
        );

        ClassificationResult {
            category,
            worker_label,
            scores,
            matched_keywords,
        }
    }

    /// Classify a prompt and return the target worker label string.
    ///
    /// This is the primary method callers use to decide where to send a request.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn route(&self, prompt: &str) -> String {
        self.classify_detailed(prompt).worker_label
    }

    /// Return the configured worker label for a given category, falling back
    /// to the default if no override is configured.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn worker_label_for(&self, category: RoutingCategory) -> String {
        self.config
            .worker_labels
            .get(&category)
            .cloned()
            .unwrap_or_else(|| category.default_worker_label().to_string())
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Count how many keywords from `table` appear in `haystack`.
///
/// Returns `(count, matched_keywords)`.  Keyword matching is substring-based.
/// The `case_insensitive` flag is informational only — callers are expected to
/// lower-case `haystack` before calling when `case_insensitive` is `true`.
fn count_matches(haystack: &str, table: &[&str], case_insensitive: bool) -> (usize, Vec<String>) {
    let mut count = 0usize;
    let mut matched = Vec::new();

    for &kw in table {
        let needle = if case_insensitive {
            kw.to_lowercase()
        } else {
            kw.to_string()
        };
        if haystack.contains(needle.as_str()) {
            count += 1;
            if matched.len() < 5 {
                matched.push(kw.to_string());
            }
        }
    }

    (count, matched)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> SemanticRouter {
        SemanticRouter::default()
    }

    #[test]
    fn classifies_code_prompt() {
        let r = router();
        assert_eq!(
            r.classify("Write a Python function to sort a list using quicksort"),
            RoutingCategory::Code
        );
    }

    #[test]
    fn classifies_math_prompt() {
        let r = router();
        assert_eq!(
            r.classify("Compute the integral of x^2 from 0 to infinity"),
            RoutingCategory::Math
        );
    }

    #[test]
    fn classifies_creative_prompt() {
        let r = router();
        assert_eq!(
            r.classify("Write a short story about a dragon who learns to bake"),
            RoutingCategory::Creative
        );
    }

    #[test]
    fn classifies_data_prompt() {
        let r = router();
        assert_eq!(
            r.classify("Analyse this CSV and produce a chart of sales trends"),
            RoutingCategory::Data
        );
    }

    #[test]
    fn falls_back_to_general() {
        let r = router();
        assert_eq!(r.classify("Hello, how are you today?"), RoutingCategory::General);
    }

    #[test]
    fn route_returns_correct_label() {
        let r = router();
        assert_eq!(r.route("Debug this Rust async code"), "code-specialist");
        assert_eq!(r.route("Find the eigenvalue of matrix A"), "math-specialist");
    }

    #[test]
    fn custom_worker_label_override() {
        let mut cfg = SemanticRouterConfig::default();
        cfg.worker_labels
            .insert(RoutingCategory::Code, "my-code-gpu-worker".to_string());
        let r = SemanticRouter::with_config(cfg);
        assert_eq!(r.route("Implement a binary search tree in Rust"), "my-code-gpu-worker");
    }

    #[test]
    fn classify_detailed_has_scores() {
        let r = router();
        let result = r.classify_detailed("Solve the differential equation dy/dx = x");
        assert!(result.scores[&RoutingCategory::Math] > 0);
        assert!(!result.matched_keywords.is_empty());
    }

    #[test]
    fn min_matches_threshold_respected() {
        let mut cfg = SemanticRouterConfig::default();
        cfg.min_matches = 100; // impossible to meet
        let r = SemanticRouter::with_config(cfg);
        assert_eq!(
            r.classify("Write a recursive Fibonacci function in Rust"),
            RoutingCategory::General
        );
    }

    #[test]
    fn case_insensitive_matching() {
        let r = router();
        // Uppercase keyword — should still classify as Code
        assert_eq!(
            r.classify("IMPLEMENT a FUNCTION that COMPILES"),
            RoutingCategory::Code
        );
    }

    #[test]
    fn worker_label_for_general() {
        let r = router();
        assert_eq!(r.worker_label_for(RoutingCategory::General), "general-worker");
    }
}
