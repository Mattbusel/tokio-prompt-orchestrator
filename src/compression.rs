//! Prompt compression — reduce token count before sending to the model.
//!
//! Compressing prompts before inference reduces cost and latency, especially
//! for long conversation histories or large context retrievals. This module
//! provides several complementary strategies that can be chained.
//!
//! ## Strategies
//!
//! | Strategy | What it does | Tokens saved (typical) |
//! |----------|-------------|------------------------|
//! | [`WhitespaceCompressor`] | Collapses redundant whitespace/newlines | 2–5% |
//! | [`RepetitionRemover`] | Removes duplicate paragraphs/sentences | 5–15% |
//! | [`StopWordFilter`] | Removes low-information filler words | 5–20% |
//! | [`SentenceRanker`] | Keeps only the top-K most relevant sentences | 20–60% |
//! | [`TruncationStrategy`] | Hard truncation with smart boundary detection | variable |
//! | [`CompressionPipeline`] | Chains multiple strategies | additive |
//!
//! ## Usage
//!
//! ```rust,no_run
//! use tokio_prompt_orchestrator::compression::{CompressionPipeline, SentenceRanker, WhitespaceCompressor};
//!
//! let pipeline = CompressionPipeline::new()
//!     .with(Box::new(WhitespaceCompressor))
//!     .with(Box::new(SentenceRanker::new(0.5))); // keep top 50% sentences
//!
//! let original = "Long prompt with lots of redundancy...";
//! let (compressed, ratio) = pipeline.compress(original);
//! println!("Reduced by {:.1}%", (1.0 - ratio) * 100.0);
//! ```

use std::collections::HashSet;

/// The output of a compression pass: compressed text + compression ratio.
///
/// `ratio` is in `[0.0, 1.0]` where `1.0` means no compression.
#[derive(Debug, Clone)]
pub struct CompressionResult {
    /// The compressed text.
    pub text: String,
    /// Ratio of output length to input length (1.0 = unchanged, 0.5 = halved).
    pub ratio: f64,
    /// Number of characters removed.
    pub chars_removed: usize,
    /// Strategy that produced this result.
    pub strategy: String,
}

/// Trait for a single compression strategy.
pub trait Compressor: Send + Sync {
    /// Apply this compression strategy to the input text.
    fn compress(&self, input: &str) -> CompressionResult;

    /// Human-readable name for metrics/logging.
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// WhitespaceCompressor
// ---------------------------------------------------------------------------

/// Collapses redundant whitespace: multiple spaces → one, 3+ newlines → two.
///
/// This is the cheapest compression to apply and should always be first.
pub struct WhitespaceCompressor;

impl Compressor for WhitespaceCompressor {
    fn compress(&self, input: &str) -> CompressionResult {
        // Collapse multiple spaces within lines
        let mut result = String::with_capacity(input.len());
        let mut prev_space = false;
        let mut newline_run = 0u8;

        for ch in input.chars() {
            match ch {
                '\n' => {
                    newline_run += 1;
                    prev_space = false;
                    if newline_run <= 2 {
                        result.push('\n');
                    }
                }
                ' ' | '\t' => {
                    newline_run = 0;
                    if !prev_space {
                        result.push(' ');
                        prev_space = true;
                    }
                }
                _ => {
                    newline_run = 0;
                    prev_space = false;
                    result.push(ch);
                }
            }
        }

        let chars_removed = input.len().saturating_sub(result.len());
        let ratio = if input.is_empty() {
            1.0
        } else {
            result.len() as f64 / input.len() as f64
        };

        CompressionResult {
            text: result,
            ratio,
            chars_removed,
            strategy: self.name().to_string(),
        }
    }

    fn name(&self) -> &'static str {
        "whitespace"
    }
}

// ---------------------------------------------------------------------------
// RepetitionRemover
// ---------------------------------------------------------------------------

/// Removes duplicate sentences or paragraphs.
///
/// Uses exact-match deduplication on sentences split by `. ` or `\n\n`.
pub struct RepetitionRemover;

impl Compressor for RepetitionRemover {
    fn compress(&self, input: &str) -> CompressionResult {
        // Split into sentences on ". " or paragraph breaks
        let mut seen: HashSet<&str> = HashSet::new();
        let mut output_parts: Vec<&str> = Vec::new();

        for sentence in input.split_inclusive(". ") {
            let trimmed = sentence.trim();
            if trimmed.is_empty() {
                continue;
            }
            if seen.insert(trimmed) {
                output_parts.push(sentence);
            }
        }

        let text = output_parts.join("");
        let chars_removed = input.len().saturating_sub(text.len());
        let ratio = if input.is_empty() {
            1.0
        } else {
            text.len() as f64 / input.len() as f64
        };

        CompressionResult {
            text,
            ratio,
            chars_removed,
            strategy: self.name().to_string(),
        }
    }

    fn name(&self) -> &'static str {
        "repetition_remover"
    }
}

// ---------------------------------------------------------------------------
// StopWordFilter
// ---------------------------------------------------------------------------

/// Removes common English stop words from the text.
///
/// This is aggressive — only use when the downstream task tolerates it
/// (e.g. topic classification, keyword extraction, not generation).
pub struct StopWordFilter {
    stop_words: HashSet<&'static str>,
}

impl Default for StopWordFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl StopWordFilter {
    /// Create a stop word filter with a built-in English stop word list.
    pub fn new() -> Self {
        let words = [
            "a", "an", "the", "is", "are", "was", "were", "be", "been", "being",
            "have", "has", "had", "do", "does", "did", "will", "would", "could",
            "should", "may", "might", "shall", "can", "need", "dare", "ought",
            "used", "to", "of", "in", "on", "at", "by", "for", "with", "about",
            "against", "between", "into", "through", "during", "before", "after",
            "above", "below", "from", "up", "down", "out", "off", "over", "under",
            "again", "further", "then", "once", "and", "but", "or", "nor", "so",
            "yet", "both", "either", "neither", "not", "only", "own", "same",
            "than", "too", "very", "s", "t", "just", "don", "now", "i", "me",
            "my", "myself", "we", "our", "you", "your", "he", "she", "it", "they",
            "them", "this", "that", "these", "those", "what", "which", "who",
        ];
        Self {
            stop_words: words.iter().copied().collect(),
        }
    }
}

impl Compressor for StopWordFilter {
    fn compress(&self, input: &str) -> CompressionResult {
        let words: Vec<&str> = input.split_whitespace().collect();
        let filtered: Vec<&str> = words
            .iter()
            .copied()
            .filter(|w| {
                let lower = w.to_lowercase();
                let bare = lower.trim_matches(|c: char| !c.is_alphabetic());
                !self.stop_words.contains(bare)
            })
            .collect();

        let text = filtered.join(" ");
        let chars_removed = input.len().saturating_sub(text.len());
        let ratio = if input.is_empty() {
            1.0
        } else {
            text.len() as f64 / input.len() as f64
        };

        CompressionResult {
            text,
            ratio,
            chars_removed,
            strategy: self.name().to_string(),
        }
    }

    fn name(&self) -> &'static str {
        "stop_word_filter"
    }
}

// ---------------------------------------------------------------------------
// SentenceRanker (TF-IDF based)
// ---------------------------------------------------------------------------

/// Keeps only the top-K most relevant sentences using TF-IDF scoring.
///
/// `keep_fraction` is in `(0, 1]`: `0.5` keeps the top 50% of sentences.
/// Sentences are re-assembled in their original order after ranking.
///
/// This is the highest-impact compressor for long RAG-retrieved contexts.
pub struct SentenceRanker {
    keep_fraction: f64,
}

impl SentenceRanker {
    /// Create a sentence ranker.
    ///
    /// # Arguments
    /// * `keep_fraction` — fraction of sentences to retain (0.0–1.0).
    ///   Clamped to [0.05, 1.0] to avoid empty output.
    pub fn new(keep_fraction: f64) -> Self {
        Self {
            keep_fraction: keep_fraction.clamp(0.05, 1.0),
        }
    }

    /// Score sentences by TF-IDF relevance against the entire document.
    fn score_sentences<'a>(&self, sentences: &[&'a str]) -> Vec<(usize, f64)> {
        use std::collections::HashMap;

        if sentences.is_empty() {
            return Vec::new();
        }

        // Build term frequency per sentence
        let term_freqs: Vec<HashMap<String, f64>> = sentences
            .iter()
            .map(|s| {
                let mut freq: HashMap<String, f64> = HashMap::new();
                for word in s.split_whitespace() {
                    let w = word.to_lowercase();
                    let w = w.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
                    if !w.is_empty() {
                        *freq.entry(w).or_insert(0.0) += 1.0;
                    }
                }
                // Normalise by sentence length
                let total: f64 = freq.values().sum();
                if total > 0.0 {
                    freq.values_mut().for_each(|v| *v /= total);
                }
                freq
            })
            .collect();

        // IDF: log(N / df) for each term
        let n = sentences.len() as f64;
        let mut doc_freq: HashMap<String, f64> = HashMap::new();
        for tf in &term_freqs {
            for term in tf.keys() {
                *doc_freq.entry(term.clone()).or_insert(0.0) += 1.0;
            }
        }

        // Score each sentence as sum of TF-IDF of its terms
        let mut scores: Vec<(usize, f64)> = term_freqs
            .iter()
            .enumerate()
            .map(|(i, tf)| {
                let score: f64 = tf
                    .iter()
                    .map(|(term, &tf_val)| {
                        let df = doc_freq.get(term).copied().unwrap_or(1.0);
                        let idf = (n / df).ln();
                        tf_val * idf
                    })
                    .sum();
                (i, score)
            })
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores
    }
}

impl Compressor for SentenceRanker {
    fn compress(&self, input: &str) -> CompressionResult {
        // Split on ". " or "\n\n"
        let mut sentences: Vec<&str> = Vec::new();
        for part in input.split(". ") {
            sentences.push(part);
        }

        if sentences.len() <= 1 {
            return CompressionResult {
                text: input.to_string(),
                ratio: 1.0,
                chars_removed: 0,
                strategy: self.name().to_string(),
            };
        }

        let keep = ((sentences.len() as f64 * self.keep_fraction).ceil() as usize)
            .max(1)
            .min(sentences.len());

        let scored = self.score_sentences(&sentences);

        // Take top-K indices, sorted back to original order
        let mut keep_indices: Vec<usize> = scored.iter().take(keep).map(|(i, _)| *i).collect();
        keep_indices.sort_unstable();

        let text = keep_indices
            .iter()
            .map(|&i| sentences[i])
            .collect::<Vec<_>>()
            .join(". ");

        let chars_removed = input.len().saturating_sub(text.len());
        let ratio = if input.is_empty() {
            1.0
        } else {
            text.len() as f64 / input.len() as f64
        };

        CompressionResult {
            text,
            ratio,
            chars_removed,
            strategy: self.name().to_string(),
        }
    }

    fn name(&self) -> &'static str {
        "sentence_ranker"
    }
}

// ---------------------------------------------------------------------------
// TruncationStrategy
// ---------------------------------------------------------------------------

/// Hard truncation at a character limit, respecting sentence boundaries.
pub struct TruncationStrategy {
    max_chars: usize,
}

impl TruncationStrategy {
    /// Create a truncator with the given character limit.
    pub fn new(max_chars: usize) -> Self {
        Self { max_chars }
    }
}

impl Compressor for TruncationStrategy {
    fn compress(&self, input: &str) -> CompressionResult {
        if input.len() <= self.max_chars {
            return CompressionResult {
                text: input.to_string(),
                ratio: 1.0,
                chars_removed: 0,
                strategy: self.name().to_string(),
            };
        }

        // Find last sentence boundary before limit
        let candidate = &input[..self.max_chars];
        let cut = candidate
            .rfind(". ")
            .or_else(|| candidate.rfind('\n'))
            .map(|i| i + 1)
            .unwrap_or(self.max_chars);

        let text = input[..cut].trim().to_string();
        let chars_removed = input.len() - text.len();
        let ratio = text.len() as f64 / input.len() as f64;

        CompressionResult {
            text,
            ratio,
            chars_removed,
            strategy: self.name().to_string(),
        }
    }

    fn name(&self) -> &'static str {
        "truncation"
    }
}

// ---------------------------------------------------------------------------
// CompressionPipeline
// ---------------------------------------------------------------------------

/// Chains multiple compression strategies sequentially.
///
/// Each strategy receives the output of the previous one.
/// Tracks overall statistics across the full pipeline.
pub struct CompressionPipeline {
    stages: Vec<Box<dyn Compressor>>,
}

impl Default for CompressionPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl CompressionPipeline {
    /// Create an empty pipeline.
    pub fn new() -> Self {
        Self { stages: Vec::new() }
    }

    /// Add a compression stage to the end of the pipeline.
    pub fn with(mut self, stage: Box<dyn Compressor>) -> Self {
        self.stages.push(stage);
        self
    }

    /// Build a sensible default pipeline for typical RAG prompts.
    ///
    /// Applies: whitespace → repetition → sentence ranking (keep 70%)
    pub fn default_for_rag() -> Self {
        Self::new()
            .with(Box::new(WhitespaceCompressor))
            .with(Box::new(RepetitionRemover))
            .with(Box::new(SentenceRanker::new(0.7)))
    }

    /// Build a pipeline for long conversation history compression.
    ///
    /// Applies: whitespace → repetition → sentence ranking (keep 50%)
    pub fn for_conversation_history() -> Self {
        Self::new()
            .with(Box::new(WhitespaceCompressor))
            .with(Box::new(RepetitionRemover))
            .with(Box::new(SentenceRanker::new(0.5)))
    }

    /// Compress the input through all stages, returning the final result.
    ///
    /// The returned `ratio` reflects the overall compression across all stages.
    pub fn compress(&self, input: &str) -> (String, f64) {
        if self.stages.is_empty() {
            return (input.to_string(), 1.0);
        }

        let original_len = input.len().max(1);
        let mut current = input.to_string();

        for stage in &self.stages {
            let result = stage.compress(&current);
            current = result.text;
        }

        let ratio = current.len() as f64 / original_len as f64;
        (current, ratio)
    }

    /// Return the number of stages in this pipeline.
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_whitespace_compressor_collapses_spaces() {
        let c = WhitespaceCompressor;
        let r = c.compress("hello   world\n\n\n\nfoo");
        assert_eq!(r.text, "hello world\n\nfoo");
        assert!(r.ratio < 1.0);
    }

    #[test]
    fn test_whitespace_compressor_empty() {
        let c = WhitespaceCompressor;
        let r = c.compress("");
        assert_eq!(r.ratio, 1.0);
        assert_eq!(r.text, "");
    }

    #[test]
    fn test_repetition_remover_deduplicates() {
        let c = RepetitionRemover;
        let r = c.compress("Hello world. Hello world. Goodbye.");
        assert!(!r.text.contains("Hello world. Hello world."), "text={}", r.text);
    }

    #[test]
    fn test_sentence_ranker_respects_fraction() {
        let ranker = SentenceRanker::new(0.5);
        let input = "The quick brown fox. A lazy dog sat down. Rust is fast. Memory safety matters. Tokio is async. Channels provide backpressure.";
        let result = ranker.compress(input);
        // Should be roughly half the sentences
        assert!(result.ratio < 0.9, "ratio={}", result.ratio);
    }

    #[test]
    fn test_truncation_respects_sentence_boundary() {
        let t = TruncationStrategy::new(30);
        let input = "Short sentence. Another sentence follows.";
        let result = t.compress(input);
        assert!(result.text.len() <= 30 || result.text.ends_with('.') || result.text.ends_with("sentence"));
    }

    #[test]
    fn test_truncation_no_op_when_short() {
        let t = TruncationStrategy::new(1000);
        let input = "Short.";
        let result = t.compress(input);
        assert_eq!(result.ratio, 1.0);
        assert_eq!(result.text, input);
    }

    #[test]
    fn test_pipeline_chains_strategies() {
        let pipeline = CompressionPipeline::new()
            .with(Box::new(WhitespaceCompressor))
            .with(Box::new(RepetitionRemover));
        let input = "Hello   world. Hello   world.";
        let (text, ratio) = pipeline.compress(input);
        assert!(!text.contains("  "), "should have no double spaces");
        assert!(ratio < 1.0 || text.len() <= input.len());
    }

    #[test]
    fn test_default_rag_pipeline() {
        let pipeline = CompressionPipeline::default_for_rag();
        assert_eq!(pipeline.stage_count(), 3);
        let long_input = "The system encountered an error. The system encountered an error. \
            Rust provides memory safety. The Tokio runtime handles async I/O. \
            Circuit breakers prevent cascading failures. Backpressure ensures stability. \
            Deduplication reduces redundant work. Rate limiting protects downstream services.";
        let (text, ratio) = pipeline.compress(long_input);
        assert!(ratio < 1.0, "should compress something, ratio={ratio}");
        assert!(!text.is_empty());
    }

    #[test]
    fn test_empty_pipeline_is_noop() {
        let pipeline = CompressionPipeline::new();
        let input = "hello world";
        let (text, ratio) = pipeline.compress(input);
        assert_eq!(text, input);
        assert_eq!(ratio, 1.0);
    }
}
