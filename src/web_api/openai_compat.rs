//! OpenAI-compatible endpoints: `POST /v1/chat/completions` and `GET /v1/models`,
//! plus the other client protocols built on the same machinery: the
//! Anthropic Messages API (`POST /v1/messages`, module `anthropic`), the
//! OpenAI Responses API (`POST /v1/responses`, module `responses`) and
//! OpenAI embeddings (`POST /v1/embeddings`, module `embeddings`).
//!
//! Point any OpenAI client at `http://<host>:<port>/v1` and its chat requests
//! go through the orchestrator pipeline (inference behind the circuit
//! breaker, timeout and retries, then post and stream) like `/api/v1/infer`
//! requests do, with these additions at this layer:
//!
//! - **Each request is honoured.** The request's messages (including
//!   assistant `tool_calls` and `tool` results), `temperature`, `top_p`,
//!   `max_tokens`, `max_completion_tokens`, `stop`, `seed`,
//!   `response_format`, `tools`, `tool_choice` and `parallel_tool_calls`
//!   are carried to the worker as a [`crate::ChatRequest`] and sent to the
//!   provider; tool calls come back as `tool_calls` with
//!   `finish_reason: "tool_calls"`. Only `n = 1` is supported. Nothing is
//!   capped by the orchestrator: without `max_tokens` the model's own limit
//!   applies (Anthropic, which requires a value, gets the route's
//!   `max_tokens`, 4096 by default). A provider's 4xx for a bad parameter
//!   comes back with the provider's status and message and does not count
//!   against the circuit breaker.
//! - **Model routing.** With [`ServerConfig::model_routes`] set, the
//!   request's `model` picks the upstream from a static `[[models]]` table
//!   of names and globs (`gpt-*`, `claude-*`, `llama-*`); each route has its
//!   own pipeline and breaker, and a glob route sends the client's model
//!   name upstream. A model that matches no route goes to the server's own
//!   pipeline (the default route: the single `--provider` / `--model`, which
//!   keeps using its configured model as before), or, with the default route
//!   turned off, is answered 400 `model_not_found` listing the configured
//!   models. See [`crate::model_routes`].
//! - **Deduplication.** The key is a SHA-256 over a canonical encoding of
//!   the route, model, messages, tools, tool choice, sampling settings and
//!   response format ([`crate::ChatRequest::dedup_key`]), so requests with
//!   different settings never share an answer. Identical requests share one
//!   upstream call while it is in flight. A completed answer is reused for
//!   [`ServerConfig::dedup_window_secs`], except when the request has
//!   `temperature > 0` and no `seed`: the client asked for a fresh sample,
//!   so such answers are shared only while in flight and never cached. The
//!   `x-orchestrator-dedup` response header says which happened: `miss`,
//!   `joined`, `cached` or `semantic`. Semantic dedup (with an embedder)
//!   only compares requests whose settings are identical.
//! - **Spend cap.** With [`ServerConfig::max_spend_usd`] set, estimated spend
//!   is tracked per upstream call and the endpoint answers 429
//!   `insufficient_quota` once the cap is reached.
//! - **Token usage.** `usage` is the provider's own count when it reports
//!   one; otherwise it is counted with the model's tokenizer for OpenAI
//!   models (feature `tiktoken`, on with `web-api`) or estimated at about 4
//!   characters per token. The spend cap uses the same counts.
//! - **Errors in OpenAI's shape.** A request the pipeline drops into the
//!   dead-letter queue is answered at once with a status that matches the
//!   reason (503 circuit open, 504 timeout, 502 provider error, 429 busy,
//!   the provider's own 4xx for a request it rejected).
//!
//! Workers without their own [`crate::ModelWorker::chat`] (the echo worker,
//! custom workers) get the messages flattened into one prompt (a single user
//! message is sent as is) and ignore the settings.
//!
//! Streaming (`"stream": true`) returns standard `chat.completion.chunk`
//! Server-Sent Events ending with `data: [DONE]`. The pipeline produces the
//! whole answer before the first chunk is sent, so errors still get a proper
//! HTTP status instead of breaking a stream half way.

use super::*;

use crate::chat::{ChatJobGuard, ChatRequest, ChatResponse};
use crate::cost_estimator::CostEstimator;
use crate::enhanced::{DeduplicationResult, Deduplicator};
use serde_json::Value;
use tokio::sync::oneshot;

mod anthropic;
pub(super) mod embeddings;
mod responses;

pub(super) use anthropic::{anthropic_unauthorized, count_tokens_handler, is_anthropic_path, messages_handler};
pub(super) use embeddings::embeddings_handler;
pub use embeddings::EmbeddingsBackend;
pub(super) use responses::responses_handler;

/// How long a failure is kept so requests that joined the failed call get
/// the same error.
const FAILURE_MEMORY: Duration = Duration::from_secs(30);

/// How many times a request re-checks the dedup table after the call it
/// joined was cancelled.
const MAX_JOIN_ATTEMPTS: usize = 3;

/// Most settings fingerprints that keep a semantic dedup index of their own.
const MAX_SEMANTIC_SCOPES: u64 = 256;

/// What the pipeline did with one request: its text, or the dead-letter reason.
pub(super) type PipelineOutcome = Result<String, String>;

/// Semantic dedup settings and one index per settings fingerprint, so a
/// reworded prompt is only matched against requests with identical settings.
struct SemanticScopes {
    embedder: Arc<dyn crate::embedding::Embedder>,
    threshold: f32,
    scopes: moka::sync::Cache<String, Deduplicator>,
}

/// Per-server state for the OpenAI-compatible endpoints.
pub(super) struct OpenAiState {
    /// Answers reusable for the dedup window.
    dedup: Deduplicator,
    /// Sampled requests (`temperature > 0`, no `seed`): shared while in
    /// flight, never cached.
    in_flight_only: Deduplicator,
    /// Present when the server has an embedder.
    semantic: Option<SemanticScopes>,
    window: Duration,
    /// Pipeline request id -> handler waiting for its outcome.
    waiters: DashMap<String, oneshot::Sender<PipelineOutcome>>,
    /// Dedup key -> recent failure, for requests that joined a failed call.
    failures: DashMap<String, (Instant, ApiError)>,
    /// Estimated USD spent on upstream calls made through this endpoint.
    spent_usd: parking_lot::Mutex<f64>,
    /// Unix time the server started; reported as `created` by `/v1/models`.
    started_unix: u64,
    /// Cache, in-flight table and per-route backends for `/v1/embeddings`.
    embeddings: embeddings::EmbeddingsState,
}

impl OpenAiState {
    /// Builds the proxy state; the dedup window comes from `config.dedup_window_secs`.
    pub(super) fn new(config: &ServerConfig) -> Self {
        let window = Duration::from_secs(config.dedup_window_secs);
        Self {
            dedup: Deduplicator::new(window),
            in_flight_only: Deduplicator::new(Duration::ZERO),
            semantic: config.embedder.as_ref().map(|embedder| SemanticScopes {
                embedder: Arc::clone(embedder),
                threshold: config.semantic_threshold,
                scopes: moka::sync::Cache::new(MAX_SEMANTIC_SCOPES),
            }),
            window,
            waiters: DashMap::new(),
            failures: DashMap::new(),
            spent_usd: parking_lot::Mutex::new(0.0),
            started_unix: unix_now(),
            embeddings: embeddings::EmbeddingsState::new(window),
        }
    }

    /// The deduplicator for `chat` sent to the route labelled `label`.
    fn dedup_for(&self, chat: &ChatRequest, label: &str) -> Deduplicator {
        if !chat.is_cacheable() {
            return self.in_flight_only.clone();
        }
        match &self.semantic {
            Some(s) => s.scopes.get_with(chat.settings_fingerprint(label), || {
                Deduplicator::new(self.window).with_embedder(Arc::clone(&s.embedder), s.threshold)
            }),
            None => self.dedup.clone(),
        }
    }

    /// Deliver a pipeline outcome to the handler waiting on `request_id`, if any.
    pub(super) fn resolve(&self, request_id: &str, outcome: PipelineOutcome) {
        if let Some((_, tx)) = self.waiters.remove(request_id) {
            let _ = tx.send(outcome);
        }
    }

    fn remember_failure(&self, key: &str, err: &ApiError) {
        let now = Instant::now();
        self.failures
            .retain(|_, (at, _)| now.duration_since(*at) < FAILURE_MEMORY);
        self.failures.insert(key.to_string(), (now, err.clone()));
    }

    fn recent_failure(&self, key: &str) -> Option<ApiError> {
        self.failures
            .get(key)
            .filter(|f| f.0.elapsed() < FAILURE_MEMORY)
            .map(|f| f.1.clone())
    }
}

// ============================================================================
// Errors
// ============================================================================

/// An error rendered in OpenAI's shape:
/// `{"error": {"message", "type", "param", "code"}}`.
#[derive(Debug, Clone)]
pub(super) struct ApiError {
    status: StatusCode,
    message: String,
    kind: &'static str,
    param: Option<&'static str>,
    code: &'static str,
    retry_after_secs: Option<u64>,
    /// Sent as `x-should-retry`, the header the OpenAI and Anthropic SDKs
    /// check before their own retry logic. `Some(false)` stops their
    /// automatic retries; `None` leaves the header out.
    should_retry: Option<bool>,
}

impl ApiError {
    fn new(status: StatusCode, kind: &'static str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            kind,
            param: None,
            code,
            retry_after_secs: None,
            should_retry: None,
        }
    }

    fn invalid(param: Option<&'static str>, message: impl Into<String>) -> Self {
        Self {
            param,
            ..Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", "invalid_request", message)
        }
    }

    fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    fn should_retry(mut self, retry: bool) -> Self {
        self.should_retry = Some(retry);
        self
    }

    /// Error for a request the pipeline dropped, with what the circuit
    /// breaker knows: a refusal carries `Retry-After` set to when the
    /// breaker lets the next probe through.
    fn from_dlq(reason: &str, breaker: &crate::enhanced::CircuitBreaker) -> Self {
        let err = Self::from_dlq_reason(reason);
        if err.code == "circuit_open" {
            if let Some(wait) = breaker.retry_after_sync() {
                // Round up: a client that waits this long finds the probe due.
                let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                return err.retry_after(secs.max(1));
            }
        }
        err
    }

    /// Add `Retry-After` and `x-should-retry` to `resp`.
    fn add_headers(&self, resp: &mut Response) {
        if let Some(secs) = self.retry_after_secs {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        if let Some(retry) = self.should_retry {
            resp.headers_mut().insert(
                "x-should-retry",
                HeaderValue::from_static(if retry { "true" } else { "false" }),
            );
        }
    }

    /// Map a dead-letter reason from the pipeline to an HTTP error.
    fn from_dlq_reason(reason: &str) -> Self {
        if reason == "circuit_breaker_open" {
            Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "circuit_open",
                "The upstream model is failing, so the orchestrator's circuit breaker is open and \
                 this request was refused without calling it. Retry later.",
            )
            // Retrying at once only hits the open breaker again, so tell
            // SDK clients not to (they would otherwise retry twice).
            .should_retry(false)
        } else if let Some(rest) = reason.strip_prefix("upstream_rejected:") {
            // `upstream_rejected:<status>:<provider message>`
            let (status, message) = rest.split_once(':').unwrap_or(("400", rest));
            let status = status
                .parse::<u16>()
                .ok()
                .and_then(|s| StatusCode::from_u16(s).ok())
                .filter(|s| s.is_client_error())
                .unwrap_or(StatusCode::BAD_REQUEST);
            Self::new(
                status,
                "invalid_request_error",
                "upstream_rejected",
                format!("The upstream provider rejected the request: {message}"),
            )
            .should_retry(false)
        } else if let Some(t) = reason.strip_prefix("inference_timeout:") {
            Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                "server_error",
                "upstream_timeout",
                format!("The upstream model did not answer within {t}."),
            )
        } else if reason == "deadline_expired" {
            Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                "server_error",
                "deadline_expired",
                "The request's deadline passed before the model could be called.",
            )
        } else if reason.starts_with("backpressure:") {
            Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "server_busy",
                format!("The orchestrator is at capacity ({reason}). Retry shortly."),
            )
            .retry_after(5)
        } else if let Some(detail) = reason.strip_prefix("inference_failure:") {
            if detail.contains("rate limited") {
                Self::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limit_error",
                    "upstream_rate_limited",
                    format!("The upstream provider is rate limiting: {detail}"),
                )
            } else {
                Self::new(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "upstream_error",
                    format!("The upstream model call failed: {detail}"),
                )
            }
        } else {
            Self::new(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "pipeline_dropped",
                format!("The pipeline dropped the request: {reason}"),
            )
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "param": self.param,
                "code": self.code,
            }
        });
        let mut resp = (self.status, Json(body)).into_response();
        self.add_headers(&mut resp);
        resp
    }
}

/// `true` for the paths served by this module (auth errors use OpenAI's shape).
pub(super) fn is_openai_path(path: &str) -> bool {
    matches!(path, "/v1/chat/completions" | "/v1/models" | "/v1/responses" | "/v1/embeddings")
}

/// The 401 an OpenAI client expects for a missing or wrong key.
pub(super) fn unauthorized() -> Response {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_request_error",
        "invalid_api_key",
        "Missing or incorrect API key. Send the orchestrator's key (ORCHESTRATOR_API_KEY) \
         as 'Authorization: Bearer <key>'.",
    )
    .into_response()
}

// ============================================================================
// Request types
// ============================================================================

#[derive(Debug, Deserialize)]
struct ChatBody {
    #[serde(default)]
    model: Option<String>,
    messages: Vec<Value>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    stream_options: Option<StreamOptions>,
    #[serde(default)]
    n: Option<u32>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    max_completion_tokens: Option<u32>,
    #[serde(default)]
    stop: Option<Value>,
    #[serde(default)]
    seed: Option<i64>,
    #[serde(default)]
    response_format: Option<Value>,
    #[serde(default)]
    tools: Option<Vec<Value>>,
    #[serde(default)]
    tool_choice: Option<Value>,
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
}

/// A message as the prompt renderer sees it: role and content.
#[derive(Debug)]
struct ChatMessage {
    role: String,
    content: Option<Value>,
}

impl ChatMessage {
    fn from_value(v: &Value) -> Result<Self, ApiError> {
        let role = v
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid(Some("messages"), "Every message needs a string 'role'."))?;
        Ok(Self {
            role: role.to_string(),
            content: v.get("content").cloned(),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct StreamOptions {
    #[serde(default)]
    include_usage: bool,
}

/// Text of one message: a string, or the `text` parts of a content array.
fn message_text(msg: &ChatMessage) -> Result<String, ApiError> {
    match &msg.content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = Vec::new();
            for part in parts {
                match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            out.push(t.to_string());
                        }
                    }
                    other => {
                        return Err(ApiError::invalid(
                            Some("messages"),
                            format!(
                                "Content part type '{}' is not supported; only text is.",
                                other.unwrap_or("unknown")
                            ),
                        ))
                    }
                }
            }
            Ok(out.join("\n"))
        }
        Some(_) => Err(ApiError::invalid(
            Some("messages"),
            "Message content must be a string or an array of content parts.",
        )),
    }
}

/// Validate the conversation and flatten it into the single prompt the
/// pipeline carries (and workers without `chat` receive).
///
/// A lone user message is sent exactly as written; anything longer becomes a
/// `Role: text` transcript separated by blank lines.
fn render_prompt(messages: &[ChatMessage]) -> Result<String, ApiError> {
    if messages.is_empty() {
        return Err(ApiError::invalid(Some("messages"), "'messages' must contain at least one message."));
    }
    let mut turns = Vec::with_capacity(messages.len());
    for m in messages {
        let label = match m.role.as_str() {
            "system" | "developer" => "System",
            "user" => "User",
            "assistant" => "Assistant",
            "tool" | "function" => "Tool",
            other => {
                return Err(ApiError::invalid(
                    Some("messages"),
                    format!("Unknown message role '{other}'."),
                ))
            }
        };
        turns.push((label, message_text(m)?));
    }
    if turns.len() == 1 && turns[0].0 == "User" {
        let only = turns.remove(0).1;
        if only.trim().is_empty() {
            return Err(ApiError::invalid(Some("messages"), "The message content is empty."));
        }
        return Ok(only);
    }
    let prompt = turns
        .iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(label, text)| format!("{label}: {text}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    if prompt.trim().is_empty() {
        return Err(ApiError::invalid(Some("messages"), "All messages are empty."));
    }
    Ok(prompt)
}

/// Check the shape of the settings that are passed through untyped.
fn validate_settings(body: &ChatBody) -> Result<(), ApiError> {
    if let Some(stop) = &body.stop {
        let ok = match stop {
            Value::String(_) => true,
            Value::Array(items) => items.iter().all(Value::is_string),
            _ => false,
        };
        if !ok {
            return Err(ApiError::invalid(Some("stop"), "'stop' must be a string or an array of strings."));
        }
    }
    if body.response_format.as_ref().is_some_and(|f| !f.is_object()) {
        return Err(ApiError::invalid(Some("response_format"), "'response_format' must be an object."));
    }
    if let Some(tools) = &body.tools {
        if tools.iter().any(|t| !t.is_object()) {
            return Err(ApiError::invalid(Some("tools"), "Every entry of 'tools' must be an object."));
        }
    }
    if body
        .tool_choice
        .as_ref()
        .is_some_and(|c| !(c.is_string() || c.is_object()))
    {
        return Err(ApiError::invalid(Some("tool_choice"), "'tool_choice' must be a string or an object."));
    }
    if body.temperature.is_some_and(|t| !t.is_finite() || t < 0.0) {
        return Err(ApiError::invalid(Some("temperature"), "'temperature' must be a number of at least 0."));
    }
    if body.top_p.is_some_and(|p| !p.is_finite() || p < 0.0) {
        return Err(ApiError::invalid(Some("top_p"), "'top_p' must be a number of at least 0."));
    }
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ============================================================================
// Routing
// ============================================================================

/// Where one request goes.
#[derive(Debug, Clone)]
struct Target {
    /// Route index in [`ServerConfig::model_routes`]; `None` is the server's
    /// own pipeline (the default route).
    route: Option<usize>,
    /// Part of the dedup key, so routes never share answers.
    label: String,
    /// Model name sent upstream; empty means the worker's own model.
    upstream_model: String,
    /// Model name reported back to the client.
    reported_model: String,
    /// Provider kind, for cost and capability checks.
    provider: String,
}

/// Pick the route for a request asking for `requested`.
fn resolve_target(state: &AppState, requested: Option<&str>) -> Result<Target, ApiError> {
    let requested = requested.map(str::trim).filter(|m| !m.is_empty());
    let routes = state.config.model_routes.as_deref();
    let default_on = routes.is_none_or(|r| r.has_default_route());
    let default = || Target {
        route: None,
        label: "default".to_string(),
        upstream_model: String::new(),
        reported_model: state.config.model.clone(),
        provider: state.config.provider.clone(),
    };
    let Some(routes) = routes else {
        return Ok(default());
    };
    if let Some(model) = requested {
        if default_on && model.eq_ignore_ascii_case(&state.config.model) {
            return Ok(default());
        }
        if let Some(i) = routes.resolve(model) {
            if let Some(route) = routes.route(i) {
                let upstream = route.upstream_model(model);
                return Ok(Target {
                    route: Some(i),
                    label: format!("route:{}", route.name.to_ascii_lowercase()),
                    reported_model: upstream.clone(),
                    upstream_model: upstream,
                    provider: route.provider.as_str().to_string(),
                });
            }
        }
    }
    if default_on {
        return Ok(default());
    }
    let configured: Vec<String> = routes.routes().map(|r| r.name.clone()).collect();
    let message = match requested {
        Some(model) => format!(
            "The model '{model}' is not served by this orchestrator. Configured models: {}.",
            configured.join(", ")
        ),
        None => format!(
            "'model' is required. Configured models: {}.",
            configured.join(", ")
        ),
    };
    Err(ApiError {
        param: Some("model"),
        ..ApiError::new(StatusCode::BAD_REQUEST, "invalid_request_error", "model_not_found", message)
    })
}

/// Reject settings the target's provider cannot honour.
fn check_capabilities(target: &Target, chat: &ChatRequest) -> Result<(), ApiError> {
    if target.provider == "anthropic" {
        let format = chat
            .response_format
            .as_ref()
            .and_then(|f| f.get("type"))
            .and_then(Value::as_str);
        if format.is_some_and(|t| t != "text") {
            return Err(ApiError::invalid(
                Some("response_format"),
                "This model is served by Anthropic's Messages API, which has no response_format. \
                 Ask for JSON in the prompt or use a tool instead.",
            ));
        }
    }
    Ok(())
}

// ============================================================================
// Running one completion through the pipeline
// ============================================================================

/// How a request was answered, reported in the `x-orchestrator-dedup` header.
#[derive(Clone, Copy)]
enum DedupOutcome {
    /// This request made the upstream call.
    Miss,
    /// Shared an identical request that was already in flight.
    Joined,
    /// Reused a completed answer from the dedup window.
    Cached,
    /// Reused the answer to a differently worded prompt with the same
    /// meaning (semantic dedup), at this cosine similarity.
    Semantic(f32),
}

impl DedupOutcome {
    fn as_str(self) -> &'static str {
        match self {
            DedupOutcome::Miss => "miss",
            DedupOutcome::Joined => "joined",
            DedupOutcome::Cached => "cached",
            DedupOutcome::Semantic(_) => "semantic",
        }
    }
}

/// Submit one chat request to its pipeline and wait for the answer or the
/// dead-letter reason.
async fn run_pipeline(
    state: &Arc<AppState>,
    target: &Target,
    request_id: &str,
    session: String,
    prompt: String,
    chat: ChatRequest,
) -> Result<ChatResponse, ApiError> {
    let route_handles = target
        .route
        .and_then(|i| state.config.model_routes.as_ref().and_then(|r| r.pipeline(i)));
    let (pipeline_tx, breaker) = match route_handles {
        Some(h) => (&h.input_tx, &h.circuit_breaker),
        None => (&state.pipeline_tx, &state.circuit_breaker),
    };

    if !state.tracker.try_insert(
        request_id.to_string(),
        TrackedRequest {
            status: RequestStatus::Processing,
            result: None,
            error: None,
            completed_at: None,
        },
    ) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "tracker_full",
            "The orchestrator is tracking too many requests. Retry shortly.",
        )
        .retry_after(5));
    }

    // Register before sending so a fast pipeline cannot answer before we listen.
    let job = ChatJobGuard::register(request_id, chat);
    let (tx, rx) = oneshot::channel();
    state.openai.waiters.insert(request_id.to_string(), tx);

    let mut meta = HashMap::new();
    meta.insert(crate::META_PROMPT_MODE.to_string(), "raw".to_string());
    meta.insert("endpoint".to_string(), "openai_chat_completions".to_string());
    let req = PromptRequest {
        session: SessionId::new(session),
        request_id: request_id.to_string(),
        input: prompt,
        meta,
        deadline: None,
    };

    let send_err = match pipeline_tx.try_send(req) {
        Ok(()) => None,
        Err(mpsc::error::TrySendError::Full(_)) => Some(
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "server_busy",
                "The orchestrator's queue is full. Retry shortly.",
            )
            .retry_after(5),
        ),
        Err(mpsc::error::TrySendError::Closed(_)) => Some(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "pipeline_closed",
            "The orchestrator pipeline has stopped.",
        )),
    };
    if let Some(err) = send_err {
        state.openai.waiters.remove(request_id);
        state.tracker.status.remove(request_id);
        return Err(err);
    }

    let timeout = Duration::from_secs(state.config.timeout_seconds);
    match tokio::time::timeout(timeout, rx).await {
        // The worker's structured answer; a pipeline whose inference stage
        // does not run chat requests leaves only the text.
        Ok(Ok(Ok(text))) => Ok(job.take_response().unwrap_or_else(|| ChatResponse::from_text(text))),
        Ok(Ok(Err(reason))) => Err(ApiError::from_dlq(&reason, breaker)),
        Ok(Err(_closed)) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "pipeline_closed",
            "The orchestrator stopped before the request finished.",
        )),
        Err(_elapsed) => {
            state.openai.waiters.remove(request_id);
            if let Some(mut t) = state.tracker.status.get_mut(request_id) {
                t.status = RequestStatus::Timeout;
                t.completed_at = Some(Instant::now());
            }
            Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "server_error",
                "timeout",
                format!("No answer within {} seconds.", state.config.timeout_seconds),
            ))
        }
    }
}

/// Estimated USD cost of one upstream call (`echo`, `llama` and
/// `llama_cpp` are free).
fn estimate_cost(provider: &str, model: &str, prompt_tokens: usize, completion_tokens: usize) -> f64 {
    match provider {
        "echo" | "llama" | "llama_cpp" => 0.0,
        _ => CostEstimator::new().cost_for_tokens(prompt_tokens, completion_tokens, model),
    }
}

/// Approximate token count (about 4 characters per token), 0 for empty text.
fn approx_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        CostEstimator::estimate_tokens(text)
    }
}

/// Prompt tokens of the flattened prompt: exact for OpenAI models (content
/// plus message framing, counted with the model's own tokenizer), else the
/// 4-characters-per-token estimate. Used when the provider reports no usage.
fn prompt_tokens(model: &str, prompt: &str) -> usize {
    if prompt.is_empty() {
        return 0;
    }
    crate::token_counter::exact_chat_prompt_tokens(model, &[("user", prompt)])
        .unwrap_or_else(|| approx_tokens(prompt))
}

/// Completion tokens of an answer: exact for OpenAI models, else estimated.
fn completion_tokens(model: &str, text: &str) -> usize {
    crate::token_counter::exact_token_count(model, text).unwrap_or_else(|| approx_tokens(text))
}

/// Token counts for one answer: the provider's when it reported them,
/// else counted or estimated.
fn usage_for(model: &str, prompt: &str, response: &ChatResponse) -> (usize, usize) {
    match response.usage {
        Some(u) => (
            usize::try_from(u.prompt_tokens).unwrap_or(usize::MAX),
            usize::try_from(u.completion_tokens).unwrap_or(usize::MAX),
        ),
        None => {
            let mut completion_text = response.text().to_string();
            for call in &response.tool_calls {
                completion_text.push_str(&call.to_string());
            }
            (prompt_tokens(model, prompt), completion_tokens(model, &completion_text))
        }
    }
}

/// Encode an answer for the dedup cache.
fn encode_answer(response: &ChatResponse) -> String {
    serde_json::to_string(response).unwrap_or_else(|_| response.text().to_string())
}

/// Decode an answer from the dedup cache.
fn decode_answer(text: &str) -> ChatResponse {
    serde_json::from_str(text).unwrap_or_else(|_| ChatResponse::from_text(text))
}

/// Answer `chat`, sharing identical in-flight or recent requests.
async fn complete(
    state: &Arc<AppState>,
    target: &Target,
    request_id: &str,
    session: String,
    prompt: String,
    chat: ChatRequest,
) -> Result<(ChatResponse, DedupOutcome), ApiError> {
    let key = chat.dedup_key(&target.label);
    let dedup = state.openai.dedup_for(&chat, &target.label);

    for _ in 0..MAX_JOIN_ATTEMPTS {
        let (found, similar) = dedup.check_and_register_semantic(&key, &prompt).await;
        match found {
            DeduplicationResult::Cached(text) => {
                let outcome = match similar {
                    Some(m) => DedupOutcome::Semantic(m.similarity),
                    None => DedupOutcome::Cached,
                };
                return Ok((decode_answer(&text), outcome));
            }
            DeduplicationResult::InProgress => match dedup.wait_for_result(&key).await {
                Some(text) if !crate::enhanced::dedup::is_cancelled_result(&text) => {
                    return Ok((decode_answer(&text), DedupOutcome::Joined));
                }
                _ => {
                    if let Some(err) = state.openai.recent_failure(&key) {
                        return Err(err);
                    }
                    // The call we joined was cancelled; try again.
                    continue;
                }
            },
            DeduplicationResult::New(token) => {
                // Run the upstream call in its own task so a client that
                // disconnects does not cancel it for everyone who joined.
                let task_state = Arc::clone(state);
                let task_target = target.clone();
                let task_dedup = dedup.clone();
                let rid = request_id.to_string();
                let key = key.clone();
                let task = tokio::spawn(async move {
                    let result = run_pipeline(
                        &task_state,
                        &task_target,
                        &rid,
                        session,
                        prompt.clone(),
                        chat,
                    )
                    .await;
                    match &result {
                        Ok(response) => {
                            let (p, c) = usage_for(&task_target.reported_model, &prompt, response);
                            let cost =
                                estimate_cost(&task_target.provider, &task_target.reported_model, p, c);
                            if cost > 0.0 {
                                *task_state.openai.spent_usd.lock() += cost;
                                crate::metrics::record_inference_cost(cost);
                            }
                            task_state.openai.failures.remove(&key);
                            task_dedup.complete(token, encode_answer(response)).await;
                        }
                        Err(err) => {
                            task_state.openai.remember_failure(&key, err);
                            // Dropping the token wakes joined requests and
                            // clears the key so the next request retries.
                            drop(token);
                        }
                    }
                    result
                });
                return match task.await {
                    Ok(r) => r.map(|response| (response, DedupOutcome::Miss)),
                    Err(e) => Err(ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "internal_error",
                        format!("Internal error: {e}"),
                    )),
                };
            }
        }
    }
    Err(ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "server_error",
        "dedup_retry_exhausted",
        "An identical request kept being cancelled. Retry.",
    )
    .retry_after(1))
}

/// 429 once the spend cap is reached.
fn check_spend_cap(state: &AppState, kind: &'static str) -> Result<(), ApiError> {
    if let Some(cap) = state.config.max_spend_usd {
        let spent = *state.openai.spent_usd.lock();
        if spent >= cap {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                kind,
                "insufficient_quota",
                format!(
                    "The orchestrator's spend cap of ${cap:.2} is reached (estimated spend ${spent:.4}). \
                     Restart it or raise --max-spend."
                ),
            ));
        }
    }
    Ok(())
}

/// Set the dedup headers on a response.
fn add_dedup_headers(resp: &mut Response, outcome: DedupOutcome) {
    resp.headers_mut().insert(
        "x-orchestrator-dedup",
        HeaderValue::from_static(outcome.as_str()),
    );
    if let DedupOutcome::Semantic(similarity) = outcome {
        if let Ok(v) = HeaderValue::from_str(&format!("{similarity:.4}")) {
            resp.headers_mut().insert("x-orchestrator-similarity", v);
        }
    }
}

// ============================================================================
// Handlers
// ============================================================================

/// `POST /v1/chat/completions`: OpenAI Chat Completions, JSON or SSE.
pub(super) async fn chat_completions_handler(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    match chat_completions(state, body).await {
        Ok(resp) => resp,
        Err(err) => err.into_response(),
    }
}

async fn chat_completions(state: Arc<AppState>, body: axum::body::Bytes) -> Result<Response, ApiError> {
    if state.shutting_down.load(AtomicOrdering::Relaxed) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "shutting_down",
            "The orchestrator is shutting down.",
        ));
    }

    let req: ChatBody = serde_json::from_slice(&body).map_err(|e| {
        ApiError::invalid(None, format!("Could not parse the request body as a chat completion request: {e}"))
    })?;
    if req.n.is_some_and(|n| n != 1) {
        return Err(ApiError::invalid(Some("n"), "Only n=1 is supported."));
    }
    let views = req
        .messages
        .iter()
        .map(ChatMessage::from_value)
        .collect::<Result<Vec<_>, _>>()?;
    let prompt = render_prompt(&views)?;
    validate_settings(&req)?;
    let target = resolve_target(&state, req.model.as_deref())?;

    let mut chat = ChatRequest::new(target.upstream_model.clone(), req.messages);
    chat.temperature = req.temperature;
    chat.top_p = req.top_p;
    chat.max_tokens = req.max_tokens;
    chat.max_completion_tokens = req.max_completion_tokens;
    chat.stop = req.stop;
    chat.seed = req.seed;
    chat.response_format = req.response_format;
    chat.tools = req.tools.filter(|t| !t.is_empty());
    // tool_choice and parallel_tool_calls without tools are dropped: OpenAI
    // answers 400 for either one alone, and SDK wrappers often send them
    // with an empty tool list.
    if chat.tools.is_some() {
        chat.tool_choice = req.tool_choice;
        chat.parallel_tool_calls = req.parallel_tool_calls;
    }
    check_capabilities(&target, &chat)?;
    check_spend_cap(&state, "insufficient_quota")?;

    let id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let session = req
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("openai-{id}"));
    debug!(
        request_id = %id,
        requested_model = req.model.as_deref().unwrap_or(""),
        route = %target.label,
        "POST /v1/chat/completions"
    );

    let (answer, outcome) = complete(&state, &target, &id, session, prompt.clone(), chat).await?;
    let (prompt_tokens, completion_tokens) = usage_for(&target.reported_model, &prompt, &answer);
    let usage = serde_json::json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    });
    let created = unix_now();
    let model = target.reported_model.clone();
    let finish_reason = answer.finish_reason.clone().unwrap_or_else(|| {
        if answer.tool_calls.is_empty() { "stop".to_string() } else { "tool_calls".to_string() }
    });
    let text = answer.text().to_string();

    let mut resp = if req.stream.unwrap_or(false) {
        let include_usage = req.stream_options.unwrap_or_default().include_usage;
        let chunk = |delta: Value, finish: Option<&str>| {
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "system_fingerprint": null,
                "choices": [{
                    "index": 0,
                    "delta": delta,
                    "logprobs": null,
                    "finish_reason": finish,
                }],
            })
        };
        let first_content = if answer.content.is_some() { Value::from("") } else { Value::Null };
        let mut events: Vec<Value> =
            vec![chunk(serde_json::json!({"role": "assistant", "content": first_content}), None)];
        for piece in text.split_inclusive(' ') {
            events.push(chunk(serde_json::json!({"content": piece}), None));
        }
        if !answer.tool_calls.is_empty() {
            let calls: Vec<Value> = answer
                .tool_calls
                .iter()
                .enumerate()
                .map(|(i, call)| {
                    let mut c = call.clone();
                    if let Value::Object(map) = &mut c {
                        map.insert("index".into(), Value::from(i));
                    }
                    c
                })
                .collect();
            events.push(chunk(serde_json::json!({"tool_calls": calls}), None));
        }
        events.push(chunk(serde_json::json!({}), Some(&finish_reason)));
        if include_usage {
            events.push(serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "system_fingerprint": null,
                "choices": [],
                "usage": usage,
            }));
        }
        let frames = events
            .into_iter()
            .map(|v| Event::default().data(v.to_string()))
            .chain(std::iter::once(Event::default().data("[DONE]")))
            .map(Ok::<_, std::convert::Infallible>);
        Sse::new(stream::iter(frames)).into_response()
    } else {
        let mut message = serde_json::json!({
            "role": "assistant",
            "content": answer.content,
            "refusal": null,
        });
        if !answer.tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(answer.tool_calls.clone());
        }
        Json(serde_json::json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "system_fingerprint": null,
            "choices": [{
                "index": 0,
                "message": message,
                "logprobs": null,
                "finish_reason": finish_reason,
            }],
            "usage": usage,
        }))
        .into_response()
    };
    add_dedup_headers(&mut resp, outcome);
    Ok(resp)
}

/// `GET /v1/models`: the default route's model (when the default route is
/// on) and every configured route. A glob route is listed by its pattern
/// (e.g. `gpt-*`).
pub(super) async fn models_handler(State(state): State<Arc<AppState>>) -> Response {
    let created = state.openai.started_unix;
    let routes = state.config.model_routes.as_deref();
    let mut data = Vec::new();
    if routes.is_none_or(|r| r.has_default_route()) {
        data.push(serde_json::json!({
            "id": state.config.model,
            "object": "model",
            "created": created,
            "owned_by": state.config.provider,
        }));
    }
    for route in routes.into_iter().flat_map(|r| r.routes()) {
        let id = route.model.clone().filter(|_| !route.is_glob()).unwrap_or_else(|| route.name.clone());
        data.push(serde_json::json!({
            "id": id,
            "object": "model",
            "created": created,
            "owned_by": route.provider.as_str(),
        }));
    }
    Json(serde_json::json!({"object": "list", "data": data})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: Value) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: Some(content),
        }
    }

    #[test]
    fn single_user_message_is_sent_verbatim() {
        let p = render_prompt(&[msg("user", serde_json::json!("Hi there"))]);
        assert_eq!(p.ok().as_deref(), Some("Hi there"));
    }

    #[test]
    fn conversation_becomes_transcript() {
        let p = render_prompt(&[
            msg("system", serde_json::json!("Be brief.")),
            msg("user", serde_json::json!([{"type": "text", "text": "Hi"}])),
        ]);
        assert_eq!(p.ok().as_deref(), Some("System: Be brief.\n\nUser: Hi"));
    }

    #[test]
    fn rendered_prompt_matches_the_chat_transcript() {
        let wire = vec![
            serde_json::json!({"role": "system", "content": "Be brief."}),
            serde_json::json!({"role": "user", "content": "Hi"}),
            serde_json::json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1"}]}),
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "content": "42"}),
        ];
        let views: Vec<ChatMessage> = wire.iter().map(|v| ChatMessage::from_value(v).expect("view")).collect();
        let rendered = render_prompt(&views).expect("renders");
        assert_eq!(rendered, ChatRequest::new("m", wire).transcript());
    }

    #[test]
    fn image_parts_are_rejected() {
        let p = render_prompt(&[msg(
            "user",
            serde_json::json!([{"type": "image_url", "image_url": {"url": "x"}}]),
        )]);
        assert!(p.is_err());
    }

    #[test]
    fn dlq_reasons_map_to_statuses() {
        assert_eq!(ApiError::from_dlq_reason("circuit_breaker_open").status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(ApiError::from_dlq_reason("inference_timeout:120s").status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(ApiError::from_dlq_reason("backpressure:rag").status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(ApiError::from_dlq_reason("inference_failure:boom").status, StatusCode::BAD_GATEWAY);
        let rejected = ApiError::from_dlq_reason("upstream_rejected:404:The model `x` does not exist");
        assert_eq!(rejected.status, StatusCode::NOT_FOUND);
        assert_eq!(rejected.code, "upstream_rejected");
        assert!(rejected.message.contains("does not exist"));
        assert_eq!(
            ApiError::from_dlq_reason("upstream_rejected:500:odd").status,
            StatusCode::BAD_REQUEST,
            "only client errors keep their status"
        );
    }

    #[test]
    fn settings_are_validated() {
        let body = |v: Value| -> ChatBody { serde_json::from_value(v).expect("parses") };
        let base = serde_json::json!({"messages": [{"role": "user", "content": "x"}]});
        assert!(validate_settings(&body(base.clone())).is_ok());
        let mut bad = base.clone();
        bad["stop"] = serde_json::json!([1, 2]);
        assert!(validate_settings(&body(bad)).is_err());
        let mut bad = base.clone();
        bad["response_format"] = serde_json::json!("json");
        assert!(validate_settings(&body(bad)).is_err());
        let mut bad = base;
        bad["temperature"] = serde_json::json!(-1.0);
        assert!(validate_settings(&body(bad)).is_err());
    }
}
