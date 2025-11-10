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
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Trait for model inference workers
///
/// Implementations must be thread-safe (Send + Sync) for use across tasks.
/// The trait is object-safe to allow dynamic dispatch via Arc<dyn ModelWorker>.
#[async_trait]
pub trait ModelWorker: Send + Sync {
    /// Perform inference on the given prompt
    ///
    /// Returns tokens as a vector of strings.
    /// For streaming implementations, this should be the final token set.
    ///
    /// TODO: Add streaming variant:
    /// ```ignore
    /// async fn infer_stream(&self, prompt: &str)
    ///     -> Result<impl Stream<Item = String>, OrchestratorError>;
    /// ```
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError>;
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
    pub fn new() -> Self {
        Self { delay_ms: 10 }
    }

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
        let tokens: Vec<String> = prompt
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

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
    prompt: String,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
}

/// OpenAI API response
#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    text: String,
}

/// OpenAI API worker (GPT-4, GPT-3.5-turbo-instruct, etc.)
///
/// Requires OPENAI_API_KEY environment variable.
///
/// ## Example
///
/// ```no_run
/// use tokio_prompt_orchestrator::OpenAiWorker;
/// use std::sync::Arc;
///
/// let worker = Arc::new(
///     OpenAiWorker::new("gpt-3.5-turbo-instruct")
///         .with_max_tokens(512)
///         .with_temperature(0.7)
/// );
/// ```
pub struct OpenAiWorker {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
    timeout: Duration,
}

impl OpenAiWorker {
    /// Create a new OpenAI worker
    ///
    /// Reads API key from OPENAI_API_KEY environment variable.
    /// Panics if the key is not set.
    pub fn new(model: impl Into<String>) -> Self {
        let api_key = std::env::var("OPENAI_API_KEY")
            .expect("OPENAI_API_KEY environment variable not set");

        Self {
            client: reqwest::Client::new(),
            api_key,
            model: model.into(),
            max_tokens: 256,
            temperature: 0.7,
            timeout: Duration::from_secs(30),
        }
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
}

#[async_trait]
impl ModelWorker for OpenAiWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let request = OpenAiRequest {
            model: self.model.clone(),
            prompt: prompt.to_string(),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            stop: None,
        };

        let response = self
            .client
            .post("https://api.openai.com/v1/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| OrchestratorError::Inference(format!("OpenAI request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(OrchestratorError::Inference(format!(
                "OpenAI API error {}: {}",
                status, error_text
            )));
        }

        let api_response: OpenAiResponse = response
            .json()
            .await
            .map_err(|e| OrchestratorError::Inference(format!("Failed to parse response: {}", e)))?;

        if api_response.choices.is_empty() {
            return Err(OrchestratorError::Inference(
                "No choices in OpenAI response".to_string(),
            ));
        }

        // Split response into tokens (simple whitespace split)
        let tokens: Vec<String> = api_response.choices[0]
            .text
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        Ok(tokens)
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
/// use tokio_prompt_orchestrator::AnthropicWorker;
/// use std::sync::Arc;
///
/// let worker = Arc::new(
///     AnthropicWorker::new("claude-3-5-sonnet-20241022")
///         .with_max_tokens(1024)
///         .with_temperature(1.0)
/// );
/// ```
pub struct AnthropicWorker {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
    timeout: Duration,
}

impl AnthropicWorker {
    /// Create a new Anthropic worker
    ///
    /// Reads API key from ANTHROPIC_API_KEY environment variable.
    /// Panics if the key is not set.
    pub fn new(model: impl Into<String>) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .expect("ANTHROPIC_API_KEY environment variable not set");

        Self {
            client: reqwest::Client::new(),
            api_key,
            model: model.into(),
            max_tokens: 1024,
            temperature: 1.0,
            timeout: Duration::from_secs(60),
        }
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
            .post("https://api.anthropic.com/v1/complete")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .timeout(self.timeout)
            .json(&request)
            .send()
            .await
            .map_err(|e| OrchestratorError::Inference(format!("Anthropic request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
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
        let url = std::env::var("LLAMA_CPP_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());

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
            let error_text = response.text().await.unwrap_or_default();
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
        let url =
            std::env::var("VLLM_URL").unwrap_or_else(|_| "http://localhost:8000".to_string());

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
            let error_text = response.text().await.unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_echo_worker() {
        let worker = EchoWorker::with_delay(1);
        let result = worker.infer("hello world").await.unwrap();
        assert_eq!(result, vec!["hello", "world"]);
    }
}
