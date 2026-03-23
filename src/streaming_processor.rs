//! Streaming LLM response processor — token-by-token accumulation, transforms, filters, stats.

/// Reason the stream finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// Normal end-of-response.
    Stop,
    /// Model hit its maximum token limit.
    Length,
    /// Response was blocked by a content policy.
    ContentFilter,
    /// Model requested a function / tool call.
    FunctionCall,
    /// An error occurred during generation.
    Error,
}

/// A single token emitted by the LLM streaming endpoint.
#[derive(Debug, Clone)]
pub struct StreamToken {
    /// Text content of this token.
    pub content: String,
    /// Reason the stream finished, if this is the final token.
    pub finish_reason: Option<FinishReason>,
    /// Zero-based sequential index of this token in the stream.
    pub index: u64,
    /// Wall-clock timestamp at which this token was received (ms since epoch).
    pub timestamp_ms: u64,
}

/// Current state of a stream.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamState {
    /// Stream is still producing tokens.
    Active,
    /// Stream ended cleanly.
    Completed(FinishReason),
    /// Stream ended with an error.
    Error(String),
}

/// Accumulates tokens and emits complete sentences / paragraphs.
#[derive(Debug)]
pub struct StreamBuffer {
    buffer: String,
    total: u64,
    state: StreamState,
}

impl StreamBuffer {
    /// Create a new, empty buffer.
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            total: 0,
            state: StreamState::Active,
        }
    }

    /// Push a token into the buffer.
    ///
    /// Returns any complete sentences (ending with `.`, `!`, or `?`) that were
    /// flushed as a result of this push.  Updates the internal state when
    /// `finish_reason` is set.
    pub fn push(&mut self, token: StreamToken) -> Vec<String> {
        self.total += 1;

        // Update state from finish reason before processing content.
        if let Some(ref reason) = token.finish_reason {
            self.state = StreamState::Completed(reason.clone());
        }

        self.buffer.push_str(&token.content);

        let mut completed: Vec<String> = Vec::new();
        loop {
            // Find the earliest sentence terminator.
            let maybe_pos = self.buffer.char_indices().find_map(|(i, c)| {
                if c == '.' || c == '!' || c == '?' {
                    Some(i + c.len_utf8())
                } else {
                    None
                }
            });
            match maybe_pos {
                Some(end) => {
                    let sentence = self.buffer[..end].trim().to_string();
                    self.buffer = self.buffer[end..].trim_start().to_string();
                    if !sentence.is_empty() {
                        completed.push(sentence);
                    }
                }
                None => break,
            }
        }
        completed
    }

    /// Flush any remaining buffered content (may not end with a terminator).
    pub fn flush(&mut self) -> String {
        let remaining = self.buffer.trim().to_string();
        self.buffer.clear();
        remaining
    }

    /// Total number of tokens pushed so far.
    pub fn total_tokens(&self) -> u64 {
        self.total
    }

    /// Current stream state.
    pub fn state(&self) -> &StreamState {
        &self.state
    }
}

/// Transformations that can be applied to a token's content.
#[derive(Debug, Clone)]
pub enum StreamTransform {
    /// Convert content to UPPERCASE.
    Uppercase,
    /// Convert content to lowercase.
    Lowercase,
    /// Replace occurrences of a regex-like literal pattern with `[REDACTED]`.
    RedactPattern(String),
    /// Truncate content to at most `n` bytes.
    Truncate(usize),
    /// Prepend a fixed prefix to the content.
    PrependPrefix(String),
}

/// Filters that determine whether a token should pass downstream.
#[derive(Debug, Clone)]
pub enum StreamFilter {
    /// Drop tokens whose `index` is >= the given limit.
    MaxTokens(u64),
    /// Drop tokens whose content contains the given keyword (case-insensitive).
    BlockKeyword(String),
    /// Drop tokens whose content length (in bytes) is < the given minimum.
    MinLength(usize),
}

/// Aggregate statistics computed over a slice of tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamStats {
    /// Total number of tokens.
    pub total_tokens: u64,
    /// Total character count across all tokens.
    pub total_chars: u64,
    /// Average characters per token.
    pub avg_chars_per_token: f64,
    /// Duration from first to last token in milliseconds.
    pub duration_ms: u64,
    /// Tokens per second (0 if duration is 0).
    pub tokens_per_second: f64,
}

/// Stateless processor for applying transforms, filters, and computing stats.
pub struct StreamingProcessor;

impl StreamingProcessor {
    /// Apply `transforms` in order to produce a new [`StreamToken`].
    pub fn transform(token: &StreamToken, transforms: &[StreamTransform]) -> StreamToken {
        let mut content = token.content.clone();
        for t in transforms {
            content = match t {
                StreamTransform::Uppercase => content.to_uppercase(),
                StreamTransform::Lowercase => content.to_lowercase(),
                StreamTransform::RedactPattern(pat) => content.replace(pat.as_str(), "[REDACTED]"),
                StreamTransform::Truncate(n) => {
                    if content.len() > *n {
                        content[..*n].to_string()
                    } else {
                        content
                    }
                }
                StreamTransform::PrependPrefix(prefix) => format!("{}{}", prefix, content),
            };
        }
        StreamToken {
            content,
            finish_reason: token.finish_reason.clone(),
            index: token.index,
            timestamp_ms: token.timestamp_ms,
        }
    }

    /// Return `true` if the token passes **all** filters.
    pub fn filter(token: &StreamToken, filters: &[StreamFilter]) -> bool {
        for f in filters {
            let pass = match f {
                StreamFilter::MaxTokens(limit) => token.index < *limit,
                StreamFilter::BlockKeyword(kw) => {
                    !token.content.to_lowercase().contains(&kw.to_lowercase())
                }
                StreamFilter::MinLength(min) => token.content.len() >= *min,
            };
            if !pass {
                return false;
            }
        }
        true
    }

    /// Compute aggregate statistics over a slice of tokens.
    pub fn aggregate_stats(tokens: &[StreamToken]) -> StreamStats {
        if tokens.is_empty() {
            return StreamStats {
                total_tokens: 0,
                total_chars: 0,
                avg_chars_per_token: 0.0,
                duration_ms: 0,
                tokens_per_second: 0.0,
            };
        }

        let total_tokens = tokens.len() as u64;
        let total_chars: u64 = tokens.iter().map(|t| t.content.len() as u64).sum();
        let avg_chars_per_token = total_chars as f64 / total_tokens as f64;

        let min_ts = tokens.iter().map(|t| t.timestamp_ms).min().unwrap_or(0);
        let max_ts = tokens.iter().map(|t| t.timestamp_ms).max().unwrap_or(0);
        let duration_ms = max_ts.saturating_sub(min_ts);

        let tokens_per_second = if duration_ms > 0 {
            total_tokens as f64 / (duration_ms as f64 / 1000.0)
        } else {
            0.0
        };

        StreamStats {
            total_tokens,
            total_chars,
            avg_chars_per_token,
            duration_ms,
            tokens_per_second,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(content: &str, index: u64, ts: u64) -> StreamToken {
        StreamToken {
            content: content.to_string(),
            finish_reason: None,
            index,
            timestamp_ms: ts,
        }
    }

    fn tok_final(content: &str, index: u64, ts: u64, reason: FinishReason) -> StreamToken {
        StreamToken {
            content: content.to_string(),
            finish_reason: Some(reason),
            index,
            timestamp_ms: ts,
        }
    }

    // --- StreamBuffer ---

    #[test]
    fn buffer_emits_sentence_on_period() {
        let mut buf = StreamBuffer::new();
        let out = buf.push(tok("Hello world.", 0, 0));
        assert_eq!(out, vec!["Hello world."]);
        assert_eq!(buf.total_tokens(), 1);
    }

    #[test]
    fn buffer_emits_multiple_sentences() {
        let mut buf = StreamBuffer::new();
        let out = buf.push(tok("Hi. Bye!", 0, 0));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], "Hi.");
        assert_eq!(out[1], "Bye!");
    }

    #[test]
    fn buffer_accumulates_partial() {
        let mut buf = StreamBuffer::new();
        let out1 = buf.push(tok("Hello", 0, 0));
        assert!(out1.is_empty());
        let out2 = buf.push(tok(" world.", 1, 1));
        assert_eq!(out2, vec!["Hello world."]);
    }

    #[test]
    fn buffer_flush_returns_remainder() {
        let mut buf = StreamBuffer::new();
        buf.push(tok("Incomplete sentence", 0, 0));
        let rem = buf.flush();
        assert_eq!(rem, "Incomplete sentence");
        assert_eq!(buf.flush(), "");
    }

    #[test]
    fn buffer_tracks_state_on_finish() {
        let mut buf = StreamBuffer::new();
        buf.push(tok_final("Done.", 0, 0, FinishReason::Stop));
        assert_eq!(buf.state(), &StreamState::Completed(FinishReason::Stop));
    }

    #[test]
    fn buffer_question_mark_terminates() {
        let mut buf = StreamBuffer::new();
        let out = buf.push(tok("Really?", 0, 0));
        assert_eq!(out, vec!["Really?"]);
    }

    // --- StreamingProcessor::transform ---

    #[test]
    fn transform_uppercase() {
        let t = tok("hello", 0, 0);
        let out = StreamingProcessor::transform(&t, &[StreamTransform::Uppercase]);
        assert_eq!(out.content, "HELLO");
    }

    #[test]
    fn transform_lowercase() {
        let t = tok("WORLD", 0, 0);
        let out = StreamingProcessor::transform(&t, &[StreamTransform::Lowercase]);
        assert_eq!(out.content, "world");
    }

    #[test]
    fn transform_redact() {
        let t = tok("my secret password123", 0, 0);
        let out = StreamingProcessor::transform(
            &t,
            &[StreamTransform::RedactPattern("password123".to_string())],
        );
        assert_eq!(out.content, "my secret [REDACTED]");
    }

    #[test]
    fn transform_truncate() {
        let t = tok("abcdef", 0, 0);
        let out = StreamingProcessor::transform(&t, &[StreamTransform::Truncate(3)]);
        assert_eq!(out.content, "abc");
    }

    #[test]
    fn transform_prepend_prefix() {
        let t = tok("world", 0, 0);
        let out =
            StreamingProcessor::transform(&t, &[StreamTransform::PrependPrefix("hello ".to_string())]);
        assert_eq!(out.content, "hello world");
    }

    #[test]
    fn transform_chained() {
        let t = tok("hello", 0, 0);
        let out = StreamingProcessor::transform(
            &t,
            &[
                StreamTransform::Uppercase,
                StreamTransform::PrependPrefix("[A] ".to_string()),
            ],
        );
        assert_eq!(out.content, "[A] HELLO");
    }

    #[test]
    fn transform_preserves_metadata() {
        let t = tok_final("hi", 5, 999, FinishReason::Length);
        let out = StreamingProcessor::transform(&t, &[StreamTransform::Uppercase]);
        assert_eq!(out.index, 5);
        assert_eq!(out.timestamp_ms, 999);
        assert_eq!(out.finish_reason, Some(FinishReason::Length));
    }

    // --- StreamingProcessor::filter ---

    #[test]
    fn filter_max_tokens_passes() {
        let t = tok("hi", 4, 0);
        assert!(StreamingProcessor::filter(&t, &[StreamFilter::MaxTokens(5)]));
    }

    #[test]
    fn filter_max_tokens_blocks() {
        let t = tok("hi", 5, 0);
        assert!(!StreamingProcessor::filter(&t, &[StreamFilter::MaxTokens(5)]));
    }

    #[test]
    fn filter_block_keyword() {
        let t = tok("buy cheap drugs now", 0, 0);
        assert!(!StreamingProcessor::filter(
            &t,
            &[StreamFilter::BlockKeyword("drugs".to_string())]
        ));
    }

    #[test]
    fn filter_block_keyword_case_insensitive() {
        let t = tok("DRUGS are bad", 0, 0);
        assert!(!StreamingProcessor::filter(
            &t,
            &[StreamFilter::BlockKeyword("drugs".to_string())]
        ));
    }

    #[test]
    fn filter_min_length_passes() {
        let t = tok("hello", 0, 0);
        assert!(StreamingProcessor::filter(&t, &[StreamFilter::MinLength(3)]));
    }

    #[test]
    fn filter_min_length_blocks() {
        let t = tok("hi", 0, 0);
        assert!(!StreamingProcessor::filter(&t, &[StreamFilter::MinLength(3)]));
    }

    #[test]
    fn filter_all_must_pass() {
        let t = tok("hello world", 2, 0);
        // passes MaxTokens(10) and MinLength(5), but blocked by keyword
        let filters = vec![
            StreamFilter::MaxTokens(10),
            StreamFilter::BlockKeyword("world".to_string()),
            StreamFilter::MinLength(5),
        ];
        assert!(!StreamingProcessor::filter(&t, &filters));
    }

    // --- StreamingProcessor::aggregate_stats ---

    #[test]
    fn stats_empty() {
        let stats = StreamingProcessor::aggregate_stats(&[]);
        assert_eq!(stats.total_tokens, 0);
        assert_eq!(stats.total_chars, 0);
        assert_eq!(stats.duration_ms, 0);
        assert_eq!(stats.tokens_per_second, 0.0);
    }

    #[test]
    fn stats_single_token() {
        let stats = StreamingProcessor::aggregate_stats(&[tok("hello", 0, 1000)]);
        assert_eq!(stats.total_tokens, 1);
        assert_eq!(stats.total_chars, 5);
        assert_eq!(stats.avg_chars_per_token, 5.0);
        assert_eq!(stats.duration_ms, 0);
    }

    #[test]
    fn stats_multiple() {
        let tokens = vec![
            tok("hi", 0, 0),
            tok("bye", 1, 1000),
            tok("ok", 2, 2000),
        ];
        let stats = StreamingProcessor::aggregate_stats(&tokens);
        assert_eq!(stats.total_tokens, 3);
        assert_eq!(stats.total_chars, 7);
        assert!((stats.avg_chars_per_token - 7.0 / 3.0).abs() < 1e-9);
        assert_eq!(stats.duration_ms, 2000);
        assert!((stats.tokens_per_second - 1.5).abs() < 1e-9);
    }
}
