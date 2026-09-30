//! OpenAI-compatible endpoints: `POST /v1/chat/completions` and `GET /v1/models`.
//!
//! Point any OpenAI client at `http://<host>:<port>/v1` and its chat requests
//! go through the orchestrator pipeline (RAG, assemble, inference behind the
//! circuit breaker and timeout, post, stream) like `/api/v1/infer` requests
//! do, with three additions at this layer:
//!
//! - **Deduplication.** Identical conversations share one upstream call while
//!   it is in flight, and a completed answer is reused for
//!   [`ServerConfig::dedup_window_secs`]. The `x-orchestrator-dedup` response
//!   header says which happened: `miss`, `joined` or `cached`.
//! - **Spend cap.** With [`ServerConfig::max_spend_usd`] set, estimated spend
//!   is tracked per upstream call and the endpoint answers 429
//!   `insufficient_quota` once the cap is reached.
//! - **Errors in OpenAI's shape.** A request the pipeline drops into the
//!   dead-letter queue is answered at once with a status that matches the
//!   reason (503 circuit open, 504 timeout, 502 provider error, 429 busy).
//!
//! The request's `model`, `temperature`, `max_tokens` and other sampling
//! fields are accepted and ignored: the pipeline's worker uses the model and
//! settings the orchestrator was started with, and that model is what the
//! response reports. Messages are flattened into one prompt (a single user
//! message is sent as is) and sent to the model verbatim.
//!
//! Streaming (`"stream": true`) returns standard `chat.completion.chunk`
//! Server-Sent Events ending with `data: [DONE]`. The pipeline produces the
//! whole answer before the first chunk is sent, so errors still get a proper
//! HTTP status instead of breaking a stream half way.

use super::*;

use crate::cost_estimator::CostEstimator;
use crate::enhanced::{DeduplicationResult, Deduplicator};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

/// How long a failure is kept so requests that joined the failed call get
/// the same error.
const FAILURE_MEMORY: Duration = Duration::from_secs(30);

/// How many times a request re-checks the dedup table after the call it
/// joined was cancelled.
const MAX_JOIN_ATTEMPTS: usize = 3;

/// What the pipeline did with one request: its text, or the dead-letter reason.
pub(super) type PipelineOutcome = Result<String, String>;

/// Per-server state for the OpenAI-compatible endpoints.
pub(super) struct OpenAiState {
    dedup: Deduplicator,
    /// Pipeline request id -> handler waiting for its outcome.
    waiters: DashMap<String, oneshot::Sender<PipelineOutcome>>,
    /// Dedup key -> recent failure, for requests that joined a failed call.
    failures: DashMap<String, (Instant, ApiError)>,
    /// Estimated USD spent on upstream calls made through this endpoint.
    spent_usd: parking_lot::Mutex<f64>,
    /// Unix time the server started; reported as `created` by `/v1/models`.
    started_unix: u64,
}

impl OpenAiState {
    /// Must be called inside a Tokio runtime (the deduplicator spawns a sweeper).
    pub(super) fn new(config: &ServerConfig) -> Self {
        Self {
            dedup: Deduplicator::new(Duration::from_secs(config.dedup_window_secs)),
            waiters: DashMap::new(),
            failures: DashMap::new(),
            spent_usd: parking_lot::Mutex::new(0.0),
            started_unix: unix_now(),
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
        if let Some(secs) = self.retry_after_secs {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

/// `true` for the paths served by this module (auth errors use OpenAI's shape).
pub(super) fn is_openai_path(path: &str) -> bool {
    path == "/v1/chat/completions" || path == "/v1/models"
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
struct ChatRequest {
    #[serde(default)]
    model: Option<String>,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    stream_options: Option<StreamOptions>,
    #[serde(default)]
    n: Option<u32>,
    #[serde(default)]
    user: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(default)]
    content: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamOptions {
    #[serde(default)]
    include_usage: bool,
}

/// Text of one message: a string, or the `text` parts of a content array.
fn message_text(msg: &ChatMessage) -> Result<String, ApiError> {
    match &msg.content {
        None | Some(serde_json::Value::Null) => Ok(String::new()),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(serde_json::Value::Array(parts)) => {
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

/// Flatten the conversation into the single prompt the pipeline carries.
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

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
}

impl DedupOutcome {
    fn as_str(self) -> &'static str {
        match self {
            DedupOutcome::Miss => "miss",
            DedupOutcome::Joined => "joined",
            DedupOutcome::Cached => "cached",
        }
    }
}

/// Submit one prompt to the pipeline and wait for its output or dead-letter reason.
async fn run_pipeline(
    state: &Arc<AppState>,
    request_id: &str,
    session: String,
    prompt: String,
) -> Result<String, ApiError> {
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

    let send_err = match state.pipeline_tx.try_send(req) {
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
        Ok(Ok(Ok(text))) => Ok(text),
        Ok(Ok(Err(reason))) => Err(ApiError::from_dlq_reason(&reason)),
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

/// Estimated USD cost of one upstream call (`echo` and `llama` are free).
fn estimate_cost(config: &ServerConfig, prompt_tokens: usize, completion_tokens: usize) -> f64 {
    match config.provider.as_str() {
        "echo" | "llama" => 0.0,
        _ => CostEstimator::new().cost_for_tokens(prompt_tokens, completion_tokens, &config.model),
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

/// Answer `prompt`, sharing identical in-flight or recent requests.
async fn complete(
    state: &Arc<AppState>,
    request_id: &str,
    session: String,
    prompt: String,
) -> Result<(String, DedupOutcome), ApiError> {
    let key = hex::encode(Sha256::digest(prompt.as_bytes()));

    for _ in 0..MAX_JOIN_ATTEMPTS {
        match state.openai.dedup.check_and_register(&key).await {
            DeduplicationResult::Cached(text) => return Ok((text, DedupOutcome::Cached)),
            DeduplicationResult::InProgress => match state.openai.dedup.wait_for_result(&key).await {
                Some(text) if !crate::enhanced::dedup::is_cancelled_result(&text) => {
                    return Ok((text, DedupOutcome::Joined));
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
                let rid = request_id.to_string();
                let task = tokio::spawn(async move {
                    let prompt_tokens = approx_tokens(&prompt);
                    let result = run_pipeline(&task_state, &rid, session, prompt).await;
                    match &result {
                        Ok(text) => {
                            let cost = estimate_cost(
                                &task_state.config,
                                prompt_tokens,
                                approx_tokens(text),
                            );
                            if cost > 0.0 {
                                *task_state.openai.spent_usd.lock() += cost;
                                crate::metrics::record_inference_cost(cost);
                            }
                            task_state.openai.failures.remove(&key);
                            task_state.openai.dedup.complete(token, text.clone()).await;
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
                    Ok(r) => r.map(|text| (text, DedupOutcome::Miss)),
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

    let req: ChatRequest = serde_json::from_slice(&body).map_err(|e| {
        ApiError::invalid(None, format!("Could not parse the request body as a chat completion request: {e}"))
    })?;
    if req.n.is_some_and(|n| n != 1) {
        return Err(ApiError::invalid(Some("n"), "Only n=1 is supported."));
    }
    let prompt = render_prompt(&req.messages)?;

    if let Some(cap) = state.config.max_spend_usd {
        let spent = *state.openai.spent_usd.lock();
        if spent >= cap {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "insufficient_quota",
                "insufficient_quota",
                format!(
                    "The orchestrator's spend cap of ${cap:.2} is reached (estimated spend ${spent:.4}). \
                     Restart it or raise --max-spend."
                ),
            ));
        }
    }

    let id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let session = req
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("openai-{id}"));
    debug!(
        request_id = %id,
        requested_model = req.model.as_deref().unwrap_or(""),
        "POST /v1/chat/completions"
    );

    let prompt_tokens = approx_tokens(&prompt);
    let (text, outcome) = complete(&state, &id, session, prompt).await?;
    let completion_tokens = approx_tokens(&text);
    let usage = serde_json::json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    });
    let created = unix_now();
    let model = state.config.model.clone();

    let mut resp = if req.stream.unwrap_or(false) {
        let include_usage = req.stream_options.unwrap_or_default().include_usage;
        let chunk = |delta: serde_json::Value, finish: Option<&str>| {
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
        let mut events: Vec<serde_json::Value> =
            vec![chunk(serde_json::json!({"role": "assistant", "content": ""}), None)];
        for piece in text.split_inclusive(' ') {
            events.push(chunk(serde_json::json!({"content": piece}), None));
        }
        events.push(chunk(serde_json::json!({}), Some("stop")));
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
        Json(serde_json::json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "system_fingerprint": null,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text, "refusal": null},
                "logprobs": null,
                "finish_reason": "stop",
            }],
            "usage": usage,
        }))
        .into_response()
    };
    resp.headers_mut().insert(
        "x-orchestrator-dedup",
        HeaderValue::from_static(outcome.as_str()),
    );
    Ok(resp)
}

/// `GET /v1/models`: the one model this orchestrator serves.
pub(super) async fn models_handler(State(state): State<Arc<AppState>>) -> Response {
    Json(serde_json::json!({
        "object": "list",
        "data": [{
            "id": state.config.model,
            "object": "model",
            "created": state.openai.started_unix,
            "owned_by": state.config.provider,
        }],
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: serde_json::Value) -> ChatMessage {
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
    }
}
