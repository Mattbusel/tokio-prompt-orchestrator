//! LLM response classification and quality scoring.
//!
//! Classifies free-text LLM responses into semantic categories, computes
//! multi-dimensional quality scores, detects refusals, citations, and
//! computes a Flesch-Kincaid readability grade level.

use std::fmt;

// ── ResponseCategory ──────────────────────────────────────────────────────────

/// High-level semantic category of an LLM response.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResponseCategory {
    /// The response presents objective facts.
    Factual,
    /// The response expresses a subjective opinion.
    Opinion,
    /// The response is creative writing (story, poem, etc.).
    Creative,
    /// The response describes a sequence of steps or a procedure.
    Procedural,
    /// The response is casual conversational text.
    Conversational,
    /// The response covers a technical or engineering topic.
    Technical,
    /// The response involves numerical computation or math.
    Mathematical,
    /// The model declined to answer.
    Refusal,
    /// The response is itself an error message.
    ErrorResponse,
    /// The category could not be determined with confidence.
    Uncertain,
}

impl fmt::Display for ResponseCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ResponseCategory::Factual => "Factual",
            ResponseCategory::Opinion => "Opinion",
            ResponseCategory::Creative => "Creative",
            ResponseCategory::Procedural => "Procedural",
            ResponseCategory::Conversational => "Conversational",
            ResponseCategory::Technical => "Technical",
            ResponseCategory::Mathematical => "Mathematical",
            ResponseCategory::Refusal => "Refusal",
            ResponseCategory::ErrorResponse => "ErrorResponse",
            ResponseCategory::Uncertain => "Uncertain",
        };
        write!(f, "{s}")
    }
}

// ── QualityDimension ──────────────────────────────────────────────────────────

/// An axis along which response quality is evaluated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QualityDimension {
    /// Logical flow and internal consistency.
    Coherence,
    /// Whether the response fully addresses the prompt.
    Completeness,
    /// Factual correctness (heuristic approximation).
    Accuracy,
    /// Brevity vs. verbosity balance.
    Conciseness,
    /// How well the response stays on topic.
    Relevance,
    /// Use of structure (headers, lists, code blocks).
    Formatting,
}

impl fmt::Display for QualityDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            QualityDimension::Coherence => "Coherence",
            QualityDimension::Completeness => "Completeness",
            QualityDimension::Accuracy => "Accuracy",
            QualityDimension::Conciseness => "Conciseness",
            QualityDimension::Relevance => "Relevance",
            QualityDimension::Formatting => "Formatting",
        };
        write!(f, "{s}")
    }
}

// ── QualityScore ──────────────────────────────────────────────────────────────

/// A scored measurement on a single quality dimension.
#[derive(Debug, Clone)]
pub struct QualityScore {
    /// Which dimension was measured.
    pub dimension: QualityDimension,
    /// Score in [0.0, 1.0].
    pub score: f64,
    /// Confidence in the score in [0.0, 1.0].
    pub confidence: f64,
    /// Human-readable explanation of the score.
    pub reasoning: String,
}

// ── ClassificationResult ──────────────────────────────────────────────────────

/// The full result of classifying one LLM response.
#[derive(Debug, Clone)]
pub struct ClassificationResult {
    /// Predicted semantic category.
    pub category: ResponseCategory,
    /// Confidence in the category prediction in [0.0, 1.0].
    pub confidence: f64,
    /// Per-dimension quality scores.
    pub quality_scores: Vec<QualityScore>,
    /// Weighted mean of all dimension scores.
    pub overall_quality: f64,
    /// Whether the model refused to answer.
    pub is_refusal: bool,
    /// Whether citations or references were detected.
    pub has_citations: bool,
    /// Number of words in the response.
    pub word_count: usize,
    /// Flesch-Kincaid grade level.
    pub reading_grade: f64,
}

// ── ResponseClassifier ────────────────────────────────────────────────────────

/// Stateless classifier for LLM responses.
#[derive(Debug, Default)]
pub struct ResponseClassifier;

impl ResponseClassifier {
    /// Create a new classifier.
    pub fn new() -> Self {
        Self
    }

    /// Classify a single response given the original prompt.
    pub fn classify(&self, response: &str, prompt: &str) -> ClassificationResult {
        let is_refusal = Self::detect_refusal(response);
        let has_citations = Self::detect_citations(response);
        let word_count = count_words(response);
        let reading_grade = Self::flesch_kincaid_grade(response);

        let (category, confidence) = if is_refusal {
            (ResponseCategory::Refusal, 0.95)
        } else {
            Self::detect_category(response)
        };

        let coherence = QualityScore {
            dimension: QualityDimension::Coherence,
            score: Self::score_coherence(response),
            confidence: 0.6,
            reasoning: "Sentence transition and topic consistency heuristic.".to_string(),
        };
        let completeness = QualityScore {
            dimension: QualityDimension::Completeness,
            score: Self::score_completeness(response, prompt),
            confidence: 0.55,
            reasoning: "Question-word coverage relative to prompt.".to_string(),
        };
        let conciseness = QualityScore {
            dimension: QualityDimension::Conciseness,
            score: Self::score_conciseness(response),
            confidence: 0.5,
            reasoning: "Filler phrase and repetition penalty.".to_string(),
        };
        let formatting = QualityScore {
            dimension: QualityDimension::Formatting,
            score: Self::score_formatting(response),
            confidence: 0.7,
            reasoning: "Presence of lists, headers, and code blocks.".to_string(),
        };
        // Accuracy: proxy — higher for factual/technical, lower for uncertain.
        let accuracy_score = match &category {
            ResponseCategory::Factual | ResponseCategory::Technical => 0.75,
            ResponseCategory::Mathematical => 0.80,
            ResponseCategory::Opinion | ResponseCategory::Creative => 0.60,
            ResponseCategory::Refusal | ResponseCategory::ErrorResponse => 0.30,
            _ => 0.55,
        };
        let accuracy = QualityScore {
            dimension: QualityDimension::Accuracy,
            score: accuracy_score,
            confidence: 0.4,
            reasoning: "Category-based accuracy proxy.".to_string(),
        };
        // Relevance: overlap between prompt tokens and response tokens.
        let relevance_score = compute_token_overlap(prompt, response);
        let relevance = QualityScore {
            dimension: QualityDimension::Relevance,
            score: relevance_score,
            confidence: 0.6,
            reasoning: "Token overlap between prompt and response.".to_string(),
        };

        let quality_scores = vec![coherence, completeness, accuracy, conciseness, relevance, formatting];
        let overall_quality = quality_scores.iter().map(|s| s.score).sum::<f64>()
            / quality_scores.len() as f64;

        ClassificationResult {
            category,
            confidence,
            quality_scores,
            overall_quality,
            is_refusal,
            has_citations,
            word_count,
            reading_grade,
        }
    }

    /// Detect the response category using keyword/pattern matching.
    ///
    /// Returns `(category, confidence)`.
    pub fn detect_category(text: &str) -> (ResponseCategory, f64) {
        let lower = text.to_lowercase();

        // Scores accumulated per category.
        let mut scores: Vec<(ResponseCategory, f64)> = Vec::new();

        // Mathematical: digits, operators, equations.
        let math_score = {
            let eq_count = lower.matches('=').count();
            let digit_density = lower.chars().filter(|c| c.is_ascii_digit()).count() as f64
                / lower.len().max(1) as f64;
            let kw = count_keywords(&lower, &["equation", "calculate", "formula", "integral",
                "derivative", "matrix", "theorem", "proof", "sum", "product"]);
            (eq_count as f64 * 0.05 + digit_density * 2.0 + kw as f64 * 0.1).min(1.0)
        };
        scores.push((ResponseCategory::Mathematical, math_score));

        // Refusal: handled before calling this function, but guard anyway.
        if Self::detect_refusal(text) {
            return (ResponseCategory::Refusal, 0.95);
        }

        // Error response.
        let err_score = {
            let kw = count_keywords(&lower, &["error:", "exception:", "traceback", "stack trace",
                "syntax error", "runtime error", "null pointer", "segmentation fault"]);
            (kw as f64 * 0.25).min(1.0)
        };
        scores.push((ResponseCategory::ErrorResponse, err_score));

        // Technical.
        let tech_score = {
            let kw = count_keywords(&lower, &["function", "struct", "impl", "class", "module",
                "algorithm", "api", "database", "server", "protocol", "async", "thread",
                "memory", "cpu", "network", "interface", "library", "framework", "compile",
                "runtime", "binary", "architecture"]);
            let code_blocks = lower.matches("```").count() as f64;
            (kw as f64 * 0.06 + code_blocks * 0.15).min(1.0)
        };
        scores.push((ResponseCategory::Technical, tech_score));

        // Procedural: step-by-step, numbered lists.
        let proc_score = {
            let kw = count_keywords(&lower, &["step", "first", "second", "third", "next",
                "then", "finally", "install", "configure", "run", "execute", "follow"]);
            let numbered = lower.lines().filter(|l| {
                let t = l.trim();
                t.starts_with("1.") || t.starts_with("2.") || t.starts_with("3.")
            }).count();
            (kw as f64 * 0.05 + numbered as f64 * 0.1).min(1.0)
        };
        scores.push((ResponseCategory::Procedural, proc_score));

        // Factual.
        let fact_score = {
            let kw = count_keywords(&lower, &["according to", "research shows", "studies indicate",
                "published", "evidence", "data", "statistics", "was born", "founded in",
                "located in", "discovered", "invented", "historically"]);
            (kw as f64 * 0.12).min(1.0)
        };
        scores.push((ResponseCategory::Factual, fact_score));

        // Opinion.
        let opinion_score = {
            let kw = count_keywords(&lower, &["i think", "i believe", "in my opinion",
                "i feel", "personally", "i would say", "i recommend", "arguably", "seems to me"]);
            (kw as f64 * 0.15).min(1.0)
        };
        scores.push((ResponseCategory::Opinion, opinion_score));

        // Creative.
        let creative_score = {
            let kw = count_keywords(&lower, &["once upon a time", "she said", "he said",
                "chapter", "verse", "rhyme", "stanza", "protagonist", "narrative",
                "story", "poem", "fiction", "character"]);
            (kw as f64 * 0.1).min(1.0)
        };
        scores.push((ResponseCategory::Creative, creative_score));

        // Conversational: short, informal.
        let conv_score = {
            let word_count = count_words(text);
            let kw = count_keywords(&lower, &["sure!", "of course", "happy to", "great question",
                "thanks", "you're welcome", "absolutely", "definitely"]);
            let short_bonus = if word_count < 60 { 0.2 } else { 0.0 };
            (kw as f64 * 0.1 + short_bonus).min(1.0)
        };
        scores.push((ResponseCategory::Conversational, conv_score));

        // Pick highest score.
        let best = scores.into_iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        match best {
            Some((cat, score)) if score >= 0.15 => (cat, (score + 0.4).min(1.0)),
            _ => (ResponseCategory::Uncertain, 0.3),
        }
    }

    /// Detect if the response is a refusal.
    pub fn detect_refusal(text: &str) -> bool {
        let lower = text.to_lowercase();
        let phrases = [
            "i cannot", "i can't", "i am unable", "i'm unable",
            "i won't", "i will not", "as an ai", "as a language model",
            "i don't have the ability", "i'm not able", "i am not able",
            "i must decline", "i refuse", "that's not something i",
            "i cannot assist", "i'm afraid i cannot",
        ];
        phrases.iter().any(|p| lower.contains(p))
    }

    /// Detect whether the response contains citations or references.
    pub fn detect_citations(text: &str) -> bool {
        let lower = text.to_lowercase();
        // [1] style footnotes
        let has_numeric_ref = text.contains("[1]") || text.contains("[2]") || text.contains("[3]");
        // "according to", "source:", "cf.", "see:"
        let has_phrases = lower.contains("according to")
            || lower.contains("source:")
            || lower.contains(" cf.")
            || lower.contains("see:")
            || lower.contains("cited in")
            || lower.contains("references:");
        has_numeric_ref || has_phrases
    }

    /// Score sentence-level coherence (0-1).
    ///
    /// Heuristic: rewards smooth sentence beginnings, penalises abrupt stops.
    pub fn score_coherence(text: &str) -> f64 {
        let sentences: Vec<&str> = split_sentences(text);
        if sentences.len() < 2 {
            return 0.7;
        }

        let transition_words = ["however", "therefore", "furthermore", "additionally",
            "moreover", "consequently", "thus", "hence", "also", "similarly",
            "in contrast", "on the other hand", "for example", "as a result"];

        let transitions = sentences.iter().filter(|s| {
            let lower = s.to_lowercase();
            transition_words.iter().any(|t| lower.starts_with(t) || lower.contains(&format!(", {t}")))
        }).count();

        let transition_rate = transitions as f64 / sentences.len() as f64;

        // Average sentence length variance: very short or very long sentences hurt coherence.
        let lengths: Vec<usize> = sentences.iter().map(|s| count_words(s)).collect();
        let mean_len = lengths.iter().sum::<usize>() as f64 / lengths.len() as f64;
        let variance = lengths.iter().map(|&l| {
            let diff = l as f64 - mean_len;
            diff * diff
        }).sum::<f64>() / lengths.len() as f64;
        let length_penalty = (variance / 100.0).min(0.3);

        (0.5 + transition_rate * 0.5 - length_penalty).clamp(0.0, 1.0)
    }

    /// Score how completely the response addresses the prompt (0-1).
    pub fn score_completeness(response: &str, prompt: &str) -> f64 {
        // Check if question words in the prompt are addressed in the response.
        let prompt_lower = prompt.to_lowercase();
        let response_lower = response.to_lowercase();

        let question_words = ["what", "why", "how", "when", "where", "who", "which", "explain"];
        let asked: Vec<&str> = question_words.iter().filter(|w| prompt_lower.contains(*w)).copied().collect();

        if asked.is_empty() {
            // No explicit question words — check basic length adequacy.
            let words = count_words(response);
            return if words >= 50 { 0.8 } else { 0.5 };
        }

        let answered = asked.iter().filter(|w| response_lower.contains(*w)).count();
        let base = answered as f64 / asked.len() as f64;

        // Length bonus: very short answers likely incomplete.
        let words = count_words(response);
        let length_bonus = if words >= 100 { 0.15 } else if words >= 40 { 0.05 } else { 0.0 };

        (base * 0.85 + length_bonus).clamp(0.0, 1.0)
    }

    /// Score conciseness (0-1). Higher = more concise.
    pub fn score_conciseness(text: &str) -> f64 {
        let lower = text.to_lowercase();
        let filler_phrases = [
            "it is important to note that",
            "it should be noted that",
            "in order to",
            "due to the fact that",
            "at this point in time",
            "for the purpose of",
            "in the event that",
            "the fact that",
            "it is worth mentioning",
            "needless to say",
            "as a matter of fact",
        ];

        let filler_count = filler_phrases.iter().filter(|p| lower.contains(*p)).count();
        let filler_penalty = (filler_count as f64 * 0.08).min(0.4);

        // Repetition: count how many 4-gram sequences repeat.
        let words: Vec<&str> = text.split_whitespace().collect();
        let mut ngrams: std::collections::HashMap<[&str; 4], usize> = std::collections::HashMap::new();
        for w in words.windows(4) {
            *ngrams.entry([w[0], w[1], w[2], w[3]]).or_insert(0) += 1;
        }
        let repeated = ngrams.values().filter(|&&c| c > 1).count();
        let repetition_penalty = (repeated as f64 * 0.05).min(0.3);

        (1.0 - filler_penalty - repetition_penalty).clamp(0.0, 1.0)
    }

    /// Score formatting quality (0-1).
    pub fn score_formatting(text: &str) -> f64 {
        let has_code_block = text.contains("```");
        let has_numbered_list = text.lines().any(|l| {
            let t = l.trim();
            t.len() > 2 && t.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) && t.contains(". ")
        });
        let has_bullet_list = text.lines().any(|l| {
            let t = l.trim();
            t.starts_with("- ") || t.starts_with("* ") || t.starts_with("• ")
        });
        let has_header = text.lines().any(|l| l.trim().starts_with('#'));

        let mut score: f64 = 0.4; // baseline
        if has_code_block { score += 0.2; }
        if has_numbered_list { score += 0.15; }
        if has_bullet_list { score += 0.15; }
        if has_header { score += 0.1; }

        score.clamp(0.0, 1.0)
    }

    /// Compute the Flesch-Kincaid Grade Level.
    ///
    /// FK Grade = 0.39 * (words/sentences) + 11.8 * (syllables/words) - 15.59
    pub fn flesch_kincaid_grade(text: &str) -> f64 {
        let word_count = count_words(text);
        if word_count == 0 {
            return 0.0;
        }
        let sentence_count = split_sentences(text).len().max(1);
        let syllable_count = text.split_whitespace().map(count_syllables).sum::<usize>();

        let words_per_sentence = word_count as f64 / sentence_count as f64;
        let syllables_per_word = syllable_count as f64 / word_count as f64;

        let grade = 0.39 * words_per_sentence + 11.8 * syllables_per_word - 15.59;
        grade.clamp(0.0, 20.0)
    }

    /// Classify a batch of (response, prompt) pairs.
    pub fn batch_classify(&self, responses: &[(String, String)]) -> Vec<ClassificationResult> {
        responses.iter().map(|(resp, prompt)| self.classify(resp, prompt)).collect()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

fn count_keywords(text: &str, keywords: &[&str]) -> usize {
    keywords.iter().filter(|k| text.contains(*k)).count()
}

fn split_sentences(text: &str) -> Vec<&str> {
    // Simple heuristic: split on `. `, `! `, `? `
    let mut result = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if (bytes[i] == b'.' || bytes[i] == b'!' || bytes[i] == b'?')
            && i + 1 < len && bytes[i + 1] == b' '
        {
            let s = text[start..=i].trim();
            if !s.is_empty() {
                result.push(s);
            }
            start = i + 2;
            i += 2;
        } else {
            i += 1;
        }
    }
    if start < len {
        let s = text[start..].trim();
        if !s.is_empty() {
            result.push(s);
        }
    }
    if result.is_empty() {
        result.push(text.trim());
    }
    result
}

/// Approximate syllable count for a single word.
fn count_syllables(word: &str) -> usize {
    let lower = word.to_lowercase();
    let vowels = "aeiouy";
    let chars: Vec<char> = lower.chars().collect();
    let mut count = 0usize;
    let mut prev_vowel = false;
    for &c in &chars {
        let is_vowel = vowels.contains(c);
        if is_vowel && !prev_vowel {
            count += 1;
        }
        prev_vowel = is_vowel;
    }
    // Silent 'e' at end.
    if lower.ends_with('e') && count > 1 {
        count -= 1;
    }
    count.max(1)
}

fn compute_token_overlap(prompt: &str, response: &str) -> f64 {
    let prompt_tokens: std::collections::HashSet<&str> = prompt.split_whitespace().collect();
    let response_tokens: std::collections::HashSet<&str> = response.split_whitespace().collect();
    if prompt_tokens.is_empty() {
        return 0.5;
    }
    let overlap = prompt_tokens.intersection(&response_tokens).count();
    (overlap as f64 / prompt_tokens.len() as f64).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_refusal_detection() {
        assert!(ResponseClassifier::detect_refusal("I cannot help with that request."));
        assert!(ResponseClassifier::detect_refusal("As an AI, I won't provide that."));
        assert!(!ResponseClassifier::detect_refusal("The answer is 42."));
    }

    #[test]
    fn test_citation_detection() {
        assert!(ResponseClassifier::detect_citations("See [1] for more details."));
        assert!(ResponseClassifier::detect_citations("According to research, this is true."));
        assert!(!ResponseClassifier::detect_citations("The quick brown fox."));
    }

    #[test]
    fn test_flesch_kincaid() {
        let text = "The cat sat on the mat. It was a good cat.";
        let grade = ResponseClassifier::flesch_kincaid_grade(text);
        assert!(grade >= 0.0 && grade <= 20.0);
    }

    #[test]
    fn test_classify_basic() {
        let classifier = ResponseClassifier::new();
        let result = classifier.classify(
            "The function takes two arguments and returns a struct.",
            "What does this function do?",
        );
        assert!(result.overall_quality > 0.0 && result.overall_quality <= 1.0);
        assert!(result.word_count > 0);
    }

    #[test]
    fn test_batch_classify() {
        let classifier = ResponseClassifier::new();
        let pairs = vec![
            ("Hello!".to_string(), "Hi".to_string()),
            ("The Earth is 4.5 billion years old.".to_string(), "How old is Earth?".to_string()),
        ];
        let results = classifier.batch_classify(&pairs);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_score_formatting_with_code() {
        let text = "Here is the code:\n```rust\nfn main() {}\n```";
        let score = ResponseClassifier::score_formatting(text);
        assert!(score > 0.4);
    }
}
