//! Chain-of-thought prompting with structured scratchpad and step decomposition.
//!
//! Provides [`CotPromptBuilder`] to construct prompts that elicit step-by-step
//! reasoning, and [`CotParser`] to extract structured [`ThoughtStep`]s from
//! model responses.

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// A single reasoning step extracted from a chain-of-thought response.
#[derive(Debug, Clone, PartialEq)]
pub struct ThoughtStep {
    /// 1-based ordinal index within the chain.
    pub step_id: u32,
    /// The reasoning / thinking performed at this step.
    pub thought: String,
    /// The conclusion drawn from this step's reasoning.
    pub conclusion: String,
    /// Heuristic confidence score in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Whether this step indicated that a tool call is required.
    pub requires_tool: bool,
}

/// A complete chain-of-thought response with all steps and a final answer.
#[derive(Debug, Clone)]
pub struct ChainOfThought {
    /// Ordered reasoning steps.
    pub steps: Vec<ThoughtStep>,
    /// The final distilled answer extracted from the response.
    pub final_answer: String,
    /// Aggregate confidence across all steps.
    pub total_confidence: f64,
}

// ---------------------------------------------------------------------------
// CotStrategy
// ---------------------------------------------------------------------------

/// Strategy used to construct the chain-of-thought prompt.
#[derive(Debug, Clone)]
pub enum CotStrategy {
    /// Zero-shot: ask the model to reason step-by-step without examples.
    ZeroShot,
    /// Few-shot: provide example (question, CoT answer) strings before the
    /// target question.
    FewShot(Vec<String>),
    /// Tree-of-thought: instruct the model to explore branching solution paths.
    TreeOfThought {
        /// Number of parallel branches to explore.
        branching_factor: u8,
    },
    /// Self-consistency: ask the model to produce multiple independent
    /// reasoning chains and select the most consistent answer.
    SelfConsistency {
        /// Number of independent reasoning samples to request.
        samples: u8,
    },
}

// ---------------------------------------------------------------------------
// CotPromptBuilder
// ---------------------------------------------------------------------------

/// Builds prompts that elicit chain-of-thought reasoning.
#[derive(Debug, Default)]
pub struct CotPromptBuilder;

impl CotPromptBuilder {
    /// Construct a new builder.
    pub fn new() -> Self {
        Self
    }

    /// Build a CoT prompt for `question` using the given `strategy`.
    pub fn build_cot_prompt(&self, question: &str, strategy: &CotStrategy) -> String {
        match strategy {
            CotStrategy::ZeroShot => {
                format!(
                    "Answer the following question by thinking step-by-step.\n\
                     For each step, write:\n\
                     Step N: <your reasoning> -> <conclusion>\n\n\
                     After all steps, write:\n\
                     Therefore: <final answer>\n\n\
                     Question: {question}"
                )
            }

            CotStrategy::FewShot(examples) => {
                let mut prompt = String::from(
                    "Answer the following question by thinking step-by-step, \
                     following the examples below.\n\n",
                );
                for (i, ex) in examples.iter().enumerate() {
                    prompt.push_str(&format!("--- Example {} ---\n{}\n\n", i + 1, ex));
                }
                prompt.push_str("--- Your turn ---\n");
                prompt.push_str(&format!(
                    "For each step write:\n\
                     Step N: <your reasoning> -> <conclusion>\n\n\
                     After all steps write:\n\
                     Therefore: <final answer>\n\n\
                     Question: {question}"
                ));
                prompt
            }

            CotStrategy::TreeOfThought { branching_factor } => {
                self.tree_of_thought_prompt(question, *branching_factor)
            }

            CotStrategy::SelfConsistency { samples } => {
                format!(
                    "Produce {samples} independent reasoning chains for the question below.\n\
                     Number each chain as \"Chain 1:\", \"Chain 2:\", etc.\n\
                     Within each chain, write each step as:\n\
                     Step N: <reasoning> -> <conclusion>\n\
                     End each chain with:\n\
                     Therefore: <answer>\n\n\
                     After all chains, identify the most consistent answer and write:\n\
                     Answer: <consensus answer>\n\n\
                     Question: {question}"
                )
            }
        }
    }

    /// Return a set of (question, CoT answer) example pairs for common
    /// reasoning types.
    pub fn few_shot_examples(&self) -> Vec<(String, String)> {
        vec![
            (
                "If a train travels 60 km/h for 2.5 hours, how far does it go?".to_string(),
                "Step 1: Identify the formula. Distance = speed × time. -> Formula is d = v × t.\n\
                 Step 2: Substitute values. d = 60 km/h × 2.5 h. -> d = 150 km.\n\
                 Step 3: Check units. km/h × h = km. -> Units are consistent.\n\
                 Therefore: The train travels 150 km."
                    .to_string(),
            ),
            (
                "Is 97 a prime number?".to_string(),
                "Step 1: Check divisibility by 2. 97 is odd. -> Not divisible by 2.\n\
                 Step 2: Check divisibility by 3. 9+7=16, not divisible by 3. -> Not divisible by 3.\n\
                 Step 3: Check up to sqrt(97) ≈ 9.8. Test 5, 7. Neither divides 97. -> No factors found.\n\
                 Therefore: 97 is a prime number."
                    .to_string(),
            ),
            (
                "What is the capital of Australia?".to_string(),
                "Step 1: Recall common misconception. Many assume Sydney is the capital. -> \
                 Sydney is the largest city but not the capital.\n\
                 Step 2: Recall founding history. Canberra was purpose-built as a compromise \
                 between Sydney and Melbourne. -> Canberra is the capital.\n\
                 Therefore: The capital of Australia is Canberra."
                    .to_string(),
            ),
        ]
    }

    /// Build a tree-of-thought prompt that instructs the model to explore
    /// `branching` parallel solution paths.
    pub fn tree_of_thought_prompt(&self, question: &str, branching: u8) -> String {
        let branching = branching.max(2);
        format!(
            "Use the Tree-of-Thought method to answer the question below.\n\
             Explore exactly {branching} distinct solution approaches in parallel.\n\n\
             For each approach, label it \"Branch N:\" (N = 1..{branching}) and \
             within each branch write each step as:\n\
             Step N: <reasoning> -> <conclusion>\n\n\
             After exploring all branches, evaluate them:\n\
             Evaluation: <which branch is strongest and why>\n\n\
             Then write the final answer:\n\
             Therefore: <final answer>\n\n\
             Question: {question}"
        )
    }
}

// ---------------------------------------------------------------------------
// CotParser
// ---------------------------------------------------------------------------

/// Parses chain-of-thought responses into structured [`ThoughtStep`]s.
#[derive(Debug, Default)]
pub struct CotParser;

impl CotParser {
    /// Create a new parser.
    pub fn new() -> Self {
        Self
    }

    /// Extract numbered steps from a response.
    ///
    /// Recognises lines of the form:
    /// ```text
    /// Step N: <thought> -> <conclusion>
    /// Step N: <thought> → <conclusion>
    /// ```
    /// If no arrow separator is present the entire text is treated as the
    /// thought and the conclusion is left empty.
    pub fn parse_steps(&self, response: &str) -> Vec<ThoughtStep> {
        let mut steps = Vec::new();

        for line in response.lines() {
            let trimmed = line.trim();

            // Match "Step N:" prefix (case-insensitive).
            let lower = trimmed.to_lowercase();
            if !lower.starts_with("step ") {
                continue;
            }
            // Find the colon after the step number.
            let colon_pos = match trimmed.find(':') {
                Some(p) => p,
                None => continue,
            };
            let step_num_str = trimmed[5..colon_pos].trim();
            let step_id: u32 = match step_num_str.parse() {
                Ok(n) => n,
                Err(_) => continue,
            };

            let body = trimmed[colon_pos + 1..].trim();

            // Split on "->" or "→".
            let (thought, conclusion) = if let Some(pos) = body.find("->") {
                (body[..pos].trim().to_string(), body[pos + 2..].trim().to_string())
            } else if let Some(pos) = body.find('\u{2192}') {
                // Unicode arrow →
                (
                    body[..pos].trim().to_string(),
                    body[pos + '\u{2192}'.len_utf8()..].trim().to_string(),
                )
            } else {
                (body.to_string(), String::new())
            };

            let requires_tool = thought.to_lowercase().contains("tool")
                || thought.to_lowercase().contains("search")
                || thought.to_lowercase().contains("lookup");

            let confidence = Self::heuristic_confidence(&thought);

            steps.push(ThoughtStep {
                step_id,
                thought,
                conclusion,
                confidence,
                requires_tool,
            });
        }

        steps
    }

    /// Extract the final answer from a response.
    ///
    /// Looks for lines beginning with `Therefore:`, `Answer:`, or
    /// `Conclusion:` (case-insensitive) and returns the text that follows.
    /// If none is found, returns an empty string.
    pub fn extract_final_answer(&self, response: &str) -> String {
        let markers = ["therefore:", "answer:", "conclusion:"];
        for line in response.lines() {
            let lower = line.trim().to_lowercase();
            for marker in &markers {
                if lower.starts_with(marker) {
                    let after = line.trim()[marker.len()..].trim();
                    return after.to_string();
                }
            }
        }
        String::new()
    }

    /// Compute average confidence across all steps.
    ///
    /// Heuristic: longer thought text implies more thorough reasoning, which
    /// maps to a higher confidence, capped at `1.0`.
    pub fn compute_confidence(&self, steps: &[ThoughtStep]) -> f64 {
        if steps.is_empty() {
            return 0.0;
        }
        let sum: f64 = steps.iter().map(|s| s.confidence).sum();
        (sum / steps.len() as f64).min(1.0)
    }

    /// Convenience: parse steps and final answer, then produce a full
    /// [`ChainOfThought`].
    pub fn parse(&self, response: &str) -> ChainOfThought {
        let steps = self.parse_steps(response);
        let final_answer = self.extract_final_answer(response);
        let total_confidence = self.compute_confidence(&steps);
        ChainOfThought {
            steps,
            final_answer,
            total_confidence,
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Heuristic confidence based on word count of the thought text.
    ///
    /// - 0 words  → 0.0
    /// - 1 word   → 0.2
    /// - 5 words  → 0.5
    /// - 10+ words → approaches 1.0 (asymptotic)
    fn heuristic_confidence(thought: &str) -> f64 {
        let words = thought.split_whitespace().count();
        if words == 0 {
            return 0.0;
        }
        // Sigmoid-like: score = words / (words + 10) scaled to [0.2, 1.0].
        let raw = words as f64 / (words as f64 + 10.0);
        // Map [0, 1) → [0.2, 1.0).
        0.2 + raw * 0.8
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- CotPromptBuilder ---

    #[test]
    fn test_zero_shot_contains_question() {
        let builder = CotPromptBuilder::new();
        let prompt = builder.build_cot_prompt("Why is the sky blue?", &CotStrategy::ZeroShot);
        assert!(prompt.contains("Why is the sky blue?"));
        assert!(prompt.contains("Step N:"));
        assert!(prompt.contains("Therefore:"));
    }

    #[test]
    fn test_few_shot_contains_examples() {
        let builder = CotPromptBuilder::new();
        let examples = vec!["Q: 2+2? A: Step 1: 2+2=4 -> 4.\nTherefore: 4".to_string()];
        let prompt =
            builder.build_cot_prompt("What is 3+3?", &CotStrategy::FewShot(examples));
        assert!(prompt.contains("Example 1"));
        assert!(prompt.contains("What is 3+3?"));
    }

    #[test]
    fn test_tree_of_thought_prompt_has_branches() {
        let builder = CotPromptBuilder::new();
        let prompt = builder.tree_of_thought_prompt("Solve X", 3);
        assert!(prompt.contains("Branch N:"));
        assert!(prompt.contains("3 distinct solution"));
    }

    #[test]
    fn test_tree_of_thought_minimum_branching() {
        let builder = CotPromptBuilder::new();
        // branching_factor of 0 should be clamped to 2.
        let prompt = builder.tree_of_thought_prompt("Q", 0);
        assert!(prompt.contains("2 distinct solution"));
    }

    #[test]
    fn test_self_consistency_prompt() {
        let builder = CotPromptBuilder::new();
        let prompt = builder.build_cot_prompt(
            "Is 11 prime?",
            &CotStrategy::SelfConsistency { samples: 3 },
        );
        assert!(prompt.contains("3 independent reasoning chains"));
        assert!(prompt.contains("Chain 1:"));
    }

    #[test]
    fn test_few_shot_examples_non_empty() {
        let builder = CotPromptBuilder::new();
        let examples = builder.few_shot_examples();
        assert!(!examples.is_empty());
        for (q, a) in &examples {
            assert!(!q.is_empty());
            assert!(!a.is_empty());
        }
    }

    // --- CotParser ---

    #[test]
    fn test_parse_steps_arrow() {
        let parser = CotParser::new();
        let response = "Step 1: Identify values -> values are 2 and 3.\n\
                        Step 2: Add them -> result is 5.\n\
                        Therefore: 5";
        let steps = parser.parse_steps(response);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].step_id, 1);
        assert_eq!(steps[0].conclusion, "values are 2 and 3.");
        assert_eq!(steps[1].step_id, 2);
        assert_eq!(steps[1].conclusion, "result is 5.");
    }

    #[test]
    fn test_parse_steps_no_arrow() {
        let parser = CotParser::new();
        let response = "Step 1: Just thinking here.";
        let steps = parser.parse_steps(response);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].thought, "Just thinking here.");
        assert!(steps[0].conclusion.is_empty());
    }

    #[test]
    fn test_parse_steps_requires_tool() {
        let parser = CotParser::new();
        let response = "Step 1: Need to tool-call the API -> result pending.";
        let steps = parser.parse_steps(response);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].requires_tool);
    }

    #[test]
    fn test_extract_final_answer_therefore() {
        let parser = CotParser::new();
        let resp = "Step 1: x -> y.\nTherefore: The answer is 42.";
        assert_eq!(parser.extract_final_answer(resp), "The answer is 42.");
    }

    #[test]
    fn test_extract_final_answer_answer_marker() {
        let parser = CotParser::new();
        let resp = "Step 1: x -> y.\nAnswer: Yes, it is.";
        assert_eq!(parser.extract_final_answer(resp), "Yes, it is.");
    }

    #[test]
    fn test_extract_final_answer_conclusion_marker() {
        let parser = CotParser::new();
        let resp = "Step 1: x -> y.\nConclusion: Definitely true.";
        assert_eq!(parser.extract_final_answer(resp), "Definitely true.");
    }

    #[test]
    fn test_extract_final_answer_missing() {
        let parser = CotParser::new();
        let resp = "Step 1: x -> y.";
        assert!(parser.extract_final_answer(resp).is_empty());
    }

    #[test]
    fn test_compute_confidence_empty() {
        let parser = CotParser::new();
        assert_eq!(parser.compute_confidence(&[]), 0.0);
    }

    #[test]
    fn test_compute_confidence_increases_with_length() {
        let parser = CotParser::new();
        let short = ThoughtStep {
            step_id: 1,
            thought: "hi".to_string(),
            conclusion: String::new(),
            confidence: CotParser::heuristic_confidence("hi"),
            requires_tool: false,
        };
        let long = ThoughtStep {
            step_id: 2,
            thought: "a b c d e f g h i j k l m n o p q r s t u v w x y z".to_string(),
            conclusion: String::new(),
            confidence: CotParser::heuristic_confidence(
                "a b c d e f g h i j k l m n o p q r s t u v w x y z",
            ),
            requires_tool: false,
        };
        assert!(long.confidence > short.confidence);
        assert!(long.confidence <= 1.0);
    }

    #[test]
    fn test_parse_full_chain() {
        let parser = CotParser::new();
        let response = "Step 1: Consider the problem carefully -> problem is addition.\n\
                        Step 2: Compute 3 + 4 -> result is 7.\n\
                        Therefore: 7";
        let cot = parser.parse(response);
        assert_eq!(cot.steps.len(), 2);
        assert_eq!(cot.final_answer, "7");
        assert!(cot.total_confidence > 0.0);
        assert!(cot.total_confidence <= 1.0);
    }

    #[test]
    fn test_confidence_capped_at_one() {
        // Very long thought should not exceed 1.0.
        let very_long = "word ".repeat(100);
        let conf = CotParser::heuristic_confidence(&very_long);
        assert!(conf <= 1.0);
        assert!(conf > 0.0);
    }
}
