//! Streaming token processor for SSE/streaming LLM responses.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Events emitted by the streaming processor.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A text token fragment.
    Token(String),
    /// A tool/function call request.
    ToolCall { name: String, arguments: String },
    /// Stream completed successfully.
    Done { total_tokens: usize },
    /// An error occurred during streaming.
    Error(String),
    /// Keepalive heartbeat (empty SSE comment).
    Heartbeat,
}

/// Bounded queue of [`StreamEvent`]s with backpressure support.
pub struct StreamBuffer {
    inner: VecDeque<StreamEvent>,
    max_size: usize,
    total_received: usize,
}

impl StreamBuffer {
    /// Create a new buffer with the given capacity.
    pub fn new(max_size: usize) -> Self {
        Self {
            inner: VecDeque::with_capacity(max_size),
            max_size,
            total_received: 0,
        }
    }

    /// Push an event into the buffer.
    ///
    /// Returns `false` (backpressure) if the buffer is already full.
    pub fn push(&mut self, event: StreamEvent) -> bool {
        if self.inner.len() >= self.max_size {
            return false;
        }
        self.total_received += 1;
        self.inner.push_back(event);
        true
    }

    /// Pop the oldest event from the buffer.
    pub fn pop(&mut self) -> Option<StreamEvent> {
        self.inner.pop_front()
    }

    /// Concatenate and remove all [`StreamEvent::Token`] events, returning the result.
    pub fn drain_tokens(&mut self) -> String {
        let mut out = String::new();
        self.inner.retain(|e| {
            if let StreamEvent::Token(t) = e {
                out.push_str(t);
                false
            } else {
                true
            }
        });
        out
    }

    /// Returns `true` if the buffer has reached its maximum size.
    pub fn is_full(&self) -> bool {
        self.inner.len() >= self.max_size
    }

    /// Number of events currently in the buffer.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the buffer contains no events.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Total number of events ever pushed (including dropped ones).
    pub fn total_received(&self) -> usize {
        self.total_received
    }
}

/// Result produced by [`TokenAccumulator::finish`].
#[derive(Debug, Clone)]
pub struct AccumulatorResult {
    /// Full accumulated text.
    pub text: String,
    /// Number of tokens accumulated.
    pub token_count: usize,
    /// Wall-clock duration from first to last token, in milliseconds.
    pub duration_ms: u64,
    /// Tokens per second (0 if no duration elapsed).
    pub tokens_per_second: f64,
}

/// Accumulates token fragments and tracks timing statistics.
pub struct TokenAccumulator {
    accumulated: String,
    token_count: usize,
    char_count: usize,
    first_token_at: Option<Instant>,
    last_token_at: Option<Instant>,
}

impl TokenAccumulator {
    /// Create a new, empty accumulator.
    pub fn new() -> Self {
        Self {
            accumulated: String::new(),
            token_count: 0,
            char_count: 0,
            first_token_at: None,
            last_token_at: None,
        }
    }

    /// Append a token fragment.
    pub fn push_token(&mut self, token: &str) {
        let now = Instant::now();
        if self.first_token_at.is_none() {
            self.first_token_at = Some(now);
        }
        self.last_token_at = Some(now);
        self.accumulated.push_str(token);
        self.token_count += 1;
        self.char_count += token.len();
    }

    /// Duration from the very first token to the very first token (i.e. always `Some(0)` after
    /// the first push, but semantically "time to first token" measured at call site).
    ///
    /// Returns `None` if no tokens have been pushed yet.
    pub fn time_to_first_token(&self) -> Option<Duration> {
        self.first_token_at.map(|t| t.elapsed())
    }

    /// Tokens per second calculated over the full accumulation window.
    ///
    /// Returns `0.0` if fewer than two tokens were pushed or no time has elapsed.
    pub fn tokens_per_second(&self) -> f64 {
        match (self.first_token_at, self.last_token_at) {
            (Some(first), Some(last)) => {
                let secs = last.duration_since(first).as_secs_f64();
                if secs > 0.0 {
                    self.token_count as f64 / secs
                } else {
                    0.0
                }
            }
            _ => 0.0,
        }
    }

    /// Snapshot the current accumulation state as an [`AccumulatorResult`].
    pub fn finish(&self) -> AccumulatorResult {
        let duration_ms = match (self.first_token_at, self.last_token_at) {
            (Some(first), Some(last)) => last.duration_since(first).as_millis() as u64,
            _ => 0,
        };
        AccumulatorResult {
            text: self.accumulated.clone(),
            token_count: self.token_count,
            duration_ms,
            tokens_per_second: self.tokens_per_second(),
        }
    }

    /// Reference to the accumulated text so far.
    pub fn text(&self) -> &str {
        &self.accumulated
    }

    /// Number of tokens pushed so far.
    pub fn token_count(&self) -> usize {
        self.token_count
    }

    /// Total characters accumulated.
    pub fn char_count(&self) -> usize {
        self.char_count
    }
}

impl Default for TokenAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Running statistics for a [`StreamingProcessor`].
#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    /// Total SSE events processed.
    pub events_processed: usize,
    /// Total token fragments received.
    pub tokens_received: usize,
    /// Total tool-call events seen.
    pub tool_calls_seen: usize,
    /// Total error events seen.
    pub errors_seen: usize,
    /// Number of times the buffer was full (backpressure activations).
    pub buffer_full_count: usize,
}

/// Parses SSE chunks from LLM streaming endpoints, accumulates tokens, and
/// maintains a bounded [`StreamBuffer`] with backpressure.
pub struct StreamingProcessor {
    buffer: StreamBuffer,
    accumulator: TokenAccumulator,
    stats: StreamStats,
}

impl StreamingProcessor {
    /// Create a new processor with the given buffer capacity.
    pub fn new(buffer_size: usize) -> Self {
        Self {
            buffer: StreamBuffer::new(buffer_size),
            accumulator: TokenAccumulator::new(),
            stats: StreamStats::default(),
        }
    }

    /// Parse a raw SSE chunk and return the extracted events.
    ///
    /// Handles:
    /// - Lines prefixed with `"data: "`
    /// - The `"[DONE]"` sentinel
    /// - JSON payloads with `choices[0].delta.content` (OpenAI-style)
    pub fn process_chunk(&mut self, raw: &str) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // SSE comment → heartbeat
            if line.starts_with(':') {
                events.push(StreamEvent::Heartbeat);
                continue;
            }
            // Strip "data: " prefix
            let data = if let Some(rest) = line.strip_prefix("data: ") {
                rest.trim()
            } else {
                continue;
            };

            // [DONE] sentinel
            if data == "[DONE]" {
                let total = self.accumulator.token_count();
                events.push(StreamEvent::Done { total_tokens: total });
                continue;
            }

            // Try to parse as JSON
            match serde_json::from_str::<serde_json::Value>(data) {
                Ok(json) => {
                    // Extract delta content (OpenAI-style)
                    if let Some(content) = json
                        .pointer("/choices/0/delta/content")
                        .and_then(|v| v.as_str())
                    {
                        if !content.is_empty() {
                            events.push(StreamEvent::Token(content.to_string()));
                        }
                    }

                    // Tool calls embedded in delta
                    let tool_events = Self::extract_tool_calls(data);
                    for (name, arguments) in tool_events {
                        events.push(StreamEvent::ToolCall { name, arguments });
                    }

                    // finish_reason → Done
                    if let Some(reason) = Self::detect_finish_reason(data) {
                        if reason == "stop" || reason == "length" {
                            let total = self.accumulator.token_count();
                            events.push(StreamEvent::Done { total_tokens: total });
                        }
                    }
                }
                Err(e) => {
                    events.push(StreamEvent::Error(format!("JSON parse error: {}", e)));
                }
            }
        }

        // Feed events into buffer and accumulator
        for event in &events {
            self.stats.events_processed += 1;
            match event {
                StreamEvent::Token(t) => {
                    self.accumulator.push_token(t);
                    self.stats.tokens_received += 1;
                }
                StreamEvent::ToolCall { .. } => self.stats.tool_calls_seen += 1,
                StreamEvent::Error(_) => self.stats.errors_seen += 1,
                _ => {}
            }
            if !self.buffer.push(event.clone()) {
                self.stats.buffer_full_count += 1;
            }
        }

        events
    }

    /// Extract tool/function call blocks from a raw JSON string.
    ///
    /// Returns a list of `(name, arguments)` pairs.
    pub fn extract_tool_calls(raw: &str) -> Vec<(String, String)> {
        let mut results = Vec::new();
        let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) else {
            return results;
        };

        // OpenAI style: choices[0].delta.tool_calls[]
        if let Some(tool_calls) = json.pointer("/choices/0/delta/tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                let name = tc
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = tc
                    .pointer("/function/arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}")
                    .to_string();
                if !name.is_empty() {
                    results.push((name, arguments));
                }
            }
        }

        // Anthropic style: type == "tool_use"
        if json.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            let name = json.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let arguments = json
                .get("input")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "{}".to_string());
            if !name.is_empty() {
                results.push((name, arguments));
            }
        }

        results
    }

    /// Detect the finish reason from a raw JSON SSE payload.
    ///
    /// Recognises `"stop"`, `"length"`, and `"tool_calls"`.
    pub fn detect_finish_reason(raw: &str) -> Option<String> {
        let json = serde_json::from_str::<serde_json::Value>(raw).ok()?;
        let reason = json
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())?;
        match reason {
            "stop" | "length" | "tool_calls" => Some(reason.to_string()),
            _ => None,
        }
    }

    /// Feed a batch of pre-parsed events into the accumulator and return a reference to it.
    pub fn accumulate(&mut self, events: &[StreamEvent]) -> &TokenAccumulator {
        for event in events {
            if let StreamEvent::Token(t) = event {
                self.accumulator.push_token(t);
            }
        }
        &self.accumulator
    }

    /// Current processor statistics.
    pub fn stats(&self) -> StreamStats {
        self.stats.clone()
    }

    /// Reference to the internal buffer.
    pub fn buffer(&self) -> &StreamBuffer {
        &self.buffer
    }

    /// Mutable reference to the internal buffer.
    pub fn buffer_mut(&mut self) -> &mut StreamBuffer {
        &mut self.buffer
    }

    /// Reference to the token accumulator.
    pub fn accumulator(&self) -> &TokenAccumulator {
        &self.accumulator
    }
}
