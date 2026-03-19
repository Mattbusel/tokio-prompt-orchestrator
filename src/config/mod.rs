//! # Stage: Declarative Pipeline Configuration
//!
//! ## Responsibility
//! Parse, validate, and hot-reload TOML pipeline configuration files.
//! Users define an entire pipeline topology declaratively and run it with:
//! ```text
//! cargo run -- --config pipeline.toml
//! ```
//!
//! ## Guarantees
//! - Deterministic: same TOML input always produces the same `PipelineConfig`
//! - Validated: all semantic constraints are checked before a config is accepted
//! - Type-safe: invalid field combinations are caught at parse time via serde
//! - Hot-reloadable: file changes are detected and validated before applying
//! - Schema-exportable: JSON Schema output enables IDE autocomplete
//!
//! ## NOT Responsible For
//! - Building the runtime pipeline from config (that belongs to `stages`)
//! - Managing worker connections (that belongs to `worker`)
//! - Metrics collection (that belongs to `metrics`)

pub mod loader;
pub mod validation;
pub mod watcher;

#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Default value functions ──────────────────────────────────────────────

/// Default RAG stage timeout: 5000ms.
fn default_timeout_ms() -> u64 {
    5000
}

/// Default maximum context tokens for the RAG stage.
fn default_max_context_tokens() -> usize {
    2048
}

/// Default retry base delay: 100ms.
fn default_retry_base_ms() -> u64 {
    100
}

/// Default retry maximum delay: 5000ms.
fn default_retry_max_ms() -> u64 {
    5000
}

/// Default deduplication window: 300 seconds (5 minutes).
fn default_dedup_window_s() -> u64 {
    300
}

/// Default deduplication max entries: 10 000.
fn default_dedup_max_entries() -> usize {
    10_000
}

/// Default channel capacity for pipeline stages.
fn default_channel_capacity() -> usize {
    512
}

/// Default enabled state: true.
fn default_true() -> bool {
    true
}

// ── Channel sizes ────────────────────────────────────────────────────────

/// Per-channel capacity overrides for the five inter-stage pipeline channels.
///
/// All fields are optional; when absent the stage default is used
/// (512, 512, 1024, 512, 256 respectively).
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ChannelSizes {
    /// Capacity of the RAG → Assemble channel (default: 512).
    pub rag_to_assemble: Option<usize>,
    /// Capacity of the Assemble → Inference channel (default: 512).
    pub assemble_to_inference: Option<usize>,
    /// Capacity of the Inference → Post channel (default: 1024).
    pub inference_to_post: Option<usize>,
    /// Capacity of the Post → Stream channel (default: 512).
    pub post_to_stream: Option<usize>,
    /// Capacity of the Stream output channel (default: 256).
    pub stream_output: Option<usize>,
}

// ── Top-level config ─────────────────────────────────────────────────────

/// Root configuration for a pipeline instance.
///
/// Deserialized from a TOML file and validated before use.
/// Every field has either a required value or a documented default.
///
/// # Example
///
/// ```toml
/// [pipeline]
/// name = "production"
/// version = "1.0"
///
/// [stages.inference]
/// worker = "open_ai"
/// model = "gpt-4"
/// ```
///
/// # Panics
///
/// This type never panics during construction or access.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    /// Pipeline identity and version metadata.
    pub pipeline: PipelineSection,
    /// Per-stage configuration for the five pipeline stages.
    pub stages: StagesConfig,
    /// Resilience settings: retries, circuit breaker, backpressure.
    pub resilience: ResilienceConfig,
    /// Rate limiting settings to control request throughput.
    #[serde(default)]
    pub rate_limits: RateLimitConfig,
    /// Deduplication settings for request coalescing.
    pub deduplication: DeduplicationConfig,
    /// Observability: logging, metrics, tracing.
    pub observability: ObservabilityConfig,
    /// Optional per-channel capacity overrides.  When absent, each stage uses
    /// its compiled-in default (512 / 512 / 1024 / 512 / 256).
    #[serde(default)]
    pub channel_sizes: Option<ChannelSizes>,
}

// ── Pipeline identity ────────────────────────────────────────────────────

/// Pipeline identity and version metadata.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PipelineSection {
    /// Human-readable pipeline name (e.g., "production", "staging").
    pub name: String,
    /// Semantic version of this configuration (e.g., "1.0").
    pub version: String,
    /// Optional description for documentation purposes.
    pub description: Option<String>,
}

// ── Stage configs ────────────────────────────────────────────────────────

/// Configuration for all five pipeline stages.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StagesConfig {
    /// RAG (retrieval-augmented generation) stage settings.
    pub rag: RagStageConfig,
    /// Prompt assembly stage settings.
    pub assemble: AssembleStageConfig,
    /// Model inference stage settings.
    pub inference: InferenceStageConfig,
    /// Post-processing stage settings.
    pub post_process: PostProcessStageConfig,
    /// Output streaming stage settings.
    pub stream: StreamStageConfig,
}

/// RAG stage configuration.
///
/// Controls retrieval timeout, context limits, and channel sizing.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RagStageConfig {
    /// Whether the RAG stage is enabled. Disabled stages pass-through.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum time (ms) to wait for retrieval results.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum context tokens to prepend from retrieval.
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: usize,
    /// Channel buffer capacity for this stage. `None` uses the pipeline default (512).
    pub channel_capacity: Option<usize>,
}

/// Assembly stage configuration.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AssembleStageConfig {
    /// Whether the assembly stage is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Channel buffer capacity for this stage.
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
}

/// Default number of concurrent inference workers (1 = backward-compatible single worker).
fn default_inference_workers() -> usize {
    1
}

/// Inference stage configuration.
///
/// Specifies the worker backend, model, and generation parameters.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct InferenceStageConfig {
    /// Which model worker backend to use.
    pub worker: WorkerKind,
    /// Model name or identifier (e.g., "gpt-4", "claude-3-opus").
    pub model: String,
    /// Maximum tokens to generate. `None` uses the worker default.
    pub max_tokens: Option<u32>,
    /// Sampling temperature. `None` uses the worker default.
    pub temperature: Option<f32>,
    /// Inference timeout in milliseconds. `None` uses no explicit timeout.
    pub timeout_ms: Option<u64>,
    /// Number of concurrent inference worker tasks reading from the same
    /// input channel.  Default 1 (backward-compatible).  Increase to add
    /// parallelism for high-throughput deployments.
    #[serde(default = "default_inference_workers")]
    pub inference_workers: usize,
}

/// Supported model worker backends.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    /// OpenAI-compatible API (GPT-4, GPT-3.5, etc.).
    OpenAi,
    /// Anthropic Claude API.
    Anthropic,
    /// Local llama.cpp server.
    LlamaCpp,
    /// vLLM inference server.
    Vllm,
    /// Echo worker for testing — returns the prompt as tokens.
    Echo,
}

/// Post-processing stage configuration.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PostProcessStageConfig {
    /// Whether post-processing is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Channel buffer capacity for this stage.
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
}

/// Output streaming stage configuration.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StreamStageConfig {
    /// Whether output streaming is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Channel buffer capacity for this stage.
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
}

// ── Rate limiting ────────────────────────────────────────────────────────

/// Default requests-per-second limit: 100.
fn default_rps() -> u32 {
    100
}

/// Default burst capacity: 20.
fn default_burst() -> u32 {
    20
}

/// Default enabled state for rate limiting: false.
fn default_rate_limit_enabled() -> bool {
    false
}

/// Rate limiting configuration.
///
/// Controls the token-bucket rate limiter that caps inbound request throughput.
/// When enabled, requests exceeding the allowed rate are rejected with HTTP 429.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Whether rate limiting is enabled.
    #[serde(default = "default_rate_limit_enabled")]
    pub enabled: bool,
    /// Maximum sustained requests per second (token refill rate).
    #[serde(default = "default_rps")]
    pub requests_per_second: u32,
    /// Maximum burst capacity above the sustained rate.
    #[serde(default = "default_burst")]
    pub burst_capacity: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: default_rate_limit_enabled(),
            requests_per_second: default_rps(),
            burst_capacity: default_burst(),
        }
    }
}

// ── Resilience ───────────────────────────────────────────────────────────

/// Resilience configuration for retries and circuit breaking.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResilienceConfig {
    /// Maximum number of retry attempts before failing a request.
    pub retry_attempts: u32,
    /// Base delay (ms) for exponential backoff. Must be ≤ `retry_max_ms`.
    #[serde(default = "default_retry_base_ms")]
    pub retry_base_ms: u64,
    /// Maximum delay (ms) cap for exponential backoff.
    #[serde(default = "default_retry_max_ms")]
    pub retry_max_ms: u64,
    /// Number of consecutive failures before the circuit breaker opens.
    pub circuit_breaker_threshold: u32,
    /// Seconds to keep the circuit breaker open before allowing a probe.
    pub circuit_breaker_timeout_s: u64,
    /// Required success rate (0.0–1.0) over the sliding window.
    pub circuit_breaker_success_rate: f64,
}

// ── Deduplication ────────────────────────────────────────────────────────

/// Deduplication configuration for request coalescing.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeduplicationConfig {
    /// Whether deduplication is enabled.
    pub enabled: bool,
    /// Time window (seconds) within which duplicate requests are coalesced.
    #[serde(default = "default_dedup_window_s")]
    pub window_s: u64,
    /// Maximum number of dedup entries held in memory.
    #[serde(default = "default_dedup_max_entries")]
    pub max_entries: usize,
}

// ── Observability ────────────────────────────────────────────────────────

/// Observability configuration: logging, metrics endpoint, and tracing.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// Log output format.
    pub log_format: LogFormat,
    /// Port for the Prometheus metrics HTTP endpoint. `None` disables it.
    pub metrics_port: Option<u16>,
    /// OpenTelemetry tracing collector endpoint. `None` disables distributed tracing.
    pub tracing_endpoint: Option<String>,
}

/// Log output format.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// Human-readable, colorized log output.
    Pretty,
    /// Structured JSON log output for machine consumption.
    Json,
}

/// Export the JSON Schema for `PipelineConfig`.
///
/// This enables IDE autocomplete when editing TOML config files.
///
/// # Errors
///
/// Returns `serde_json::Error` if schema serialization fails (should not
/// happen with well-formed derive macros).
///
/// # Panics
///
/// This function never panics.
#[cfg(feature = "schema")]
pub fn export_schema() -> Result<String, serde_json::Error> {
    let schema = schemars::schema_for!(PipelineConfig);
    serde_json::to_string_pretty(&schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_timeout_ms_returns_5000() {
        assert_eq!(default_timeout_ms(), 5000);
    }

    #[test]
    fn test_default_max_context_tokens_returns_2048() {
        assert_eq!(default_max_context_tokens(), 2048);
    }

    #[test]
    fn test_default_retry_base_ms_returns_100() {
        assert_eq!(default_retry_base_ms(), 100);
    }

    #[test]
    fn test_default_retry_max_ms_returns_5000() {
        assert_eq!(default_retry_max_ms(), 5000);
    }

    #[test]
    fn test_default_dedup_window_s_returns_300() {
        assert_eq!(default_dedup_window_s(), 300);
    }

    #[test]
    fn test_default_dedup_max_entries_returns_10000() {
        assert_eq!(default_dedup_max_entries(), 10_000);
    }

    #[test]
    fn test_default_channel_capacity_returns_512() {
        assert_eq!(default_channel_capacity(), 512);
    }

    #[test]
    fn test_default_true_returns_true() {
        assert!(default_true());
    }

    #[test]
    fn test_worker_kind_serializes_to_snake_case() {
        let json = serde_json::to_string(&WorkerKind::OpenAi).expect("test: serialization");
        assert_eq!(json, "\"open_ai\"");
    }

    #[test]
    fn test_worker_kind_deserializes_from_snake_case() {
        let kind: WorkerKind =
            serde_json::from_str("\"llama_cpp\"").expect("test: deserialization");
        assert_eq!(kind, WorkerKind::LlamaCpp);
    }

    #[test]
    fn test_log_format_serializes_to_snake_case() {
        let json = serde_json::to_string(&LogFormat::Pretty).expect("test: serialization");
        assert_eq!(json, "\"pretty\"");
    }

    #[test]
    fn test_log_format_deserializes_from_snake_case() {
        let fmt: LogFormat = serde_json::from_str("\"json\"").expect("test: deserialization");
        assert_eq!(fmt, LogFormat::Json);
    }

    #[cfg(feature = "schema")]
    #[test]
    fn test_export_schema_produces_valid_json() {
        let schema = export_schema().expect("test: schema export");
        let parsed: serde_json::Value =
            serde_json::from_str(&schema).expect("test: schema is valid JSON");
        // Should contain top-level properties
        assert!(parsed.get("properties").is_some() || parsed.get("$ref").is_some());
    }

    #[test]
    fn test_pipeline_config_minimal_toml_parses() {
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
        let config: PipelineConfig = toml::from_str(toml_str).expect("test: minimal TOML parses");
        assert_eq!(config.pipeline.name, "test");
        assert_eq!(config.stages.inference.worker, WorkerKind::Echo);
        assert_eq!(config.resilience.retry_base_ms, 100); // default applied
        assert!(!config.deduplication.enabled);
    }

    #[test]
    fn test_pipeline_config_full_toml_parses() {
        let toml_str = r#"
[pipeline]
name = "production"
version = "1.0"
description = "Production pipeline with OpenAI GPT-4"

[stages.rag]
enabled = true
timeout_ms = 5000
max_context_tokens = 2048

[stages.assemble]
enabled = true
channel_capacity = 256

[stages.inference]
worker = "open_ai"
model = "gpt-4"
max_tokens = 1024
temperature = 0.7
timeout_ms = 30000

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
enabled = true
window_s = 300
max_entries = 10000

[observability]
log_format = "json"
metrics_port = 9090
"#;
        let config: PipelineConfig = toml::from_str(toml_str).expect("test: full TOML parses");
        assert_eq!(config.pipeline.name, "production");
        assert_eq!(config.stages.inference.worker, WorkerKind::OpenAi);
        assert_eq!(config.stages.inference.temperature, Some(0.7));
        assert_eq!(config.stages.assemble.channel_capacity, 256);
        assert_eq!(config.observability.metrics_port, Some(9090));
    }

    #[test]
    fn test_pipeline_config_serialize_deserialize_roundtrip() {
        let config = PipelineConfig {
            pipeline: PipelineSection {
                name: "roundtrip".into(),
                version: "2.0".into(),
                description: Some("Roundtrip test".into()),
            },
            stages: StagesConfig {
                rag: RagStageConfig {
                    enabled: true,
                    timeout_ms: 3000,
                    max_context_tokens: 1024,
                    channel_capacity: Some(256),
                },
                assemble: AssembleStageConfig {
                    enabled: true,
                    channel_capacity: 512,
                },
                inference: InferenceStageConfig {
                    worker: WorkerKind::Anthropic,
                    model: "claude-3-opus".into(),
                    max_tokens: Some(2048),
                    temperature: Some(0.5),
                    timeout_ms: Some(60000),
                },
                post_process: PostProcessStageConfig {
                    enabled: true,
                    channel_capacity: 512,
                },
                stream: StreamStageConfig {
                    enabled: false,
                    channel_capacity: 128,
                },
            },
            resilience: ResilienceConfig {
                retry_attempts: 5,
                retry_base_ms: 200,
                retry_max_ms: 10000,
                circuit_breaker_threshold: 10,
                circuit_breaker_timeout_s: 120,
                circuit_breaker_success_rate: 0.9,
            },
            deduplication: DeduplicationConfig {
                enabled: true,
                window_s: 600,
                max_entries: 20000,
            },
            observability: ObservabilityConfig {
                log_format: LogFormat::Json,
                metrics_port: Some(8080),
                tracing_endpoint: Some("http://jaeger:14268".into()),
            },
            rate_limits: RateLimitConfig::default(),
            channel_sizes: None,
        };

        let toml_str = toml::to_string_pretty(&config).expect("test: serialize to TOML");
        let deserialized: PipelineConfig =
            toml::from_str(&toml_str).expect("test: deserialize from TOML");
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_pipeline_config_json_roundtrip() {
        let config = PipelineConfig {
            pipeline: PipelineSection {
                name: "json-rt".into(),
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
                    model: "echo".into(),
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
                    channel_capacity: 512,
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
                enabled: false,
                window_s: 300,
                max_entries: 10000,
            },
            observability: ObservabilityConfig {
                log_format: LogFormat::Pretty,
                metrics_port: None,
                tracing_endpoint: None,
            },
            rate_limits: RateLimitConfig::default(),
            channel_sizes: None,
        };

        let json = serde_json::to_string(&config).expect("test: serialize to JSON");
        let deserialized: PipelineConfig =
            serde_json::from_str(&json).expect("test: deserialize from JSON");
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_all_worker_kinds_roundtrip_toml() {
        // TOML requires a table wrapper for enum serialization
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Wrapper {
            kind: WorkerKind,
        }

        let kinds = vec![
            WorkerKind::OpenAi,
            WorkerKind::Anthropic,
            WorkerKind::LlamaCpp,
            WorkerKind::Vllm,
            WorkerKind::Echo,
        ];
        for kind in kinds {
            let w = Wrapper { kind: kind.clone() };
            let s = toml::to_string(&w).expect("test: serialize worker kind");
            let deserialized: Wrapper = toml::from_str(&s).expect("test: deserialize worker kind");
            assert_eq!(w, deserialized);
        }
    }

    #[test]
    fn test_pipeline_section_optional_description_omitted() {
        let toml_str = r#"
name = "no-desc"
version = "1.0"
"#;
        let section: PipelineSection =
            toml::from_str(toml_str).expect("test: parse without description");
        assert!(section.description.is_none());
    }

    #[test]
    fn test_rag_stage_defaults_applied_when_omitted() {
        let toml_str = r#"
enabled = true
"#;
        let rag: RagStageConfig = toml::from_str(toml_str).expect("test: parse with defaults");
        assert_eq!(rag.timeout_ms, 5000);
        assert_eq!(rag.max_context_tokens, 2048);
        assert!(rag.channel_capacity.is_none());
    }

    #[test]
    fn test_resilience_defaults_applied_when_omitted() {
        let toml_str = r#"
retry_attempts = 3
circuit_breaker_threshold = 5
circuit_breaker_timeout_s = 60
circuit_breaker_success_rate = 0.8
"#;
        let resilience: ResilienceConfig =
            toml::from_str(toml_str).expect("test: parse with defaults");
        assert_eq!(resilience.retry_base_ms, 100);
        assert_eq!(resilience.retry_max_ms, 5000);
    }

    #[test]
    fn test_deduplication_defaults_applied_when_omitted() {
        let toml_str = r#"
enabled = true
"#;
        let dedup: DeduplicationConfig =
            toml::from_str(toml_str).expect("test: parse with defaults");
        assert_eq!(dedup.window_s, 300);
        assert_eq!(dedup.max_entries, 10_000);
    }

    #[test]
    fn test_unknown_field_in_resilience_is_rejected() {
        // deny_unknown_fields must fire so typos in TOML are caught at parse time
        let bad_toml = r#"
retry_attempts = 3
circuit_breaker_threshold = 5
circuit_breaker_timeout_s = 60
circuit_breaker_success_rate = 0.8
typo_field = "oops"
"#;
        let result = toml::from_str::<ResilienceConfig>(bad_toml);
        assert!(result.is_err(), "unknown fields must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("typo_field") || msg.contains("unknown"),
            "error should mention the unknown field: {msg}"
        );
    }

    #[test]
    fn test_unknown_field_in_deduplication_is_rejected() {
        let bad_toml = r#"
enabled = true
unknwon_key = 42
"#;
        let result = toml::from_str::<DeduplicationConfig>(bad_toml);
        assert!(result.is_err(), "unknown fields must be rejected");
    }
}
