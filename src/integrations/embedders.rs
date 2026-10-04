//! [`Embedder`] implementations for semantic deduplication.

#[cfg(any(feature = "async-openai", feature = "genai", feature = "fastembed"))]
use async_trait::async_trait;

#[cfg(any(feature = "async-openai", feature = "genai", feature = "fastembed"))]
use crate::embedding::Embedder;
#[cfg(any(feature = "async-openai", feature = "genai", feature = "fastembed"))]
use crate::OrchestratorError;

// ---------------------------------------------------------------------------
// fastembed: local ONNX models
// ---------------------------------------------------------------------------

/// Local text embeddings with [fastembed](https://docs.rs/fastembed): ONNX
/// models such as BGE-small and all-MiniLM run on the CPU, with no API key
/// and no per-call cost.
///
/// The model is downloaded once on first use (128 MB for the default
/// BGE-small-en-v1.5) into `FASTEMBED_CACHE_DIR` if set, otherwise the
/// user's cache directory (see [`FastEmbedder::default_cache_dir`]). Embedding runs on Tokio's
/// blocking pool so it never stalls the async runtime.
///
/// ```no_run
/// use std::{sync::Arc, time::Duration};
/// use tokio_prompt_orchestrator::{enhanced::Deduplicator, integrations::FastEmbedder};
///
/// # fn main() -> Result<(), tokio_prompt_orchestrator::OrchestratorError> {
/// let dedup = Deduplicator::new(Duration::from_secs(600))
///     .with_embedder(Arc::new(FastEmbedder::try_default()?), 0.9);
/// # Ok(())
/// # }
/// ```
///
/// Requires the `fastembed` feature (Rust 1.88 or newer).
#[cfg(feature = "fastembed")]
#[cfg_attr(docsrs, doc(cfg(feature = "fastembed")))]
#[derive(Clone)]
pub struct FastEmbedder {
    model: std::sync::Arc<std::sync::Mutex<fastembed::TextEmbedding>>,
}

#[cfg(feature = "fastembed")]
impl std::fmt::Debug for FastEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastEmbedder").finish_non_exhaustive()
    }
}

#[cfg(feature = "fastembed")]
impl FastEmbedder {
    /// Load `BGE-small-en-v1.5` (384 dimensions, English), downloading it
    /// (128 MB) on first use into [`default_cache_dir`](Self::default_cache_dir).
    ///
    /// # Errors
    ///
    /// [`OrchestratorError::ConfigError`] if the model cannot be downloaded
    /// or loaded.
    pub fn try_default() -> Result<Self, OrchestratorError> {
        Self::try_new(
            fastembed::TextInitOptions::new(fastembed::EmbeddingModel::BGESmallENV15)
                .with_cache_dir(Self::default_cache_dir()),
        )
    }

    /// Where models are stored: `FASTEMBED_CACHE_DIR` when set, otherwise
    /// `tokio-prompt-orchestrator/fastembed` under the user's cache directory
    /// (`%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` or `~/.cache` on
    /// Linux, `~/Library/Caches` on macOS). fastembed's own default is a
    /// folder in the current directory, which leaves a large model wherever
    /// a program happens to be started.
    pub fn default_cache_dir() -> std::path::PathBuf {
        if let Some(dir) = std::env::var_os("FASTEMBED_CACHE_DIR") {
            return dir.into();
        }
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
        } else if cfg!(target_os = "macos") {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Caches"))
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache"))
                })
        };
        base.unwrap_or_else(std::env::temp_dir)
            .join("tokio-prompt-orchestrator")
            .join("fastembed")
    }

    /// Load the model described by `options` (any fastembed text model,
    /// cache directory, thread count and so on).
    ///
    /// # Errors
    ///
    /// [`OrchestratorError::ConfigError`] if the model cannot be downloaded
    /// or loaded.
    pub fn try_new(options: fastembed::TextInitOptions) -> Result<Self, OrchestratorError> {
        let model = fastembed::TextEmbedding::try_new(options)
            .map_err(|e| OrchestratorError::ConfigError(format!("fastembed: {e}")))?;
        Ok(Self {
            model: std::sync::Arc::new(std::sync::Mutex::new(model)),
        })
    }
}

#[cfg(feature = "fastembed")]
#[async_trait]
impl Embedder for FastEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, OrchestratorError> {
        let model = std::sync::Arc::clone(&self.model);
        let text = text.to_string();
        tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|_| OrchestratorError::Other("fastembed model lock poisoned".into()))?;
            model
                .embed([text], None)
                .map_err(|e| OrchestratorError::Other(format!("fastembed: {e}")))?
                .into_iter()
                .next()
                .ok_or_else(|| OrchestratorError::Other("fastembed returned no vector".into()))
        })
        .await
        .map_err(|e| OrchestratorError::Other(format!("embedding task failed: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// async-openai: the OpenAI embeddings API
// ---------------------------------------------------------------------------

/// Embeddings from the OpenAI embeddings API (or any server that implements
/// it, such as Ollama or vLLM) through an [`async_openai::Client`].
///
/// ```no_run
/// use async_openai::Client;
/// use tokio_prompt_orchestrator::integrations::OpenAiEmbedder;
///
/// let embedder = OpenAiEmbedder::new(Client::new(), "text-embedding-3-small");
/// ```
///
/// Requires the `async-openai` feature.
#[cfg(feature = "async-openai")]
#[cfg_attr(docsrs, doc(cfg(feature = "async-openai")))]
#[derive(Clone)]
pub struct OpenAiEmbedder<C: async_openai::config::Config = async_openai::config::OpenAIConfig> {
    client: async_openai::Client<C>,
    model: String,
}

#[cfg(feature = "async-openai")]
impl<C: async_openai::config::Config> std::fmt::Debug for OpenAiEmbedder<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEmbedder")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "async-openai")]
impl<C: async_openai::config::Config> OpenAiEmbedder<C> {
    /// Embed with `model` through `client`.
    pub fn new(client: async_openai::Client<C>, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
        }
    }
}

#[cfg(feature = "async-openai")]
#[async_trait]
impl<C: async_openai::config::Config + 'static> Embedder for OpenAiEmbedder<C> {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, OrchestratorError> {
        use async_openai::types::embeddings::{CreateEmbeddingRequestArgs, EmbeddingInput};
        let request = CreateEmbeddingRequestArgs::default()
            .model(self.model.clone())
            .input(EmbeddingInput::String(text.to_string()))
            .build()
            .map_err(|e| OrchestratorError::ConfigError(e.to_string()))?;
        let response = self
            .client
            .embeddings()
            .create(request)
            .await
            .map_err(|e| match e {
                async_openai::error::OpenAIError::ApiError(api) => {
                    super::error_from_status(api.status_code.as_u16(), None, api.api_error.message)
                }
                other => OrchestratorError::Inference(other.to_string()),
            })?;
        response
            .data
            .into_iter()
            .next()
            .map(|e| e.embedding)
            .ok_or_else(|| OrchestratorError::Inference("no embedding in the response".into()))
    }
}

// ---------------------------------------------------------------------------
// genai: any provider genai supports embeddings for
// ---------------------------------------------------------------------------

/// Embeddings through a [`genai::Client`], for any provider genai supports
/// embeddings for (OpenAI, Gemini, Cohere, Ollama, ...), picked by model
/// name.
///
/// ```no_run
/// use tokio_prompt_orchestrator::integrations::GenaiEmbedder;
///
/// // A local Ollama embedding model; no API key needed.
/// let embedder = GenaiEmbedder::new(genai::Client::default(), "nomic-embed-text");
/// ```
///
/// Requires the `genai` feature.
#[cfg(feature = "genai")]
#[cfg_attr(docsrs, doc(cfg(feature = "genai")))]
#[derive(Clone)]
pub struct GenaiEmbedder {
    client: genai::Client,
    model: String,
}

#[cfg(feature = "genai")]
impl std::fmt::Debug for GenaiEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenaiEmbedder")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "genai")]
impl GenaiEmbedder {
    /// Embed with `model` through `client`.
    pub fn new(client: genai::Client, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
        }
    }
}

#[cfg(feature = "genai")]
#[async_trait]
impl Embedder for GenaiEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, OrchestratorError> {
        let response = self
            .client
            .embed(self.model.as_str(), text, None)
            .await
            .map_err(|e| OrchestratorError::Inference(e.to_string()))?;
        response
            .into_vectors()
            .into_iter()
            .next()
            .ok_or_else(|| OrchestratorError::Inference("no embedding in the response".into()))
    }
}
