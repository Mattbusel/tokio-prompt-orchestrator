//! # Intent Classifier
//!
//! Rule-based classifier that infers the user's intent from prompt text.
//! Produces an [`IntentCategory`] variant and optional confidence scores.
//!
//! ## Quick Start
//!
//! ```rust
//! use tokio_prompt_orchestrator::intent_classifier::{IntentClassifier, IntentCategory};
//!
//! let classifier = IntentClassifier::new();
//! let category = classifier.classify("How do I implement a binary search in Rust?");
//! assert_eq!(category, IntentCategory::CodeRequest);
//! ```

/// The inferred category of a user's prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IntentCategory {
    /// A question seeking information or clarification.
    Question,
    /// A directive to perform an action.
    Command,
    /// A request for creative writing, storytelling, or artistic output.
    CreativeRequest,
    /// A request to write, explain, debug, or review code.
    CodeRequest,
    /// A request to analyse data, text, or a situation.
    AnalysisRequest,
    /// General conversational exchange.
    Conversation,
    /// Could not be confidently classified.
    Unknown,
}

impl IntentCategory {
    /// A human-readable description of this intent category.
    pub fn description(&self) -> &str {
        match self {
            Self::Question        => "User is asking a question seeking information or clarification.",
            Self::Command         => "User is issuing a directive to perform a specific action.",
            Self::CreativeRequest => "User wants creative writing, storytelling, or artistic content.",
            Self::CodeRequest     => "User wants code written, explained, debugged, or reviewed.",
            Self::AnalysisRequest => "User wants data, text, or a situation analysed.",
            Self::Conversation    => "General conversational exchange with no specific task.",
            Self::Unknown         => "Intent could not be confidently determined.",
        }
    }

    /// The recommended model capability tier for this intent.
    ///
    /// Returns one of `"fast"`, `"balanced"`, or `"powerful"`.
    pub fn suggested_model_tier(&self) -> &str {
        match self {
            Self::Conversation    => "fast",
            Self::Question        => "fast",
            Self::Command         => "balanced",
            Self::CreativeRequest => "balanced",
            Self::AnalysisRequest => "powerful",
            Self::CodeRequest     => "powerful",
            Self::Unknown         => "balanced",
        }
    }
}

// ---------------------------------------------------------------------------
// Feature extraction
// ---------------------------------------------------------------------------

/// Lightweight surface-level features extracted from a prompt.
#[derive(Debug, Clone)]
pub struct IntentFeatures {
    /// Whether the text contains at least one `?` character.
    pub has_question_mark: bool,
    /// Whether the first meaningful word is an imperative verb.
    pub starts_with_imperative: bool,
    /// Whether any recognised code-domain keyword appears in the text.
    pub contains_code_keywords: bool,
    /// Whether any recognised creative-domain keyword appears in the text.
    pub contains_creative_keywords: bool,
    /// Approximate number of sentences (split on `.`, `!`, `?`).
    pub sentence_count: usize,
    /// Average word length in characters.
    pub avg_word_length: f64,
}

// ---------------------------------------------------------------------------
// Keyword lists
// ---------------------------------------------------------------------------

const CODE_KEYWORDS: &[&str] = &[
    "fn", "function", "class", "def", "code", "implement", "debug",
    "error", "compile", "rust", "python", "javascript", "algorithm",
];

const CREATIVE_KEYWORDS: &[&str] = &[
    "write", "story", "poem", "creative", "imagine", "generate",
    "design", "create", "art",
];

/// Common imperative verbs that suggest a command-style intent.
const IMPERATIVE_VERBS: &[&str] = &[
    "list", "show", "find", "get", "set", "run", "execute", "make",
    "build", "open", "close", "delete", "remove", "add", "start",
    "stop", "install", "update", "check", "print", "display", "fetch",
    "send", "move", "copy", "rename", "convert", "parse", "sort",
    "filter", "search", "count", "calculate", "compute", "generate",
    "create", "write", "define", "summarise", "summarize", "translate",
    "explain", "describe", "analyse", "analyze", "compare", "classify",
];

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

/// Rule-based intent classifier.
///
/// All methods take `&self` and are safe to call from multiple threads once
/// the classifier has been constructed.
pub struct IntentClassifier;

impl Default for IntentClassifier {
    fn default() -> Self {
        Self::new()
    }
}

impl IntentClassifier {
    /// Create a new classifier with the default keyword lists.
    pub fn new() -> Self {
        Self
    }

    // ── Feature extraction ────────────────────────────────────────────────

    /// Extract surface-level features from `text`.
    pub fn extract_features(&self, text: &str) -> IntentFeatures {
        let lower = text.to_lowercase();

        let has_question_mark = text.contains('?');

        // Sentence count: count sentence-ending punctuation characters.
        let sentence_count = text
            .chars()
            .filter(|&c| c == '.' || c == '!' || c == '?')
            .count()
            .max(1); // treat absence of punctuation as a single sentence

        // Word statistics.
        let words: Vec<&str> = lower.split_whitespace().collect();
        let avg_word_length = if words.is_empty() {
            0.0
        } else {
            words.iter().map(|w| w.len()).sum::<usize>() as f64 / words.len() as f64
        };

        // Code-keyword presence: check each word against the keyword list.
        let contains_code_keywords = words
            .iter()
            .any(|w| CODE_KEYWORDS.contains(&strip_punctuation(w).as_str()));

        // Also check substrings for multi-word code keywords.
        let contains_code_keywords = contains_code_keywords
            || CODE_KEYWORDS.iter().any(|kw| lower.contains(kw));

        // Creative-keyword presence.
        let contains_creative_keywords = CREATIVE_KEYWORDS.iter().any(|kw| lower.contains(kw));

        // Imperative: does the first word match an imperative verb?
        let starts_with_imperative = words
            .first()
            .map(|w| {
                let bare = strip_punctuation(w);
                IMPERATIVE_VERBS.contains(&bare.as_str())
            })
            .unwrap_or(false);

        IntentFeatures {
            has_question_mark,
            starts_with_imperative,
            contains_code_keywords,
            contains_creative_keywords,
            sentence_count,
            avg_word_length,
        }
    }

    // ── Primary classify ──────────────────────────────────────────────────

    /// Classify the intent of `text` into a single [`IntentCategory`].
    ///
    /// Uses the highest-confidence category from [`classify_with_confidence`].
    pub fn classify(&self, text: &str) -> IntentCategory {
        let mut ranked = self.classify_with_confidence(text);
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.into_iter().next().map(|(cat, _)| cat).unwrap_or(IntentCategory::Unknown)
    }

    // ── Confidence scoring ────────────────────────────────────────────────

    /// Return a score for every [`IntentCategory`], sorted highest-first.
    ///
    /// Scores are not probabilities; they are rule weights in `[0.0, 1.0]`.
    pub fn classify_with_confidence(&self, text: &str) -> Vec<(IntentCategory, f64)> {
        let f = self.extract_features(text);

        let mut scores: Vec<(IntentCategory, f64)> = vec![
            (IntentCategory::Question,        self.score_question(&f)),
            (IntentCategory::Command,         self.score_command(&f)),
            (IntentCategory::CreativeRequest, self.score_creative(&f)),
            (IntentCategory::CodeRequest,     self.score_code(&f)),
            (IntentCategory::AnalysisRequest, self.score_analysis(text, &f)),
            (IntentCategory::Conversation,    self.score_conversation(&f)),
            (IntentCategory::Unknown,         0.05),
        ];

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores
    }

    // ── Batch classify ────────────────────────────────────────────────────

    /// Classify a slice of texts, returning one [`IntentCategory`] per item.
    pub fn batch_classify(&self, texts: &[&str]) -> Vec<IntentCategory> {
        texts.iter().map(|t| self.classify(t)).collect()
    }

    // ── Internal scoring helpers ──────────────────────────────────────────

    fn score_question(&self, f: &IntentFeatures) -> f64 {
        let mut score = 0.0_f64;
        if f.has_question_mark {
            score += 0.60;
        }
        // Typical question starters (what, how, why, when, where, who, which).
        score.min(1.0)
    }

    fn score_command(&self, f: &IntentFeatures) -> f64 {
        let mut score = 0.0_f64;
        if f.starts_with_imperative {
            score += 0.55;
        }
        if !f.has_question_mark {
            score += 0.10;
        }
        score.min(1.0)
    }

    fn score_creative(&self, f: &IntentFeatures) -> f64 {
        let mut score = 0.0_f64;
        if f.contains_creative_keywords {
            score += 0.65;
        }
        if !f.contains_code_keywords {
            score += 0.10;
        }
        score.min(1.0)
    }

    fn score_code(&self, f: &IntentFeatures) -> f64 {
        let mut score = 0.0_f64;
        if f.contains_code_keywords {
            score += 0.70;
        }
        if f.avg_word_length > 5.5 {
            // Technical prompts tend to use longer words.
            score += 0.10;
        }
        score.min(1.0)
    }

    fn score_analysis(&self, text: &str, f: &IntentFeatures) -> f64 {
        let lower = text.to_lowercase();
        let mut score = 0.0_f64;
        let analysis_words = ["analyse", "analyze", "analysis", "compare",
                               "evaluate", "assess", "review", "examine",
                               "investigate", "breakdown", "break down",
                               "summarise", "summarize", "interpret"];
        for kw in &analysis_words {
            if lower.contains(kw) {
                score += 0.50;
                break;
            }
        }
        if f.sentence_count > 2 {
            score += 0.10;
        }
        score.min(1.0)
    }

    fn score_conversation(&self, f: &IntentFeatures) -> f64 {
        let mut score = 0.15_f64; // small baseline
        if f.sentence_count == 1 && f.avg_word_length < 5.0 {
            score += 0.30;
        }
        if !f.contains_code_keywords && !f.contains_creative_keywords {
            score += 0.10;
        }
        score.min(1.0)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Remove leading/trailing punctuation from a word slice.
fn strip_punctuation(s: &str) -> String {
    s.trim_matches(|c: char| !c.is_alphanumeric()).to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn classifier() -> IntentClassifier {
        IntentClassifier::new()
    }

    // ── IntentCategory helpers ────────────────────────────────────────────

    #[test]
    fn description_non_empty_for_all_variants() {
        let variants = [
            IntentCategory::Question,
            IntentCategory::Command,
            IntentCategory::CreativeRequest,
            IntentCategory::CodeRequest,
            IntentCategory::AnalysisRequest,
            IntentCategory::Conversation,
            IntentCategory::Unknown,
        ];
        for v in &variants {
            assert!(!v.description().is_empty(), "{v:?} has empty description");
        }
    }

    #[test]
    fn suggested_model_tier_valid_values() {
        let valid = ["fast", "balanced", "powerful"];
        let variants = [
            IntentCategory::Question,
            IntentCategory::Command,
            IntentCategory::CreativeRequest,
            IntentCategory::CodeRequest,
            IntentCategory::AnalysisRequest,
            IntentCategory::Conversation,
            IntentCategory::Unknown,
        ];
        for v in &variants {
            assert!(
                valid.contains(&v.suggested_model_tier()),
                "{v:?} returned invalid tier"
            );
        }
    }

    // ── Feature extraction ────────────────────────────────────────────────

    #[test]
    fn extract_features_question_mark() {
        let c = classifier();
        let f = c.extract_features("What is Rust?");
        assert!(f.has_question_mark);
    }

    #[test]
    fn extract_features_no_question_mark() {
        let c = classifier();
        let f = c.extract_features("Tell me about Rust.");
        assert!(!f.has_question_mark);
    }

    #[test]
    fn extract_features_imperative() {
        let c = classifier();
        let f = c.extract_features("List all files in the directory.");
        assert!(f.starts_with_imperative);
    }

    #[test]
    fn extract_features_not_imperative() {
        let c = classifier();
        let f = c.extract_features("The quick brown fox.");
        assert!(!f.starts_with_imperative);
    }

    #[test]
    fn extract_features_code_keywords() {
        let c = classifier();
        let f = c.extract_features("Help me debug this rust function.");
        assert!(f.contains_code_keywords);
    }

    #[test]
    fn extract_features_creative_keywords() {
        let c = classifier();
        let f = c.extract_features("Write a poem about autumn.");
        assert!(f.contains_creative_keywords);
    }

    #[test]
    fn extract_features_sentence_count() {
        let c = classifier();
        let f = c.extract_features("First sentence. Second sentence! Third?");
        assert_eq!(f.sentence_count, 3);
    }

    #[test]
    fn extract_features_avg_word_length_positive() {
        let c = classifier();
        let f = c.extract_features("hello world");
        assert!(f.avg_word_length > 0.0);
    }

    // ── classify ─────────────────────────────────────────────────────────

    #[test]
    fn classify_code_request() {
        let c = classifier();
        assert_eq!(
            c.classify("How do I implement a binary search algorithm in Rust?"),
            IntentCategory::CodeRequest
        );
    }

    #[test]
    fn classify_creative_request() {
        let c = classifier();
        assert_eq!(
            c.classify("Write me a short story about a lonely robot."),
            IntentCategory::CreativeRequest
        );
    }

    #[test]
    fn classify_question() {
        let c = classifier();
        assert_eq!(
            c.classify("What is the capital of France?"),
            IntentCategory::Question
        );
    }

    #[test]
    fn classify_command() {
        let c = classifier();
        assert_eq!(
            c.classify("List all the environment variables."),
            IntentCategory::Command
        );
    }

    #[test]
    fn classify_analysis() {
        let c = classifier();
        assert_eq!(
            c.classify("Analyse the trade-offs between SQL and NoSQL databases."),
            IntentCategory::AnalysisRequest
        );
    }

    // ── classify_with_confidence ──────────────────────────────────────────

    #[test]
    fn confidence_sorted_descending() {
        let c = classifier();
        let scores = c.classify_with_confidence("How do I write a function in Python?");
        for w in scores.windows(2) {
            assert!(w[0].1 >= w[1].1, "scores not sorted: {:?}", scores);
        }
    }

    #[test]
    fn confidence_all_categories_present() {
        let c = classifier();
        let scores = c.classify_with_confidence("hi there");
        assert_eq!(scores.len(), 7);
    }

    // ── batch_classify ────────────────────────────────────────────────────

    #[test]
    fn batch_classify_length_matches() {
        let c = classifier();
        let texts = ["hello", "write a poem", "debug this code"];
        let results = c.batch_classify(&texts);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn batch_classify_empty_input() {
        let c = classifier();
        let results = c.batch_classify(&[]);
        assert!(results.is_empty());
    }

    #[test]
    fn batch_classify_mixed_intents() {
        let c = classifier();
        let texts = [
            "Implement a Rust function.",
            "Write me a poem about the ocean.",
        ];
        let results = c.batch_classify(&texts);
        assert_eq!(results[0], IntentCategory::CodeRequest);
        assert_eq!(results[1], IntentCategory::CreativeRequest);
    }

    // ── Default impl ──────────────────────────────────────────────────────

    #[test]
    fn default_impl_works() {
        let c = IntentClassifier::default();
        let _ = c.classify("test");
    }
}
