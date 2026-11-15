//! Model worker abstraction and implementations
//!
//! Provides the ModelWorker trait and production-ready implementations:
//! - EchoWorker: Testing/demo worker
//! - OpenAiWorker: OpenAI API (GPT-4, GPT-3.5, etc.)
//! - AnthropicWorker: Anthropic Claude API
//! - LlamaCppWorker: Local llama.cpp server
//! - VllmWorker: vLLM inference server
//!
//! ## Environment Variables
//!
//! - `OPENAI_API_KEY`: Required for OpenAiWorker
//! - `ANTHROPIC_API_KEY`: Required for AnthropicWorker
//! - `LLAMA_CPP_URL`: llama.cpp server URL (default: http://localhost:8080)
//! - `VLLM_URL`: vLLM server URL (default: http://localhost:8000)
//!
//! ## Design note: workers are pipeline components
//!
//! Workers are designed to be used through the pipeline orchestrated in
//! `stages.rs`, not called directly in production code.  The pipeline provides
//! circuit breaking, backpressure, dead-letter queuing, and timeout handling
//! around every worker call.
//!
//! If you call a worker directly (e.g. in tests or custom integrations), you
//! must supply your own retry, timeout, and backoff strategy.
//!
//! # Note (debug builds only)
//!
//! In debug builds (`cfg(debug_assertions)`) consider adding assertions that
//! verify a pipeline context is present when calling workers directly, to
//! catch accidental direct use in integration tests.

use crate::chat::{ChatRequest, ChatResponse, ChatUsage};
use crate::{metrics, OrchestratorError};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Boxed streaming token iterator returned by `infer_stream`.
pub type TokenStream =
    Pin<Box<dyn Stream<Item = Result<String, OrchestratorError>> + Send + 'static>>;

/// Parse a `Retry-After` header value (seconds integer or HTTP-date) into
/// a `Duration`.  Returns `None` if the header is absent or unparseable.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    // Prefer integer seconds; fall back to a fixed 60 s if it's an HTTP-date.
    let secs: u64 = value.trim().parse().unwrap_or(60);
    Some(Duration::from_secs(secs))
}

/// Read `x-ratelimit-remaining-requests` and log a warning when low.
fn warn_if_low_remaining(headers: &reqwest::header::HeaderMap, provider: &str) {
    if let Some(val) = headers
        .get("x-ratelimit-remaining-requests")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        if val < 10 {
            tracing::warn!(
                provider = provider,
                remaining_requests = val,
                "approaching provider rate limit"
            );
        }
    }
}

/// Trait for model inference workers
///
/// Implementations must be thread-safe (Send + Sync) for use across tasks.
/// The trait is object-safe to allow dynamic dispatch via `Arc<dyn ModelWorker>`.
///
/// # Resilience
///
/// Worker implementations do **not** include retry logic. Retries are handled
/// by the pipeline's inference stage (see `stages::spawn_inference_pool`). If you
/// call a worker directly outside of the pipeline, you are responsible for
/// implementing appropriate retry, timeout, and backoff logic.
#[async_trait]
pub trait ModelWorker: Send + Sync {
    /// Perform inference on the given prompt.
    ///
    /// Returns tokens as a vector of strings.
    /// For streaming implementations, this should be the final token set.
    ///
    /// An empty token vector is valid but callers should treat it as an empty response.
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError>;

    /// Stream inference tokens as they arrive from the provider.
    ///
    /// The default implementation calls `infer` and yields each token in order,
    /// so workers that don't override this still work with streaming consumers.
    /// Override for true SSE/chunked streaming from the provider.
    async fn infer_stream(&self, prompt: &str) -> Result<TokenStream, OrchestratorError> {
        let tokens = self.infer(prompt).await?;
        let stream = futures::stream::iter(tokens.into_iter().map(Ok));
        Ok(Box::pin(stream))
    }

    /// Answer a structured chat request: the client's messages, model and
    /// settings (`temperature`, `top_p`, `max_tokens`, `stop`, `seed`,
    /// `response_format`, `tools`, `tool_choice`, ...).
    ///
    /// `OpenAiWorker`, `AnthropicWorker` and `LlamaCppWorker` send all of it
    /// to the provider and return the provider's text, tool calls, finish
    /// reason and usage. An empty `request.model` means the worker's own
    /// model. Settings the request leaves unset fall back to the worker's
    /// configured defaults (and are left out when there is none).
    ///
    /// The default implementation, used by workers that do not override it,
    /// flattens the messages with [`ChatRequest::transcript`] and calls
    /// [`infer`](Self::infer); the settings are ignored there.
    ///
    /// The pipeline calls this instead of `infer` for requests that carry a
    /// chat request (every `POST /v1/chat/completions` call), inside the same
    /// circuit breaker, timeout and retries.
    ///
    /// [`ChatRequest::transcript`]: crate::ChatRequest::transcript
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, OrchestratorError> {
        let tokens = self.infer(&request.transcript()).await?;
        Ok(ChatResponse::from_text(tokens.join(" ")))
    }
}

// ============================================================================
// Chat helpers shared by the HTTP workers
// ============================================================================

/// Longest provider error message carried into an error value.
const MAX_ERROR_MESSAGE: usize = 2000;

/// Turn a non-success provider response into the right error: 429 is
/// rate limiting, 401/403 an auth failure, other client errors (except 408
/// and 409, which are transient) an [`OrchestratorError::UpstreamRejected`]
/// carrying the provider's own message, and everything else a retryable
/// [`OrchestratorError::Inference`].
async fn chat_http_error(response: reqwest::Response, provider: &str) -> OrchestratorError {
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = parse_retry_after(response.headers()).unwrap_or(Duration::from_secs(60));
        return OrchestratorError::RateLimited {
            retry_after_secs: retry_after.as_secs(),
        };
    }
    let body = response.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return OrchestratorError::AuthFailed(format!("HTTP {status}"));
    }
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message").or(Some(e)))
                .and_then(|m| m.as_str().map(str::to_string))
        })
        .unwrap_or(body);
    let message: String = message.chars().take(MAX_ERROR_MESSAGE).collect();
    let transient = matches!(status.as_u16(), 408 | 409);
    if status.is_client_error() && !transient {
        OrchestratorError::UpstreamRejected {
            status: status.as_u16(),
            message,
        }
    } else {
        OrchestratorError::Inference(format!("{provider} API error {status}: {message}"))
    }
}

/// POST an OpenAI Chat Completions body and parse the first choice.
async fn post_openai_chat(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    timeout: Duration,
    body: &serde_json::Value,
    provider: &str,
) -> Result<ChatResponse, OrchestratorError> {
    let mut req = client.post(url).timeout(timeout).json(body);
    if let Some(key) = bearer.filter(|k| !k.is_empty()) {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let response = req
        .send()
        .await
        .map_err(|e| OrchestratorError::Inference(format!("{provider} request failed: {e}")))?;
    warn_if_low_remaining(response.headers(), provider);
    if !response.status().is_success() {
        return Err(chat_http_error(response, provider).await);
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|e| OrchestratorError::Inference(format!("Failed to parse {provider} response: {e}")))?;
    ChatResponse::from_openai(&value)
        .map_err(|e| OrchestratorError::Inference(format!("{provider} response: {e}")))
}

/// Stream tokens from any [`ModelWorker`].
///
/// Spawns inference in a background task and returns a [`tokio::sync::mpsc::Receiver`].
/// Tokens arrive one at a time; the channel closes when inference completes or errors.
///
/// The default channel capacity is 64. For workers that return many tokens you may
/// want a larger buffer, but 64 is sufficient for typical LLM token-by-token delivery.
///
/// # Example
/// ```no_run
/// # use std::sync::Arc;
/// # use tokio_prompt_orchestrator::{worker::{EchoWorker, ModelWorker}, worker::stream_worker};
/// # tokio_test::block_on(async {
/// let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
/// let mut rx = stream_worker(worker, "hello world".to_string());
/// while let Some(result) = rx.recv().await {
///     match result {
///         Ok(token) => print!("{token}"),
///         Err(e) => eprintln!("error: {e}"),
///     }
/// }
/// # });
/// ```
pub fn stream_worker(
    worker: Arc<dyn ModelWorker>,
    prompt: String,
) -> tokio::sync::mpsc::Receiver<Result<String, OrchestratorError>> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    // Task is intentionally fire-and-forget; errors are logged above.
    let _stream_task = tokio::spawn(async move {
        match worker.infer(&prompt).await {
            Ok(tokens) => {
                for token in tokens {
                    if tx.send(Ok(token)).await.is_err() {
                        // Receiver was dropped; stop sending.
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "stream_worker: inference failed");
                let _ = tx.send(Err(e)).await;
            }
        }
    });
    rx
}

// ============================================================================
// Echo Worker (Testing)
// ============================================================================

/// Dummy echo worker for testing
///
/// Simply splits the prompt into words and returns them as tokens.
/// Useful for pipeline smoke tests without real model dependencies.
pub struct EchoWorker {
    /// Simulated inference delay
    pub delay_ms: u64,
}

impl EchoWorker {
    /// Create a new `EchoWorker` with a default 10 ms simulated delay.
    ///
    /// The echo worker requires no API keys or external services. It splits the
    /// prompt on whitespace and returns each word as a token, making it ideal
    /// for pipeline smoke tests and local development.
    ///
    /// # Errors
    ///
    /// This constructor never returns an error.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_prompt_orchestrator::worker::EchoWorker;
    /// use std::sync::Arc;
    ///
    /// # tokio_test::block_on(async {
    /// let worker = Arc::new(EchoWorker::new());
    /// // Use worker with spawn_pipeline or directly
    /// # });
    /// ```
    pub fn new() -> Self {
        Self { delay_ms: 10 }
    }

    /// Create a new `EchoWorker` with a custom simulated inference delay in milliseconds.
    pub fn with_delay(delay_ms: u64) -> Self {
        Self { delay_ms }
    }
}

impl Default for EchoWorker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelWorker for EchoWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        // Simulate inference latency
        tokio::time::sleep(tokio::time::Duration::from_millis(self.delay_ms)).await;

        // Echo back the prompt as tokens
        let tokens: Vec<String> = prompt.split_whitespace().map(str::to_string).collect();

        Ok(tokens)
    }
}

// ============================================================================
// OpenAI Worker
// ============================================================================

/// OpenAI API request payload
#[derive(Debug, Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    /// Omitted unless set, so the provider's own limit applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    temperature: f32,
}

#[derive(Debug, Serialize)]
struct OpenAiMessage {
    role: String,
    content: String,
}

/// OpenAI chat/completions API response
#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponseMessage {
    content: String,
}

/// OpenAI API worker (GPT-4, GPT-3.5-turbo-instruct, etc.)
///
/// Build it with [`OpenAiWorker::with_key`] when the key comes from your own
/// config or secret store, or [`OpenAiWorker::from_env`] to read
/// `OPENAI_API_KEY`. [`OpenAiWorker::with_http_client`] shares your
/// `reqwest::Client` (and its connection pool, proxy and TLS settings).
///
/// No `max_tokens` is sent unless you set one with
/// [`OpenAiWorker::with_max_tokens`], so replies are not cut short by a
/// default of ours.
///
/// ## Example
///
/// ```no_run
/// # use tokio_prompt_orchestrator::{OpenAiWorker, OrchestratorError};
/// # use std::sync::Arc;
/// # fn example() -> Result<(), OrchestratorError> {
/// let worker = Arc::new(
///     OpenAiWorker::new("gpt-3.5-turbo-instruct")?
///         .with_max_tokens(512)
///         .with_temperature(0.7)
/// );
/// # Ok(()) }
/// ```
///
/// # Resilience
///
/// `OpenAiWorker` does not retry internally. Retry logic is handled by the
/// pipeline's inference stage. See [`ModelWorker`] for details.
#[derive(Debug)]
pub struct OpenAiWorker {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: Option<u32>,
    temperature: f32,
    /// `true` once `with_temperature` was called; only then does
    /// [`ModelWorker::chat`] send a temperature the client left unset.
    temperature_set: bool,
    timeout: Duration,
    /// API base URL — override for OpenAI-compatible endpoints or testing.
    base_url: String,
}

impl OpenAiWorker {
    /// Create a new `OpenAiWorker` for the given model, reading the API key
    /// from `OPENAI_API_KEY`. Same as [`from_env`](Self::from_env).
    ///
    /// Default settings: no `max_tokens` (the provider's limit applies),
    /// temperature 0.7, 30 s timeout. All defaults are overridable via the
    /// builder methods.
    ///
    /// # Environment Variables
    ///
    /// | Variable | Required | Description |
    /// |----------|----------|-------------|
    /// | `OPENAI_API_KEY` | Yes | Bearer token sent with every request |
    ///
    /// # Errors
    ///
    /// Returns [`OrchestratorError::ConfigError`] if `OPENAI_API_KEY` is not
    /// set in the environment.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_prompt_orchestrator::OpenAiWorker;
    /// use std::sync::Arc;
    ///
    /// // OPENAI_API_KEY must be set in the environment.
    /// let worker = Arc::new(
    ///     OpenAiWorker::new("gpt-4o")
    ///         .expect("OPENAI_API_KEY must be set")
    ///         .with_max_tokens(512)
    ///         .with_temperature(0.7),
    /// );
    /// ```
    pub fn new(model: impl Into<String>) -> Result<Self, OrchestratorError> {
        Self::from_env(model)
    }

    /// Create a worker for `model` that reads its API key from the
    /// `OPENAI_API_KEY` environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`OrchestratorError::ConfigError`] if `OPENAI_API_KEY` is not
    /// set.
    pub fn from_env(model: impl Into<String>) -> Result<Self, OrchestratorError> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            OrchestratorError::ConfigError("OPENAI_API_KEY environment variable not set".into())
        })?;
        Ok(Self::with_key(model, api_key))
    }

    /// Create a worker for `model` with an API key you supply, from a vault,
    /// a config file or per tenant. Touches no environment variable.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_prompt_orchestrator::OpenAiWorker;
    ///
    /// let http = reqwest::Client::new(); // your existing client
    /// let worker = OpenAiWorker::with_key("gpt-4o-mini", "sk-from-your-vault")
    ///     .with_http_client(http)
    ///     .with_base_url("http://localhost:8080/v1");
    /// ```
    pub fn with_key(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: None,
            temperature: 0.7,
            temperature_set: false,
            timeout: Duration::from_secs(30),
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    /// Send requests through `client` instead of a client of our own, to
    /// share its connection pool, proxy and TLS settings. The per-request
    /// timeout set by [`with_timeout`](Self::with_timeout) still applies.
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Cap the reply at `max_tokens`. Unset by default: the request then
    /// carries no `max_tokens` and the model's own limit applies.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set temperature (0.0 – 2.0 for OpenAI).
    ///
    /// Values outside `[0.0, 2.0]` are clamped and a `WARN`-level log line is
    /// emitted.  The OpenAI API rejects values outside this range with HTTP 400.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        if !(0.0..=2.0).contains(&temperature) {
            tracing::warn!(
                temperature = temperature,
                "OpenAI temperature out of range [0.0, 2.0] — clamping"
            );
            self.temperature = temperature.clamp(0.0, 2.0);
        } else {
            self.temperature = temperature;
        }
        self.temperature_set = true;
        self
    }

    /// Set request timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the API base URL.
    ///
    /// Useful for OpenAI-compatible endpoints (Azure OpenAI, Groq, local proxies)
    /// and for pointing at a mock server in tests.
    /// Default: `"https://api.openai.com/v1"`.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[async_trait]
impl ModelWorker for OpenAiWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let _infer_start = Instant::now();
        let request = OpenAiRequest {
            model: self.model.clone(),
            messages: vec![OpenAiMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            max_tokens: self.max_tokens,
            temperature: self.temperature,
        };

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| OrchestratorError::Inference(format!("OpenAI request failed: {}", e)))?;

        warn_if_low_remaining(response.headers(), "openai");

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs =
                parse_retry_after(response.headers()).unwrap_or(Duration::from_secs(60));
            return Err(OrchestratorError::RateLimited {
                retry_after_secs: retry_after_secs.as_secs(),
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| String::new());
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(OrchestratorError::AuthFailed(format!("HTTP {status}")));
            }
            return Err(OrchestratorError::Inference(format!(
                "OpenAI API error {}: {}",
                status, error_text
            )));
        }

        let api_response: OpenAiResponse = response.json().await.map_err(|e| {
            OrchestratorError::Inference(format!("Failed to parse response: {}", e))
        })?;

        if api_response.choices.is_empty() {
            return Err(OrchestratorError::Inference(
                "No choices in OpenAI response".to_string(),
            ));
        }

        // Return the full content as a single token rather than splitting on
        // whitespace (which loses punctuation context).  Filter out
        // whitespace-only responses so callers see an empty vec rather than
        // a single blank token.
        let content = api_response
            .choices
            .first()
            .ok_or_else(|| {
                OrchestratorError::Inference("No choices in OpenAI response".to_string())
            })?
            .message
            .content
            .clone();

        let result = if content.trim().is_empty() {
            Ok(vec![])
        } else {
            Ok(vec![content])
        };
        tracing::debug!(
            worker = "openai",
            model = %self.model,
            latency_ms = %_infer_start.elapsed().as_millis(),
            "inference completed"
        );
        result
    }

    /// Sends the request to `{base_url}/chat/completions` as the client
    /// wrote it: messages (with tool calls and tool results), `model` (the
    /// worker's own when empty), `temperature`, `top_p`, `max_tokens`,
    /// `max_completion_tokens`, `stop`, `seed`, `response_format`, `tools`,
    /// `tool_choice` and `parallel_tool_calls`. A temperature or reply cap
    /// set on the worker applies only when the client sent none.
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, OrchestratorError> {
        let start = Instant::now();
        let temperature = self.temperature_set.then_some(f64::from(self.temperature));
        let body = request.to_openai_body(&self.model, temperature, self.max_tokens);
        let url = format!("{}/chat/completions", self.base_url);
        let result =
            post_openai_chat(&self.client, &url, Some(&self.api_key), self.timeout, &body, "openai")
                .await;
        tracing::debug!(
            worker = "openai",
            model = %body["model"].as_str().unwrap_or(""),
            latency_ms = %start.elapsed().as_millis(),
            ok = result.is_ok(),
            "chat completed"
        );
        result
    }

    async fn infer_stream(&self, prompt: &str) -> Result<TokenStream, OrchestratorError> {
        use futures::StreamExt;

        // OpenAI SSE streaming: POST with stream=true, parse `data: {...}` chunks.
        #[derive(Deserialize)]
        struct StreamChunk {
            choices: Vec<StreamChoice>,
        }
        #[derive(Deserialize)]
        struct StreamChoice {
            delta: StreamDelta,
        }
        #[derive(Deserialize)]
        struct StreamDelta {
            #[serde(default)]
            content: Option<String>,
        }

        let mut request = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": self.temperature,
            "stream": true
        });
        if let Some(max_tokens) = self.max_tokens {
            request["max_tokens"] = max_tokens.into();
        }

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                OrchestratorError::Inference(format!("OpenAI stream request failed: {e}"))
            })?;

        warn_if_low_remaining(response.headers(), "openai");

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs =
                parse_retry_after(response.headers()).unwrap_or(Duration::from_secs(60));
            return Err(OrchestratorError::RateLimited {
                retry_after_secs: retry_after_secs.as_secs(),
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(OrchestratorError::AuthFailed(format!("HTTP {status}")));
            }
            return Err(OrchestratorError::Inference(format!(
                "OpenAI stream error {status}: {body}"
            )));
        }

        // Convert the raw byte stream into SSE token chunks.
        let request_start = Instant::now();
        let model_name = self.model.clone();
        let byte_stream = response.bytes_stream();
        let token_stream = byte_stream.filter_map(|chunk| async move {
            let bytes = chunk.ok()?;
            let text = std::str::from_utf8(&bytes).ok()?;
            // SSE lines look like: `data: {...}` or `data: [DONE]`
            let mut tokens = Vec::new();
            for line in text.lines() {
                let Some(json_str) = line.strip_prefix("data: ") else {
                    continue;
                };
                if json_str.trim() == "[DONE]" {
                    break;
                }
                if let Ok(chunk) = serde_json::from_str::<StreamChunk>(json_str) {
                    for choice in chunk.choices {
                        if let Some(content) = choice.delta.content {
                            if !content.is_empty() {
                                tokens.push(content);
                            }
                        }
                    }
                }
            }
            if tokens.is_empty() {
                None
            } else {
                Some(Ok(tokens.join("")))
            }
        });

        // Wrap the stream to record TTFT on the very first token.
        let mut first_token_seen = false;
        let ttft_stream = token_stream.map(move |item| {
            if !first_token_seen {
                first_token_seen = true;
                metrics::record_ttft("openai", &model_name, request_start.elapsed());
            }
            item
        });

        Ok(Box::pin(ttft_stream))
    }
}

// ============================================================================
// Anthropic Worker
// ============================================================================

/// Anthropic Claude API worker
///
/// Requires ANTHROPIC_API_KEY environment variable.
///
/// ## Example
///
/// ```no_run
/// # use tokio_prompt_orchestrator::{AnthropicWorker, OrchestratorError};
/// # use std::sync::Arc;
/// # fn example() -> Result<(), OrchestratorError> {
/// let worker = Arc::new(
///     AnthropicWorker::new("claude-3-5-sonnet-20241022")?
///         .with_max_tokens(1024)
///         .with_temperature(1.0)
/// );
/// # Ok(()) }
/// ```
///
/// # Resilience
///
/// `AnthropicWorker` does not retry internally. Retry logic is handled by the
/// pipeline's inference stage. See [`ModelWorker`] for details.
#[derive(Debug)]
pub struct AnthropicWorker {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
    /// `true` once `with_temperature` was called; only then does
    /// [`ModelWorker::chat`] send a temperature the client left unset.
    temperature_set: bool,
    timeout: Duration,
    /// API base URL — override for Anthropic-compatible endpoints or testing.
    base_url: String,
}

impl AnthropicWorker {
    /// Create a new `AnthropicWorker` for the given model, reading the API
    /// key from `ANTHROPIC_API_KEY`. Same as [`from_env`](Self::from_env).
    ///
    /// Default settings: 1024 max tokens (the Messages API requires a
    /// value), temperature 1.0, 60 s timeout. All defaults are overridable
    /// via the builder methods.
    ///
    /// # Environment Variables
    ///
    /// | Variable | Required | Description |
    /// |----------|----------|-------------|
    /// | `ANTHROPIC_API_KEY` | Yes | API key sent via `x-api-key` header |
    ///
    /// # Errors
    ///
    /// Returns [`OrchestratorError::ConfigError`] if `ANTHROPIC_API_KEY` is
    /// not set in the environment.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_prompt_orchestrator::AnthropicWorker;
    /// use std::sync::Arc;
    ///
    /// // ANTHROPIC_API_KEY must be set in the environment.
    /// let worker = Arc::new(
    ///     AnthropicWorker::new("claude-3-5-sonnet-20241022")
    ///         .expect("ANTHROPIC_API_KEY must be set")
    ///         .with_max_tokens(1024)
    ///         .with_temperature(1.0),
    /// );
    /// ```
    pub fn new(model: impl Into<String>) -> Result<Self, OrchestratorError> {
        Self::from_env(model)
    }

    /// Create a worker for `model` that reads its API key from the
    /// `ANTHROPIC_API_KEY` environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`OrchestratorError::ConfigError`] if `ANTHROPIC_API_KEY` is
    /// not set.
    pub fn from_env(model: impl Into<String>) -> Result<Self, OrchestratorError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            OrchestratorError::ConfigError("ANTHROPIC_API_KEY environment variable not set".into())
        })?;
        Ok(Self::with_key(model, api_key))
    }

    /// Create a worker for `model` with an API key you supply, from a vault,
    /// a config file or per tenant. Touches no environment variable.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_prompt_orchestrator::AnthropicWorker;
    ///
    /// let worker = AnthropicWorker::with_key("claude-sonnet-4-5", "key-from-your-vault")
    ///     .with_http_client(reqwest::Client::new())
    ///     .with_max_tokens(4096);
    /// ```
    pub fn with_key(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: 1024,
            temperature: 1.0,
            temperature_set: false,
            timeout: Duration::from_secs(60),
            base_url: "https://api.anthropic.com/v1".to_string(),
        }
    }

    /// Send requests through `client` instead of a client of our own, to
    /// share its connection pool, proxy and TLS settings. The per-request
    /// timeout set by [`with_timeout`](Self::with_timeout) still applies.
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Set maximum tokens to generate
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set temperature (0.0 – 1.0 for Anthropic).
    ///
    /// Values outside `[0.0, 1.0]` are clamped and a `WARN`-level log line is
    /// emitted.  The Anthropic API rejects values outside this range with HTTP 400.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        if !(0.0..=1.0).contains(&temperature) {
            tracing::warn!(
                temperature = temperature,
                "Anthropic temperature out of range [0.0, 1.0] — clamping"
            );
            self.temperature = temperature.clamp(0.0, 1.0);
        } else {
            self.temperature = temperature;
        }
        self.temperature_set = true;
        self
    }

    /// Set request timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the API base URL.
    ///
    /// Useful for Anthropic-compatible endpoints or for pointing at a mock server
    /// in tests. Default: `"https://api.anthropic.com/v1"`.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[async_trait]
impl ModelWorker for AnthropicWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let _infer_start = Instant::now();
        // Use the Messages API (same as infer_stream)
        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "messages": [{"role": "user", "content": prompt}]
        });

        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                OrchestratorError::Inference(format!("Anthropic request failed: {}", e))
            })?;

        warn_if_low_remaining(response.headers(), "anthropic");

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs =
                parse_retry_after(response.headers()).unwrap_or(Duration::from_secs(60));
            return Err(OrchestratorError::RateLimited {
                retry_after_secs: retry_after_secs.as_secs(),
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| String::new());
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(OrchestratorError::AuthFailed(format!("HTTP {status}")));
            }
            return Err(OrchestratorError::Inference(format!(
                "Anthropic API error {}: {}",
                status, error_text
            )));
        }

        // Parse Messages API response: {"content": [{"type": "text", "text": "..."}]}
        #[derive(Deserialize)]
        struct MessagesResponse {
            content: Vec<ContentBlock>,
        }
        #[derive(Deserialize)]
        struct ContentBlock {
            #[serde(rename = "type")]
            block_type: String,
            #[serde(default)]
            text: String,
        }

        let api_response: MessagesResponse = response.json().await.map_err(|e| {
            OrchestratorError::Inference(format!("Failed to parse response: {}", e))
        })?;

        // Collect text from all text content blocks.  Filter out
        // whitespace-only results so callers receive an empty vec rather than
        // a single blank token.
        let full_text: String = api_response
            .content
            .into_iter()
            .filter(|b| b.block_type == "text")
            .map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");

        let result = if full_text.trim().is_empty() {
            Ok(vec![])
        } else {
            Ok(vec![full_text])
        };
        tracing::debug!(
            worker = "anthropic",
            model = %self.model,
            latency_ms = %_infer_start.elapsed().as_millis(),
            "inference completed"
        );
        result
    }

    /// Translates the OpenAI-style request to the Messages API and back:
    /// system and developer messages become `system`; assistant
    /// `tool_calls` become `tool_use` blocks and `tool` messages
    /// `tool_result` blocks; `tools`, `tool_choice` (`auto`, `none`,
    /// `required` -> `any`, a named function -> `tool`),
    /// `parallel_tool_calls: false`, `temperature`, `top_p` and `stop`
    /// (`stop_sequences`) are sent. `max_tokens` /
    /// `max_completion_tokens` set the reply cap; without either the
    /// worker's own `max_tokens` is sent, because the Messages API requires
    /// one. The Messages API has no `seed` or `response_format`; `seed` is
    /// not sent (the OpenAI-compatible endpoint answers 400 for a
    /// `response_format` other than `text` on an Anthropic route).
    ///
    /// The reply's `tool_use` blocks come back as OpenAI `tool_calls`, and
    /// `stop_reason` as `finish_reason` (`end_turn` -> `stop`, `max_tokens`
    /// -> `length`, `tool_use` -> `tool_calls`).
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, OrchestratorError> {
        let start = Instant::now();
        let body = anthropic_chat_body(request, self);
        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| OrchestratorError::Inference(format!("Anthropic request failed: {e}")))?;
        warn_if_low_remaining(response.headers(), "anthropic");
        if !response.status().is_success() {
            return Err(chat_http_error(response, "anthropic").await);
        }
        let value: serde_json::Value = response.json().await.map_err(|e| {
            OrchestratorError::Inference(format!("Failed to parse anthropic response: {e}"))
        })?;
        let result = anthropic_chat_response(&value);
        tracing::debug!(
            worker = "anthropic",
            model = %body["model"].as_str().unwrap_or(""),
            latency_ms = %start.elapsed().as_millis(),
            "chat completed"
        );
        Ok(result)
    }

    async fn infer_stream(&self, prompt: &str) -> Result<TokenStream, OrchestratorError> {
        use futures::StreamExt;

        // Anthropic Messages API with stream=true emits SSE events.
        // We parse `content_block_delta` events to extract text deltas.
        #[derive(Deserialize)]
        struct StreamEvent {
            #[serde(rename = "type")]
            event_type: String,
            delta: Option<StreamDelta>,
        }
        #[derive(Deserialize)]
        struct StreamDelta {
            #[serde(rename = "type")]
            delta_type: String,
            #[serde(default)]
            text: String,
        }

        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "stream": true,
            "messages": [{"role": "user", "content": prompt}]
        });

        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                OrchestratorError::Inference(format!("Anthropic stream request failed: {e}"))
            })?;

        warn_if_low_remaining(response.headers(), "anthropic");

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs =
                parse_retry_after(response.headers()).unwrap_or(Duration::from_secs(60));
            return Err(OrchestratorError::RateLimited {
                retry_after_secs: retry_after_secs.as_secs(),
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(OrchestratorError::AuthFailed(format!("HTTP {status}")));
            }
            return Err(OrchestratorError::Inference(format!(
                "Anthropic stream error {status}: {body}"
            )));
        }

        let request_start = Instant::now();
        let model_name = self.model.clone();
        let byte_stream = response.bytes_stream();
        let token_stream = byte_stream.filter_map(|chunk| async move {
            let bytes = chunk.ok()?;
            let text = std::str::from_utf8(&bytes).ok()?;
            let mut out = String::new();
            for line in text.lines() {
                // SSE data lines: `data: {...}`
                let Some(json_str) = line.strip_prefix("data: ") else {
                    continue;
                };
                if json_str.trim() == "[DONE]" {
                    break;
                }
                if let Ok(event) = serde_json::from_str::<StreamEvent>(json_str) {
                    if event.event_type == "content_block_delta" {
                        if let Some(delta) = event.delta {
                            if delta.delta_type == "text_delta" && !delta.text.is_empty() {
                                out.push_str(&delta.text);
                            }
                        }
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Ok(out))
            }
        });

        // Wrap the stream to record TTFT on the very first token.
        let mut first_token_seen = false;
        let ttft_stream = token_stream.map(move |item| {
            if !first_token_seen {
                first_token_seen = true;
                metrics::record_ttft("anthropic", &model_name, request_start.elapsed());
            }
            item
        });

        Ok(Box::pin(ttft_stream))
    }
}

/// Content blocks for one OpenAI message's `content` (text parts only).
fn anthropic_text_blocks(content: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    use serde_json::{json, Value};
    match content {
        Some(Value::String(s)) if !s.is_empty() => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .filter(|t| !t.is_empty())
            .map(|t| json!({"type": "text", "text": t}))
            .collect(),
        _ => Vec::new(),
    }
}

/// The Messages API body for an OpenAI-style chat request.
fn anthropic_chat_body(request: &ChatRequest, worker: &AnthropicWorker) -> serde_json::Value {
    use serde_json::{json, Value};

    let mut system: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for m in &request.messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" | "developer" => {
                let text = crate::chat::message_text(m);
                if !text.is_empty() {
                    system.push(text);
                }
            }
            "assistant" => {
                let mut blocks = anthropic_text_blocks(m.get("content"));
                for call in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let function = call.get("function").cloned().unwrap_or(Value::Null);
                    let input = function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.get("id").cloned().unwrap_or(Value::Null),
                        "name": function.get("name").cloned().unwrap_or(Value::Null),
                        "input": input,
                    }));
                }
                if !blocks.is_empty() {
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            "tool" | "function" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").cloned().unwrap_or(Value::Null),
                    "content": crate::chat::message_text(m),
                });
                // Results of one assistant turn go back in one user message.
                let merged = match messages.last_mut() {
                    Some(last) if last["role"] == "user" => match last.get_mut("content") {
                        Some(Value::Array(blocks))
                            if blocks.iter().all(|b| b["type"] == "tool_result") =>
                        {
                            blocks.push(block.clone());
                            true
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if !merged {
                    messages.push(json!({"role": "user", "content": [block]}));
                }
            }
            _ => {
                let blocks = anthropic_text_blocks(m.get("content"));
                if !blocks.is_empty() {
                    messages.push(json!({"role": "user", "content": blocks}));
                }
            }
        }
    }

    let model = if request.model.is_empty() {
        worker.model.clone()
    } else {
        request.model.clone()
    };
    let mut body = json!({
        "model": model,
        "max_tokens": request.reply_limit().unwrap_or(worker.max_tokens),
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    match request.temperature {
        Some(t) => body["temperature"] = json!(t),
        None if worker.temperature_set => body["temperature"] = json!(worker.temperature),
        None => {}
    }
    if let Some(p) = request.top_p {
        body["top_p"] = json!(p);
    }
    let stop = request.stop_sequences();
    if !stop.is_empty() {
        body["stop_sequences"] = json!(stop);
    }
    // Without tools, tool_choice and parallel_tool_calls are not sent: the
    // Messages API has nothing for them to apply to.
    let tools = request.tools.as_deref().filter(|t| !t.is_empty());
    if let Some(tools) = tools {
        let converted: Vec<Value> = tools
            .iter()
            .map(|t| {
                let f = t.get("function").unwrap_or(t);
                let mut tool = json!({
                    "name": f.get("name").cloned().unwrap_or(Value::Null),
                    "input_schema": f
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                });
                if let Some(d) = f.get("description") {
                    tool["description"] = d.clone();
                }
                tool
            })
            .collect();
        body["tools"] = json!(converted);
    }
    let tool_choice = request.tool_choice.as_ref().filter(|_| tools.is_some());
    let mut choice = match tool_choice {
        Some(Value::String(s)) => match s.as_str() {
            "none" => Some(json!({"type": "none"})),
            "required" => Some(json!({"type": "any"})),
            _ => Some(json!({"type": "auto"})),
        },
        Some(v @ Value::Object(_)) => v
            .get("function")
            .and_then(|f| f.get("name"))
            .map(|name| json!({"type": "tool", "name": name})),
        _ => None,
    };
    if request.parallel_tool_calls == Some(false) && tools.is_some() {
        let c = choice.get_or_insert_with(|| json!({"type": "auto"}));
        if c["type"] != "none" {
            c["disable_parallel_tool_use"] = json!(true);
        }
    }
    if let Some(c) = choice {
        body["tool_choice"] = c;
    }
    body
}

/// An OpenAI-style answer from a Messages API response.
fn anthropic_chat_response(value: &serde_json::Value) -> ChatResponse {
    use serde_json::{json, Value};
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in value.get("content").and_then(Value::as_array).into_iter().flatten() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => tool_calls.push(json!({
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": block.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": block.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into()),
                },
            })),
            _ => {}
        }
    }
    let finish_reason = match value.get("stop_reason").and_then(Value::as_str) {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "stop",
    };
    let usage = value.get("usage").and_then(|u| {
        Some(ChatUsage {
            prompt_tokens: u.get("input_tokens")?.as_u64()?,
            completion_tokens: u.get("output_tokens")?.as_u64()?,
        })
    });
    let mut response = ChatResponse::from_text(text);
    if response.text().is_empty() && !tool_calls.is_empty() {
        response.content = None;
    }
    response.tool_calls = tool_calls;
    response.finish_reason = Some(finish_reason.to_string());
    response.usage = usage;
    response
}

// ============================================================================
// llama.cpp Worker
// ============================================================================

/// llama.cpp server request payload
#[derive(Debug, Serialize)]
struct LlamaCppRequest {
    prompt: String,
    /// Omitted unless set, so the server's own limit applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    n_predict: Option<i32>,
    temperature: f32,
    stop: Vec<String>,
}

/// llama.cpp server response
#[derive(Debug, Deserialize)]
struct LlamaCppResponse {
    content: String,
}

/// llama.cpp HTTP server worker
///
/// Connects to a llama.cpp server instance.
/// Server URL can be set via LLAMA_CPP_URL environment variable
/// or defaults to http://localhost:8080
///
/// ## Example
///
/// ```no_run
/// use tokio_prompt_orchestrator::LlamaCppWorker;
/// use std::sync::Arc;
///
/// let worker = Arc::new(
///     LlamaCppWorker::new()
///         .with_url("http://localhost:8080")
///         .with_max_tokens(512)
/// );
/// ```
pub struct LlamaCppWorker {
    client: reqwest::Client,
    url: String,
    max_tokens: Option<i32>,
    temperature: f32,
    /// `true` once `with_temperature` was called; only then does
    /// [`ModelWorker::chat`] send a temperature the client left unset.
    temperature_set: bool,
    timeout: Duration,
}

impl LlamaCppWorker {
    /// Create a new `LlamaCppWorker` pointing at a llama.cpp HTTP server.
    ///
    /// Reads the server URL from the `LLAMA_CPP_URL` environment variable. If
    /// the variable is not set the worker falls back to
    /// `http://localhost:8080`. Default settings: no `n_predict` (the
    /// server's limit applies), temperature 0.8, 30 s timeout.
    ///
    /// # Environment Variables
    ///
    /// | Variable | Required | Description |
    /// |----------|----------|-------------|
    /// | `LLAMA_CPP_URL` | No | llama.cpp server base URL (default: `http://localhost:8080`) |
    ///
    /// # Errors
    ///
    /// This constructor never returns an error. Network errors are deferred
    /// until [`ModelWorker::infer`] is called.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_prompt_orchestrator::LlamaCppWorker;
    /// use std::sync::Arc;
    ///
    /// // Optionally set LLAMA_CPP_URL=http://gpu-host:8080 in the environment.
    /// let worker = Arc::new(
    ///     LlamaCppWorker::new()
    ///         .with_max_tokens(512)
    ///         .with_temperature(0.8),
    /// );
    /// ```
    pub fn new() -> Self {
        let url =
            std::env::var("LLAMA_CPP_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());

        Self {
            client: reqwest::Client::new(),
            url,
            max_tokens: None,
            temperature: 0.8,
            temperature_set: false,
            timeout: Duration::from_secs(30),
        }
    }

    /// Set server URL
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Cap the reply at `max_tokens` (llama.cpp's `n_predict`). Unset by
    /// default: the request then carries no `n_predict` and the server's own
    /// limit applies.
    pub fn with_max_tokens(mut self, max_tokens: i32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Send requests through `client` instead of a client of our own, to
    /// share its connection pool, proxy and TLS settings.
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Set temperature (0.0 – 2.0).
    ///
    /// Values outside `[0.0, 2.0]` are clamped and a `WARN`-level log line is
    /// emitted.  Most llama.cpp-compatible servers reject values outside this range.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        if !(0.0..=2.0).contains(&temperature) {
            tracing::warn!(
                temperature = temperature,
                "LlamaCpp temperature out of range [0.0, 2.0] — clamping"
            );
            self.temperature = temperature.clamp(0.0, 2.0);
        } else {
            self.temperature = temperature;
        }
        self.temperature_set = true;
        self
    }

    /// Set request timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Default for LlamaCppWorker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelWorker for LlamaCppWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let _infer_start = Instant::now();
        let request = LlamaCppRequest {
            prompt: prompt.to_string(),
            n_predict: self.max_tokens,
            temperature: self.temperature,
            stop: vec!["</s>".to_string(), "Human:".to_string()],
        };

        let response = self
            .client
            .post(format!("{}/completion", self.url))
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                OrchestratorError::Inference(format!("llama.cpp request failed: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| String::new());
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(OrchestratorError::AuthFailed(format!("HTTP {status}")));
            }
            return Err(OrchestratorError::Inference(format!(
                "llama.cpp error {}: {}",
                status, error_text
            )));
        }

        let api_response: LlamaCppResponse = response.json().await.map_err(|e| {
            OrchestratorError::Inference(format!("Failed to parse response: {}", e))
        })?;

        // Filter out empty strings so callers receive a genuinely-empty vec when
        // the model returns no text content.
        let content = api_response.content;
        let result = if content.is_empty() {
            Ok(vec![])
        } else {
            Ok(vec![content])
        };
        tracing::debug!(
            worker = "llama_cpp",
            model = "llama.cpp",
            latency_ms = %_infer_start.elapsed().as_millis(),
            "inference completed"
        );
        result
    }

    /// Sends the request to llama.cpp's OpenAI-compatible
    /// `{url}/v1/chat/completions` with every setting the client sent
    /// (tools need a server started with `--jinja`). The worker's
    /// `max_tokens` and temperature apply only when the client sent none.
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, OrchestratorError> {
        let temperature = self.temperature_set.then_some(f64::from(self.temperature));
        let max_tokens = self.max_tokens.and_then(|n| u32::try_from(n).ok());
        let body = request.to_openai_body("local", temperature, max_tokens);
        let url = format!("{}/v1/chat/completions", self.url.trim_end_matches('/'));
        post_openai_chat(&self.client, &url, None, self.timeout, &body, "llama.cpp").await
    }
}

// ============================================================================
// vLLM Worker
// ============================================================================

/// vLLM server request payload
#[derive(Debug, Serialize)]
struct VllmRequest {
    prompt: String,
    max_tokens: u32,
    temperature: f32,
    top_p: f32,
}

/// vLLM server response
#[derive(Debug, Deserialize)]
struct VllmResponse {
    text: Vec<String>,
}

/// vLLM inference server worker
///
/// Connects to a vLLM server instance.
/// Server URL can be set via VLLM_URL environment variable
/// or defaults to http://localhost:8000
///
/// ## Example
///
/// ```no_run
/// use tokio_prompt_orchestrator::VllmWorker;
/// use std::sync::Arc;
///
/// let worker = Arc::new(
///     VllmWorker::new()
///         .with_url("http://localhost:8000")
///         .with_max_tokens(1024)
/// );
/// ```
pub struct VllmWorker {
    client: reqwest::Client,
    url: String,
    max_tokens: u32,
    temperature: f32,
    top_p: f32,
    timeout: Duration,
}

impl VllmWorker {
    /// Create a new `VllmWorker` pointing at a vLLM inference server.
    ///
    /// Reads the server URL from the `VLLM_URL` environment variable. If the
    /// variable is not set the worker falls back to `http://localhost:8000`.
    /// Default settings: 512 max tokens, temperature 0.7, top_p 0.95, 60 s
    /// timeout.
    ///
    /// # Environment Variables
    ///
    /// | Variable | Required | Description |
    /// |----------|----------|-------------|
    /// | `VLLM_URL` | No | vLLM server base URL (default: `http://localhost:8000`) |
    ///
    /// # Errors
    ///
    /// This constructor never returns an error. Network errors are deferred
    /// until [`ModelWorker::infer`] is called.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_prompt_orchestrator::VllmWorker;
    /// use std::sync::Arc;
    ///
    /// // Optionally set VLLM_URL=http://gpu-host:8000 in the environment.
    /// let worker = Arc::new(
    ///     VllmWorker::new()
    ///         .with_max_tokens(1024)
    ///         .with_temperature(0.5),
    /// );
    /// ```
    pub fn new() -> Self {
        let url = std::env::var("VLLM_URL").unwrap_or_else(|_| "http://localhost:8000".to_string());

        Self {
            client: reqwest::Client::new(),
            url,
            max_tokens: 512,
            temperature: 0.7,
            top_p: 0.95,
            timeout: Duration::from_secs(60),
        }
    }

    /// Set server URL
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Set maximum tokens to generate
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set temperature (0.0 – 2.0).
    ///
    /// Values outside `[0.0, 2.0]` are clamped and a `WARN`-level log line is
    /// emitted.  Most vLLM-compatible servers reject values outside this range.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        if !(0.0..=2.0).contains(&temperature) {
            tracing::warn!(
                temperature = temperature,
                "vLLM temperature out of range [0.0, 2.0] — clamping"
            );
            self.temperature = temperature.clamp(0.0, 2.0);
        } else {
            self.temperature = temperature;
        }
        self
    }

    /// Set top_p sampling parameter
    pub fn with_top_p(mut self, top_p: f32) -> Self {
        self.top_p = top_p;
        self
    }

    /// Set request timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Default for VllmWorker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelWorker for VllmWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let _infer_start = Instant::now();
        let request = VllmRequest {
            prompt: prompt.to_string(),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            top_p: self.top_p,
        };

        let response = self
            .client
            .post(format!("{}/generate", self.url))
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| OrchestratorError::Inference(format!("vLLM request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| String::new());
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(OrchestratorError::AuthFailed(format!("HTTP {status}")));
            }
            return Err(OrchestratorError::Inference(format!(
                "vLLM error {}: {}",
                status, error_text
            )));
        }

        let api_response: VllmResponse = response.json().await.map_err(|e| {
            OrchestratorError::Inference(format!("Failed to parse response: {}", e))
        })?;

        let content =
            api_response.text.into_iter().next().ok_or_else(|| {
                OrchestratorError::Inference("Empty response from vLLM".to_string())
            })?;

        tracing::debug!(
            worker = "vllm",
            model = "vllm",
            latency_ms = %_infer_start.elapsed().as_millis(),
            "inference completed"
        );
        Ok(vec![content])
    }
}

// ============================================================================
// Load-Balanced Worker
// ============================================================================

/// Strategy for distributing requests across a pool of workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadBalanceStrategy {
    /// Rotate through workers in order.
    RoundRobin,
    /// Pick the worker with the lowest running request count.
    LeastLoaded,
}

/// A worker pool that distributes inference requests across multiple backends.
///
/// Wraps any number of `Arc<dyn ModelWorker>` instances and routes requests
/// using a configurable [`LoadBalanceStrategy`].  All strategies are
/// thread-safe; the pool is safe to share across Tokio tasks via `Arc`.
///
/// ## Example
///
/// ```no_run
/// # use tokio_prompt_orchestrator::{AnthropicWorker, OpenAiWorker, OrchestratorError};
/// # use tokio_prompt_orchestrator::worker::LoadBalancedWorker;
/// use std::sync::Arc;
///
/// # fn example() -> Result<(), OrchestratorError> {
/// let pool = LoadBalancedWorker::round_robin(vec![
///     Arc::new(AnthropicWorker::new("claude-sonnet-4-6")?) as Arc<dyn tokio_prompt_orchestrator::ModelWorker>,
///     Arc::new(OpenAiWorker::new("gpt-4o")?) as Arc<dyn tokio_prompt_orchestrator::ModelWorker>,
/// ]);
/// # Ok(()) }
/// ```
pub struct LoadBalancedWorker {
    workers: Vec<Arc<dyn ModelWorker>>,
    strategy: LoadBalanceStrategy,
    /// Round-robin cursor (strategy = RoundRobin).
    rr_counter: std::sync::atomic::AtomicUsize,
    /// Per-worker in-flight request counts (strategy = LeastLoaded).
    in_flight: Vec<std::sync::atomic::AtomicUsize>,
    /// Optional human-readable names for each worker, used in metrics labels.
    names: Vec<String>,
}

/// RAII guard that decrements the in-flight counter when dropped.
struct InFlightGuard<'a>(&'a std::sync::atomic::AtomicUsize);
impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl LoadBalancedWorker {
    /// Create a round-robin pool.
    ///
    /// # Panics
    ///
    /// Panics if `workers` is empty.
    pub fn round_robin(workers: Vec<Arc<dyn ModelWorker>>) -> Self {
        assert!(!workers.is_empty(), "worker pool must not be empty");
        let n = workers.len();
        Self {
            workers,
            strategy: LoadBalanceStrategy::RoundRobin,
            rr_counter: std::sync::atomic::AtomicUsize::new(0),
            in_flight: (0..n)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            names: Vec::new(),
        }
    }

    /// Create a round-robin pool by replicating a single worker `n` times.
    ///
    /// All pool slots share the same underlying `Arc`; this is useful for
    /// concurrency control rather than load distribution across different backends.
    ///
    /// # Panics
    ///
    /// Panics if `n` is zero.
    pub fn replicate(worker: Arc<dyn ModelWorker>, n: usize) -> Self {
        assert!(n > 0, "replicate count must be > 0");
        let workers = std::iter::repeat_n(Arc::clone(&worker), n).collect();
        Self::round_robin(workers)
    }

    /// Create a least-loaded pool.
    ///
    /// # Panics
    ///
    /// Panics if `workers` is empty.
    pub fn least_loaded(workers: Vec<Arc<dyn ModelWorker>>) -> Self {
        assert!(!workers.is_empty(), "worker pool must not be empty");
        let n = workers.len();
        Self {
            workers,
            strategy: LoadBalanceStrategy::LeastLoaded,
            rr_counter: std::sync::atomic::AtomicUsize::new(0),
            in_flight: (0..n)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            names: Vec::new(),
        }
    }

    /// Attach human-readable names to each worker slot for metrics labels.
    ///
    /// If `names` is shorter than the pool, unnamed workers default to `"unknown"`.
    pub fn with_names(mut self, names: Vec<String>) -> Self {
        self.names = names;
        self
    }

    /// Return the number of workers in the pool.
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// Return true if the pool is empty (should never be true after construction).
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Pick the index of the worker to use for the next request.
    fn pick(&self) -> usize {
        match self.strategy {
            LoadBalanceStrategy::RoundRobin => {
                self.rr_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % self.workers.len()
            }
            LoadBalanceStrategy::LeastLoaded => self
                .in_flight
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.load(std::sync::atomic::Ordering::Relaxed))
                .map(|(i, _)| i)
                .unwrap_or(0),
        }
    }
}

#[async_trait]
impl ModelWorker for LoadBalancedWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let idx = self.pick();
        self.in_flight[idx].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _guard = InFlightGuard(&self.in_flight[idx]);
        let label = self
            .names
            .get(idx)
            .map(String::as_str)
            .unwrap_or("unknown");
        crate::metrics::set_queue_depth(
            label,
            self.in_flight[idx].load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        self.workers[idx].infer(prompt).await
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, OrchestratorError> {
        let idx = self.pick();
        self.in_flight[idx].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _guard = InFlightGuard(&self.in_flight[idx]);
        self.workers[idx].chat(request).await
    }

    async fn infer_stream(&self, prompt: &str) -> Result<TokenStream, OrchestratorError> {
        let idx = self.pick();
        self.in_flight[idx].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = self.workers[idx].infer_stream(prompt).await;
        // Note: we decrement after obtaining the stream handle.
        // The stream itself runs outside the critical section intentionally —
        // counting only the dispatch decision avoids holding a counter across
        // the full streaming duration, which would bias LeastLoaded unfairly.
        self.in_flight[idx].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Serialise all tests that read/write environment variables so they don't race.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Create an `OpenAiWorker` that points at `base_url`.
    /// Must be called while `ENV_MUTEX` is held.
    fn make_openai_worker_for(base_url: &str) -> OpenAiWorker {
        std::env::set_var("OPENAI_API_KEY", "test-key-openai");
        let w = OpenAiWorker::new("gpt-3.5-turbo-instruct")
            .expect("OpenAiWorker::new must succeed when OPENAI_API_KEY is set")
            .with_base_url(base_url);
        std::env::remove_var("OPENAI_API_KEY");
        w
    }

    /// Create an `AnthropicWorker` that points at `base_url`.
    /// Must be called while `ENV_MUTEX` is held.
    fn make_anthropic_worker_for(base_url: &str) -> AnthropicWorker {
        std::env::set_var("ANTHROPIC_API_KEY", "test-key-anthropic");
        let w = AnthropicWorker::new("claude-instant-1-2")
            .expect("AnthropicWorker::new must succeed when ANTHROPIC_API_KEY is set")
            .with_base_url(base_url);
        std::env::remove_var("ANTHROPIC_API_KEY");
        w
    }

    fn openai_success_body() -> serde_json::Value {
        serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "hello world response"}}]})
    }

    fn anthropic_success_body() -> serde_json::Value {
        // Messages API format: {"content": [{"type": "text", "text": "..."}]}
        serde_json::json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello world response"}],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 3}
        })
    }

    fn llamacpp_success_body() -> serde_json::Value {
        serde_json::json!({"content": "hello world response"})
    }

    fn vllm_success_body() -> serde_json::Value {
        serde_json::json!({"text": ["hello world response"]})
    }

    // ── EchoWorker ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_echo_worker_infer_splits_on_whitespace() {
        let worker = EchoWorker::with_delay(0);
        let tokens = worker.infer("hello world").await.unwrap();
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[tokio::test]
    async fn test_echo_worker_infer_empty_prompt_returns_empty_tokens() {
        let worker = EchoWorker::with_delay(0);
        let tokens = worker.infer("").await.unwrap();
        assert!(tokens.is_empty(), "empty prompt should produce no tokens");
    }

    #[tokio::test]
    async fn test_echo_worker_infer_single_word_returns_one_token() {
        let worker = EchoWorker::with_delay(0);
        let tokens = worker.infer("hello").await.unwrap();
        assert_eq!(tokens, vec!["hello"]);
    }

    #[tokio::test]
    async fn test_echo_worker_infer_multiple_whitespace_is_normalised() {
        // split_whitespace collapses runs of whitespace
        let worker = EchoWorker::with_delay(0);
        let tokens = worker.infer("a   b   c").await.unwrap();
        assert_eq!(tokens, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn test_echo_worker_with_delay_stores_delay_ms() {
        let worker = EchoWorker::with_delay(42);
        assert_eq!(worker.delay_ms, 42);
    }

    #[tokio::test]
    async fn test_echo_worker_new_delay_is_10ms() {
        let worker = EchoWorker::new();
        assert_eq!(worker.delay_ms, 10);
    }

    #[tokio::test]
    async fn test_echo_worker_default_via_trait_works() {
        let worker = EchoWorker::default();
        let tokens = worker.infer("one two three").await.unwrap();
        assert_eq!(tokens.len(), 3);
    }

    #[tokio::test]
    async fn test_echo_worker_infer_always_returns_ok() {
        let worker = EchoWorker::with_delay(0);
        // EchoWorker never returns an error
        assert!(worker.infer("anything").await.is_ok());
    }

    // ── OpenAiWorker — constructor ────────────────────────────────────────────

    #[test]
    fn test_openai_worker_new_missing_key_returns_config_error() {
        let _guard = ENV_MUTEX.lock();
        std::env::remove_var("OPENAI_API_KEY");
        let result = OpenAiWorker::new("gpt-4");
        assert!(
            result.is_err(),
            "Expected Err when OPENAI_API_KEY is not set"
        );
        match result.unwrap_err() {
            OrchestratorError::ConfigError(msg) => {
                assert!(
                    msg.contains("OPENAI_API_KEY"),
                    "Error should name the missing var"
                );
            }
            other => unreachable!("Expected ConfigError, got {:?}", other),
        }
    }

    #[test]
    fn test_openai_worker_new_with_key_succeeds() {
        let _guard = ENV_MUTEX.lock();
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        let result = OpenAiWorker::new("gpt-4");
        std::env::remove_var("OPENAI_API_KEY");
        assert!(result.is_ok(), "Expected Ok when OPENAI_API_KEY is set");
    }

    // ── OpenAiWorker — inference ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_openai_infer_success_parses_response_correctly() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_openai_worker_for(&server.uri())
        };
        let tokens = worker.infer("test prompt").await.unwrap();
        assert_eq!(tokens, vec!["hello world response"]);
    }

    #[tokio::test]
    async fn test_openai_infer_http_500_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_openai_worker_for(&server.uri())
        };
        let result = worker.infer("test").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OrchestratorError::Inference(msg) => {
                assert!(
                    msg.contains("500"),
                    "Error message should include the status code"
                );
            }
            other => unreachable!("Expected Inference error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_openai_infer_empty_choices_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices": []})),
            )
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_openai_worker_for(&server.uri())
        };
        let result = worker.infer("test").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OrchestratorError::Inference(msg) => {
                assert!(
                    msg.contains("choices"),
                    "Error should mention missing choices"
                );
            }
            other => unreachable!("Expected Inference error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_openai_infer_invalid_json_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not valid json {{{{"))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_openai_worker_for(&server.uri())
        };
        assert!(worker.infer("test").await.is_err());
    }

    #[tokio::test]
    async fn test_openai_infer_sends_authorization_header() {
        let server = MockServer::start().await;
        // The mock only matches if the Authorization header has the right value.
        // An unmatched request returns 404, which makes the worker return Err,
        // causing the final assert to fail — which is the desired test signal.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key-openai"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_openai_worker_for(&server.uri())
        };
        let result = worker.infer("test").await;
        assert!(
            result.is_ok(),
            "Request with correct auth header should succeed"
        );
    }

    #[tokio::test]
    async fn test_openai_infer_sends_correct_model_in_request_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_openai_worker_for(&server.uri())
        };
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "Exactly one request should be sent");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["model"], "gpt-3.5-turbo-instruct");
    }

    #[tokio::test]
    async fn test_openai_with_max_tokens_sends_correct_value() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            std::env::set_var("OPENAI_API_KEY", "test-key-openai");
            let w = OpenAiWorker::new("gpt-4")
                .unwrap()
                .with_max_tokens(1024)
                .with_base_url(&server.uri());
            std::env::remove_var("OPENAI_API_KEY");
            w
        };
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["max_tokens"], 1024);
    }

    #[tokio::test]
    async fn test_openai_with_temperature_sends_correct_value() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            std::env::set_var("OPENAI_API_KEY", "test-key-openai");
            let w = OpenAiWorker::new("gpt-4")
                .unwrap()
                .with_temperature(0.3)
                .with_base_url(&server.uri());
            std::env::remove_var("OPENAI_API_KEY");
            w
        };
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let temp = body["temperature"].as_f64().unwrap();
        assert!(
            (temp - 0.3_f64).abs() < 0.01,
            "Temperature should be ~0.3, got {temp}"
        );
    }

    // ── AnthropicWorker — constructor ─────────────────────────────────────────

    #[test]
    fn test_anthropic_worker_new_missing_key_returns_config_error() {
        let _guard = ENV_MUTEX.lock();
        std::env::remove_var("ANTHROPIC_API_KEY");
        let result = AnthropicWorker::new("claude-3-5-sonnet-20241022");
        assert!(
            result.is_err(),
            "Expected Err when ANTHROPIC_API_KEY is not set"
        );
        match result.unwrap_err() {
            OrchestratorError::ConfigError(msg) => {
                assert!(
                    msg.contains("ANTHROPIC_API_KEY"),
                    "Error should name the missing var"
                );
            }
            other => unreachable!("Expected ConfigError, got {:?}", other),
        }
    }

    #[test]
    fn test_anthropic_worker_new_with_key_succeeds() {
        let _guard = ENV_MUTEX.lock();
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test");
        let result = AnthropicWorker::new("claude-3-5-sonnet-20241022");
        std::env::remove_var("ANTHROPIC_API_KEY");
        assert!(result.is_ok(), "Expected Ok when ANTHROPIC_API_KEY is set");
    }

    // ── AnthropicWorker — inference ───────────────────────────────────────────

    #[tokio::test]
    async fn test_anthropic_infer_success_returns_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_anthropic_worker_for(&server.uri())
        };
        let tokens = worker.infer("test prompt").await.unwrap();
        assert_eq!(tokens, vec!["hello world response"]);
    }

    #[tokio::test]
    async fn test_anthropic_infer_http_500_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("error"))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_anthropic_worker_for(&server.uri())
        };
        let result = worker.infer("test").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OrchestratorError::Inference(msg) => {
                assert!(msg.contains("500"), "Error should include the status code");
            }
            other => unreachable!("Expected Inference error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_anthropic_infer_invalid_json_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_anthropic_worker_for(&server.uri())
        };
        assert!(worker.infer("test").await.is_err());
    }

    #[tokio::test]
    async fn test_anthropic_infer_sends_api_key_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(header("x-api-key", "test-key-anthropic"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_anthropic_worker_for(&server.uri())
        };
        let result = worker.infer("test").await;
        assert!(
            result.is_ok(),
            "Request with correct x-api-key header should succeed"
        );
    }

    #[tokio::test]
    async fn test_anthropic_infer_sends_version_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_anthropic_worker_for(&server.uri())
        };
        let result = worker.infer("test").await;
        assert!(
            result.is_ok(),
            "Request with correct anthropic-version header should succeed"
        );
    }

    #[tokio::test]
    async fn test_anthropic_infer_sends_correct_model_in_request_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_anthropic_worker_for(&server.uri())
        };
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "Exactly one request should be sent");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["model"], "claude-instant-1-2");
    }

    #[tokio::test]
    async fn test_anthropic_with_max_tokens_sends_correct_value() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            std::env::set_var("ANTHROPIC_API_KEY", "test-key-anthropic");
            let w = AnthropicWorker::new("claude-instant-1-2")
                .unwrap()
                .with_max_tokens(2048)
                .with_base_url(&server.uri());
            std::env::remove_var("ANTHROPIC_API_KEY");
            w
        };
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        // Messages API uses `max_tokens` (not `max_tokens_to_sample` from the legacy Complete API)
        assert_eq!(body["max_tokens"], 2048);
    }

    #[tokio::test]
    async fn test_anthropic_infer_formats_prompt_with_human_and_assistant_prefix() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_anthropic_worker_for(&server.uri())
        };
        let _ = worker.infer("my question").await;

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        // Messages API: prompt is in body["messages"][0]["content"]
        let messages = body["messages"].as_array().unwrap();
        assert!(!messages.is_empty(), "Messages array must not be empty");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(
            content.contains("my question"),
            "Prompt should include the original input"
        );
    }

    // ── LlamaCppWorker ────────────────────────────────────────────────────────

    #[test]
    fn test_llamacpp_default_constructor_builds_worker() {
        // Uses unwrap_or_else — always succeeds
        let worker = LlamaCppWorker::new();
        assert!(!worker.url.is_empty(), "URL should be non-empty");
    }

    #[tokio::test]
    async fn test_llamacpp_infer_success_returns_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completion"))
            .respond_with(ResponseTemplate::new(200).set_body_json(llamacpp_success_body()))
            .mount(&server)
            .await;

        let worker = LlamaCppWorker::new().with_url(server.uri());
        let tokens = worker.infer("test prompt").await.unwrap();
        assert_eq!(tokens, vec!["hello world response"]);
    }

    #[tokio::test]
    async fn test_llamacpp_infer_http_500_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completion"))
            .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;

        let worker = LlamaCppWorker::new().with_url(server.uri());
        let result = worker.infer("test").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OrchestratorError::Inference(msg) => {
                assert!(msg.contains("500"), "Error should include the status code");
            }
            other => unreachable!("Expected Inference error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_llamacpp_infer_invalid_json_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completion"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let worker = LlamaCppWorker::new().with_url(server.uri());
        assert!(worker.infer("test").await.is_err());
    }

    #[tokio::test]
    async fn test_llamacpp_sends_request_to_completion_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completion"))
            .respond_with(ResponseTemplate::new(200).set_body_json(llamacpp_success_body()))
            .mount(&server)
            .await;

        let worker = LlamaCppWorker::new().with_url(server.uri());
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "Exactly one request should be sent");
        assert_eq!(reqs[0].url.path(), "/completion");
    }

    #[tokio::test]
    async fn test_llamacpp_with_max_tokens_sends_n_predict_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completion"))
            .respond_with(ResponseTemplate::new(200).set_body_json(llamacpp_success_body()))
            .mount(&server)
            .await;

        let worker = LlamaCppWorker::new()
            .with_url(server.uri())
            .with_max_tokens(512);
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["n_predict"], 512);
    }

    #[tokio::test]
    async fn test_llamacpp_default_omits_n_predict() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completion"))
            .respond_with(ResponseTemplate::new(200).set_body_json(llamacpp_success_body()))
            .mount(&server)
            .await;

        let worker = LlamaCppWorker::new()
            .with_url(server.uri())
            .with_http_client(reqwest::Client::new());
        worker.infer("test").await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            body.get("n_predict").is_none(),
            "no n_predict unless set, so replies are not truncated: {body}"
        );
    }

    // ── chat(): structured requests ──────────────────────────────────────────

    fn chat_req(model: &str) -> ChatRequest {
        ChatRequest::new(model, vec![serde_json::json!({"role": "user", "content": "hi"})])
    }

    #[tokio::test]
    async fn test_openai_chat_uses_worker_defaults_only_when_set_and_unasked() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_success_body()))
            .mount(&server)
            .await;
        let plain = OpenAiWorker::with_key("gpt-4o", "k").with_base_url(server.uri());
        let r = plain.chat(&chat_req("")).await.unwrap();
        assert_eq!(r.text(), "hello world response");
        let tuned = OpenAiWorker::with_key("gpt-4o", "k")
            .with_base_url(server.uri())
            .with_temperature(0.4)
            .with_max_tokens(99);
        tuned.chat(&chat_req("")).await.unwrap();
        let mut asked = chat_req("gpt-4o-mini");
        asked.temperature = Some(1.2);
        asked.max_completion_tokens = Some(5);
        tuned.chat(&asked).await.unwrap();

        let bodies: Vec<serde_json::Value> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(bodies[0]["model"], "gpt-4o");
        assert!(bodies[0].get("temperature").is_none() && bodies[0].get("max_tokens").is_none(), "{}", bodies[0]);
        assert_eq!(bodies[1]["max_tokens"], 99);
        assert!((bodies[1]["temperature"].as_f64().unwrap() - 0.4).abs() < 1e-6);
        assert_eq!(bodies[2]["model"], "gpt-4o-mini");
        assert_eq!(bodies[2]["temperature"], 1.2);
        assert_eq!(bodies[2]["max_completion_tokens"], 5);
        assert!(bodies[2].get("max_tokens").is_none(), "{}", bodies[2]);
    }

    #[tokio::test]
    async fn test_chat_4xx_is_upstream_rejected_and_5xx_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(404).set_body_json(
                serde_json::json!({"error": {"message": "The model `nope` does not exist"}}),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .mount(&server)
            .await;
        let w = OpenAiWorker::with_key("m", "k").with_base_url(server.uri());
        match w.chat(&chat_req("nope")).await {
            Err(OrchestratorError::UpstreamRejected { status, message }) => {
                assert_eq!(status, 404);
                assert_eq!(message, "The model `nope` does not exist");
            }
            other => panic!("expected UpstreamRejected, got {other:?}"),
        }
        let e = w.chat(&chat_req("m")).await.unwrap_err();
        assert!(matches!(e, OrchestratorError::Inference(_)) && e.is_retryable(), "{e:?}");
    }

    #[tokio::test]
    async fn test_llamacpp_chat_uses_the_openai_compatible_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_success_body()))
            .mount(&server)
            .await;
        let w = LlamaCppWorker::new().with_url(server.uri());
        let mut req = chat_req("llama-3");
        req.seed = Some(3);
        req.stop = Some(serde_json::json!("END"));
        assert_eq!(w.chat(&req).await.unwrap().text(), "hello world response");
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["model"], "llama-3");
        assert_eq!(body["seed"], 3);
        assert_eq!(body["stop"], "END");
        assert!(body.get("n_predict").is_none() && body.get("max_tokens").is_none(), "{body}");
    }

    #[tokio::test]
    async fn test_default_chat_flattens_and_calls_infer() {
        let w = EchoWorker::new();
        let req = ChatRequest::new(
            "",
            vec![
                serde_json::json!({"role": "system", "content": "S"}),
                serde_json::json!({"role": "user", "content": "U"}),
            ],
        );
        let r = w.chat(&req).await.unwrap();
        assert_eq!(r.text(), "System: S User: U");
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn test_anthropic_body_merges_tool_results_and_maps_tool_choice() {
        let w = AnthropicWorker::with_key("claude-x", "k");
        let mut req = ChatRequest::new(
            "",
            vec![
                serde_json::json!({"role": "developer", "content": "Rules."}),
                serde_json::json!({"role": "user", "content": "q"}),
                serde_json::json!({"role": "assistant", "tool_calls": [
                    {"id": "a", "type": "function", "function": {"name": "f", "arguments": "{\"x\":1}"}},
                    {"id": "b", "type": "function", "function": {"name": "g", "arguments": "not json"}}
                ]}),
                serde_json::json!({"role": "tool", "tool_call_id": "a", "content": "1"}),
                serde_json::json!({"role": "tool", "tool_call_id": "b", "content": "2"}),
            ],
        );
        req.tool_choice = Some(serde_json::json!({"type": "function", "function": {"name": "f"}}));
        req.tools = Some(vec![serde_json::json!({"type": "function", "function": {"name": "f"}})]);
        let body = anthropic_chat_body(&req, &w);
        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["max_tokens"], 1024, "the worker's cap when the client sends none");
        assert_eq!(body["system"], "Rules.");
        assert!(body.get("temperature").is_none(), "{body}");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "both tool results share one user turn: {body}");
        assert_eq!(msgs[1]["content"][1]["input"], serde_json::json!({}), "bad arguments become {{}}");
        assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(body["tool_choice"], serde_json::json!({"type": "tool", "name": "f"}));
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn test_anthropic_body_sends_no_tool_choice_without_tools() {
        let w = AnthropicWorker::with_key("claude-x", "k");
        let mut req = ChatRequest::new("", vec![serde_json::json!({"role": "user", "content": "q"})]);
        req.tool_choice = Some(serde_json::json!("auto"));
        req.parallel_tool_calls = Some(false);
        for tools in [None, Some(Vec::new())] {
            req.tools = tools;
            let body = anthropic_chat_body(&req, &w);
            assert!(body.get("tools").is_none(), "{body}");
            assert!(body.get("tool_choice").is_none(), "{body}");
        }
    }

    #[tokio::test]
    async fn test_openai_default_omits_max_tokens_in_both_modes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_success_body()))
            .mount(&server)
            .await;

        // with_key needs no environment variable.
        let worker = OpenAiWorker::with_key("gpt-4o-mini", "sk-explicit")
            .with_http_client(reqwest::Client::new())
            .with_base_url(server.uri());
        worker.infer("test").await.unwrap();
        let _ = worker.infer_stream("test").await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2);
        for req in &reqs {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert!(
                body.get("max_tokens").is_none(),
                "no max_tokens unless set: {body}"
            );
            assert_eq!(
                req.headers.get("authorization").unwrap(),
                "Bearer sk-explicit"
            );
        }
    }

    #[tokio::test]
    async fn test_anthropic_with_key_uses_the_given_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_success_body()))
            .mount(&server)
            .await;

        let worker = AnthropicWorker::with_key("claude-sonnet-4-5", "sk-ant-explicit")
            .with_http_client(reqwest::Client::new())
            .with_base_url(server.uri());
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs[0].headers.get("x-api-key").unwrap(), "sk-ant-explicit");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["max_tokens"], 1024, "the Messages API requires one");
    }

    #[tokio::test]
    async fn test_llamacpp_infer_empty_content_returns_empty_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completion"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"content": ""})),
            )
            .mount(&server)
            .await;

        let worker = LlamaCppWorker::new().with_url(server.uri());
        let tokens = worker.infer("test").await.unwrap();
        assert!(tokens.is_empty(), "Empty content should produce no tokens");
    }

    #[tokio::test]
    async fn test_llamacpp_with_url_overrides_default_server() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completion"))
            .respond_with(ResponseTemplate::new(200).set_body_json(llamacpp_success_body()))
            .mount(&server)
            .await;

        // If with_url works, this request reaches our mock — not localhost:8080
        let worker = LlamaCppWorker::new().with_url(server.uri());
        let result = worker.infer("test").await;
        assert!(
            result.is_ok(),
            "Request should reach the mock server via with_url"
        );
    }

    // ── VllmWorker ────────────────────────────────────────────────────────────

    #[test]
    fn test_vllm_default_constructor_builds_worker() {
        let worker = VllmWorker::new();
        assert!(!worker.url.is_empty(), "URL should be non-empty");
    }

    #[tokio::test]
    async fn test_vllm_infer_success_returns_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vllm_success_body()))
            .mount(&server)
            .await;

        let worker = VllmWorker::new().with_url(server.uri());
        let tokens = worker.infer("test prompt").await.unwrap();
        assert_eq!(tokens, vec!["hello world response"]);
    }

    #[tokio::test]
    async fn test_vllm_infer_http_500_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generate"))
            .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;

        let worker = VllmWorker::new().with_url(server.uri());
        let result = worker.infer("test").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OrchestratorError::Inference(msg) => {
                assert!(msg.contains("500"), "Error should include the status code");
            }
            other => unreachable!("Expected Inference error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_vllm_infer_empty_text_array_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": []})))
            .mount(&server)
            .await;

        let worker = VllmWorker::new().with_url(server.uri());
        let result = worker.infer("test").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OrchestratorError::Inference(msg) => {
                assert!(msg.contains("Empty"), "Error should mention empty response");
            }
            other => unreachable!("Expected Inference error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_vllm_infer_invalid_json_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let worker = VllmWorker::new().with_url(server.uri());
        assert!(worker.infer("test").await.is_err());
    }

    #[tokio::test]
    async fn test_vllm_sends_request_to_generate_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vllm_success_body()))
            .mount(&server)
            .await;

        let worker = VllmWorker::new().with_url(server.uri());
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "Exactly one request should be sent");
        assert_eq!(reqs[0].url.path(), "/generate");
    }

    #[tokio::test]
    async fn test_vllm_with_max_tokens_sends_correct_value() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vllm_success_body()))
            .mount(&server)
            .await;

        let worker = VllmWorker::new()
            .with_url(server.uri())
            .with_max_tokens(2048);
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["max_tokens"], 2048);
    }

    #[tokio::test]
    async fn test_vllm_with_top_p_sends_correct_value() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vllm_success_body()))
            .mount(&server)
            .await;

        let worker = VllmWorker::new().with_url(server.uri()).with_top_p(0.85);
        let _ = worker.infer("test").await;

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let top_p = body["top_p"].as_f64().unwrap();
        assert!(
            (top_p - 0.85_f64).abs() < 0.01,
            "top_p should be ~0.85, got {top_p}"
        );
    }

    #[tokio::test]
    async fn test_vllm_with_url_overrides_default_server() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vllm_success_body()))
            .mount(&server)
            .await;

        // If with_url works, the request reaches our mock — not localhost:8000
        let worker = VllmWorker::new().with_url(server.uri());
        let result = worker.infer("test").await;
        assert!(
            result.is_ok(),
            "Request should reach the mock server via with_url"
        );
    }
}
