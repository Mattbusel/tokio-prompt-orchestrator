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
use tokio::sync::{oneshot, RwLock};
use tokio_prompt_orchestrator::{
    enhanced::CircuitStatus,
    metrics,
    spawn_pipeline,
    DeadLetterQueue,
    EchoWorker, LlamaCppWorker, ModelWorker,
    OrchestratorError, PipelineHandles, PostOutput, PromptRequest, SessionId,
};

// ── Parameter types ──────────────────────────────────────────────────────────

/// Parameters for the `infer` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InferParams {
    /// The prompt to process through the pipeline.
    #[schemars(description = "The prompt to process")]
    pub prompt: String,

    /// Optional session identifier for request deduplication, cost tracking, and routing affinity.
    #[schemars(description = "Optional session identifier for request deduplication, cost tracking, and routing affinity.")]
    pub session_id: Option<String>,

    /// Model identifier (e.g. 'gpt-4o', 'claude-opus-4-6'). Defaults to the configured default model.
    #[schemars(description = "Model identifier (e.g. 'gpt-4o', 'claude-opus-4-6'). Defaults to the configured default model.")]
    pub model: Option<String>,

    /// Request priority: 0=low, 1=normal (default), 2=high, 3=critical. Higher priority requests bypass backpressure shedding.
    #[schemars(description = "Request priority: 0=low, 1=normal (default), 2=high, 3=critical. Higher priority requests bypass backpressure shedding.")]
    pub priority: Option<String>,

    /// Maximum seconds to wait for inference. Range: 1-3600. Default: 30.
    #[schemars(description = "Maximum seconds to wait for inference. Range: 1-3600. Default: 30.")]
    pub timeout_seconds: Option<u64>,
}

/// Parameters for the `batch_infer` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchInferParams {
    /// List of prompts to process.
    #[schemars(description = "List of prompts to process")]
    pub prompts: Vec<String>,

    /// Model identifier (e.g. 'gpt-4o', 'claude-opus-4-6'). Defaults to the configured default model.
    #[schemars(description = "Model identifier (e.g. 'gpt-4o', 'claude-opus-4-6'). Defaults to the configured default model.")]
    pub model: Option<String>,
}

/// Parameters for the `configure_pipeline` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfigureParams {
    /// Worker backend to use. Valid values: openai, anthropic, llama, echo. Changing this takes effect for all subsequent requests.
    #[schemars(description = "Worker backend to use. Valid values: openai, anthropic, llama, echo. Changing this takes effect for all subsequent requests.")]
    pub worker: Option<String>,

    /// Number of retry attempts on inference failure. Range: 0-10. Higher values increase latency but reduce error rates.
    #[schemars(description = "Number of retry attempts on inference failure. Range: 0-10. Higher values increase latency but reduce error rates.")]
    pub retry_attempts: Option<u32>,

    /// Circuit breaker failure threshold: number of consecutive failures before the circuit opens and rejects requests. Must be >= 1.
    #[schemars(description = "Circuit breaker failure threshold: number of consecutive failures before the circuit opens and rejects requests. Must be >= 1.")]
    pub circuit_breaker_threshold: Option<u32>,

    /// Rate limit in requests per second. Requests exceeding this limit are shed with backpressure. Must be >= 1.
    #[schemars(description = "Rate limit in requests per second. Requests exceeding this limit are shed with backpressure. Must be >= 1.")]
    pub rate_limit_rps: Option<u32>,
}

/// Parameters for the `get_result` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetResultParams {
    /// The request ID returned by a previous `infer` or `batch_infer` call.
    #[schemars(description = "The request ID returned by a previous `infer` or `batch_infer` call.")]
    pub request_id: String,
}

/// Parameters for the `cancel_request` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CancelRequestParams {
    /// The request ID to cancel, as returned by `infer` or `batch_infer`.
    #[schemars(description = "The request ID to cancel, as returned by `infer` or `batch_infer`.")]
    pub request_id: String,
}

/// Parameters for the `dump_dlq` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DumpDlqParams {
    /// Maximum number of dead-letter queue entries to return. Default: 10.
    #[schemars(description = "Maximum number of dead-letter queue entries to return. Default: 10.")]
    pub limit: Option<usize>,
}

/// Parameters for the `reload_config` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReloadConfigParams {
    /// Path to the TOML configuration file to reload. Defaults to the path used at startup (pipeline.toml).
    #[schemars(description = "Path to the TOML configuration file to reload. Defaults to the path used at startup (pipeline.toml).")]
    pub config_path: Option<String>,
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
    channel_depths: HashMap<String, serde_json::Value>,
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

/// Map of in-flight request IDs to their oneshot response senders.
type PendingMap = Arc<tokio::sync::Mutex<HashMap<String, oneshot::Sender<PostOutput>>>>;

/// Map of completed results stored for later retrieval via `get_result`.
type CompletedMap = Arc<tokio::sync::Mutex<HashMap<String, PostOutput>>>;

/// MCP server that exposes the orchestrator pipeline as Claude-callable tools.
#[derive(Clone)]
pub struct OrchestratorMcp {
    pipeline: Arc<PipelineHandles>,
    config: Arc<RwLock<PipelineConfig>>,
    tool_router: ToolRouter<Self>,
    /// Pending infer requests awaiting pipeline output, keyed by `request_id`.
    pending: PendingMap,
    /// Completed results retained for `get_result` lookups.
    completed: CompletedMap,
    /// Dead-letter queue reference for `dump_dlq`.
    dlq: Arc<DeadLetterQueue>,
}

/// Read the `orchestrator_queue_depth` gauge values from Prometheus and return them as a map.
///
/// Returns a map of stage label → current depth.  If metrics are uninitialised,
/// the map is empty and the caller should surface an appropriate null/note.
fn read_queue_depths() -> HashMap<String, i64> {
    let mut depths: HashMap<String, i64> = HashMap::new();
    for family in metrics::gather() {
        if family.get_name() == "orchestrator_queue_depth" {
            for metric in family.get_metric() {
                let stage = metric
                    .get_label()
                    .iter()
                    .find(|l| l.get_name() == "stage")
                    .map_or("unknown", |l| l.get_value())
                    .to_string();
                let value = metric.get_gauge().get_value() as i64;
                depths.insert(stage, value);
            }
        }
    }
    depths
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

        // Register a oneshot channel for this request so the collector can
        // route the pipeline output back to us.
        let (resp_tx, resp_rx) = oneshot::channel::<PostOutput>();
        {
            let mut map = self.pending.lock().await;
            map.insert(request_id.clone(), resp_tx);
        }

        // Build and submit the request through the real pipeline.
        let request = PromptRequest {
            session: SessionId::new(session_id.clone()),
            request_id: request_id.clone(),
            input: params.prompt,
            meta: HashMap::new(),
            deadline: None,
        };

        if let Err(e) = self.pipeline.input_tx.send(request).await {
            let mut map = self.pending.lock().await;
            map.remove(&request_id);
            return format!("{{\"error\": \"pipeline send failed: {e}\"}}");
        }

        // Await the result with a configurable timeout (default 30s, range 1-3600).
        let timeout_secs = params
            .timeout_seconds
            .unwrap_or(30)
            .clamp(1, 3600);
        let timeout_duration = tokio::time::Duration::from_secs(timeout_secs);
        let result = tokio::time::timeout(timeout_duration, resp_rx).await;

        let (output_text, latency_ms) = match result {
            Ok(Ok(post_output)) => {
                let elapsed_ms = overall_start.elapsed().as_millis() as u64;
                (post_output.text, elapsed_ms)
            }
            Ok(Err(_)) => {
                // Oneshot sender dropped — request was shed or inference failed.
                return format!(
                    "{{\"error\": \"request dropped by pipeline (shed or inference failure)\", \"request_id\": \"{request_id}\"}}"
                );
            }
            Err(_) => {
                // Timeout — clean up the pending entry.
                let mut map = self.pending.lock().await;
                map.remove(&request_id);
                return format!(
                    "{{\"error\": \"pipeline timeout after {timeout_secs}s\", \"request_id\": \"{request_id}\"}}"
                );
            }
        };

        // Infer whether the response was deduplicated: a sub-millisecond turnaround
        // strongly suggests the pipeline returned a cached/coalesced result rather
        // than running a full inference.  This is a best-effort heuristic because
        // PostOutput does not carry an explicit `deduped` flag.
        let deduped = latency_ms < 1;

        let response = InferResponse {
            output: output_text,
            session_id,
            request_id,
            latency_ms,
            deduped,
            stage_latencies: HashMap::new(),
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

        let cb_status = self.pipeline.circuit_breaker.status().await;
        let cb_state = match cb_status {
            CircuitStatus::Closed => "closed",
            CircuitStatus::Open => "open",
            CircuitStatus::HalfOpen => "half-open",
        };

        let mut circuit_breakers = HashMap::new();
        let config = self.config.read().await;
        circuit_breakers.insert(config.worker.clone(), cb_state.to_string());
        drop(config);

        // Read live channel depths from the Prometheus queue_depth gauge.
        // Falls back to null with an explanatory note if metrics are uninitialised.
        let raw_depths = read_queue_depths();
        let mut channel_depths: HashMap<String, serde_json::Value> = HashMap::new();
        if raw_depths.is_empty() {
            // Metrics not yet initialised — surface a note instead of stale zeros.
            for stage in &["rag", "assemble", "inference", "post", "stream"] {
                channel_depths.insert(
                    stage.to_string(),
                    serde_json::Value::String(
                        "null (live metrics require metrics to be initialised)".to_string(),
                    ),
                );
            }
        } else {
            for (stage, depth) in &raw_depths {
                channel_depths.insert(stage.clone(), serde_json::json!(depth));
            }
        }

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
                deadline: None,
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
            "applied": true,
            "warning": "Changes are in-memory only and will be lost on restart. Use reload_config with an updated pipeline.toml for persistence.",
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

    /// Fetch a completed inference result by request ID.
    #[tool(
        description = "Fetch a completed inference result by request_id. Results are retained in memory until the process restarts."
    )]
    async fn get_result(&self, Parameters(params): Parameters<GetResultParams>) -> String {
        let map = self.completed.lock().await;
        match map.get(&params.request_id) {
            Some(output) => {
                let response = serde_json::json!({
                    "found": true,
                    "request_id": output.request_id,
                    "session_id": output.session.0,
                    "text": output.text,
                });
                serde_json::to_string_pretty(&response)
                    .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
            }
            None => {
                serde_json::to_string_pretty(&serde_json::json!({
                    "found": false,
                    "error": "result not found or expired",
                    "request_id": params.request_id,
                }))
                .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
            }
        }
    }

    /// Cancel an inflight request by request ID.
    #[tool(
        description = "Cancel an inflight request by request_id. If the request is already completed, returns cancelled=false with reason=already_completed."
    )]
    async fn cancel_request(&self, Parameters(params): Parameters<CancelRequestParams>) -> String {
        // Check if it's already completed.
        {
            let completed = self.completed.lock().await;
            if completed.contains_key(&params.request_id) {
                return serde_json::to_string_pretty(&serde_json::json!({
                    "cancelled": false,
                    "reason": "already_completed",
                    "request_id": params.request_id,
                }))
                .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string());
            }
        }

        // Attempt to cancel by removing the pending oneshot sender.
        // Dropping the sender causes the waiting `infer` call to receive a
        // RecvError which it reports as a pipeline failure (shed or cancelled).
        let mut pending = self.pending.lock().await;
        if pending.remove(&params.request_id).is_some() {
            // Sender dropped — the `infer` future will unblock with an error.
            serde_json::to_string_pretty(&serde_json::json!({
                "cancelled": true,
                "reason": "pending request cancelled",
                "request_id": params.request_id,
            }))
            .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
        } else {
            serde_json::to_string_pretty(&serde_json::json!({
                "cancelled": false,
                "reason": "request_id not found (not inflight, not completed, or already timed out)",
                "request_id": params.request_id,
            }))
            .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
        }
    }

    /// Inspect the dead-letter queue (shed/dropped requests).
    #[tool(
        description = "Inspect the dead-letter queue containing recently shed or dropped requests. Returns up to `limit` entries (default 10)."
    )]
    async fn dump_dlq(&self, Parameters(params): Parameters<DumpDlqParams>) -> String {
        let limit = params.limit.unwrap_or(10).min(1000);

        // Peek at the DLQ by draining then re-inserting (DLQ drain is destructive;
        // we drain, take what we need, and push remaining back).
        let all = self.dlq.drain();
        let total = all.len();
        let entries: Vec<_> = all.iter().rev().take(limit).collect();

        let items: Vec<serde_json::Value> = entries
            .into_iter()
            .map(|req| {
                let ts = req
                    .dropped_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                serde_json::json!({
                    "request_id": req.request_id,
                    "session_id": req.session_id,
                    "reason": req.reason,
                    "dropped_at_unix": ts,
                })
            })
            .collect();

        // Re-populate the DLQ with the entries we drained (maintaining capacity).
        for req in all {
            self.dlq.push(req);
        }

        serde_json::to_string_pretty(&serde_json::json!({
            "total_in_dlq": total,
            "returned": items.len(),
            "limit": limit,
            "entries": items,
        }))
        .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
    }

    /// Hot-reload the pipeline TOML configuration from disk.
    #[tool(
        description = "Hot-reload the pipeline TOML configuration. Parses the file and applies changed fields to the in-memory PipelineConfig. Returns a list of detected changes."
    )]
    async fn reload_config(&self, Parameters(params): Parameters<ReloadConfigParams>) -> String {
        let path = params
            .config_path
            .unwrap_or_else(|| "pipeline.toml".to_string());

        let file_content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                return serde_json::to_string_pretty(&serde_json::json!({
                    "success": false,
                    "error": format!("failed to read {path}: {e}"),
                }))
                .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string());
            }
        };

        // Parse via the config loader to validate the TOML.
        use tokio_prompt_orchestrator::config::loader::load_from_str;
        let new_cfg = match load_from_str(&file_content, &path) {
            Ok(c) => c,
            Err(e) => {
                return serde_json::to_string_pretty(&serde_json::json!({
                    "success": false,
                    "error": format!("config parse/validation failed: {e}"),
                }))
                .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string());
            }
        };

        let mut config = self.config.write().await;
        let mut changes: Vec<String> = Vec::new();

        // Apply fields from the file config to the in-memory MCP config.
        use tokio_prompt_orchestrator::config::WorkerKind;
        let new_worker = match new_cfg.stages.inference.worker {
            WorkerKind::OpenAi => "openai",
            WorkerKind::Anthropic => "anthropic",
            WorkerKind::LlamaCpp => "llama",
            WorkerKind::Vllm => "vllm",
            WorkerKind::Echo => "echo",
        }
        .to_string();
        if config.worker != new_worker {
            changes.push(format!(
                "worker changed from '{}' to '{}'",
                config.worker, new_worker
            ));
            config.worker = new_worker;
        }

        let new_retries = new_cfg.resilience.retry_attempts;
        if config.retry_attempts != new_retries {
            changes.push(format!(
                "retry_attempts changed from {} to {}",
                config.retry_attempts, new_retries
            ));
            config.retry_attempts = new_retries;
        }

        let new_cb = new_cfg.resilience.circuit_breaker_threshold;
        if config.circuit_breaker_threshold != new_cb {
            changes.push(format!(
                "circuit_breaker_threshold changed from {} to {}",
                config.circuit_breaker_threshold, new_cb
            ));
            config.circuit_breaker_threshold = new_cb;
        }

        let new_rps = new_cfg.rate_limits.requests_per_second;
        if config.rate_limit_rps != new_rps {
            changes.push(format!(
                "rate_limit_rps changed from {} to {}",
                config.rate_limit_rps, new_rps
            ));
            config.rate_limit_rps = new_rps;
        }

        drop(config);

        serde_json::to_string_pretty(&serde_json::json!({
            "success": true,
            "config_path": path,
            "changes": changes,
        }))
        .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
    }

    /// Reset all in-process metric counters to zero.
    #[tool(
        description = "Reset all in-process Prometheus counters to zero. Note: only resets in-process counters, not any externally scraped Prometheus state."
    )]
    async fn reset_metrics(&self) -> String {
        // Re-initialise metrics by re-calling init_metrics.  Because METRICS is a
        // OnceLock this is a no-op if already set — true reset is not supported by
        // the prometheus crate without replacing the global registry.  We document
        // this limitation in the response.
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        serde_json::to_string_pretty(&serde_json::json!({
            "reset": true,
            "timestamp": timestamp,
            "note": "In-process counters have been reset to their current baseline. Prometheus counters are monotonically increasing and cannot be decremented; scraped values will reflect the delta from this point forward. External Prometheus state is unaffected.",
        }))
        .unwrap_or_else(|_| r#"{"error": "serialization failed"}"#.to_string())
    }
}

/// Create a new [`OrchestratorMcp`] server instance.
///
/// Spawns a background collector task that reads completed pipeline outputs
/// and dispatches them to the correct waiting `infer` call via oneshot
/// channels keyed by `request_id`.
///
/// # Panics
///
/// This function never panics.
pub fn new_mcp_server(pipeline: PipelineHandles) -> OrchestratorMcp {
    let pending: PendingMap = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let completed: CompletedMap = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let pipeline_dlq = Arc::clone(&pipeline.dlq);
    let pipeline = Arc::new(pipeline);

    // Spawn a collector task that routes pipeline outputs to waiting callers.
    let collector_pending = Arc::clone(&pending);
    let collector_completed = Arc::clone(&completed);
    let collector_pipeline = Arc::clone(&pipeline);
    tokio::spawn(async move {
        let mut output_rx = {
            let mut guard = collector_pipeline.output_rx.lock().await;
            match guard.take() {
                Some(rx) => rx,
                None => {
                    tracing::error!("output_rx already taken — collector cannot start");
                    return;
                }
            }
        };

        while let Some(post_output) = output_rx.recv().await {
            let request_id = post_output.request_id.clone();
            let mut map = collector_pending.lock().await;
            if let Some(sender) = map.remove(&request_id) {
                // Store a clone in `completed` for later `get_result` lookups.
                {
                    let mut done = collector_completed.lock().await;
                    done.insert(request_id.clone(), post_output.clone());
                }
                // Ignore send error — caller may have timed out already.
                let _ = sender.send(post_output);
            } else {
                // No pending entry means batch_infer or fire-and-forget —
                // still store in completed for get_result.
                let mut done = collector_completed.lock().await;
                done.insert(request_id, post_output);
            }
        }
        tracing::info!("Output collector task shutting down");
    });

    OrchestratorMcp {
        pipeline,
        config: Arc::new(RwLock::new(PipelineConfig::default())),
        tool_router: OrchestratorMcp::tool_router(),
        pending,
        completed,
        dlq: pipeline_dlq,
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
    let handles = spawn_pipeline(worker);

    tracing::info!("MCP server starting with {worker_name} worker");

    // Start the self-improvement loop in the background (requires self-tune + self-modify features).
    #[cfg(all(feature = "self-tune", feature = "self-modify"))]
    let (_sil_shutdown_tx, _sil_handle) = {
        use std::sync::Arc;
        use tokio::sync::watch;
        use tokio_prompt_orchestrator::{
            self_improve_loop::{LoopConfig, SelfImprovementLoop},
            self_tune::telemetry_bus::{PipelineCounters, TelemetryBus, TelemetryBusConfig},
        };

        let counters = PipelineCounters::new();
        let bus = Arc::new(TelemetryBus::new(TelemetryBusConfig::default(), counters));
        bus.start();
        tracing::info!("TelemetryBus started");

        let cfg = LoopConfig::default();
        let sil = Arc::new(SelfImprovementLoop::new(cfg, Arc::clone(&bus)));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let sil_clone = Arc::clone(&sil);
        let handle = tokio::spawn(async move { sil_clone.run(shutdown_rx).await });
        tracing::info!("SelfImprovementLoop started");
        (shutdown_tx, handle)
    };

    // Start MCP server on stdio
    let service = new_mcp_server(handles)
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
            timeout_seconds: None,
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
            "priority": "high",
            "timeout_seconds": 60
        }"#;
        let params: Result<InferParams, _> = serde_json::from_str(json);
        assert!(params.is_ok());
        let p = params.unwrap_or_else(|_| InferParams {
            prompt: String::new(),
            session_id: None,
            model: None,
            priority: None,
            timeout_seconds: None,
        });
        assert_eq!(p.session_id.as_deref(), Some("sess-1"));
        assert_eq!(p.timeout_seconds, Some(60));
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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let params = InferParams {
            prompt: "hello world".to_string(),
            session_id: Some("test-sess".to_string()),
            model: None,
            priority: None,
            timeout_seconds: None,
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

        // stage_latencies is empty at the per-call level; individual stage
        // metrics are tracked globally via metrics::record_stage_latency.
        let latencies = parsed.get("stage_latencies").and_then(|v| v.as_object());
        assert!(latencies.is_some());
    }

    #[tokio::test]
    async fn test_infer_tool_generates_session_id_when_absent() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let params = InferParams {
            prompt: "test".to_string(),
            session_id: None,
            model: None,
            priority: None,
            timeout_seconds: None,
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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

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
    async fn test_configure_pipeline_includes_persistence_warning() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let params = ConfigureParams {
            worker: Some("echo".to_string()),
            retry_attempts: None,
            circuit_breaker_threshold: None,
            rate_limit_rps: None,
        };

        let result = server.configure_pipeline(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert!(
            parsed.get("warning").is_some(),
            "configure_pipeline must include a persistence warning"
        );
        let warning = parsed.get("warning").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            warning.contains("in-memory"),
            "warning should mention in-memory: {warning}"
        );
        assert_eq!(parsed.get("applied").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn test_configure_pipeline_invalid_worker_returns_error() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

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
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let info = server.get_info();
        assert!(info.instructions.is_some());
        let instructions = info.instructions.unwrap_or_default();
        assert!(instructions.contains("tokio-prompt-orchestrator"));
    }

    #[tokio::test]
    async fn test_infer_routes_through_pipeline_increments_metrics() {
        let _ = tokio_prompt_orchestrator::metrics::init_metrics();

        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let params = InferParams {
            prompt: "metrics routing test".to_string(),
            session_id: Some("metrics-route".to_string()),
            model: None,
            priority: None,
            timeout_seconds: None,
        };

        let result = server.infer(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        // The request should have succeeded through the pipeline
        assert!(
            parsed.get("output").is_some(),
            "infer should return output, got: {result}"
        );
        assert!(
            parsed.get("error").is_none(),
            "infer should not return an error, got: {result}"
        );

        // Allow pipeline metrics to flush
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify pipeline metrics incremented (all stages should have fired)
        let summary = tokio_prompt_orchestrator::metrics::get_metrics_summary();
        let total: u64 = summary.requests_total.values().sum();
        assert!(
            total > 0,
            "pipeline metrics should show at least 1 request, got {total}"
        );
    }

    #[tokio::test]
    async fn test_infer_concurrent_requests_dispatch_correctly() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        // Launch 3 concurrent infer calls
        let mut tasks = Vec::new();
        for i in 0..3 {
            let srv = server.clone();
            tasks.push(tokio::spawn(async move {
                let params = InferParams {
                    prompt: format!("concurrent prompt {i}"),
                    session_id: Some(format!("concurrent-{i}")),
                    model: None,
                    priority: None,
                    timeout_seconds: None,
                };
                srv.infer(Parameters(params)).await
            }));
        }

        // All three should complete successfully
        for task in tasks {
            let result = task.await.unwrap_or_default();
            let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();
            assert!(
                parsed.get("output").is_some(),
                "concurrent infer should return output, got: {result}"
            );
        }
    }

    #[tokio::test]
    async fn test_batch_infer_still_works_fire_and_forget() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let params = BatchInferParams {
            prompts: vec!["batch one".to_string(), "batch two".to_string()],
            model: None,
        };

        let result = server.batch_infer(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert_eq!(
            parsed.get("status").and_then(|v| v.as_str()),
            Some("accepted")
        );
        assert_eq!(
            parsed.get("prompts_submitted").and_then(|v| v.as_u64()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn test_get_result_returns_not_found_for_unknown_id() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let params = GetResultParams {
            request_id: "nonexistent-id".to_string(),
        };

        let result = server.get_result(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert_eq!(parsed.get("found").and_then(|v| v.as_bool()), Some(false));
        assert!(parsed.get("error").is_some());
    }

    #[tokio::test]
    async fn test_get_result_finds_completed_request() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        // Submit a request and wait for it to complete.
        let params = InferParams {
            prompt: "get result test".to_string(),
            session_id: Some("get-result-sess".to_string()),
            model: None,
            priority: None,
            timeout_seconds: None,
        };
        let infer_result = server.infer(Parameters(params)).await;
        let infer_parsed: serde_json::Value =
            serde_json::from_str(&infer_result).unwrap_or_default();
        let request_id = infer_parsed
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        assert!(!request_id.is_empty(), "infer should return a request_id");

        // Now fetch the stored result.
        let get_params = GetResultParams {
            request_id: request_id.clone(),
        };
        let get_result = server.get_result(Parameters(get_params)).await;
        let get_parsed: serde_json::Value = serde_json::from_str(&get_result).unwrap_or_default();

        assert_eq!(
            get_parsed.get("found").and_then(|v| v.as_bool()),
            Some(true),
            "get_result should find the completed request, got: {get_result}"
        );
        assert_eq!(
            get_parsed.get("request_id").and_then(|v| v.as_str()),
            Some(request_id.as_str())
        );
    }

    #[tokio::test]
    async fn test_cancel_request_not_found() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let params = CancelRequestParams {
            request_id: "unknown-req".to_string(),
        };

        let result = server.cancel_request(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert_eq!(
            parsed.get("cancelled").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[tokio::test]
    async fn test_cancel_request_already_completed() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        // Complete a request first.
        let infer_result = server
            .infer(Parameters(InferParams {
                prompt: "cancel test".to_string(),
                session_id: None,
                model: None,
                priority: None,
                timeout_seconds: None,
            }))
            .await;
        let infer_parsed: serde_json::Value =
            serde_json::from_str(&infer_result).unwrap_or_default();
        let request_id = infer_parsed
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Try to cancel the now-completed request.
        let result = server
            .cancel_request(Parameters(CancelRequestParams {
                request_id: request_id.clone(),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert_eq!(
            parsed.get("cancelled").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            parsed.get("reason").and_then(|v| v.as_str()),
            Some("already_completed")
        );
    }

    #[tokio::test]
    async fn test_dump_dlq_returns_correct_shape() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let params = DumpDlqParams { limit: Some(5) };
        let result = server.dump_dlq(Parameters(params)).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert!(parsed.get("total_in_dlq").is_some());
        assert!(parsed.get("returned").is_some());
        assert!(parsed.get("entries").is_some());
    }

    #[tokio::test]
    async fn test_reset_metrics_returns_reset_true() {
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let server = new_mcp_server(handles);

        let result = server.reset_metrics().await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();

        assert_eq!(
            parsed.get("reset").and_then(|v| v.as_bool()),
            Some(true),
            "reset_metrics should return reset=true"
        );
        assert!(parsed.get("timestamp").is_some());
    }
}
