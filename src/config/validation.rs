//! Configuration validation engine.
//!
//! ## Responsibility
//! Validate semantic constraints on a parsed [`PipelineConfig`] that cannot
//! be expressed through the type system alone (e.g., range checks, cross-field
//! invariants).
//!
//! ## Guarantees
//! - Every validation rule has at least one test that triggers it
//! - Validation collects *all* errors before returning (no short-circuit)
//! - Error messages include the field path and the invalid value
//!
//! ## NOT Responsible For
//! - Parsing TOML (that belongs to `loader`)
//! - File I/O (that belongs to `loader`)

use super::PipelineConfig;

/// Errors arising from configuration parsing, validation, or I/O.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// TOML parsing failed.
    #[error("Parse error in {file}: {source}")]
    Parse {
        /// Path of the file that failed to parse.
        file: String,
        /// Underlying TOML deserialization error.
        #[source]
        source: toml::de::Error,
    },

    /// One or more semantic validation rules failed.
    #[error("Validation failed: {0}")]
    Validation(String),

    /// A specific field has an out-of-range or contradictory value.
    #[error("Field '{field}' has invalid value {value}: {reason}")]
    InvalidField {
        /// Dot-separated field path (e.g., "resilience.retry_base_ms").
        field: String,
        /// String representation of the invalid value.
        value: String,
        /// Human-readable explanation of the constraint.
        reason: String,
    },

    /// File I/O error.
    #[error("IO error reading {file}: {source}")]
    Io {
        /// Path of the file that could not be read.
        file: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Validate all semantic constraints on a [`PipelineConfig`].
///
/// Collects every violation before returning so the caller sees the full
/// scope of issues at once.
///
/// # Arguments
///
/// * `config` — The parsed config to validate.
///
/// # Returns
///
/// - `Ok(())` if all constraints pass.
/// - `Err(Vec<ConfigError>)` with every violation found.
///
/// # Panics
///
/// This function never panics.
pub fn validate(config: &PipelineConfig) -> Result<(), Vec<ConfigError>> {
    let mut errors = Vec::new();

    // ── Retry settings ───────────────────────────────────────────────
    if config.resilience.retry_base_ms > config.resilience.retry_max_ms {
        errors.push(ConfigError::InvalidField {
            field: "resilience.retry_base_ms".into(),
            value: config.resilience.retry_base_ms.to_string(),
            reason: "must be \u{2264} retry_max_ms".into(),
        });
    }

    if config.resilience.retry_attempts == 0 {
        errors.push(ConfigError::InvalidField {
            field: "resilience.retry_attempts".into(),
            value: "0".into(),
            reason: "must be at least 1".into(),
        });
    }

    // ── Circuit breaker ──────────────────────────────────────────────
    if config.resilience.circuit_breaker_success_rate > 1.0
        || config.resilience.circuit_breaker_success_rate < 0.0
    {
        errors.push(ConfigError::InvalidField {
            field: "resilience.circuit_breaker_success_rate".into(),
            value: config.resilience.circuit_breaker_success_rate.to_string(),
            reason: "must be between 0.0 and 1.0".into(),
        });
    }

    if config.resilience.circuit_breaker_threshold == 0 {
        errors.push(ConfigError::InvalidField {
            field: "resilience.circuit_breaker_threshold".into(),
            value: "0".into(),
            reason: "must be at least 1".into(),
        });
    }

    if config.resilience.circuit_breaker_timeout_s == 0 {
        errors.push(ConfigError::InvalidField {
            field: "resilience.circuit_breaker_timeout_s".into(),
            value: "0".into(),
            reason: "must be at least 1 second".into(),
        });
    }

    // ── Inference temperature ────────────────────────────────────────
    if let Some(temp) = config.stages.inference.temperature {
        if !(0.0..=2.0).contains(&temp) {
            errors.push(ConfigError::InvalidField {
                field: "stages.inference.temperature".into(),
                value: temp.to_string(),
                reason: "must be between 0.0 and 2.0".into(),
            });
        }
    }

    // ── Model name non-empty ─────────────────────────────────────────
    if config.stages.inference.model.trim().is_empty() {
        errors.push(ConfigError::InvalidField {
            field: "stages.inference.model".into(),
            value: String::new(),
            reason: "model name must not be empty".into(),
        });
    }

    // ── Pipeline name non-empty ──────────────────────────────────────
    if config.pipeline.name.trim().is_empty() {
        errors.push(ConfigError::InvalidField {
            field: "pipeline.name".into(),
            value: String::new(),
            reason: "pipeline name must not be empty".into(),
        });
    }

    // ── Pipeline version non-empty ───────────────────────────────────
    if config.pipeline.version.trim().is_empty() {
        errors.push(ConfigError::InvalidField {
            field: "pipeline.version".into(),
            value: String::new(),
            reason: "pipeline version must not be empty".into(),
        });
    }

    // ── RAG timeout > 0 ─────────────────────────────────────────────
    if config.stages.rag.enabled && config.stages.rag.timeout_ms == 0 {
        errors.push(ConfigError::InvalidField {
            field: "stages.rag.timeout_ms".into(),
            value: "0".into(),
            reason: "timeout must be at least 1ms when RAG is enabled".into(),
        });
    }

    // ── RAG max_context_tokens > 0 ──────────────────────────────────
    if config.stages.rag.enabled && config.stages.rag.max_context_tokens == 0 {
        errors.push(ConfigError::InvalidField {
            field: "stages.rag.max_context_tokens".into(),
            value: "0".into(),
            reason: "max_context_tokens must be at least 1 when RAG is enabled".into(),
        });
    }

    // ── Channel capacities > 0 ──────────────────────────────────────
    if let Some(cap) = config.stages.rag.channel_capacity {
        if cap == 0 {
            errors.push(ConfigError::InvalidField {
                field: "stages.rag.channel_capacity".into(),
                value: "0".into(),
                reason: "channel capacity must be at least 1".into(),
            });
        }
    }

    if config.stages.assemble.channel_capacity == 0 {
        errors.push(ConfigError::InvalidField {
            field: "stages.assemble.channel_capacity".into(),
            value: "0".into(),
            reason: "channel capacity must be at least 1".into(),
        });
    }

    if config.stages.post_process.channel_capacity == 0 {
        errors.push(ConfigError::InvalidField {
            field: "stages.post_process.channel_capacity".into(),
            value: "0".into(),
            reason: "channel capacity must be at least 1".into(),
        });
    }

    if config.stages.stream.channel_capacity == 0 {
        errors.push(ConfigError::InvalidField {
            field: "stages.stream.channel_capacity".into(),
            value: "0".into(),
            reason: "channel capacity must be at least 1".into(),
        });
    }

    // ── Deduplication constraints ────────────────────────────────────
    if config.deduplication.enabled && config.deduplication.window_s == 0 {
        errors.push(ConfigError::InvalidField {
            field: "deduplication.window_s".into(),
            value: "0".into(),
            reason: "dedup window must be at least 1s when enabled".into(),
        });
    }

    if config.deduplication.enabled && config.deduplication.max_entries == 0 {
        errors.push(ConfigError::InvalidField {
            field: "deduplication.max_entries".into(),
            value: "0".into(),
            reason: "max_entries must be at least 1 when dedup is enabled".into(),
        });
    }

    // ── Metrics port range ──────────────────────────────────────────
    if let Some(port) = config.observability.metrics_port {
        if port == 0 {
            errors.push(ConfigError::InvalidField {
                field: "observability.metrics_port".into(),
                value: "0".into(),
                reason: "metrics port must be at least 1".into(),
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;

    /// Helper to build a valid config that can be mutated for negative tests.
    fn valid_config() -> PipelineConfig {
        PipelineConfig {
            pipeline: PipelineSection {
                name: "test".into(),
                version: "1.0".into(),
                description: None,
            },
            stages: StagesConfig {
                rag: RagStageConfig {
                    enabled: true,
                    timeout_ms: 5000,
                    max_context_tokens: 2048,
                    channel_capacity: None,
                },
                assemble: AssembleStageConfig {
                    enabled: true,
                    channel_capacity: 512,
                },
                inference: InferenceStageConfig {
                    worker: WorkerKind::Echo,
                    model: "echo-model".into(),
                    max_tokens: None,
                    temperature: None,
                    timeout_ms: None,
                },
                post_process: PostProcessStageConfig {
                    enabled: true,
                    channel_capacity: 512,
                },
                stream: StreamStageConfig {
                    enabled: true,
                    channel_capacity: 256,
                },
            },
            resilience: ResilienceConfig {
                retry_attempts: 3,
                retry_base_ms: 100,
                retry_max_ms: 5000,
                circuit_breaker_threshold: 5,
                circuit_breaker_timeout_s: 60,
                circuit_breaker_success_rate: 0.8,
            },
            deduplication: DeduplicationConfig {
                enabled: true,
                window_s: 300,
                max_entries: 10_000,
            },
            observability: ObservabilityConfig {
                log_format: LogFormat::Pretty,
                metrics_port: Some(9090),
                tracing_endpoint: None,
            },
        }
    }

    // ── Valid config passes ──────────────────────────────────────────

    #[test]
    fn test_validate_valid_config_passes() {
        let config = valid_config();
        assert!(validate(&config).is_ok());
    }

    // ── Retry validation ────────────────────────────────────────────

    #[test]
    fn test_validate_retry_base_exceeds_max_fails() {
        let mut config = valid_config();
        config.resilience.retry_base_ms = 10000;
        config.resilience.retry_max_ms = 1000;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. } if field == "resilience.retry_base_ms")
        }));
    }

    #[test]
    fn test_validate_retry_base_equals_max_passes() {
        let mut config = valid_config();
        config.resilience.retry_base_ms = 5000;
        config.resilience.retry_max_ms = 5000;
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_retry_attempts_zero_fails() {
        let mut config = valid_config();
        config.resilience.retry_attempts = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. } if field == "resilience.retry_attempts")
        }));
    }

    // ── Circuit breaker validation ──────────────────────────────────

    #[test]
    fn test_validate_cb_success_rate_above_1_fails() {
        let mut config = valid_config();
        config.resilience.circuit_breaker_success_rate = 1.5;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "resilience.circuit_breaker_success_rate")
        }));
    }

    #[test]
    fn test_validate_cb_success_rate_negative_fails() {
        let mut config = valid_config();
        config.resilience.circuit_breaker_success_rate = -0.1;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "resilience.circuit_breaker_success_rate")
        }));
    }

    #[test]
    fn test_validate_cb_success_rate_zero_passes() {
        let mut config = valid_config();
        config.resilience.circuit_breaker_success_rate = 0.0;
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_cb_success_rate_one_passes() {
        let mut config = valid_config();
        config.resilience.circuit_breaker_success_rate = 1.0;
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_cb_threshold_zero_fails() {
        let mut config = valid_config();
        config.resilience.circuit_breaker_threshold = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "resilience.circuit_breaker_threshold")
        }));
    }

    #[test]
    fn test_validate_cb_timeout_zero_fails() {
        let mut config = valid_config();
        config.resilience.circuit_breaker_timeout_s = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "resilience.circuit_breaker_timeout_s")
        }));
    }

    // ── Temperature validation ──────────────────────────────────────

    #[test]
    fn test_validate_temperature_negative_fails() {
        let mut config = valid_config();
        config.stages.inference.temperature = Some(-0.1);
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "stages.inference.temperature")
        }));
    }

    #[test]
    fn test_validate_temperature_above_2_fails() {
        let mut config = valid_config();
        config.stages.inference.temperature = Some(2.1);
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "stages.inference.temperature")
        }));
    }

    #[test]
    fn test_validate_temperature_zero_passes() {
        let mut config = valid_config();
        config.stages.inference.temperature = Some(0.0);
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_temperature_two_passes() {
        let mut config = valid_config();
        config.stages.inference.temperature = Some(2.0);
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_temperature_none_passes() {
        let mut config = valid_config();
        config.stages.inference.temperature = None;
        assert!(validate(&config).is_ok());
    }

    // ── Name/version validation ─────────────────────────────────────

    #[test]
    fn test_validate_empty_model_name_fails() {
        let mut config = valid_config();
        config.stages.inference.model = "  ".into();
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "stages.inference.model")
        }));
    }

    #[test]
    fn test_validate_empty_pipeline_name_fails() {
        let mut config = valid_config();
        config.pipeline.name = String::new();
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "pipeline.name")
        }));
    }

    #[test]
    fn test_validate_empty_pipeline_version_fails() {
        let mut config = valid_config();
        config.pipeline.version = "   ".into();
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "pipeline.version")
        }));
    }

    // ── RAG constraints ─────────────────────────────────────────────

    #[test]
    fn test_validate_rag_enabled_zero_timeout_fails() {
        let mut config = valid_config();
        config.stages.rag.enabled = true;
        config.stages.rag.timeout_ms = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "stages.rag.timeout_ms")
        }));
    }

    #[test]
    fn test_validate_rag_disabled_zero_timeout_passes() {
        let mut config = valid_config();
        config.stages.rag.enabled = false;
        config.stages.rag.timeout_ms = 0;
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_rag_enabled_zero_context_tokens_fails() {
        let mut config = valid_config();
        config.stages.rag.enabled = true;
        config.stages.rag.max_context_tokens = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "stages.rag.max_context_tokens")
        }));
    }

    // ── Channel capacity ────────────────────────────────────────────

    #[test]
    fn test_validate_rag_channel_capacity_zero_fails() {
        let mut config = valid_config();
        config.stages.rag.channel_capacity = Some(0);
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "stages.rag.channel_capacity")
        }));
    }

    #[test]
    fn test_validate_assemble_channel_capacity_zero_fails() {
        let mut config = valid_config();
        config.stages.assemble.channel_capacity = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "stages.assemble.channel_capacity")
        }));
    }

    #[test]
    fn test_validate_post_process_channel_capacity_zero_fails() {
        let mut config = valid_config();
        config.stages.post_process.channel_capacity = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "stages.post_process.channel_capacity")
        }));
    }

    #[test]
    fn test_validate_stream_channel_capacity_zero_fails() {
        let mut config = valid_config();
        config.stages.stream.channel_capacity = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "stages.stream.channel_capacity")
        }));
    }

    // ── Dedup constraints ───────────────────────────────────────────

    #[test]
    fn test_validate_dedup_enabled_zero_window_fails() {
        let mut config = valid_config();
        config.deduplication.enabled = true;
        config.deduplication.window_s = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "deduplication.window_s")
        }));
    }

    #[test]
    fn test_validate_dedup_disabled_zero_window_passes() {
        let mut config = valid_config();
        config.deduplication.enabled = false;
        config.deduplication.window_s = 0;
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_dedup_enabled_zero_max_entries_fails() {
        let mut config = valid_config();
        config.deduplication.enabled = true;
        config.deduplication.max_entries = 0;
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "deduplication.max_entries")
        }));
    }

    // ── Metrics port ────────────────────────────────────────────────

    #[test]
    fn test_validate_metrics_port_zero_fails() {
        let mut config = valid_config();
        config.observability.metrics_port = Some(0);
        let errors = validate(&config).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, ConfigError::InvalidField { field, .. }
                if field == "observability.metrics_port")
        }));
    }

    #[test]
    fn test_validate_metrics_port_none_passes() {
        let mut config = valid_config();
        config.observability.metrics_port = None;
        assert!(validate(&config).is_ok());
    }

    // ── Multiple errors collected ───────────────────────────────────

    #[test]
    fn test_validate_collects_multiple_errors() {
        let mut config = valid_config();
        config.resilience.retry_base_ms = 99999;
        config.resilience.retry_max_ms = 100;
        config.resilience.circuit_breaker_success_rate = 2.0;
        config.stages.inference.temperature = Some(-1.0);
        config.pipeline.name = String::new();
        let errors = validate(&config).unwrap_err();
        // Should have at least 4 distinct errors
        assert!(
            errors.len() >= 4,
            "expected >=4 errors, got {}",
            errors.len()
        );
    }

    // ── Error display ───────────────────────────────────────────────

    #[test]
    fn test_config_error_parse_display() {
        let toml_err = toml::from_str::<PipelineConfig>("invalid toml [[[").unwrap_err();
        let err = ConfigError::Parse {
            file: "test.toml".into(),
            source: toml_err,
        };
        let msg = err.to_string();
        assert!(msg.contains("test.toml"));
    }

    #[test]
    fn test_config_error_invalid_field_display() {
        let err = ConfigError::InvalidField {
            field: "resilience.retry_base_ms".into(),
            value: "99999".into(),
            reason: "must be \u{2264} retry_max_ms".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("resilience.retry_base_ms"));
        assert!(msg.contains("99999"));
    }

    #[test]
    fn test_config_error_validation_display() {
        let err = ConfigError::Validation("multiple issues".into());
        assert!(err.to_string().contains("multiple issues"));
    }

    #[test]
    fn test_config_error_io_display() {
        let err = ConfigError::Io {
            file: "missing.toml".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        let msg = err.to_string();
        assert!(msg.contains("missing.toml"));
    }
}
