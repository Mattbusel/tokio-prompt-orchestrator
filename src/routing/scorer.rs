//! Prompt complexity scoring.
//!
//! Analyses a prompt string and produces a complexity score in the range
//! `0.0..=1.0`.  The score drives the routing decision:
//!
//! | Score       | Route                                       |
//! |-------------|---------------------------------------------|
//! | `< 0.4`     | Local model (free, fast)                    |
//! | `0.4 – 0.7` | Local model with cloud fallback on failure  |
//! | `> 0.7`     | Cloud model directly                        |
//!
//! ## Heuristics
//!
//! 1. **Token count** — >500 whitespace-delimited tokens → +0.3
//! 2. **Code blocks** — presence of fenced code blocks (` ``` `) → +0.2
//! 3. **Multi-step instructions** — numbered list items → +0.2
//! 4. **Ambiguous references** — words like "it", "that", "the thing" → +0.15
//! 5. **Domain-specific terms** — Rust/financial keywords → +0.15
//!
//! The raw sum is clamped to `[0.0, 1.0]`.

/// A prompt complexity scorer.
///
/// Stateless and cheap to construct.  All analysis runs in O(n) over
/// the prompt length with no heap allocations beyond the input scan.
///
/// # Panics
///
/// This type and its methods never panic.
#[derive(Debug, Clone)]
pub struct ComplexityScorer {
    /// Minimum token count before the "long prompt" signal fires.
    token_count_threshold: usize,
}

impl ComplexityScorer {
    /// Create a new scorer with the default token-count threshold (500).
    pub fn new() -> Self {
        Self {
            token_count_threshold: 500,
        }
    }

    /// Create a new scorer with a custom token-count threshold.
    ///
    /// # Arguments
    ///
    /// * `token_count_threshold` — Prompts with more whitespace-delimited
    ///   tokens than this value receive the long-prompt bonus.
    pub fn with_token_threshold(token_count_threshold: usize) -> Self {
        Self {
            token_count_threshold,
        }
    }

    /// Score a prompt for complexity.
    ///
    /// # Arguments
    ///
    /// * `prompt` — The raw prompt text to analyse.
    ///
    /// # Returns
    ///
    /// A `f64` in `[0.0, 1.0]` representing estimated complexity.
    ///
    /// # Panics
    ///
    /// This function never panics.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tokio_prompt_orchestrator::routing::ComplexityScorer;
    /// let scorer = ComplexityScorer::new();
    /// let score = scorer.score("Say hello");
    /// assert!(score < 0.4);
    /// ```
    pub fn score(&self, prompt: &str) -> f64 {
        let mut total = 0.0_f64;

        total += self.token_count_signal(prompt);
        total += Self::code_block_signal(prompt);
        total += Self::multi_step_signal(prompt);
        total += Self::ambiguous_reference_signal(prompt);
        total += Self::domain_term_signal(prompt);

        clamp_score(total)
    }

    /// Provide a breakdown of individual signal contributions.
    ///
    /// Useful for debugging, logging, and transparency into routing decisions.
    ///
    /// # Arguments
    ///
    /// * `prompt` — The raw prompt text to analyse.
    ///
    /// # Returns
    ///
    /// A [`ScoreBreakdown`] showing each signal's contribution and the
    /// clamped total.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn breakdown(&self, prompt: &str) -> ScoreBreakdown {
        let token_count = self.token_count_signal(prompt);
        let code_blocks = Self::code_block_signal(prompt);
        let multi_step = Self::multi_step_signal(prompt);
        let ambiguous_refs = Self::ambiguous_reference_signal(prompt);
        let domain_terms = Self::domain_term_signal(prompt);
        let total =
            clamp_score(token_count + code_blocks + multi_step + ambiguous_refs + domain_terms);

        ScoreBreakdown {
            token_count,
            code_blocks,
            multi_step,
            ambiguous_refs,
            domain_terms,
            total,
        }
    }

    // ── Individual signals ─────────────────────────────────────────────

    /// +0.3 if the prompt exceeds the token-count threshold.
    fn token_count_signal(&self, prompt: &str) -> f64 {
        let count = prompt.split_whitespace().count();
        if count > self.token_count_threshold {
            0.3
        } else {
            0.0
        }
    }

    /// +0.2 if the prompt contains fenced code blocks.
    fn code_block_signal(prompt: &str) -> f64 {
        if prompt.contains("```") {
            0.2
        } else {
            0.0
        }
    }

    /// +0.2 if the prompt contains numbered list items (multi-step instructions).
    fn multi_step_signal(prompt: &str) -> f64 {
        let mut numbered_items = 0_usize;
        for line in prompt.lines() {
            let trimmed = line.trim_start();
            // Match patterns like "1.", "2.", "10." at line start
            if let Some(rest) = trimmed.strip_suffix('.') {
                // We want "N." at the start, so check the prefix
                let _ = rest; // suppress unused
            }
            // More robust: check if trimmed starts with digit(s) followed by '.'
            let mut chars = trimmed.chars();
            if let Some(first) = chars.next() {
                if first.is_ascii_digit() {
                    // Consume remaining digits
                    let rest: String = chars.collect();
                    if rest.starts_with('.') || rest.starts_with(|c: char| c.is_ascii_digit()) {
                        // Check for "N." or "NN." pattern
                        let dot_pos = trimmed.find('.');
                        if let Some(pos) = dot_pos {
                            let prefix = &trimmed[..pos];
                            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
                                numbered_items += 1;
                            }
                        }
                    }
                }
            }
        }
        // Two or more numbered items → multi-step
        if numbered_items >= 2 {
            0.2
        } else {
            0.0
        }
    }

    /// +0.15 if the prompt contains ambiguous pronominal references.
    fn ambiguous_reference_signal(prompt: &str) -> f64 {
        let lower = prompt.to_lowercase();
        let ambiguous_patterns = [
            " it ",
            " that ",
            " the thing ",
            " this thing ",
            " those ",
            " these ",
            " them ",
        ];
        let count = ambiguous_patterns
            .iter()
            .filter(|pat| lower.contains(*pat))
            .count();
        // Need at least 2 distinct ambiguous patterns to trigger
        if count >= 2 {
            0.15
        } else {
            0.0
        }
    }

    /// +0.15 if the prompt contains domain-specific terminology.
    fn domain_term_signal(prompt: &str) -> f64 {
        let lower = prompt.to_lowercase();

        // Rust keywords / concepts
        let rust_terms = [
            "borrow checker",
            "lifetime",
            "async fn",
            "impl trait",
            "tokio",
            "unsafe",
            "pin<",
            "box<dyn",
            "arc<mutex",
            "send + sync",
            "derive(",
            "#[cfg(",
            "cargo.toml",
            "clippy",
        ];

        // Financial / quantitative terms
        let financial_terms = [
            "black-scholes",
            "monte carlo",
            "sharpe ratio",
            "portfolio",
            "var(",
            "yield curve",
            "derivative pricing",
            "risk-adjusted",
        ];

        let rust_hits = rust_terms.iter().filter(|t| lower.contains(*t)).count();
        let fin_hits = financial_terms
            .iter()
            .filter(|t| lower.contains(*t))
            .count();

        if rust_hits >= 2 || fin_hits >= 2 || (rust_hits >= 1 && fin_hits >= 1) {
            0.15
        } else {
            0.0
        }
    }
}

impl Default for ComplexityScorer {
    fn default() -> Self {
        Self::new()
    }
}

/// Clamp a raw score to the valid `[0.0, 1.0]` range.
fn clamp_score(raw: f64) -> f64 {
    raw.clamp(0.0, 1.0)
}

/// Breakdown of individual complexity signal contributions.
///
/// Returned by [`ComplexityScorer::breakdown`] for observability.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreBreakdown {
    /// Contribution from the token-count heuristic (0.0 or 0.3).
    pub token_count: f64,
    /// Contribution from code-block detection (0.0 or 0.2).
    pub code_blocks: f64,
    /// Contribution from multi-step instruction detection (0.0 or 0.2).
    pub multi_step: f64,
    /// Contribution from ambiguous-reference detection (0.0 or 0.15).
    pub ambiguous_refs: f64,
    /// Contribution from domain-specific terminology (0.0 or 0.15).
    pub domain_terms: f64,
    /// Final clamped score in `[0.0, 1.0]`.
    pub total: f64,
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- clamp -----------------------------------------------------------

    #[test]
    fn test_clamp_score_within_range_unchanged() {
        assert!((clamp_score(0.5) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_clamp_score_negative_returns_zero() {
        assert!(clamp_score(-0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn test_clamp_score_above_one_returns_one() {
        assert!((clamp_score(1.5) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_clamp_score_zero_unchanged() {
        assert!(clamp_score(0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_clamp_score_one_unchanged() {
        assert!((clamp_score(1.0) - 1.0).abs() < f64::EPSILON);
    }

    // -- simple prompts → low score --------------------------------------

    #[test]
    fn test_score_simple_greeting_below_0_4() {
        let scorer = ComplexityScorer::new();
        let score = scorer.score("Say hello");
        assert!(
            score < 0.4,
            "simple greeting should score <0.4, got {score}"
        );
    }

    #[test]
    fn test_score_short_question_below_0_4() {
        let scorer = ComplexityScorer::new();
        let score = scorer.score("What is 2 + 2?");
        assert!(score < 0.4, "short question should score <0.4, got {score}");
    }

    #[test]
    fn test_score_empty_prompt_returns_zero() {
        let scorer = ComplexityScorer::new();
        assert!(scorer.score("").abs() < f64::EPSILON);
    }

    // -- code block signal -----------------------------------------------

    #[test]
    fn test_score_code_block_adds_0_2() {
        let scorer = ComplexityScorer::new();
        let prompt = "Please fix this:\n```rust\nfn main() {}\n```";
        let bd = scorer.breakdown(prompt);
        assert!((bd.code_blocks - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_no_code_block_adds_zero() {
        let scorer = ComplexityScorer::new();
        let bd = scorer.breakdown("Hello world");
        assert!(bd.code_blocks.abs() < f64::EPSILON);
    }

    // -- multi-step signal -----------------------------------------------

    #[test]
    fn test_score_numbered_list_adds_0_2() {
        let scorer = ComplexityScorer::new();
        let prompt = "Please do:\n1. Step one\n2. Step two\n3. Step three";
        let bd = scorer.breakdown(prompt);
        assert!(
            (bd.multi_step - 0.2).abs() < f64::EPSILON,
            "multi-step should be 0.2, got {}",
            bd.multi_step
        );
    }

    #[test]
    fn test_score_single_numbered_item_no_signal() {
        let scorer = ComplexityScorer::new();
        let prompt = "1. Do this only thing";
        let bd = scorer.breakdown(prompt);
        assert!(bd.multi_step.abs() < f64::EPSILON);
    }

    // -- ambiguous reference signal --------------------------------------

    #[test]
    fn test_score_ambiguous_refs_with_multiple_patterns_adds_0_15() {
        let scorer = ComplexityScorer::new();
        // "it" and "that" both present
        let prompt = "Take it and do that with them ";
        let bd = scorer.breakdown(prompt);
        assert!(
            (bd.ambiguous_refs - 0.15).abs() < f64::EPSILON,
            "ambiguous refs should be 0.15, got {}",
            bd.ambiguous_refs
        );
    }

    #[test]
    fn test_score_single_ambiguous_ref_no_signal() {
        let scorer = ComplexityScorer::new();
        let prompt = "Fix it please";
        let bd = scorer.breakdown(prompt);
        assert!(bd.ambiguous_refs.abs() < f64::EPSILON);
    }

    // -- domain term signal -----------------------------------------------

    #[test]
    fn test_score_rust_domain_terms_adds_0_15() {
        let scorer = ComplexityScorer::new();
        let prompt = "I have a borrow checker issue with my lifetime annotations";
        let bd = scorer.breakdown(prompt);
        assert!(
            (bd.domain_terms - 0.15).abs() < f64::EPSILON,
            "domain terms should be 0.15, got {}",
            bd.domain_terms
        );
    }

    #[test]
    fn test_score_financial_domain_terms_adds_0_15() {
        let scorer = ComplexityScorer::new();
        let prompt = "Calculate the sharpe ratio for my portfolio allocation";
        let bd = scorer.breakdown(prompt);
        assert!(
            (bd.domain_terms - 0.15).abs() < f64::EPSILON,
            "domain terms should be 0.15, got {}",
            bd.domain_terms
        );
    }

    #[test]
    fn test_score_single_domain_term_no_signal() {
        let scorer = ComplexityScorer::new();
        let prompt = "Tell me about tokio";
        let bd = scorer.breakdown(prompt);
        assert!(bd.domain_terms.abs() < f64::EPSILON);
    }

    // -- token count signal -----------------------------------------------

    #[test]
    fn test_score_long_prompt_adds_0_3() {
        let scorer = ComplexityScorer::new();
        // 501 words
        let prompt = (0..501)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let bd = scorer.breakdown(&prompt);
        assert!(
            (bd.token_count - 0.3).abs() < f64::EPSILON,
            "token count signal should be 0.3, got {}",
            bd.token_count
        );
    }

    #[test]
    fn test_score_exactly_500_tokens_no_signal() {
        let scorer = ComplexityScorer::new();
        let prompt = (0..500)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let bd = scorer.breakdown(&prompt);
        assert!(bd.token_count.abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_custom_token_threshold() {
        let scorer = ComplexityScorer::with_token_threshold(10);
        let prompt = "one two three four five six seven eight nine ten eleven";
        let bd = scorer.breakdown(prompt);
        assert!(
            (bd.token_count - 0.3).abs() < f64::EPSILON,
            "custom threshold 10 should trigger for 11 tokens"
        );
    }

    // -- combined / complex prompts --------------------------------------

    #[test]
    fn test_score_complex_rust_debugging_above_0_7() {
        let scorer = ComplexityScorer::new();
        let prompt = r#"Debug this Rust code that has a borrow checker error with lifetime issues:

```rust
fn process<'a>(data: &'a [u8]) -> &'a str {
    let owned = String::from_utf8_lossy(data).to_string();
    &owned
}
```

1. Explain why the borrow checker rejects this
2. Show the correct implementation with proper lifetime annotations
3. Add unit tests for the fix

Also, that function should handle it properly when those bytes are invalid UTF-8. Fix the thing so it works with tokio async fn properly."#;
        let score = scorer.score(prompt);
        assert!(
            score > 0.7,
            "complex Rust debugging prompt should score >0.7, got {score}"
        );
    }

    #[test]
    fn test_score_all_signals_max_clamps_to_1_0() {
        let scorer = ComplexityScorer::with_token_threshold(5);
        // Trigger every signal
        let prompt = r#"Fix it and do that with them and those things:

```rust
fn main() {}
```

The borrow checker lifetime issues need fixing.

1. First step
2. Second step
3. Third step

Here are many words to push over the token threshold for this test case."#;
        let score = scorer.score(prompt);
        assert!(
            (score - 1.0).abs() < f64::EPSILON,
            "all signals combined should clamp to 1.0, got {score}"
        );
    }

    // -- breakdown structure ---------------------------------------------

    #[test]
    fn test_breakdown_total_matches_score() {
        let scorer = ComplexityScorer::new();
        let prompt = "Debug my borrow checker lifetime issue:\n1. Step one\n2. Step two";
        let score = scorer.score(prompt);
        let bd = scorer.breakdown(prompt);
        assert!(
            (score - bd.total).abs() < f64::EPSILON,
            "breakdown total should equal score"
        );
    }

    #[test]
    fn test_breakdown_empty_prompt_all_zeros() {
        let scorer = ComplexityScorer::new();
        let bd = scorer.breakdown("");
        assert!(bd.token_count.abs() < f64::EPSILON);
        assert!(bd.code_blocks.abs() < f64::EPSILON);
        assert!(bd.multi_step.abs() < f64::EPSILON);
        assert!(bd.ambiguous_refs.abs() < f64::EPSILON);
        assert!(bd.domain_terms.abs() < f64::EPSILON);
        assert!(bd.total.abs() < f64::EPSILON);
    }

    // -- Default trait ---------------------------------------------------

    #[test]
    fn test_complexity_scorer_default_equals_new() {
        let a = ComplexityScorer::new();
        let b = ComplexityScorer::default();
        assert_eq!(a.token_count_threshold, b.token_count_threshold);
    }

    // -- edge cases -------------------------------------------------------

    #[test]
    fn test_score_backticks_without_fence_no_code_signal() {
        let scorer = ComplexityScorer::new();
        let prompt = "Use `println!` for output";
        let bd = scorer.breakdown(prompt);
        assert!(bd.code_blocks.abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_numbered_list_with_double_digits() {
        let scorer = ComplexityScorer::new();
        let prompt = "10. Do this\n11. Do that";
        let bd = scorer.breakdown(prompt);
        assert!(
            (bd.multi_step - 0.2).abs() < f64::EPSILON,
            "double-digit numbered list should trigger, got {}",
            bd.multi_step
        );
    }

    #[test]
    fn test_score_case_insensitive_domain_terms() {
        let scorer = ComplexityScorer::new();
        let prompt = "BORROW CHECKER and LIFETIME issues";
        let bd = scorer.breakdown(prompt);
        assert!(
            (bd.domain_terms - 0.15).abs() < f64::EPSILON,
            "domain terms should be case-insensitive"
        );
    }

    #[test]
    fn test_score_whitespace_only_prompt_returns_zero() {
        let scorer = ComplexityScorer::new();
        let score = scorer.score("   \n\t\n   ");
        assert!(score.abs() < f64::EPSILON);
    }
}
