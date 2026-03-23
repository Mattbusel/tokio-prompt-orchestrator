//! # Prompt Pipeline
//!
//! A composable, ordered sequence of text-transformation stages.  Each stage
//! receives the previous stage's output as its input.  Stages are async and
//! may be composed with [`PipelineBuilder`].
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::pipeline::{PipelineBuilder, TrimStage, TruncateStage};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let pipeline = PipelineBuilder::new()
//!     .add(TrimStage)
//!     .add(TruncateStage { max_chars: 100 })
//!     .build();
//!
//! let result = pipeline.run("  hello world  ".to_string()).await.unwrap();
//! assert_eq!(result.output, "hello world");
//! # }
//! ```

use async_trait::async_trait;
use regex::Regex;
use std::time::Instant;
use thiserror::Error;

// ── Error ─────────────────────────────────────────────────────────────────────

/// Errors produced by pipeline stages.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// A stage produced an I/O or processing error.
    #[error("stage '{stage}' failed: {message}")]
    StageError {
        /// Name of the stage that failed.
        stage: String,
        /// Human-readable description of the failure.
        message: String,
    },

    /// A regex pattern provided to [`RegexReplaceStage`] was invalid.
    #[error("invalid regex pattern '{pattern}': {source}")]
    InvalidRegex {
        /// The offending pattern string.
        pattern: String,
        /// The underlying regex compile error.
        #[source]
        source: regex::Error,
    },
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// A single processing stage in a [`Pipeline`].
///
/// Implementors transform an input string and return the (possibly mutated)
/// result.  Stages are async to allow future network-based transformations
/// (e.g. embedding lookups, remote filters) without blocking the Tokio runtime.
#[async_trait]
pub trait PipelineStage: Send + Sync {
    /// Process `input` and return the transformed string.
    async fn process(&self, input: String) -> Result<String, PipelineError>;

    /// A short human-readable name used in error messages and statistics.
    fn name(&self) -> &str;
}

// ── Built-in stages ───────────────────────────────────────────────────────────

/// Strips leading and trailing ASCII whitespace from the input.
pub struct TrimStage;

#[async_trait]
impl PipelineStage for TrimStage {
    async fn process(&self, input: String) -> Result<String, PipelineError> {
        Ok(input.trim().to_string())
    }

    fn name(&self) -> &str {
        "TrimStage"
    }
}

/// Truncates the input to at most `max_chars` characters, breaking at the last
/// word boundary that fits within the limit.
///
/// If `max_chars` is zero the output is always an empty string.
pub struct TruncateStage {
    /// Maximum number of characters to keep.
    pub max_chars: usize,
}

#[async_trait]
impl PipelineStage for TruncateStage {
    async fn process(&self, input: String) -> Result<String, PipelineError> {
        if input.len() <= self.max_chars {
            return Ok(input);
        }
        if self.max_chars == 0 {
            return Ok(String::new());
        }
        // Find the last whitespace boundary within max_chars.
        let truncated = &input[..self.max_chars];
        let result = match truncated.rfind(|c: char| c.is_whitespace()) {
            Some(pos) if pos > 0 => &truncated[..pos],
            _ => truncated, // No word boundary found; hard-cut.
        };
        Ok(result.to_string())
    }

    fn name(&self) -> &str {
        "TruncateStage"
    }
}

/// Prepends a fixed string (e.g. a system-prompt prefix) to the input.
pub struct PrependStage {
    /// The string to prepend.
    pub prefix: String,
}

#[async_trait]
impl PipelineStage for PrependStage {
    async fn process(&self, input: String) -> Result<String, PipelineError> {
        Ok(format!("{}{}", self.prefix, input))
    }

    fn name(&self) -> &str {
        "PrependStage"
    }
}

/// Appends a fixed string (e.g. a context suffix or citation block) to the input.
pub struct AppendStage {
    /// The string to append.
    pub suffix: String,
}

#[async_trait]
impl PipelineStage for AppendStage {
    async fn process(&self, input: String) -> Result<String, PipelineError> {
        Ok(format!("{}{}", input, self.suffix))
    }

    fn name(&self) -> &str {
        "AppendStage"
    }
}

/// Performs a regex substitution on the input.
///
/// The `pattern` is compiled once at construction time and reused for every
/// call to [`process`](PipelineStage::process).
#[derive(Debug)]
pub struct RegexReplaceStage {
    /// The compiled regex pattern.
    compiled: Regex,
    /// Original pattern string, kept for error messages.
    pattern_str: String,
    /// Replacement string (supports `$1`, `${name}` capture group references).
    pub replacement: String,
}

impl RegexReplaceStage {
    /// Construct a new stage, compiling `pattern` immediately.
    ///
    /// Returns [`PipelineError::InvalidRegex`] if the pattern is invalid.
    pub fn new(pattern: &str, replacement: &str) -> Result<Self, PipelineError> {
        let compiled = Regex::new(pattern).map_err(|e| PipelineError::InvalidRegex {
            pattern: pattern.to_string(),
            source: e,
        })?;
        Ok(Self {
            compiled,
            pattern_str: pattern.to_string(),
            replacement: replacement.to_string(),
        })
    }

    /// Return the original pattern string.
    pub fn pattern(&self) -> &str {
        &self.pattern_str
    }
}

#[async_trait]
impl PipelineStage for RegexReplaceStage {
    async fn process(&self, input: String) -> Result<String, PipelineError> {
        Ok(self.compiled.replace_all(&input, self.replacement.as_str()).to_string())
    }

    fn name(&self) -> &str {
        "RegexReplaceStage"
    }
}

/// Detects whether the text is predominantly ASCII/Latin script or uses other
/// Unicode scripts, then prepends a `[lang:en]` or `[lang:other]` tag.
///
/// The heuristic counts bytes: if ≥ 80 % of the non-whitespace characters are
/// in the ASCII range (0x00–0x7F) the text is labelled `en`, otherwise `other`.
pub struct LanguageDetectStage;

#[async_trait]
impl PipelineStage for LanguageDetectStage {
    async fn process(&self, input: String) -> Result<String, PipelineError> {
        let non_ws: Vec<char> = input.chars().filter(|c| !c.is_whitespace()).collect();
        let tag = if non_ws.is_empty() {
            "en"
        } else {
            let ascii_count = non_ws.iter().filter(|c| c.is_ascii()).count();
            let ratio = ascii_count as f64 / non_ws.len() as f64;
            if ratio >= 0.80 { "en" } else { "other" }
        };
        Ok(format!("[lang:{}] {}", tag, input))
    }

    fn name(&self) -> &str {
        "LanguageDetectStage"
    }
}

// ── Pipeline stats ─────────────────────────────────────────────────────────────

/// Timing and length statistics for a single [`Pipeline::run`] call.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineStats {
    /// Number of stages that were executed (including those that were no-ops).
    pub stages_run: usize,
    /// Length of the original input string in bytes.
    pub input_len: usize,
    /// Length of the final output string in bytes.
    pub output_len: usize,
    /// Wall-clock time taken by the entire pipeline in milliseconds.
    pub elapsed_ms: u64,
}

// ── Pipeline run result ────────────────────────────────────────────────────────

/// The result of a successful [`Pipeline::run`] call.
#[derive(Debug, Clone)]
pub struct PipelineResult {
    /// The final transformed string.
    pub output: String,
    /// Execution statistics.
    pub stats: PipelineStats,
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

/// An ordered sequence of [`PipelineStage`]s.
///
/// Each stage's output is fed as the next stage's input.  Build pipelines with
/// [`PipelineBuilder`].
pub struct Pipeline {
    stages: Vec<Box<dyn PipelineStage>>,
}

impl Pipeline {
    /// Execute all stages in order, returning the transformed string and
    /// execution statistics.
    ///
    /// Fails fast: if any stage returns an error the pipeline stops and
    /// propagates the error.
    pub async fn run(&self, input: String) -> Result<PipelineResult, PipelineError> {
        let start = Instant::now();
        let input_len = input.len();
        let mut current = input;

        for stage in &self.stages {
            current = stage.process(current).await?;
        }

        let elapsed_ms = start.elapsed().as_millis() as u64;
        let output_len = current.len();
        Ok(PipelineResult {
            output: current,
            stats: PipelineStats {
                stages_run: self.stages.len(),
                input_len,
                output_len,
                elapsed_ms,
            },
        })
    }

    /// Return the number of stages in this pipeline.
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Fluent builder for constructing a [`Pipeline`].
///
/// ## Example
///
/// ```rust
/// use tokio_prompt_orchestrator::pipeline::{PipelineBuilder, TrimStage, PrependStage};
///
/// let pipeline = PipelineBuilder::new()
///     .add(TrimStage)
///     .add(PrependStage { prefix: "System: ".to_string() })
///     .build();
/// ```
#[derive(Default)]
pub struct PipelineBuilder {
    stages: Vec<Box<dyn PipelineStage>>,
}

impl PipelineBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self { stages: Vec::new() }
    }

    /// Append a stage to the pipeline.
    pub fn add<S: PipelineStage + 'static>(mut self, stage: S) -> Self {
        self.stages.push(Box::new(stage));
        self
    }

    /// Consume the builder and return the configured [`Pipeline`].
    pub fn build(self) -> Pipeline {
        Pipeline { stages: self.stages }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TrimStage ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn trim_stage_removes_leading_whitespace() {
        let s = TrimStage;
        let out = s.process("   hello".to_string()).await.unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn trim_stage_removes_trailing_whitespace() {
        let s = TrimStage;
        let out = s.process("hello   ".to_string()).await.unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn trim_stage_empty_string() {
        let s = TrimStage;
        let out = s.process("   ".to_string()).await.unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn trim_stage_no_op_when_already_trimmed() {
        let s = TrimStage;
        let out = s.process("hello world".to_string()).await.unwrap();
        assert_eq!(out, "hello world");
    }

    // ── TruncateStage ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn truncate_stage_short_input_unchanged() {
        let s = TruncateStage { max_chars: 100 };
        let out = s.process("hello".to_string()).await.unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn truncate_stage_breaks_at_word_boundary() {
        let s = TruncateStage { max_chars: 10 };
        let out = s.process("hello world foo".to_string()).await.unwrap();
        // "hello worl" -> last space at index 5 -> "hello"
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn truncate_stage_zero_max_chars() {
        let s = TruncateStage { max_chars: 0 };
        let out = s.process("hello".to_string()).await.unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn truncate_stage_exact_length() {
        let s = TruncateStage { max_chars: 5 };
        let out = s.process("hello".to_string()).await.unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn truncate_stage_no_word_boundary_hard_cuts() {
        let s = TruncateStage { max_chars: 3 };
        let out = s.process("hello".to_string()).await.unwrap();
        assert_eq!(out, "hel");
    }

    // ── PrependStage ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn prepend_stage_adds_prefix() {
        let s = PrependStage { prefix: "System: ".to_string() };
        let out = s.process("hello".to_string()).await.unwrap();
        assert_eq!(out, "System: hello");
    }

    #[tokio::test]
    async fn prepend_stage_empty_prefix() {
        let s = PrependStage { prefix: String::new() };
        let out = s.process("hello".to_string()).await.unwrap();
        assert_eq!(out, "hello");
    }

    // ── AppendStage ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn append_stage_adds_suffix() {
        let s = AppendStage { suffix: " [END]".to_string() };
        let out = s.process("hello".to_string()).await.unwrap();
        assert_eq!(out, "hello [END]");
    }

    #[tokio::test]
    async fn append_stage_empty_suffix() {
        let s = AppendStage { suffix: String::new() };
        let out = s.process("hello".to_string()).await.unwrap();
        assert_eq!(out, "hello");
    }

    // ── RegexReplaceStage ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn regex_replace_stage_basic_substitution() {
        let s = RegexReplaceStage::new(r"\bfoo\b", "bar").unwrap();
        let out = s.process("foo is foo".to_string()).await.unwrap();
        assert_eq!(out, "bar is bar");
    }

    #[tokio::test]
    async fn regex_replace_stage_no_match_unchanged() {
        let s = RegexReplaceStage::new(r"\bxyz\b", "abc").unwrap();
        let out = s.process("hello world".to_string()).await.unwrap();
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn regex_replace_stage_invalid_pattern_errors() {
        let result = RegexReplaceStage::new(r"[invalid", "x");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PipelineError::InvalidRegex { .. }));
    }

    #[tokio::test]
    async fn regex_replace_stage_capture_group_replacement() {
        let s = RegexReplaceStage::new(r"(\w+)\s+(\w+)", "$2 $1").unwrap();
        let out = s.process("hello world".to_string()).await.unwrap();
        assert_eq!(out, "world hello");
    }

    // ── LanguageDetectStage ───────────────────────────────────────────────────

    #[tokio::test]
    async fn language_detect_ascii_text_tagged_en() {
        let s = LanguageDetectStage;
        let out = s.process("Hello world".to_string()).await.unwrap();
        assert!(out.starts_with("[lang:en]"));
    }

    #[tokio::test]
    async fn language_detect_other_script_tagged_other() {
        let s = LanguageDetectStage;
        // Cyrillic text
        let out = s.process("Привет мир".to_string()).await.unwrap();
        assert!(out.starts_with("[lang:other]"));
    }

    #[tokio::test]
    async fn language_detect_empty_string_tagged_en() {
        let s = LanguageDetectStage;
        let out = s.process(String::new()).await.unwrap();
        assert!(out.starts_with("[lang:en]"));
    }

    // ── Pipeline ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn pipeline_runs_stages_in_order() {
        let pipeline = PipelineBuilder::new()
            .add(TrimStage)
            .add(PrependStage { prefix: ">>".to_string() })
            .build();

        let result = pipeline.run("  hello  ".to_string()).await.unwrap();
        assert_eq!(result.output, ">>hello");
    }

    #[tokio::test]
    async fn pipeline_stats_stage_count() {
        let pipeline = PipelineBuilder::new()
            .add(TrimStage)
            .add(AppendStage { suffix: "!".to_string() })
            .build();

        let result = pipeline.run("hi".to_string()).await.unwrap();
        assert_eq!(result.stats.stages_run, 2);
    }

    #[tokio::test]
    async fn pipeline_stats_input_output_len() {
        let pipeline = PipelineBuilder::new()
            .add(AppendStage { suffix: "XYZ".to_string() })
            .build();

        let result = pipeline.run("hello".to_string()).await.unwrap();
        assert_eq!(result.stats.input_len, 5);
        assert_eq!(result.stats.output_len, 8);
    }

    #[tokio::test]
    async fn pipeline_empty_no_stages() {
        let pipeline = PipelineBuilder::new().build();
        let result = pipeline.run("hello".to_string()).await.unwrap();
        assert_eq!(result.output, "hello");
        assert_eq!(result.stats.stages_run, 0);
    }

    #[tokio::test]
    async fn pipeline_full_chain() {
        let regex_stage = RegexReplaceStage::new(r"\bLLM\b", "AI model").unwrap();
        let pipeline = PipelineBuilder::new()
            .add(TrimStage)
            .add(TruncateStage { max_chars: 200 })
            .add(PrependStage { prefix: "Context: ".to_string() })
            .add(AppendStage { suffix: " [end]".to_string() })
            .add(regex_stage)
            .build();

        let input = "  Ask the LLM to summarize.  ";
        let result = pipeline.run(input.to_string()).await.unwrap();
        assert!(result.output.contains("AI model"));
        assert!(result.output.starts_with("Context: "));
        assert!(result.output.ends_with("[end]"));
    }

    #[tokio::test]
    async fn pipeline_builder_stage_count() {
        let pipeline = PipelineBuilder::new()
            .add(TrimStage)
            .add(TrimStage)
            .add(TrimStage)
            .build();
        assert_eq!(pipeline.stage_count(), 3);
    }
}
