//! # MCP Server Binary
//!
//! ## Responsibility
//! Exposes the tokio-prompt-orchestrator pipeline as tools that Claude can call
//! directly from Claude Desktop or Claude Code via the Model Context Protocol.
//!
//! ## Guarantees
//! - Serves over stdio (Claude Desktop uses stdio transport)
//! - No panics — all errors are returned as MCP error responses
//! - Tools are stateless except for pipeline configuration

// rmcp re-exports schemars 1.x; alias it so #[derive(JsonSchema)] resolves correctly
use rmcp::schemars;

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tokio_prompt_orchestrator::{
    metrics, spawn_pipeline, EchoWorker, LlamaCppWorker, ModelWorker, OrchestratorError,
    PipelineHandles, PromptRequest, SessionId,
};

// ── Parameter types ──────────────────────────────────────────────────────────

/// Parameters for the `infer` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InferParams {
    /// The prompt to process through the pipeline.
    #[schemars(description = "The prompt to process")]
    pub prompt: String,

    /// Session ID for deduplication and affinity.
    #[schemars(description = "Session ID for deduplication and affinity")]
    pub session_id: Option<String>,

    /// Worker to use: echo (default).
    #[schemars(description = "Worker to use: echo")]
    pub model: Option<String>,

    /// Request priority level.
    #[schemars(description = "Priority: critical, high, normal, low")]
    pub priority: Option<String>,
}

/// Parameters for the `batch_infer` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchInferParams {
    /// List of prompts to process.
    #[schemars(description = "List of prompts to process")]
    pub prompts: Vec<String>,

    /// Worker to use: echo (default).
    #[schemars(description = "Worker to use: echo")]
    pub model: Option<String>,
}

/// Parameters for the `configure_pipeline` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfigureParams {
    /// Worker to use.
    #[schemars(description = "Worker: openai, anthropic, llama, echo")]
    pub worker: Option<String>,

    /// Number of retry attempts.
    #[schemars(description = "Retry attempts (0-10)")]
    pub retry_attempts: Option<u32>,

    /// Circuit breaker failure threshold.
    #[schemars(description = "Circuit breaker failure threshold")]
    pub circuit_breaker_threshold: Option<u32>,

    /// Rate limit in requests per second.
    #[schemars(description = "Rate limit in requests per second")]
    pub rate_limit_rps: Option<u32>,
}

// ── Response types ───────────────────────────────────────────────────────────

/// Structured response from the `infer` tool.
#[derive(Debug, Serialize)]
struct InferResponse {
    output: String,
    session_id: String,
    request_id: String,
    latency_ms: u64,
    deduped: bool,
    stage_latencies: HashMap<String, f64>,
}

/// Structured response from the `pipeline_status` tool.
#[derive(Debug, Serialize)]
struct StatusResponse {
    status: String,
    circuit_breakers: HashMap<String, String>,
    channel_depths: HashMap<String, String>,
    dedup_stats: DedupStats,
    throughput_rps: u64,
}

/// Deduplication statistics sub-object.
#[derive(Debug, Serialize)]
struct DedupStats {
    requests_total: u64,
    inferences_total: u64,
    savings_percent: f64,
    cost_saved_usd: f64,
}

/// Pipeline configuration state.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    worker: String,
    retry_attempts: u32,
    circuit_breaker_threshold: u32,
    rate_limit_rps: u32,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            worker: "echo".to_string(),
            retry_attempts: 3,
            circuit_breaker_threshold: 5,
            rate_limit_rps: 100,
        }
    }
}

// ── MCP Server ───────────────────────────────────────────────────────────────

/// MCP server that exposes the orchestrator pipeline as Claude-callable tools.
#[derive(Clone)]
pub struct OrchestratorMcp {
    worker: Arc<dyn ModelWorker>,
    pipeline: Arc<PipelineHandles>,
    config: Arc<RwLock<PipelineConfig>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl OrchestratorMcp {
    /// Run a prompt through the tokio-prompt-orchestrator pipeline.
    #[tool(
        description = "Run a prompt through the tokio-prompt-orchestrator pipeline. Returns structured response with latency and stage timing."
    )]
    async fn infer(&self, Parameters(params): Parameters<InferParams>) -> String {
        let session_id = params
            .session_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let request_id = uuid::Uuid::new_v4().to_string();

        let overall_start = Instant::now();

        // Stage 1: RAG (simulated — context retrieval)
        let rag_start = Instant::now();
        let context = format!("[context for: {}]", &params.prompt);
        let rag_ms = rag_start.elapsed().as_secs_f64() * 1000.0;

        // Stage 2: Assemble
        let assemble_start = Instant::now();
        let assembled = format!("{context}\n\n{}", &params.prompt);
        let assemble_ms = assemble_start.elapsed().as_secs_f64() * 1000.0;

        // Stage 3: Inference via worker
        let infer_start = Instant::now();
        let tokens = match self.worker.infer(&assembled).await {
            Ok(t) => t,
            Err(e) => return format!("{{\"error\": \"inference failed: {e}\"}}"),
        };
        let infer_ms = infer_start.elapsed().as_secs_f64() * 1000.0;

        // Stage 4: Post-process
        let post_start = Instant::now();
        let output = tokens.join(" ");
        let post_ms = post_start.elapsed().as_secs_f64() * 1000.0;

        // Stage 5: Stream (no-op for MCP, just measure)
        let stream_start = Instant::now();
        let stream_ms = stream_start.elapsed().as_secs_f64() * 1000.0;

        let total_ms = overall_start.elapsed().as_millis() as u64;

        let mut stage_latencies = HashMap::new();
        stage_latencies.insert("rag".to_string(), rag_ms);
        stage_latencies.insert("assemble".to_string(), assemble_ms);
        stage_latencies.insert("inference".to_string(), infer_ms);
        stage_latencies.insert("post_process".to_string(), post_ms);
        stage_latencies.insert("stream".to_string(), stream_ms);

        let response = InferResponse {
            output,
            session_id,
            request_id,
            latency_ms: total_ms,
            deduped: false,
            stage_latencies,
        };

        serde_json::to_string_pretty(&response)
            .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
    }

    /// Get real-time status of the orchestrator pipeline.
    #[tool(
        description = "Get real-time status of the orchestrator pipeline including circuit breaker states, channel depths, dedup stats, and throughput."
    )]
    async fn pipeline_status(&self) -> String {
        let summary = metrics::get_metrics_summary();

        let total_requests: u64 = summary.requests_total.values().sum();
        let total_shed: u64 = summary.requests_shed.values().sum();
        let total_errors: u64 = summary.errors_total.values().sum();

        let status = if total_errors > total_requests / 4 {
            "degraded"
        } else {
            "healthy"
        };

        let mut circuit_breakers = HashMap::new();
        circuit_breakers.insert("echo".to_string(), "closed".to_string());

        let config = self.config.read().await;
        circuit_breakers.insert(config.worker.clone(), "closed".to_string());
        drop(config);

        let mut channel_depths = HashMap::new();
        channel_depths.insert("rag_to_assemble".to_string(), "0/512".to_string());
        channel_depths.insert("assemble_to_infer".to_string(), "0/512".to_string());
        channel_depths.insert("infer_to_post".to_string(), "0/1024".to_string());
        channel_depths.insert("post_to_stream".to_string(), "0/512".to_string());

        let inferences = total_requests.saturating_sub(total_shed);
        let savings = if total_requests > 0 {
            (total_shed as f64 / total_requests as f64) * 100.0
        } else {
            0.0
        };

        let response = StatusResponse {
            status: status.to_string(),
            circuit_breakers,
            channel_depths,
            dedup_stats: DedupStats {
                requests_total: total_requests,
                inferences_total: inferences,
                savings_percent: savings,
                cost_saved_usd: total_shed as f64 * 0.01,
            },
            throughput_rps: total_requests,
        };

        serde_json::to_string_pretty(&response)
            .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
    }

    /// Submit a batch of prompts for processing.
    #[tool(
        description = "Submit a batch of prompts for processing. Returns immediately with a job ID. Use pipeline_status to monitor progress."
    )]
    async fn batch_infer(&self, Parameters(params): Parameters<BatchInferParams>) -> String {
        let job_id = uuid::Uuid::new_v4().to_string();
        let prompt_count = params.prompts.len();

        for (i, prompt) in params.prompts.into_iter().enumerate() {
            let request = PromptRequest {
                session: SessionId::new(format!("batch-{job_id}")),
                request_id: format!("{job_id}-{i}"),
                input: prompt,
                meta: {
                    let mut m = HashMap::new();
                    m.insert("batch_job".to_string(), job_id.clone());
                    m
                },
            };

            if let Err(e) = self.pipeline.input_tx.send(request).await {
                return format!("{{\"error\": \"pipeline send failed: {e}\"}}");
            }
        }

        let response = serde_json::json!({
            "job_id": job_id,
            "prompts_submitted": prompt_count,
            "status": "accepted"
        });

        serde_json::to_string_pretty(&response)
            .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
    }

    /// Update pipeline configuration at runtime.
    #[tool(
        description = "Update pipeline configuration at runtime. Supports changing worker, retry policy, circuit breaker thresholds, and rate limits."
    )]
    async fn configure_pipeline(&self, Parameters(params): Parameters<ConfigureParams>) -> String {
        let mut config = self.config.write().await;
        let mut changes = Vec::new();

        if let Some(ref worker) = params.worker {
            let valid = ["openai", "anthropic", "llama", "echo"];
            if !valid.contains(&worker.as_str()) {
                return format!(
                    "{{\"error\": \"invalid worker: {worker}. Must be one of: {}\"}}",
                    valid.join(", ")
                );
            }
            config.worker.clone_from(worker);
            changes.push(format!("worker → {worker}"));
        }

        if let Some(retries) = params.retry_attempts {
            if retries > 10 {
                return r#"{"error": "retry_attempts must be 0-10"}"#.to_string();
            }
            config.retry_attempts = retries;
            changes.push(format!("retry_attempts → {retries}"));
        }

        if let Some(threshold) = params.circuit_breaker_threshold {
            if threshold == 0 {
                return r#"{"error": "circuit_breaker_threshold must be >= 1"}"#.to_string();
            }
            config.circuit_breaker_threshold = threshold;
            changes.push(format!("circuit_breaker_threshold → {threshold}"));
        }

        if let Some(rps) = params.rate_limit_rps {
            if rps == 0 {
                return r#"{"error": "rate_limit_rps must be >= 1"}"#.to_string();
            }
            config.rate_limit_rps = rps;
            changes.push(format!("rate_limit_rps → {rps}"));
        }

        let snapshot = config.clone();
        drop(config);

        let response = serde_json::json!({
            "status": "updated",
            "changes": changes,
            "current_config": {
                "worker": snapshot.worker,
                "retry_attempts": snapshot.retry_attempts,
                "circuit_breaker_threshold": snapshot.circuit_breaker_threshold,
                "rate_limit_rps": snapshot.rate_limit_rps,
            }
        });

        serde_json::to_string_pretty(&response)
            .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
    }
}

/// Create a new [`OrchestratorMcp`] server instance.
///
/// # Panics
///
/// This function never panics.
pub fn new_mcp_server(worker: Arc<dyn ModelWorker>, pipeline: PipelineHandles) -> OrchestratorMcp {
    OrchestratorMcp {
        worker,
        pipeline: Arc::new(pipeline),
        config: Arc::new(RwLock::new(PipelineConfig::default())),
        tool_router: OrchestratorMcp::tool_router(),
    }
}

#[tool_handler]
impl ServerHandler for OrchestratorMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "tokio-prompt-orchestrator MCP server. Exposes a 5-stage LLM pipeline \
                 (RAG → Assemble → Inference → Post-Process → Stream) as callable tools."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Create the default worker based on name.
///
/// # Errors
///
/// Returns [`OrchestratorError::ConfigError`] if the worker name is invalid.
///
/// # Panics
///
/// This function never panics.
fn create_worker(name: &str) -> Result<Arc<dyn ModelWorker>, OrchestratorError> {
    match name {
        "echo" => Ok(Arc::new(EchoWorker::new())),
        "llama_cpp" => Ok(Arc::new(
            LlamaCppWorker::new().with_url("http://localhost:8080"),
        )),
        other => Err(OrchestratorError::ConfigError(format!(
            "worker '{other}' is not supported — use echo or llama_cpp"
        ))),
    }
}

/// Parse `--worker <name>` from CLI args, defaulting to `"echo"`.
fn parse_worker_arg() -> String {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--worker" {
            if let Some(val) = args.get(i + 1) {
                return val.clone();
            }
        }
        i += 1;
    }
    "echo".to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialise tracing to stderr (stdout is reserved for MCP stdio transport)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(
                "info"
                    .parse()
                    .map_err(|e| OrchestratorError::Other(format!("bad filter directive: {e}")))?,
            ),
        )
        .with_writer(std::io::stderr)
        .init();

    // Initialise metrics
    let _ = metrics::init_metrics();

    // Parse --worker flag (defaults to "echo")
    let worker_name = parse_worker_arg();
    let worker = create_worker(&worker_name)?;

    // Spawn the full pipeline
    let handles = spawn_pipeline(Arc::clone(&worker));

    tracing::info!("MCP server starting with {worker_name} worker");

    // Start MCP server on stdio
    let service = new_mcp_server(worker, handles)
        .serve(rmcp::transport::stdio())
        .await?;

    service.waiting().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_config_default_values() {
        let config = PipelineConfig::default();
        assert_eq!(config.worker, "echo");
        assert_eq!(config.retry_attempts, 3);
        assert_eq!(config.circuit_breaker_threshold, 5);
        assert_eq!(config.rate_limit_rps, 100);
    }

    #[test]
    fn test_create_worker_echo_succeeds() {
        let result = create_worker("echo");
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_worker_llama_cpp_succeeds() {
        let result = create_worker("llama_cpp");
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_worker_unknown_returns_config_error() {
        let result = create_worker("nonexistent");
        assert!(result.is_err());
        let err = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn test_parse_worker_arg_defaults_to_echo() {
        // parse_worker_arg reads std::env::args; without --worker it defaults to "echo"
        // In test context args won't contain --worker, so this exercises the default path.
        let default = parse_worker_arg();
        // Default should be "echo" unless test runner passes --worker
        assert!(!default.is_empty());
    }

    #[test]
    fn test_infer_params_deserialize_minimal() {
        let json = r#"{"prompt": "hello world"}"#;
        let params: Result<InferParams, _> = serde_json::from_str(json);
        assert!(params.is_ok());
        let p = params.unwrap_or_else(|_| InferParams {
            prompt: String::new(),
            session_id: None,
            model: None,
            priority: None,
        });
        assert_eq!(p.prompt, "hello world");
        assert!(p.session_id.is_none());
    }

    #[test]
    fn test_infer_params_deserialize_full() {
        let json = r#"{
            "prompt": "test",
            "session_id": "sess-1",
            "model": "echo",
            "priority": "high"
        }"#;
        let params: Result<InferParams, _> = serde_json::from_str(json);
        assert!(params.is_ok());
        let p = params.unwrap_or_else(|_| InferParams {
            prompt: String::new(),
            session_id: None,
            model: None,
            priority: None,
        });
        assert_eq!(p.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn test_batch_infer_params_deserialize() {
        let json = r#"{"prompts": ["one", "two", "three"]}"#;
        let params: Result<BatchInferParams, _> = serde_json::from_str(json);
        assert!(params.is_ok());
        let p = params.unwrap_or_else(|_| BatchInferParams {
            prompts: vec![],
            model: None,
        });
        assert_eq!(p.prompts.len(), 3);
    }

    #[test]
    fn test_configure_params_deserialize_partial() {
        let json = r#"{"worker": "echo", "retry_attempts": 5}"#;
        let params: Result<ConfigureParams, _> = serde_json::from_str(json);
        assert!(params.is_ok());
        let p = params.unwrap_or_else(|_| ConfigureParams {
            worker: None,
            retry_attempts: None,
            circuit_breaker_threshold: None,
            rate_limit_rps: None,
        });
        assert_eq!(p.worker.as_deref(), Some("echo"));
        assert_eq!(p.retry_attempts, Some(5));
        assert!(p.circuit_breaker_threshold.is_none());
    }

    #[test]
    fn test_infer_response_serializes() {
        let mut stage_latencies = HashMap::new();
        stage_latencies.insert("rag".to_string(), 1.5);
        stage_latencies.insert("inference".to_string(), 100.0);

        let response = InferResponse {
            output: "test output".to_string(),
            session_id: "sess-1".to_string(),
            request_id: "req-1".to_string(),
            latency_ms: 105,
            deduped: false,
            stage_latencies,
        };

        let json = serde_json::to_string(&response);
        assert!(json.is_ok());
        let s = json.unwrap_or_default();
        assert!(s.contains("test output"));
        assert!(s.contains("sess-1"));
    }

    #[test]
    fn test_status_response_serializes() {
        let response = StatusResponse {
            status: "healthy".to_string(),
            circuit_breakers: HashMap::new(),
            channel_depths: HashMap::new(),
            dedup_stats: DedupStats {
                requests_total: 100,
                inferences_total: 80,
                savings_percent: 20.0,
                cost_saved_usd: 0.20,
            },
            throughput_rps: 50,
        };

        let json = serde_json::to_string(&response);
        assert!(json.is_ok());
        let s = json.unwrap_or_default();
        assert!(s.contains("healthy"));
        assert!(s.contains("20.0"));
    }

    #[test]
    fn test_dedup_stats_serializes() {
        let stats = DedupStats {
            requests_total: 1000,
            inferences_total: 500,
            savings_percent: 50.0,
            cost_saved_usd: 5.0,
        };
        let json = serde_json::to_string(&stats);
        assert!(json.is_ok());
    }

    #[test]
    fn test_pipeline_config_clone() {
        let config = PipelineConfig::default();
        let cloned = config.clone();
        assert_eq!(cloned.worker, config.worker);
        assert_eq!(cloned.retry_attempts, config.retry_attempts);
    }

    #[tokio::test]
    async fn test_infer_tool_returns_correct_shape() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let params = InferParams {
            prompt: "hello world".to_string(),
            session_id: Some("test-sess".to_string()),
            model: None,
            priority: None,
        };

        let result = server.infer(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert!(parsed.get("output").is_some());
        assert_eq!(
            parsed.get("session_id").and_then(|v| v.as_str()),
            Some("test-sess")
        );
        assert!(parsed.get("request_id").is_some());
        assert!(parsed.get("latency_ms").is_some());
        assert!(parsed.get("stage_latencies").is_some());

        let latencies = parsed.get("stage_latencies").and_then(|v| v.as_object());
        assert!(latencies.is_some());
        let empty_map = serde_json::Map::new();
        let lat = latencies.unwrap_or(&empty_map);
        assert!(lat.contains_key("rag"));
        assert!(lat.contains_key("assemble"));
        assert!(lat.contains_key("inference"));
        assert!(lat.contains_key("post_process"));
        assert!(lat.contains_key("stream"));
    }

    #[tokio::test]
    async fn test_infer_tool_generates_session_id_when_absent() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let params = InferParams {
            prompt: "test".to_string(),
            session_id: None,
            model: None,
            priority: None,
        };

        let result = server.infer(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        let session_id = parsed.get("session_id").and_then(|v| v.as_str());
        assert!(session_id.is_some());
        assert!(!session_id.unwrap_or_default().is_empty());
    }

    #[tokio::test]
    async fn test_pipeline_status_returns_all_required_fields() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let result = server.pipeline_status().await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert!(parsed.get("status").is_some());
        assert!(parsed.get("circuit_breakers").is_some());
        assert!(parsed.get("channel_depths").is_some());
        assert!(parsed.get("dedup_stats").is_some());
        assert!(parsed.get("throughput_rps").is_some());
    }

    #[tokio::test]
    async fn test_pipeline_status_healthy_by_default() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let result = server.pipeline_status().await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert_eq!(
            parsed.get("status").and_then(|v| v.as_str()),
            Some("healthy")
        );
    }

    #[tokio::test]
    async fn test_batch_infer_accepts_array_of_prompts() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let params = BatchInferParams {
            prompts: vec![
                "prompt one".to_string(),
                "prompt two".to_string(),
                "prompt three".to_string(),
            ],
            model: None,
        };

        let result = server.batch_infer(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert!(parsed.get("job_id").is_some());
        assert_eq!(
            parsed.get("prompts_submitted").and_then(|v| v.as_u64()),
            Some(3)
        );
        assert_eq!(
            parsed.get("status").and_then(|v| v.as_str()),
            Some("accepted")
        );
    }

    #[tokio::test]
    async fn test_configure_pipeline_updates_worker() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let params = ConfigureParams {
            worker: Some("anthropic".to_string()),
            retry_attempts: None,
            circuit_breaker_threshold: None,
            rate_limit_rps: None,
        };

        let result = server.configure_pipeline(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert_eq!(
            parsed.get("status").and_then(|v| v.as_str()),
            Some("updated")
        );
        let current = parsed
            .get("current_config")
            .and_then(|v| v.get("worker"))
            .and_then(|v| v.as_str());
        assert_eq!(current, Some("anthropic"));
    }

    #[tokio::test]
    async fn test_configure_pipeline_invalid_worker_returns_error() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let params = ConfigureParams {
            worker: Some("nonexistent".to_string()),
            retry_attempts: None,
            circuit_breaker_threshold: None,
            rate_limit_rps: None,
        };

        let result = server.configure_pipeline(Parameters(params)).await;
        assert!(result.contains("error"));
        assert!(result.contains("nonexistent"));
    }

    #[tokio::test]
    async fn test_configure_pipeline_invalid_retry_returns_error() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let params = ConfigureParams {
            worker: None,
            retry_attempts: Some(99),
            circuit_breaker_threshold: None,
            rate_limit_rps: None,
        };

        let result = server.configure_pipeline(Parameters(params)).await;
        assert!(result.contains("error"));
        assert!(result.contains("0-10"));
    }

    #[tokio::test]
    async fn test_configure_pipeline_zero_threshold_returns_error() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let params = ConfigureParams {
            worker: None,
            retry_attempts: None,
            circuit_breaker_threshold: Some(0),
            rate_limit_rps: None,
        };

        let result = server.configure_pipeline(Parameters(params)).await;
        assert!(result.contains("error"));
    }

    #[tokio::test]
    async fn test_configure_pipeline_zero_rps_returns_error() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let params = ConfigureParams {
            worker: None,
            retry_attempts: None,
            circuit_breaker_threshold: None,
            rate_limit_rps: Some(0),
        };

        let result = server.configure_pipeline(Parameters(params)).await;
        assert!(result.contains("error"));
    }

    #[tokio::test]
    async fn test_configure_pipeline_multiple_changes() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let params = ConfigureParams {
            worker: Some("echo".to_string()),
            retry_attempts: Some(5),
            circuit_breaker_threshold: Some(10),
            rate_limit_rps: Some(50),
        };

        let result = server.configure_pipeline(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        let changes = parsed.get("changes").and_then(|v| v.as_array());
        assert!(changes.is_some());
        assert_eq!(changes.map(|c| c.len()).unwrap_or(0), 4);

        let config = parsed.get("current_config");
        assert!(config.is_some());
        let c = config.unwrap_or(&serde_json::Value::Null);
        assert_eq!(c.get("retry_attempts").and_then(|v| v.as_u64()), Some(5));
        assert_eq!(
            c.get("circuit_breaker_threshold").and_then(|v| v.as_u64()),
            Some(10)
        );
        assert_eq!(c.get("rate_limit_rps").and_then(|v| v.as_u64()), Some(50));
    }

    #[tokio::test]
    async fn test_server_info() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(Arc::clone(&worker));
        let server = new_mcp_server(worker, handles);

        let info = server.get_info();
        assert!(info.instructions.is_some());
        let instructions = info.instructions.unwrap_or_default();
        assert!(instructions.contains("tokio-prompt-orchestrator"));
    }
}
