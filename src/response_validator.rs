//! # Response Validator
//!
//! Validates LLM responses against a set of declarative [`ValidationRule`]s.
//! No external regex crate is required; the [`ValidationRule::MatchesRegex`]
//! variant implements simple `*`-wildcard glob matching internally.
//!
//! ## Quick Start
//!
//! ```rust
//! use tokio_prompt_orchestrator::response_validator::{ResponseValidator, ValidationRule};
//!
//! let validator = ResponseValidator::new();
//! let rules = vec![
//!     ValidationRule::MinLength(10),
//!     ValidationRule::MaxLength(500),
//!     ValidationRule::ContainsAll(vec!["Rust".to_string()]),
//! ];
//! let result = validator.validate("Rust is a systems programming language.", &rules);
//! assert!(result.passed);
//! ```

use std::ops::Range;

// ---------------------------------------------------------------------------
// ValidationRule
// ---------------------------------------------------------------------------

/// A single constraint that a response must satisfy.
#[derive(Debug, Clone)]
pub enum ValidationRule {
    /// Response must be no longer than `usize` characters.
    MaxLength(usize),
    /// Response must be at least `usize` characters long.
    MinLength(usize),
    /// All listed keywords must appear verbatim in the response.
    ContainsAll(Vec<String>),
    /// None of the listed keywords may appear in the response.
    ContainsNone(Vec<String>),
    /// Response must match the glob-style pattern (`*` matches any substring).
    MatchesRegex(String),
    /// Response must be valid JSON (parseable by `serde_json`).
    IsValidJson,
    /// Response must contain at least one fenced code block (``` … ```).
    HasCodeBlock,
    /// Number of sentences in the response must fall within the given range.
    SentenceCount(Range<usize>),
}

impl ValidationRule {
    /// Short human-readable name for this rule, used in violation messages.
    pub fn name(&self) -> &str {
        match self {
            Self::MaxLength(_)      => "MaxLength",
            Self::MinLength(_)      => "MinLength",
            Self::ContainsAll(_)    => "ContainsAll",
            Self::ContainsNone(_)   => "ContainsNone",
            Self::MatchesRegex(_)   => "MatchesRegex",
            Self::IsValidJson       => "IsValidJson",
            Self::HasCodeBlock      => "HasCodeBlock",
            Self::SentenceCount(_)  => "SentenceCount",
        }
    }
}

// ---------------------------------------------------------------------------
// ValidationViolation
// ---------------------------------------------------------------------------

/// A single failed constraint.
#[derive(Debug, Clone)]
pub struct ValidationViolation {
    /// The name of the rule that was violated.
    pub rule_name: String,
    /// Human-readable explanation of what failed and why.
    pub message: String,
}

// ---------------------------------------------------------------------------
// ValidationResult
// ---------------------------------------------------------------------------

/// Aggregate outcome of validating a single response.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// `true` when every rule passed (i.e. `violations` is empty).
    pub passed: bool,
    /// Every rule that failed, in evaluation order.
    pub violations: Vec<ValidationViolation>,
}

impl ValidationResult {
    fn new(violations: Vec<ValidationViolation>) -> Self {
        Self {
            passed: violations.is_empty(),
            violations,
        }
    }
}

// ---------------------------------------------------------------------------
// ResponseValidator
// ---------------------------------------------------------------------------

/// Validates LLM responses against a list of [`ValidationRule`]s.
pub struct ResponseValidator;

impl Default for ResponseValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseValidator {
    /// Create a new validator.
    pub fn new() -> Self {
        Self
    }

    /// Validate `response` against every rule in `rules`.
    ///
    /// All rules are evaluated even when earlier ones fail, so the full list of
    /// violations is always available.
    pub fn validate(&self, response: &str, rules: &[ValidationRule]) -> ValidationResult {
        let mut violations = Vec::new();

        for rule in rules {
            if let Some(v) = self.check_rule(response, rule) {
                violations.push(v);
            }
        }

        ValidationResult::new(violations)
    }

    /// Validate each response in `responses` against `rules`.
    ///
    /// Returns one [`ValidationResult`] per response in the same order.
    pub fn validate_all(
        &self,
        responses: &[&str],
        rules: &[ValidationRule],
    ) -> Vec<ValidationResult> {
        responses.iter().map(|r| self.validate(r, rules)).collect()
    }

    /// Fraction of results that passed all rules.
    ///
    /// Returns `0.0` when `results` is empty.
    pub fn pass_rate(&self, results: &[ValidationResult]) -> f64 {
        if results.is_empty() {
            return 0.0;
        }
        let passed = results.iter().filter(|r| r.passed).count();
        passed as f64 / results.len() as f64
    }

    // ── Per-rule check ────────────────────────────────────────────────────

    fn check_rule(&self, response: &str, rule: &ValidationRule) -> Option<ValidationViolation> {
        match rule {
            ValidationRule::MaxLength(max) => {
                let len = response.chars().count();
                if len > *max {
                    Some(ValidationViolation {
                        rule_name: rule.name().to_string(),
                        message: format!(
                            "Response length {len} chars exceeds maximum {max} chars."
                        ),
                    })
                } else {
                    None
                }
            }

            ValidationRule::MinLength(min) => {
                let len = response.chars().count();
                if len < *min {
                    Some(ValidationViolation {
                        rule_name: rule.name().to_string(),
                        message: format!(
                            "Response length {len} chars is below minimum {min} chars."
                        ),
                    })
                } else {
                    None
                }
            }

            ValidationRule::ContainsAll(keywords) => {
                let missing: Vec<&str> = keywords
                    .iter()
                    .filter(|kw| !response.contains(kw.as_str()))
                    .map(|kw| kw.as_str())
                    .collect();
                if missing.is_empty() {
                    None
                } else {
                    Some(ValidationViolation {
                        rule_name: rule.name().to_string(),
                        message: format!(
                            "Response is missing required keyword(s): {}",
                            missing.join(", ")
                        ),
                    })
                }
            }

            ValidationRule::ContainsNone(keywords) => {
                let found: Vec<&str> = keywords
                    .iter()
                    .filter(|kw| response.contains(kw.as_str()))
                    .map(|kw| kw.as_str())
                    .collect();
                if found.is_empty() {
                    None
                } else {
                    Some(ValidationViolation {
                        rule_name: rule.name().to_string(),
                        message: format!(
                            "Response contains blocked keyword(s): {}",
                            found.join(", ")
                        ),
                    })
                }
            }

            ValidationRule::MatchesRegex(pattern) => {
                if glob_match(pattern, response) {
                    None
                } else {
                    Some(ValidationViolation {
                        rule_name: rule.name().to_string(),
                        message: format!("Response does not match glob pattern: {pattern}"),
                    })
                }
            }

            ValidationRule::IsValidJson => {
                if serde_json::from_str::<serde_json::Value>(response).is_ok() {
                    None
                } else {
                    Some(ValidationViolation {
                        rule_name: rule.name().to_string(),
                        message: "Response is not valid JSON.".to_string(),
                    })
                }
            }

            ValidationRule::HasCodeBlock => {
                if response.contains("```") {
                    None
                } else {
                    Some(ValidationViolation {
                        rule_name: rule.name().to_string(),
                        message: "Response does not contain a fenced code block (```)."
                            .to_string(),
                    })
                }
            }

            ValidationRule::SentenceCount(range) => {
                let count = count_sentences(response);
                if range.contains(&count) {
                    None
                } else {
                    Some(ValidationViolation {
                        rule_name: rule.name().to_string(),
                        message: format!(
                            "Sentence count {count} is outside expected range {}..{}.",
                            range.start, range.end
                        ),
                    })
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simple `*`-wildcard glob match: `*` matches zero or more characters.
///
/// The match is case-sensitive and operates on Unicode scalar values.
fn glob_match(pattern: &str, text: &str) -> bool {
    // Recursive implementation over character slices.
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_inner(&pat, &txt)
}

fn glob_match_inner(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        // Both exhausted — matched.
        (None, None) => true,
        // Pattern exhausted but text remains — no match.
        (None, Some(_)) => false,
        // `*` in pattern: try matching zero chars (advance pattern only)
        // or one char (advance text only).
        (Some('*'), _) => {
            glob_match_inner(&pat[1..], txt)
                || (!txt.is_empty() && glob_match_inner(pat, &txt[1..]))
        }
        // Text exhausted but pattern still has non-`*` chars — no match.
        (Some(_), None) => false,
        // Literal match: both chars must be equal.
        (Some(p), Some(t)) => {
            if p == t {
                glob_match_inner(&pat[1..], &txt[1..])
            } else {
                false
            }
        }
    }
}

/// Count approximate sentence boundaries (`.`, `!`, `?`).
///
/// Returns at least 1 when the text is non-empty.
fn count_sentences(text: &str) -> usize {
    if text.trim().is_empty() {
        return 0;
    }
    let count = text.chars().filter(|&c| c == '.' || c == '!' || c == '?').count();
    count.max(1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v() -> ResponseValidator {
        ResponseValidator::new()
    }

    // ── MaxLength ─────────────────────────────────────────────────────────

    #[test]
    fn max_length_passes_when_within() {
        let r = v().validate("hello", &[ValidationRule::MaxLength(10)]);
        assert!(r.passed);
    }

    #[test]
    fn max_length_fails_when_exceeded() {
        let r = v().validate("hello world", &[ValidationRule::MaxLength(5)]);
        assert!(!r.passed);
        assert_eq!(r.violations[0].rule_name, "MaxLength");
    }

    // ── MinLength ─────────────────────────────────────────────────────────

    #[test]
    fn min_length_passes_when_above() {
        let r = v().validate("hello world", &[ValidationRule::MinLength(5)]);
        assert!(r.passed);
    }

    #[test]
    fn min_length_fails_when_below() {
        let r = v().validate("hi", &[ValidationRule::MinLength(10)]);
        assert!(!r.passed);
        assert_eq!(r.violations[0].rule_name, "MinLength");
    }

    // ── ContainsAll ───────────────────────────────────────────────────────

    #[test]
    fn contains_all_passes_when_present() {
        let r = v().validate(
            "Rust is fast and safe.",
            &[ValidationRule::ContainsAll(vec!["Rust".to_string(), "fast".to_string()])],
        );
        assert!(r.passed);
    }

    #[test]
    fn contains_all_fails_when_missing() {
        let r = v().validate(
            "Rust is great.",
            &[ValidationRule::ContainsAll(vec!["Python".to_string()])],
        );
        assert!(!r.passed);
    }

    // ── ContainsNone ──────────────────────────────────────────────────────

    #[test]
    fn contains_none_passes_when_absent() {
        let r = v().validate(
            "This is fine.",
            &[ValidationRule::ContainsNone(vec!["bad".to_string()])],
        );
        assert!(r.passed);
    }

    #[test]
    fn contains_none_fails_when_found() {
        let r = v().validate(
            "This is bad content.",
            &[ValidationRule::ContainsNone(vec!["bad".to_string()])],
        );
        assert!(!r.passed);
        assert_eq!(r.violations[0].rule_name, "ContainsNone");
    }

    // ── MatchesRegex (glob) ───────────────────────────────────────────────

    #[test]
    fn glob_star_matches_anything() {
        let r = v().validate(
            "anything at all",
            &[ValidationRule::MatchesRegex("*".to_string())],
        );
        assert!(r.passed);
    }

    #[test]
    fn glob_prefix_match() {
        let r = v().validate(
            "Hello world",
            &[ValidationRule::MatchesRegex("Hello*".to_string())],
        );
        assert!(r.passed);
    }

    #[test]
    fn glob_no_match() {
        let r = v().validate(
            "Goodbye world",
            &[ValidationRule::MatchesRegex("Hello*".to_string())],
        );
        assert!(!r.passed);
        assert_eq!(r.violations[0].rule_name, "MatchesRegex");
    }

    #[test]
    fn glob_suffix_match() {
        let r = v().validate(
            "Hello world",
            &[ValidationRule::MatchesRegex("*world".to_string())],
        );
        assert!(r.passed);
    }

    #[test]
    fn glob_infix_match() {
        let r = v().validate(
            "Hello beautiful world",
            &[ValidationRule::MatchesRegex("Hello*world".to_string())],
        );
        assert!(r.passed);
    }

    // ── IsValidJson ───────────────────────────────────────────────────────

    #[test]
    fn valid_json_passes() {
        let r = v().validate(r#"{"key": "value"}"#, &[ValidationRule::IsValidJson]);
        assert!(r.passed);
    }

    #[test]
    fn invalid_json_fails() {
        let r = v().validate("not json", &[ValidationRule::IsValidJson]);
        assert!(!r.passed);
        assert_eq!(r.violations[0].rule_name, "IsValidJson");
    }

    // ── HasCodeBlock ──────────────────────────────────────────────────────

    #[test]
    fn has_code_block_passes() {
        let r = v().validate(
            "Here is code:\n```rust\nfn main() {}\n```",
            &[ValidationRule::HasCodeBlock],
        );
        assert!(r.passed);
    }

    #[test]
    fn no_code_block_fails() {
        let r = v().validate("No code here.", &[ValidationRule::HasCodeBlock]);
        assert!(!r.passed);
        assert_eq!(r.violations[0].rule_name, "HasCodeBlock");
    }

    // ── SentenceCount ─────────────────────────────────────────────────────

    #[test]
    fn sentence_count_in_range_passes() {
        // "Hello. World." → 2 sentences
        let r = v().validate(
            "Hello. World.",
            &[ValidationRule::SentenceCount(1..4)],
        );
        assert!(r.passed);
    }

    #[test]
    fn sentence_count_out_of_range_fails() {
        // 3 sentences, range 1..2 excludes 3
        let r = v().validate(
            "One. Two. Three.",
            &[ValidationRule::SentenceCount(1..2)],
        );
        assert!(!r.passed);
        assert_eq!(r.violations[0].rule_name, "SentenceCount");
    }

    // ── Multiple rules ────────────────────────────────────────────────────

    #[test]
    fn multiple_rules_all_pass() {
        let r = v().validate(
            "Rust is great!",
            &[
                ValidationRule::MinLength(5),
                ValidationRule::MaxLength(100),
                ValidationRule::ContainsAll(vec!["Rust".to_string()]),
            ],
        );
        assert!(r.passed);
        assert!(r.violations.is_empty());
    }

    #[test]
    fn multiple_rules_collect_all_violations() {
        let r = v().validate(
            "hi",
            &[
                ValidationRule::MinLength(10),  // fails
                ValidationRule::ContainsAll(vec!["Rust".to_string()]),  // fails
            ],
        );
        assert!(!r.passed);
        assert_eq!(r.violations.len(), 2);
    }

    // ── validate_all ──────────────────────────────────────────────────────

    #[test]
    fn validate_all_returns_correct_count() {
        let validator = v();
        let responses = ["short", "also short", "fine length response here"];
        let rules = [ValidationRule::MinLength(10)];
        let results = validator.validate_all(&responses, &rules);
        assert_eq!(results.len(), 3);
    }

    // ── pass_rate ─────────────────────────────────────────────────────────

    #[test]
    fn pass_rate_all_pass() {
        let validator = v();
        let results = vec![
            ValidationResult::new(vec![]),
            ValidationResult::new(vec![]),
        ];
        assert!((validator.pass_rate(&results) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pass_rate_half_pass() {
        let validator = v();
        let results = vec![
            ValidationResult::new(vec![]),
            ValidationResult::new(vec![ValidationViolation {
                rule_name: "MaxLength".to_string(),
                message: "too long".to_string(),
            }]),
        ];
        assert!((validator.pass_rate(&results) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn pass_rate_empty_returns_zero() {
        let validator = v();
        assert_eq!(validator.pass_rate(&[]), 0.0);
    }

    // ── Default impl ──────────────────────────────────────────────────────

    #[test]
    fn default_impl_works() {
        let v = ResponseValidator::default();
        let r = v.validate("test", &[]);
        assert!(r.passed);
    }

    // ── rule.name() covers all variants ──────────────────────────────────

    #[test]
    fn rule_name_non_empty_for_all_variants() {
        let rules: Vec<ValidationRule> = vec![
            ValidationRule::MaxLength(10),
            ValidationRule::MinLength(1),
            ValidationRule::ContainsAll(vec![]),
            ValidationRule::ContainsNone(vec![]),
            ValidationRule::MatchesRegex("*".to_string()),
            ValidationRule::IsValidJson,
            ValidationRule::HasCodeBlock,
            ValidationRule::SentenceCount(0..5),
        ];
        for rule in &rules {
            assert!(!rule.name().is_empty(), "{rule:?} has empty name");
        }
    }
}
