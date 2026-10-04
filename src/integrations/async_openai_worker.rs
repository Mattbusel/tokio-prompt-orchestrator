//! [`ModelWorker`] backed by an [`async_openai::Client`].

use async_openai::config::{Config, OpenAIConfig};
use async_openai::error::OpenAIError;
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestUserMessageArgs,
    CreateChatCompletionRequest, CreateChatCompletionRequestArgs,
};
use async_openai::Client;
use async_trait::async_trait;
use futures::StreamExt;

use super::{error_from_status, reply_tokens};
use crate::worker::{ModelWorker, TokenStream};
use crate::OrchestratorError;

/// A [`ModelWorker`] that sends each prompt through an [`async_openai::Client`].
///
/// Bring the client you already configured (API key, organisation, Azure
/// deployment, or the base URL of any OpenAI-compatible server such as
/// Ollama, vLLM, llama.cpp or LM Studio) and the orchestrator adds
/// deduplication, the circuit breaker, retries and the dead-letter queue in
/// front of it.
///
/// ```no_run
/// use std::sync::Arc;
/// use async_openai::{config::OpenAIConfig, Client};
/// use tokio_prompt_orchestrator::{integrations::AsyncOpenAiWorker, spawn_pipeline};
///
/// // Any OpenAI-compatible server works; this one is a local Ollama.
/// let client = Client::with_config(
///     OpenAIConfig::new().with_api_base("http://localhost:11434/v1").with_api_key("ollama"),
/// );
/// let worker = AsyncOpenAiWorker::new(client, "llama3.2").with_max_tokens(512);
/// # tokio_test::block_on(async {
/// let handles = spawn_pipeline(Arc::new(worker));
/// # });
/// ```
///
/// Requires the `async-openai` feature.
#[derive(Clone)]
pub struct AsyncOpenAiWorker<C: Config = OpenAIConfig> {
    client: Client<C>,
    model: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

impl<C: Config> std::fmt::Debug for AsyncOpenAiWorker<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The client holds the API key, so it is left out.
        f.debug_struct("AsyncOpenAiWorker")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .finish_non_exhaustive()
    }
}

impl<C: Config> AsyncOpenAiWorker<C> {
    /// Wrap `client`, sending every prompt to `model`.
    pub fn new(client: Client<C>, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            max_tokens: None,
            temperature: None,
        }
    }

    /// Cap the length of each reply (`max_completion_tokens`).
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set the sampling temperature.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// The model every request is sent to.
    pub fn model(&self) -> &str {
        &self.model
    }

    fn request(&self, prompt: &str) -> Result<CreateChatCompletionRequest, OrchestratorError> {
        let message = ChatCompletionRequestUserMessageArgs::default()
            .content(prompt)
            .build()
            .map_err(map_error)?;
        let mut args = CreateChatCompletionRequestArgs::default();
        args.model(self.model.clone())
            .messages(vec![ChatCompletionRequestMessage::from(message)]);
        if let Some(n) = self.max_tokens {
            args.max_completion_tokens(n);
        }
        if let Some(t) = self.temperature {
            args.temperature(t);
        }
        args.build().map_err(map_error)
    }
}

#[async_trait]
impl<C: Config + 'static> ModelWorker for AsyncOpenAiWorker<C> {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let response = self
            .client
            .chat()
            .create(self.request(prompt)?)
            .await
            .map_err(map_error)?;
        let choice = response.choices.into_iter().next().ok_or_else(|| {
            OrchestratorError::Inference("the provider returned no choices".to_string())
        })?;
        Ok(reply_tokens(choice.message.content.unwrap_or_default()))
    }

    async fn infer_stream(&self, prompt: &str) -> Result<TokenStream, OrchestratorError> {
        let stream = self
            .client
            .chat()
            .create_stream(self.request(prompt)?)
            .await
            .map_err(map_error)?;
        let tokens = stream.filter_map(|chunk| async move {
            match chunk {
                Ok(chunk) => chunk
                    .choices
                    .into_iter()
                    .next()
                    .and_then(|c| c.delta.content)
                    .filter(|t| !t.is_empty())
                    .map(Ok),
                Err(e) => Some(Err(map_error(e))),
            }
        });
        Ok(Box::pin(tokens))
    }
}

fn map_error(e: OpenAIError) -> OrchestratorError {
    match e {
        OpenAIError::ApiError(api) => {
            error_from_status(api.status_code.as_u16(), None, api.api_error.message)
        }
        OpenAIError::InvalidArgument(msg) => OrchestratorError::ConfigError(msg),
        other => OrchestratorError::Inference(other.to_string()),
    }
}
