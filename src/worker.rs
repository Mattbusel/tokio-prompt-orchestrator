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

use crate::OrchestratorError;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

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
/// The trait is object-safe to allow dynamic dispatch via Arc<dyn ModelWorker>.
#[async_trait]
pub trait ModelWorker: Send + Sync {
    /// Perform inference on the given prompt.
    ///
    /// Returns the complete response split into word-level tokens.
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
        let tokens: Vec<String> = prompt.split_whitespace().map(|s| s.to_string()).collect();

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
    max_tokens: u32,
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
/// Requires OPENAI_API_KEY environment variable.
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
#[derive(Debug)]
pub struct OpenAiWorker {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
    timeout: Duration,
    /// API base URL — override for OpenAI-compatible endpoints or testing.
    base_url: String,
}

impl OpenAiWorker {
    /// Create a new OpenAI worker.
    ///
    /// Reads the API key from the `OPENAI_API_KEY` environment variable.
    ///
    /// # Errors
    ///
    /// Returns `Err(OrchestratorError::ConfigError)` if `OPENAI_API_KEY` is not set.
    pub fn new(model: impl Into<String>) -> Result<Self, OrchestratorError> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            OrchestratorError::ConfigError("OPENAI_API_KEY environment variable not set".into())
        })?;

        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model: model.into(),
            max_tokens: 256,
            temperature: 0.7,
            timeout: Duration::from_secs(30),
            base_url: "https://api.openai.com/v1".to_string(),
        })
    }

    /// Set maximum tokens to generate
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set temperature (0.0 - 2.0)
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
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

        // Split response into tokens (simple whitespace split)
        let tokens: Vec<String> = api_response
            .choices
            .first()
            .ok_or_else(|| {
                OrchestratorError::Inference("No choices in OpenAI response".to_string())
            })?
            .message
            .content
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        Ok(tokens)
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

        let request = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "stream": true
        });

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| OrchestratorError::Inference(format!("OpenAI stream request failed: {e}")))?;

        warn_if_low_remaining(response.headers(), "openai");

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs =
                parse_retry_after(response.headers()).unwrap_or(Duration::from_secs(60));
            return Err(OrchestratorError::RateLimited { retry_after_secs: retry_after_secs.as_secs() });
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OrchestratorError::Inference(format!("OpenAI stream error {status}: {body}")));
        }

        // Convert the raw byte stream into SSE token chunks.
        let byte_stream = response.bytes_stream();
        let token_stream = byte_stream.filter_map(|chunk| async move {
            let bytes = chunk.ok()?;
            let text = std::str::from_utf8(&bytes).ok()?;
            // SSE lines look like: `data: {...}` or `data: [DONE]`
            let mut tokens = Vec::new();
            for line in text.lines() {
                let Some(json_str) = line.strip_prefix("data: ") else { continue };
                if json_str.trim() == "[DONE]" { break; }
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
            if tokens.is_empty() { None } else { Some(Ok(tokens.join(""))) }
        });

        Ok(Box::pin(token_stream))
    }
}

// ============================================================================
// Anthropic Worker
// ============================================================================

/// Anthropic API request payload
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    prompt: String,
    max_tokens_to_sample: u32,
    temperature: f32,
}

/// Anthropic API response
#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    completion: String,
}

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
#[derive(Debug)]
pub struct AnthropicWorker {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
    timeout: Duration,
    /// API base URL — override for Anthropic-compatible endpoints or testing.
    base_url: String,
}

impl AnthropicWorker {
    /// Create a new Anthropic worker.
    ///
    /// Reads the API key from the `ANTHROPIC_API_KEY` environment variable.
    ///
    /// # Errors
    ///
    /// Returns `Err(OrchestratorError::ConfigError)` if `ANTHROPIC_API_KEY` is not set.
    pub fn new(model: impl Into<String>) -> Result<Self, OrchestratorError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            OrchestratorError::ConfigError("ANTHROPIC_API_KEY environment variable not set".into())
        })?;

        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model: model.into(),
            max_tokens: 1024,
            temperature: 1.0,
            timeout: Duration::from_secs(60),
            base_url: "https://api.anthropic.com/v1".to_string(),
        })
    }

    /// Set maximum tokens to generate
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set temperature (0.0 - 1.0)
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
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
        // Format prompt with Claude's expected format
        let formatted_prompt = format!("\n\nHuman: {}\n\nAssistant:", prompt);

        let request = AnthropicRequest {
            model: self.model.clone(),
            prompt: formatted_prompt,
            max_tokens_to_sample: self.max_tokens,
            temperature: self.temperature,
        };

        let response = self
            .client
            .post(format!("{}/complete", self.base_url))
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
            return Err(OrchestratorError::Inference(format!(
                "Anthropic API error {}: {}",
                status, error_text
            )));
        }

        let api_response: AnthropicResponse = response.json().await.map_err(|e| {
            OrchestratorError::Inference(format!("Failed to parse response: {}", e))
        })?;

        // Split response into tokens
        let tokens: Vec<String> = api_response
            .completion
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        Ok(tokens)
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
            return Err(OrchestratorError::Inference(format!(
                "Anthropic stream error {status}: {body}"
            )));
        }

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

        Ok(Box::pin(token_stream))
    }
}

// ============================================================================
// llama.cpp Worker
// ============================================================================

/// llama.cpp server request payload
#[derive(Debug, Serialize)]
struct LlamaCppRequest {
    prompt: String,
    n_predict: i32,
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
    max_tokens: i32,
    temperature: f32,
    timeout: Duration,
}

impl LlamaCppWorker {
    /// Create a new llama.cpp worker
    ///
    /// Reads server URL from LLAMA_CPP_URL environment variable,
    /// or defaults to http://localhost:8080
    pub fn new() -> Self {
        let url =
            std::env::var("LLAMA_CPP_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());

        Self {
            client: reqwest::Client::new(),
            url,
            max_tokens: 256,
            temperature: 0.8,
            timeout: Duration::from_secs(30),
        }
    }

    /// Set server URL
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Set maximum tokens to generate
    pub fn with_max_tokens(mut self, max_tokens: i32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set temperature
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
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
            return Err(OrchestratorError::Inference(format!(
                "llama.cpp error {}: {}",
                status, error_text
            )));
        }

        let api_response: LlamaCppResponse = response.json().await.map_err(|e| {
            OrchestratorError::Inference(format!("Failed to parse response: {}", e))
        })?;

        // Split response into tokens
        let tokens: Vec<String> = api_response
            .content
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        Ok(tokens)
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
    /// Create a new vLLM worker
    ///
    /// Reads server URL from VLLM_URL environment variable,
    /// or defaults to http://localhost:8000
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

    /// Set temperature
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
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
            return Err(OrchestratorError::Inference(format!(
                "vLLM error {}: {}",
                status, error_text
            )));
        }

        let api_response: VllmResponse = response.json().await.map_err(|e| {
            OrchestratorError::Inference(format!("Failed to parse response: {}", e))
        })?;

        if api_response.text.is_empty() {
            return Err(OrchestratorError::Inference(
                "Empty response from vLLM".to_string(),
            ));
        }

        // Split response into tokens
        let tokens: Vec<String> = api_response.text[0]
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        Ok(tokens)
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
/// use tokio_prompt_orchestrator::LoadBalancedWorker;
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
        }
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
        }
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
        let result = self.workers[idx].infer(prompt).await;
        self.in_flight[idx].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        result
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
        serde_json::json!({"completion": "hello world response"})
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
            other => panic!("Expected ConfigError, got {:?}", other),
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
        assert_eq!(tokens, vec!["hello", "world", "response"]);
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
            other => panic!("Expected Inference error, got {:?}", other),
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
            other => panic!("Expected Inference error, got {:?}", other),
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
            other => panic!("Expected ConfigError, got {:?}", other),
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
            .and(path("/complete"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_success_body()))
            .mount(&server)
            .await;

        let worker = {
            let _g = ENV_MUTEX.lock();
            make_anthropic_worker_for(&server.uri())
        };
        let tokens = worker.infer("test prompt").await.unwrap();
        assert_eq!(tokens, vec!["hello", "world", "response"]);
    }

    #[tokio::test]
    async fn test_anthropic_infer_http_500_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/complete"))
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
            other => panic!("Expected Inference error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_anthropic_infer_invalid_json_returns_inference_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/complete"))
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
            .and(path("/complete"))
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
            .and(path("/complete"))
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
            .and(path("/complete"))
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
            .and(path("/complete"))
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
        assert_eq!(body["max_tokens_to_sample"], 2048);
    }

    #[tokio::test]
    async fn test_anthropic_infer_formats_prompt_with_human_and_assistant_prefix() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/complete"))
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
        let prompt = body["prompt"].as_str().unwrap();
        assert!(
            prompt.contains("Human:"),
            "Prompt should contain 'Human:' prefix"
        );
        assert!(
            prompt.contains("Assistant:"),
            "Prompt should contain 'Assistant:' marker"
        );
        assert!(
            prompt.contains("my question"),
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
        assert_eq!(tokens, vec!["hello", "world", "response"]);
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
            other => panic!("Expected Inference error, got {:?}", other),
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
        assert_eq!(tokens, vec!["hello", "world", "response"]);
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
            other => panic!("Expected Inference error, got {:?}", other),
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
            other => panic!("Expected Inference error, got {:?}", other),
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
