//! [`ModelWorker`] backed by a [`genai::Client`].

use async_trait::async_trait;
use futures::StreamExt;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent};
use genai::webc;
use genai::Client;

use super::{error_from_status, reply_tokens};
use crate::worker::{ModelWorker, TokenStream};
use crate::OrchestratorError;

/// A [`ModelWorker`] that sends each prompt through a [`genai::Client`].
///
/// genai speaks each provider's native protocol and picks the provider from
/// the model name, so one worker type covers OpenAI, Anthropic, Gemini,
/// Ollama, Groq, DeepSeek, xAI, Cohere, OpenRouter and the rest of the
/// providers genai supports. API keys come from the usual environment
/// variables (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, ...);
/// Ollama needs none.
///
/// ```no_run
/// use std::sync::Arc;
/// use tokio_prompt_orchestrator::{integrations::GenaiWorker, spawn_pipeline};
///
/// // A local Ollama model; "claude-sonnet-4-6" or "gemini-2.5-flash" work the same way.
/// let worker = GenaiWorker::new(genai::Client::default(), "llama3.2").with_max_tokens(512);
/// # tokio_test::block_on(async {
/// let handles = spawn_pipeline(Arc::new(worker));
/// # });
/// ```
///
/// Requires the `genai` feature.
#[derive(Clone)]
pub struct GenaiWorker {
    client: Client,
    model: String,
    options: ChatOptions,
}

impl std::fmt::Debug for GenaiWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenaiWorker")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl GenaiWorker {
    /// Wrap `client`, sending every prompt to `model`.
    pub fn new(client: Client, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            options: ChatOptions::default(),
        }
    }

    /// Cap the length of each reply.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.options = self.options.with_max_tokens(max_tokens);
        self
    }

    /// Set the sampling temperature.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.options = self.options.with_temperature(temperature);
        self
    }

    /// Replace the chat options wholesale (reasoning effort, stop sequences,
    /// seed, extra headers, and anything else genai's [`ChatOptions`] supports).
    #[must_use]
    pub fn with_options(mut self, options: ChatOptions) -> Self {
        self.options = options;
        self
    }

    /// The model every request is sent to.
    pub fn model(&self) -> &str {
        &self.model
    }

    fn request(prompt: &str) -> ChatRequest {
        ChatRequest::new(vec![ChatMessage::user(prompt)])
    }
}

#[async_trait]
impl ModelWorker for GenaiWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let response = self
            .client
            .exec_chat(
                self.model.as_str(),
                Self::request(prompt),
                Some(&self.options),
            )
            .await
            .map_err(map_error)?;
        Ok(reply_tokens(response.into_first_text().unwrap_or_default()))
    }

    async fn infer_stream(&self, prompt: &str) -> Result<TokenStream, OrchestratorError> {
        let response = self
            .client
            .exec_chat_stream(
                self.model.as_str(),
                Self::request(prompt),
                Some(&self.options),
            )
            .await
            .map_err(map_error)?;
        let tokens = response.stream.filter_map(|event| async move {
            match event {
                Ok(ChatStreamEvent::Chunk(chunk)) if !chunk.content.is_empty() => {
                    Some(Ok(chunk.content))
                }
                Ok(_) => None,
                Err(e) => Some(Err(map_error(e))),
            }
        });
        Ok(Box::pin(tokens))
    }
}

fn map_error(e: genai::Error) -> OrchestratorError {
    match e {
        genai::Error::HttpError { status, body, .. } => {
            error_from_status(status.as_u16(), None, body)
        }
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            webc::Error::ResponseFailedStatus {
                status,
                body,
                headers,
            } => {
                let retry_after = headers
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                error_from_status(status.as_u16(), retry_after, body)
            }
            other => OrchestratorError::Inference(other.to_string()),
        },
        e @ (genai::Error::RequiresApiKey { .. }
        | genai::Error::NoAuthData { .. }
        | genai::Error::NoAuthResolver { .. }) => OrchestratorError::AuthFailed(e.to_string()),
        other => OrchestratorError::Inference(other.to_string()),
    }
}
