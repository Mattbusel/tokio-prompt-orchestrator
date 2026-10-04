//! [`ModelWorker`] backed by a [rig](https://docs.rs/rig-core) completion model.

use async_trait::async_trait;
use futures::StreamExt;
use rig_core::completion::CompletionRequest;
use rig_core::operation::Completion;
use rig_core::streaming::{Item, StreamEvent};
use rig_core::{DynModel, ProviderError, ProviderResponseError};

use super::{error_from_status, reply_tokens};
use crate::worker::{ModelWorker, TokenStream};
use crate::OrchestratorError;

/// A [`ModelWorker`] that sends each prompt to a rig completion model.
///
/// Any provider rig supports works: build the model the way you already do
/// and hand it over. The orchestrator then adds deduplication, the circuit
/// breaker, retries, timeouts and the dead-letter queue in front of it.
///
/// ```no_run
/// use std::sync::Arc;
/// use rig_core::providers::openai::{self, OpenAI};
/// use tokio_prompt_orchestrator::{integrations::RigWorker, spawn_pipeline};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let model = OpenAI::from_env()?.completion(openai::GPT_5_2);
/// let worker = RigWorker::new(model).with_max_tokens(512);
/// # tokio_test::block_on(async {
/// let handles = spawn_pipeline(Arc::new(worker));
/// # });
/// # Ok(())
/// # }
/// ```
///
/// Requires the `rig` feature, which needs Rust 1.95 or newer (rig-core's
/// minimum).
#[derive(Clone, Debug)]
pub struct RigWorker {
    model: DynModel<Completion>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
}

impl RigWorker {
    /// Wrap a rig completion model (a `rig_core::Model` or an already
    /// erased [`DynModel`]).
    pub fn new(model: impl Into<DynModel<Completion>>) -> Self {
        Self {
            model: model.into(),
            max_tokens: None,
            temperature: None,
        }
    }

    /// Cap the length of each reply.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set the sampling temperature.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    fn request(&self, prompt: &str) -> CompletionRequest {
        CompletionRequest::from(prompt)
            .max_tokens(self.max_tokens)
            .temperature(self.temperature)
    }
}

#[async_trait]
impl ModelWorker for RigWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let response = self
            .model
            .call(self.request(prompt))
            .await
            .map_err(map_error)?;
        Ok(reply_tokens(response.text()))
    }

    async fn infer_stream(&self, prompt: &str) -> Result<TokenStream, OrchestratorError> {
        let stream = self.model.stream(self.request(prompt)).map_err(map_error)?;
        let tokens = stream.filter_map(|item| async move {
            match item {
                Ok(Item::Event(StreamEvent::Text { text, .. })) if !text.is_empty() => {
                    Some(Ok(text))
                }
                Ok(_) => None,
                Err(e) => Some(Err(map_error(e))),
            }
        });
        Ok(Box::pin(tokens))
    }
}

fn map_error(e: ProviderError) -> OrchestratorError {
    match e {
        ProviderError::InvalidAuthentication(reply) => {
            OrchestratorError::AuthFailed(reply_summary(&reply))
        }
        ProviderError::ProviderResponse(reply) => match reply.status {
            Some(status) if !status.is_success() => {
                let retry_after = reply
                    .headers
                    .as_ref()
                    .and_then(|h| h.get("retry-after"))
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                error_from_status(status.as_u16(), retry_after, &reply.body)
            }
            _ => OrchestratorError::Inference(reply_summary(&reply)),
        },
        other => OrchestratorError::Inference(other.to_string()),
    }
}

fn reply_summary(reply: &ProviderResponseError) -> String {
    match reply.status {
        Some(status) => format!("HTTP {}: {}", status.as_u16(), reply.body),
        None => reply.body.clone(),
    }
}
