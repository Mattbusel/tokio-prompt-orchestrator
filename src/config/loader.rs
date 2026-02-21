//! Configuration file loading.
//!
//! ## Responsibility
//! Read a TOML file from disk, parse it into a [`PipelineConfig`], and run
//! validation before returning. This is the primary entry point for loading
//! pipeline configuration at startup.
//!
//! ## Guarantees
//! - A successfully loaded config is always validated
//! - I/O errors and parse errors are distinguished in the error type
//! - File path is included in every error message
//!
//! ## NOT Responsible For
//! - Hot-reloading on file changes (that belongs to `watcher`)
//! - Defining the config schema (that belongs to `mod.rs`)

use std::path::Path;

use super::validation::{self, ConfigError};
use super::PipelineConfig;

/// Load a [`PipelineConfig`] from a TOML file.
///
/// Reads the file, parses it as TOML, and validates all semantic constraints.
///
/// # Arguments
///
/// * `path` — Path to the TOML configuration file.
///
/// # Returns
///
/// - `Ok(PipelineConfig)` if the file is readable, well-formed, and valid.
/// - `Err(ConfigError::Io)` if the file cannot be read.
/// - `Err(ConfigError::Parse)` if the TOML is malformed.
/// - `Err(ConfigError::Validation)` if semantic constraints are violated.
///
/// # Panics
///
/// This function never panics.
///
/// # Example
///
/// ```rust,ignore
/// use tokio_prompt_orchestrator::config::loader::load_from_file;
/// use std::path::Path;
///
/// let config = load_from_file(Path::new("pipeline.toml"))?;
/// println!("Loaded pipeline: {}", config.pipeline.name);
/// ```
pub fn load_from_file(path: &Path) -> Result<PipelineConfig, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        file: path.display().to_string(),
        source: e,
    })?;

    load_from_str(&content, &path.display().to_string())
}

/// Load a [`PipelineConfig`] from a TOML string.
///
/// Useful for testing or embedding configs without file I/O.
///
/// # Arguments
///
/// * `content` — TOML content as a string.
/// * `source_name` — Identifier for the source (used in error messages).
///
/// # Returns
///
/// - `Ok(PipelineConfig)` if the TOML is well-formed and valid.
/// - `Err(ConfigError::Parse)` if the TOML is malformed.
/// - `Err(ConfigError::Validation)` if semantic constraints are violated.
///
/// # Panics
///
/// This function never panics.
pub fn load_from_str(content: &str, source_name: &str) -> Result<PipelineConfig, ConfigError> {
    let config: PipelineConfig = toml::from_str(content).map_err(|e| ConfigError::Parse {
        file: source_name.to_string(),
        source: e,
    })?;

    validation::validate(&config).map_err(|errors| {
        ConfigError::Validation(
            errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const VALID_TOML: &str = r#"
[pipeline]
name = "test"
version = "1.0"

[stages.rag]
enabled = true
timeout_ms = 5000
max_context_tokens = 2048

[stages.assemble]
enabled = true

[stages.inference]
worker = "echo"
model = "test-model"

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts = 3
retry_base_ms = 100
retry_max_ms = 5000
circuit_breaker_threshold = 5
circuit_breaker_timeout_s = 60
circuit_breaker_success_rate = 0.8

[deduplication]
enabled = false

[observability]
log_format = "pretty"
"#;

    #[test]
    fn test_load_from_str_valid_toml_succeeds() {
        let config = load_from_str(VALID_TOML, "test").expect("test: valid config");
        assert_eq!(config.pipeline.name, "test");
    }

    #[test]
    fn test_load_from_str_invalid_toml_returns_parse_error() {
        let result = load_from_str("not valid toml [[[", "bad.toml");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn test_load_from_str_validation_failure_returns_validation_error() {
        let toml_str = r#"
[pipeline]
name = ""
version = "1.0"

[stages.rag]
enabled = true

[stages.assemble]
enabled = true

[stages.inference]
worker = "echo"
model = "test-model"

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts = 3
circuit_breaker_threshold = 5
circuit_breaker_timeout_s = 60
circuit_breaker_success_rate = 0.8

[deduplication]
enabled = false

[observability]
log_format = "pretty"
"#;
        let result = load_from_str(toml_str, "empty-name.toml");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn test_load_from_file_valid_toml_succeeds() {
        let dir = tempfile::tempdir().expect("test: create tempdir");
        let path = dir.path().join("test.toml");
        let mut f = std::fs::File::create(&path).expect("test: create file");
        f.write_all(VALID_TOML.as_bytes()).expect("test: write");
        drop(f);

        let config = load_from_file(&path).expect("test: load from file");
        assert_eq!(config.pipeline.name, "test");
    }

    #[test]
    fn test_load_from_file_missing_file_returns_io_error() {
        let result = load_from_file(Path::new("/nonexistent/path/config.toml"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn test_load_from_file_invalid_toml_returns_parse_error() {
        let dir = tempfile::tempdir().expect("test: create tempdir");
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not valid [[[").expect("test: write");

        let result = load_from_file(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Parse { .. }));
    }

    #[test]
    fn test_load_from_file_invalid_values_returns_validation_error() {
        let toml_str = r#"
[pipeline]
name = "test"
version = "1.0"

[stages.rag]
enabled = true

[stages.assemble]
enabled = true

[stages.inference]
worker = "echo"
model = "test-model"
temperature = 5.0

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts = 3
circuit_breaker_threshold = 5
circuit_breaker_timeout_s = 60
circuit_breaker_success_rate = 0.8

[deduplication]
enabled = false

[observability]
log_format = "pretty"
"#;
        let dir = tempfile::tempdir().expect("test: create tempdir");
        let path = dir.path().join("invalid.toml");
        std::fs::write(&path, toml_str).expect("test: write");

        let result = load_from_file(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Validation(_)));
    }

    #[test]
    fn test_load_from_str_source_name_appears_in_error() {
        let result = load_from_str("invalid [[[", "my-source.toml");
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("my-source.toml"));
    }

    #[test]
    fn test_load_from_str_missing_required_field_returns_parse_error() {
        // Missing [stages.inference] entirely
        let toml_str = r#"
[pipeline]
name = "test"
version = "1.0"

[stages.rag]
enabled = true

[stages.assemble]
enabled = true

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts = 3
circuit_breaker_threshold = 5
circuit_breaker_timeout_s = 60
circuit_breaker_success_rate = 0.8

[deduplication]
enabled = false

[observability]
log_format = "pretty"
"#;
        let result = load_from_str(toml_str, "missing-inference.toml");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Parse { .. }));
    }

    #[test]
    fn test_load_from_str_all_workers_accepted() {
        for worker in &["open_ai", "anthropic", "llama_cpp", "vllm", "echo"] {
            let toml_str = format!(
                r#"
[pipeline]
name = "test"
version = "1.0"

[stages.rag]
enabled = true

[stages.assemble]
enabled = true

[stages.inference]
worker = "{worker}"
model = "m"

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts = 1
circuit_breaker_threshold = 1
circuit_breaker_timeout_s = 1
circuit_breaker_success_rate = 0.5

[deduplication]
enabled = false

[observability]
log_format = "pretty"
"#
            );
            let result = load_from_str(&toml_str, "worker-test.toml");
            assert!(result.is_ok(), "worker '{}' should parse", worker);
        }
    }

    #[test]
    fn test_load_from_str_unknown_worker_fails() {
        let toml_str = r#"
[pipeline]
name = "test"
version = "1.0"

[stages.rag]
enabled = true

[stages.assemble]
enabled = true

[stages.inference]
worker = "unknown_worker"
model = "m"

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts = 1
circuit_breaker_threshold = 1
circuit_breaker_timeout_s = 1
circuit_breaker_success_rate = 0.5

[deduplication]
enabled = false

[observability]
log_format = "pretty"
"#;
        let result = load_from_str(toml_str, "unknown-worker.toml");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Parse { .. }));
    }
}
