//! Fluent builder for prompt processing pipelines.

use std::collections::HashMap;
use std::fmt;

/// Classification of a pipeline stage's role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageKind {
    /// Normalize input (trim whitespace, change case, etc.).
    Preprocess,
    /// Validate input against constraints.
    Validate,
    /// Augment input with additional context.
    Enrich,
    /// Apply structural transformations (truncate, reformat, etc.).
    Transform,
    /// Apply finishing touches to output.
    Postprocess,
}

/// A single processing stage in a pipeline.
#[derive(Debug, Clone)]
pub struct PipelineStage {
    /// Unique human-readable name for this stage.
    pub name: String,
    /// What kind of processing this stage performs.
    pub kind: StageKind,
    /// Whether this stage participates in pipeline runs.
    pub enabled: bool,
    /// Arbitrary string→string configuration for the stage.
    pub config: HashMap<String, String>,
}

/// Result produced by running a single stage.
#[derive(Debug, Clone)]
pub struct StageResult {
    /// Name of the stage that produced this result.
    pub stage_name: String,
    /// Whether the stage altered the input string.
    pub modified: bool,
    /// The output string after stage processing.
    pub output: String,
    /// Wall-clock time taken by this stage, in microseconds.
    pub elapsed_us: u64,
    /// Arbitrary metadata emitted by the stage.
    pub metadata: HashMap<String, String>,
}

/// Errors that can occur during pipeline construction or execution.
#[derive(Debug)]
pub enum PipelineError {
    /// A specific stage produced an error.
    StageError {
        /// Name of the failing stage.
        stage: String,
        /// Human-readable reason for the failure.
        reason: String,
    },
    /// Pipeline or stage configuration is invalid.
    InvalidConfig(String),
    /// [`PipelineBuilder::build`] was called with no stages added.
    EmptyPipeline,
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::StageError { stage, reason } => {
                write!(f, "stage '{}' failed: {}", stage, reason)
            }
            PipelineError::InvalidConfig(msg) => write!(f, "invalid config: {}", msg),
            PipelineError::EmptyPipeline => write!(f, "pipeline has no stages"),
        }
    }
}

/// Full configuration for a [`Pipeline`].
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Ordered list of stages.
    pub stages: Vec<PipelineStage>,
    /// When `true`, the pipeline aborts on the first stage error.
    pub fail_fast: bool,
    /// Budget for the entire pipeline run in microseconds (not enforced in this
    /// implementation — stored for future use).
    pub max_total_us: u64,
}

/// Fluent builder for [`Pipeline`].
pub struct PipelineBuilder {
    stages: Vec<PipelineStage>,
    fail_fast: bool,
    max_total_us: u64,
}

impl PipelineBuilder {
    /// Create a new empty builder.
    pub fn new() -> Self {
        Self {
            stages: Vec::new(),
            fail_fast: false,
            max_total_us: u64::MAX,
        }
    }

    /// Append a new stage with the given name and kind.
    pub fn add_stage(mut self, name: &str, kind: StageKind) -> Self {
        self.stages.push(PipelineStage {
            name: name.to_string(),
            kind,
            enabled: true,
            config: HashMap::new(),
        });
        self
    }

    /// Set a configuration key/value pair on the most-recently added stage
    /// named `name`.  Does nothing if no stage with that name exists yet.
    pub fn configure_stage(mut self, name: &str, key: &str, value: &str) -> Self {
        if let Some(stage) = self.stages.iter_mut().rev().find(|s| s.name == name) {
            stage.config.insert(key.to_string(), value.to_string());
        }
        self
    }

    /// Disable the stage named `name`.  Does nothing if no such stage exists.
    pub fn disable_stage(mut self, name: &str) -> Self {
        if let Some(stage) = self.stages.iter_mut().find(|s| s.name == name) {
            stage.enabled = false;
        }
        self
    }

    /// Set whether the pipeline should abort on the first stage error.
    pub fn fail_fast(mut self, v: bool) -> Self {
        self.fail_fast = v;
        self
    }

    /// Consume the builder and produce a [`Pipeline`].
    /// Returns [`PipelineError::EmptyPipeline`] if no stages were added.
    pub fn build(self) -> Result<Pipeline, PipelineError> {
        if self.stages.is_empty() {
            return Err(PipelineError::EmptyPipeline);
        }
        Ok(Pipeline {
            config: PipelineConfig {
                stages: self.stages,
                fail_fast: self.fail_fast,
                max_total_us: self.max_total_us,
            },
        })
    }
}

impl Default for PipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A constructed, immutable processing pipeline.
pub struct Pipeline {
    /// The resolved configuration.
    pub config: PipelineConfig,
}

impl Pipeline {
    /// Run the pipeline on `input`, returning ordered results for each enabled stage.
    ///
    /// Stage semantics:
    /// - **Preprocess**: trim whitespace; if `config["lowercase"] == "true"`, convert to lowercase.
    /// - **Validate**: return a [`PipelineError::StageError`] if the current text is empty.
    /// - **Enrich**: prepend `config["prefix"]` (if set).
    /// - **Transform**: truncate to `config["max_len"]` characters (if set and parseable).
    /// - **Postprocess**: append `config["suffix"]` (if set).
    pub fn run(&self, input: &str) -> Result<Vec<StageResult>, PipelineError> {
        let mut current = input.to_string();
        let mut results = Vec::new();

        for stage in &self.config.stages {
            if !stage.enabled {
                continue;
            }

            let before = current.clone();
            let stage_output = apply_stage(stage, &current);

            match stage_output {
                Ok(text) => {
                    let modified = text != before;
                    current = text.clone();
                    results.push(StageResult {
                        stage_name: stage.name.clone(),
                        modified,
                        output: text,
                        elapsed_us: 0,
                        metadata: HashMap::new(),
                    });
                }
                Err(e) => {
                    if self.config.fail_fast {
                        return Err(e);
                    }
                    // On non-fail-fast, record the pre-stage text as output and continue.
                    results.push(StageResult {
                        stage_name: stage.name.clone(),
                        modified: false,
                        output: current.clone(),
                        elapsed_us: 0,
                        metadata: {
                            let mut m = HashMap::new();
                            m.insert("error".to_string(), e.to_string());
                            m
                        },
                    });
                }
            }
        }

        Ok(results)
    }
}

/// Apply a single stage's transformation to `text`.
fn apply_stage(stage: &PipelineStage, text: &str) -> Result<String, PipelineError> {
    match stage.kind {
        StageKind::Preprocess => {
            let mut out = text.trim().to_string();
            if stage.config.get("lowercase").map(|v| v == "true").unwrap_or(false) {
                out = out.to_lowercase();
            }
            Ok(out)
        }
        StageKind::Validate => {
            if text.trim().is_empty() {
                Err(PipelineError::StageError {
                    stage: stage.name.clone(),
                    reason: "input is empty".to_string(),
                })
            } else {
                Ok(text.to_string())
            }
        }
        StageKind::Enrich => {
            if let Some(prefix) = stage.config.get("prefix") {
                Ok(format!("{}{}", prefix, text))
            } else {
                Ok(text.to_string())
            }
        }
        StageKind::Transform => {
            if let Some(max_len_str) = stage.config.get("max_len") {
                match max_len_str.parse::<usize>() {
                    Ok(max_len) => Ok(text.chars().take(max_len).collect()),
                    Err(_) => Err(PipelineError::InvalidConfig(format!(
                        "stage '{}': max_len '{}' is not a valid usize",
                        stage.name, max_len_str
                    ))),
                }
            } else {
                Ok(text.to_string())
            }
        }
        StageKind::Postprocess => {
            if let Some(suffix) = stage.config.get("suffix") {
                Ok(format!("{}{}", text, suffix))
            } else {
                Ok(text.to_string())
            }
        }
    }
}

/// Return the output string of the last [`StageResult`] in `results`, or `None`
/// if the slice is empty.
pub fn last_output(results: &[StageResult]) -> Option<&str> {
    results.last().map(|r| r.output.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_three_stages_all_run() {
        let pipeline = PipelineBuilder::new()
            .add_stage("pre", StageKind::Preprocess)
            .configure_stage("pre", "lowercase", "true")
            .add_stage("validate", StageKind::Validate)
            .add_stage("post", StageKind::Postprocess)
            .configure_stage("post", "suffix", "!")
            .build()
            .unwrap();

        let results = pipeline.run("  HELLO WORLD  ").unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].output, "hello world");
        assert_eq!(results[1].output, "hello world");
        assert_eq!(results[2].output, "hello world!");
    }

    #[test]
    fn test_fail_fast_stops_on_error() {
        let pipeline = PipelineBuilder::new()
            .add_stage("validate", StageKind::Validate)
            .add_stage("post", StageKind::Postprocess)
            .configure_stage("post", "suffix", "!")
            .fail_fast(true)
            .build()
            .unwrap();

        // Empty input should fail Validate
        let err = pipeline.run("   ");
        assert!(err.is_err());
        match err.unwrap_err() {
            PipelineError::StageError { stage, .. } => assert_eq!(stage, "validate"),
            other => panic!("unexpected error: {}", other),
        }
    }

    #[test]
    fn test_disabled_stage_skipped() {
        let pipeline = PipelineBuilder::new()
            .add_stage("pre", StageKind::Preprocess)
            .configure_stage("pre", "lowercase", "true")
            .add_stage("skip_me", StageKind::Transform)
            .configure_stage("skip_me", "max_len", "3")
            .disable_stage("skip_me")
            .build()
            .unwrap();

        let results = pipeline.run("HELLO").unwrap();
        // Only "pre" ran
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].output, "hello"); // lowercase applied, NOT truncated
    }

    #[test]
    fn test_empty_pipeline_error() {
        let err = PipelineBuilder::new().build();
        assert!(matches!(err, Err(PipelineError::EmptyPipeline)));
    }

    #[test]
    fn test_lowercase_transform() {
        let pipeline = PipelineBuilder::new()
            .add_stage("pre", StageKind::Preprocess)
            .configure_stage("pre", "lowercase", "true")
            .build()
            .unwrap();

        let results = pipeline.run("FoO BaR").unwrap();
        assert_eq!(last_output(&results), Some("foo bar"));
    }

    #[test]
    fn test_enrich_prepend() {
        let pipeline = PipelineBuilder::new()
            .add_stage("enrich", StageKind::Enrich)
            .configure_stage("enrich", "prefix", ">>")
            .build()
            .unwrap();

        let results = pipeline.run("text").unwrap();
        assert_eq!(last_output(&results), Some(">>text"));
    }

    #[test]
    fn test_transform_truncate() {
        let pipeline = PipelineBuilder::new()
            .add_stage("trunc", StageKind::Transform)
            .configure_stage("trunc", "max_len", "4")
            .build()
            .unwrap();

        let results = pipeline.run("abcdefgh").unwrap();
        assert_eq!(last_output(&results), Some("abcd"));
    }
}
