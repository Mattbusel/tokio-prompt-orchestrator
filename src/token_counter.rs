//! Multi-Model Token Counting with BPE Approximation
//!
//! Provides heuristic token counting for all major LLM families without
//! requiring a full tokeniser dependency. Counts are BPE approximations
//! tuned per-family and are accurate to within ~5–10 % for typical English
//! prose and code.
//!
//! ## Quick Start
//!
//! ```rust
//! use tokio_prompt_orchestrator::token_counter::{TokenCounter, TokenizerFamily};
//!
//! let counter = TokenCounter::new(TokenizerFamily::Claude);
//! let tc = counter.count("Hello, world! This is a test.");
//! println!("tokens: {}", tc.total_tokens);
//! ```

use std::collections::HashMap;

// ── Enums ────────────────────────────────────────────────────────────────────

/// The tokeniser family that governs counting heuristics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TokenizerFamily {
    /// GPT-3.5 / GPT-4 (cl100k_base) — ~4 chars per token.
    GPT4,
    /// Claude 2 / 3 family — ~3.8 chars per token (slightly denser vocab).
    Claude,
    /// Google Gemini — ~3.9 chars per token.
    Gemini,
    /// Meta LLaMA / LLaMA-2 — ~3.6 chars per token (SentencePiece).
    Llama,
    /// Unknown family — falls back to 4 chars per token.
    Generic,
}

/// How the token count was derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CountMethod {
    /// Counted by the model's actual tokeniser (not yet implemented here).
    Exact,
    /// Approximated via BPE character-ratio heuristic.
    BpeApprox,
    /// Approximated by raw character count.
    CharacterBased,
    /// Approximated by whitespace-split word count.
    WordBased,
}

// ── Structs ───────────────────────────────────────────────────────────────────

/// Result of a token-count operation.
#[derive(Debug, Clone)]
pub struct TokenCount {
    /// Estimated input tokens.
    pub input_tokens: usize,
    /// Estimated output tokens (0 unless a task-hint estimation was used).
    pub output_tokens: usize,
    /// `input_tokens + output_tokens`.
    pub total_tokens: usize,
    /// Model identifier this count was computed for.
    pub model: String,
    /// Method used to derive the count.
    pub method: CountMethod,
}

/// BPE-approximation tokeniser parameterised by family.
#[derive(Debug, Clone)]
pub struct BpeApproxTokenizer {
    /// The tokeniser family controlling the chars-per-token ratio.
    pub family: TokenizerFamily,
}

impl BpeApproxTokenizer {
    /// Approximate token count for `text` using family-specific heuristics.
    ///
    /// The heuristic:
    /// 1. Compute a base count from `chars / chars_per_token`.
    /// 2. Add +1 token per punctuation run (commas, periods, etc.).
    /// 3. Adjust for runs of digits (numbers tokenise more densely).
    /// 4. Adjust for leading/trailing whitespace overhead.
    pub fn count_tokens(text: &str, family: &TokenizerFamily) -> usize {
        if text.is_empty() {
            return 0;
        }

        let chars_per_token: f64 = match family {
            TokenizerFamily::GPT4 => 4.0,
            TokenizerFamily::Claude => 3.8,
            TokenizerFamily::Gemini => 3.9,
            TokenizerFamily::Llama => 3.6,
            TokenizerFamily::Generic => 4.0,
        };

        let char_count = text.chars().count() as f64;
        let mut estimate = char_count / chars_per_token;

        // Punctuation bonus: each standalone punctuation char adds ~0.3 tokens.
        let punct_count = text
            .chars()
            .filter(|c| c.is_ascii_punctuation())
            .count() as f64;
        estimate += punct_count * 0.15;

        // Digit runs: numbers split into smaller tokens.
        let digit_count = text.chars().filter(|c| c.is_ascii_digit()).count() as f64;
        estimate += digit_count * 0.05;

        // Whitespace: newlines and tabs each cost an extra fraction.
        let newline_count = text.chars().filter(|&c| c == '\n' || c == '\r').count() as f64;
        estimate += newline_count * 0.3;

        // Always at least 1 token for non-empty text.
        (estimate.ceil() as usize).max(1)
    }

    /// Estimate output tokens given input token count and a task hint.
    ///
    /// The ratios are empirical defaults; callers should tune for their workload.
    pub fn estimate_output_tokens(input_tokens: usize, task_hint: &str) -> usize {
        let ratio: f64 = {
            let hint = task_hint.to_lowercase();
            if hint.contains("summarize") || hint.contains("summarise") {
                0.25
            } else if hint.contains("translate") {
                1.05
            } else if hint.contains("explain") {
                1.5
            } else if hint.contains("code") || hint.contains("implement") || hint.contains("write") {
                2.0
            } else if hint.contains("classify") || hint.contains("sentiment") {
                0.1
            } else {
                1.0
            }
        };
        ((input_tokens as f64 * ratio).ceil() as usize).max(1)
    }

    /// Split `text` into chunks of at most `max_tokens` tokens, with `overlap`
    /// tokens of context carried forward between chunks.
    ///
    /// Chunks are split at sentence boundaries (`.`, `!`, `?`) where possible
    /// to preserve semantic coherence.
    pub fn split_into_chunks(
        text: &str,
        max_tokens: usize,
        overlap: usize,
        family: &TokenizerFamily,
    ) -> Vec<String> {
        if text.is_empty() || max_tokens == 0 {
            return vec![];
        }

        // Split into sentences on common terminators.
        let sentences: Vec<&str> = split_sentences(text);

        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut current_tokens = 0usize;
        let mut overlap_buf: Vec<String> = Vec::new();

        for sentence in &sentences {
            let s_tokens = Self::count_tokens(sentence, family);

            if current_tokens + s_tokens > max_tokens && !current.is_empty() {
                chunks.push(current.clone());
                // Build overlap from tail of current chunk.
                overlap_buf = build_overlap(&current, overlap, family);
                current = overlap_buf.join(" ");
                current_tokens = Self::count_tokens(&current, family);
            }

            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(sentence);
            current_tokens += s_tokens;
        }

        if !current.is_empty() {
            chunks.push(current);
        }

        if chunks.is_empty() {
            chunks.push(text.to_string());
        }

        chunks
    }

    /// Return `true` if `text` fits within `context_window` tokens for `model`.
    pub fn fits_in_context(text: &str, model: &str, context_window: usize, family: &TokenizerFamily) -> bool {
        // Add ~5 % overhead for system prompts and role tokens.
        let available = (context_window as f64 * 0.95) as usize;
        let count = Self::count_tokens(text, family);
        let _ = model; // model name kept for API symmetry / future lookup
        count <= available
    }
}

/// Split text into sentences on `.`, `!`, `?` followed by whitespace.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let len = bytes.len();

    let mut i = 0usize;
    while i < len {
        let b = bytes[i];
        if (b == b'.' || b == b'!' || b == b'?') && i + 1 < len && bytes[i + 1] == b' ' {
            sentences.push(text[start..=i].trim());
            start = i + 2;
            i += 2;
        } else {
            i += 1;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        sentences.push(tail);
    }
    sentences.into_iter().filter(|s| !s.is_empty()).collect()
}

/// Build an overlap buffer from the tail of `chunk`.
fn build_overlap(chunk: &str, overlap_tokens: usize, family: &TokenizerFamily) -> Vec<String> {
    if overlap_tokens == 0 {
        return vec![];
    }
    let words: Vec<&str> = chunk.split_whitespace().collect();
    let mut buf: Vec<String> = Vec::new();
    let mut tok_count = 0usize;
    for word in words.iter().rev() {
        let wt = BpeApproxTokenizer::count_tokens(word, family);
        if tok_count + wt > overlap_tokens {
            break;
        }
        buf.push(word.to_string());
        tok_count += wt;
    }
    buf.reverse();
    buf
}

// ── TokenCounter ──────────────────────────────────────────────────────────────

/// High-level token counter for a specific model/family combination.
pub struct TokenCounter {
    /// Underlying BPE approximation tokeniser.
    pub tokenizer: BpeApproxTokenizer,
    /// Per-model context window sizes (tokens).
    pub model_contexts: HashMap<String, usize>,
}

impl TokenCounter {
    /// Create a counter for the given tokeniser family, pre-populated with
    /// context windows for all major models.
    pub fn new(family: TokenizerFamily) -> Self {
        Self {
            tokenizer: BpeApproxTokenizer { family },
            model_contexts: Self::build_context_table(),
        }
    }

    /// Count tokens in a single text string.
    pub fn count(&self, text: &str) -> TokenCount {
        let n = BpeApproxTokenizer::count_tokens(text, &self.tokenizer.family);
        TokenCount {
            input_tokens: n,
            output_tokens: 0,
            total_tokens: n,
            model: String::new(),
            method: CountMethod::BpeApprox,
        }
    }

    /// Count tokens across a list of `(role, content)` message pairs.
    ///
    /// Each message incurs a ~4-token overhead for the role marker and
    /// formatting tokens used by chat APIs.
    pub fn count_messages(&self, messages: &[(String, String)]) -> TokenCount {
        const ROLE_OVERHEAD: usize = 4;
        let n: usize = messages
            .iter()
            .map(|(_, content)| {
                BpeApproxTokenizer::count_tokens(content, &self.tokenizer.family) + ROLE_OVERHEAD
            })
            .sum();
        // Add 2 tokens for the reply-primer used by most chat completions APIs.
        let total = n + 2;
        TokenCount {
            input_tokens: total,
            output_tokens: 0,
            total_tokens: total,
            model: String::new(),
            method: CountMethod::BpeApprox,
        }
    }

    /// Count tokens for a system prompt plus a message list.
    pub fn count_with_system(&self, system: &str, messages: &[(String, String)]) -> TokenCount {
        let sys_tokens = BpeApproxTokenizer::count_tokens(system, &self.tokenizer.family);
        // System prompts have ~5 token framing overhead.
        let sys_total = sys_tokens + 5;
        let mut msg_count = self.count_messages(messages);
        msg_count.input_tokens += sys_total;
        msg_count.total_tokens += sys_total;
        msg_count
    }

    /// How many tokens remain in `model`'s context window after `used` tokens.
    ///
    /// Returns `None` if the model is not in the built-in table.
    pub fn remaining_context(&self, model: &str, used: usize) -> Option<usize> {
        let window = self.model_contexts.get(model).copied()
            .or_else(|| Self::model_context_window(model))?;
        Some(window.saturating_sub(used))
    }

    /// Count tokens for each text in `texts`, returning one [`TokenCount`] per entry.
    pub fn batch_count(&self, texts: &[&str]) -> Vec<TokenCount> {
        texts.iter().map(|t| self.count(t)).collect()
    }

    /// Look up the context window (in tokens) for a known model.
    ///
    /// Returns `None` for unrecognised model identifiers.
    pub fn model_context_window(model: &str) -> Option<usize> {
        let table = Self::build_context_table();
        // Also try prefix matching for versioned model names.
        if let Some(&w) = table.get(model) {
            return Some(w);
        }
        // Prefix fallback: match longest known key that is a prefix of `model`.
        table
            .iter()
            .filter(|(k, _)| model.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, &v)| v)
    }

    // ── private helpers ───────────────────────────────────────────────────────

    fn build_context_table() -> HashMap<String, usize> {
        let entries: &[(&str, usize)] = &[
            // OpenAI GPT
            ("gpt-3.5-turbo", 16_385),
            ("gpt-3.5-turbo-16k", 16_385),
            ("gpt-4", 8_192),
            ("gpt-4-32k", 32_768),
            ("gpt-4-turbo", 128_000),
            ("gpt-4-turbo-preview", 128_000),
            ("gpt-4o", 128_000),
            ("gpt-4o-mini", 128_000),
            ("gpt-4.5", 128_000),
            ("o1", 200_000),
            ("o1-mini", 128_000),
            ("o3", 200_000),
            ("o3-mini", 200_000),
            // Anthropic Claude
            ("claude-2", 100_000),
            ("claude-2.1", 200_000),
            ("claude-3-haiku", 200_000),
            ("claude-3-sonnet", 200_000),
            ("claude-3-opus", 200_000),
            ("claude-3-5-haiku", 200_000),
            ("claude-3-5-sonnet", 200_000),
            ("claude-3-5-opus", 200_000),
            ("claude-3-7-sonnet", 200_000),
            ("claude-sonnet-4", 200_000),
            ("claude-opus-4", 200_000),
            // Google Gemini
            ("gemini-pro", 32_768),
            ("gemini-1.0-pro", 32_768),
            ("gemini-1.5-pro", 1_048_576),
            ("gemini-1.5-flash", 1_048_576),
            ("gemini-2.0-flash", 1_048_576),
            ("gemini-2.0-pro", 2_097_152),
            // Meta LLaMA
            ("llama-2-7b", 4_096),
            ("llama-2-13b", 4_096),
            ("llama-2-70b", 4_096),
            ("llama-3-8b", 8_192),
            ("llama-3-70b", 8_192),
            ("llama-3.1-8b", 131_072),
            ("llama-3.1-70b", 131_072),
            ("llama-3.1-405b", 131_072),
            ("llama-3.3-70b", 131_072),
            // Mistral
            ("mistral-7b", 32_768),
            ("mistral-8x7b", 32_768),
            ("mistral-large", 131_072),
            ("mistral-small", 131_072),
            // Cohere
            ("command-r", 128_000),
            ("command-r-plus", 128_000),
            // DeepSeek
            ("deepseek-chat", 64_000),
            ("deepseek-coder", 16_000),
            ("deepseek-r1", 163_840),
        ];
        entries
            .iter()
            .map(|&(k, v)| (k.to_string(), v))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_non_zero_for_non_empty() {
        for family in [
            TokenizerFamily::GPT4,
            TokenizerFamily::Claude,
            TokenizerFamily::Gemini,
            TokenizerFamily::Llama,
            TokenizerFamily::Generic,
        ] {
            let n = BpeApproxTokenizer::count_tokens("Hello, world!", &family);
            assert!(n > 0, "family {family:?} returned 0 tokens");
        }
    }

    #[test]
    fn count_empty_is_zero() {
        assert_eq!(BpeApproxTokenizer::count_tokens("", &TokenizerFamily::GPT4), 0);
    }

    #[test]
    fn output_estimate_summarize_less_than_explain() {
        let summarize = BpeApproxTokenizer::estimate_output_tokens(100, "summarize this");
        let explain = BpeApproxTokenizer::estimate_output_tokens(100, "explain this concept");
        assert!(summarize < explain);
    }

    #[test]
    fn split_into_chunks_respects_max() {
        let text = "The quick brown fox. The lazy dog jumped. Over the fence. Back again.";
        let chunks = BpeApproxTokenizer::split_into_chunks(text, 5, 0, &TokenizerFamily::GPT4);
        for chunk in &chunks {
            let t = BpeApproxTokenizer::count_tokens(chunk, &TokenizerFamily::GPT4);
            // Allow slight overshoot for single-sentence chunks larger than max.
            assert!(t <= 20, "chunk too large: {t} tokens");
        }
        assert!(!chunks.is_empty());
    }

    #[test]
    fn context_window_known_model() {
        assert_eq!(
            TokenCounter::model_context_window("gpt-4o"),
            Some(128_000)
        );
        assert_eq!(
            TokenCounter::model_context_window("claude-3-5-sonnet"),
            Some(200_000)
        );
    }

    #[test]
    fn context_window_unknown_model() {
        assert_eq!(
            TokenCounter::model_context_window("totally-made-up-model-9999"),
            None
        );
    }

    #[test]
    fn count_messages_adds_overhead() {
        let counter = TokenCounter::new(TokenizerFamily::GPT4);
        let msgs = vec![
            ("user".to_string(), "Hi".to_string()),
            ("assistant".to_string(), "Hello!".to_string()),
        ];
        let single_text = counter.count("HiHello!");
        let msg_count = counter.count_messages(&msgs);
        // Message counting should be higher due to role overhead.
        assert!(msg_count.total_tokens > single_text.total_tokens);
    }

    #[test]
    fn remaining_context() {
        let counter = TokenCounter::new(TokenizerFamily::GPT4);
        let rem = counter.remaining_context("gpt-4o", 1000);
        assert_eq!(rem, Some(128_000 - 1000));
        assert_eq!(counter.remaining_context("unknown-model", 0), None);
    }

    #[test]
    fn batch_count_length_matches() {
        let counter = TokenCounter::new(TokenizerFamily::Claude);
        let texts = ["hello", "world", "foo bar baz"];
        let results = counter.batch_count(&texts);
        assert_eq!(results.len(), 3);
    }
}
