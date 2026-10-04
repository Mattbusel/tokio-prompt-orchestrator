//! Use the orchestrator with the Rust LLM crates you already have.
//!
//! Each integration is its own cargo feature, off by default, so you only
//! compile the one you use. They all produce a [`ModelWorker`], so the
//! deduplication, circuit breaker, retries, timeouts and dead-letter queue
//! work the same whichever client sits underneath.
//!
//! | Feature | Type | Wraps |
//! |---|---|---|
//! | `async-openai` | [`AsyncOpenAiWorker`] | an [`async_openai::Client`] you configured: OpenAI, Azure OpenAI, or any OpenAI-compatible server |
//! | `genai` | [`GenaiWorker`] | a [`genai::Client`]: OpenAI, Anthropic, Gemini, Ollama, Groq, DeepSeek, xAI, Cohere and more, picked by model name |
//! | `rig` | [`RigWorker`] | any [rig](https://docs.rs/rig-core) agent or model that implements `rig::completion::Prompt` (needs Rust 1.95) |
//! | `tower` | [`ServiceWorker`], [`WorkerService`] | any `tower::Service<String>`, in both directions |
//!
//! A [`Retriever`](crate::Retriever) that grounds prompts in your documents:
//!
//! | Feature | Type | Searches |
//! |---|---|---|
//! | `tantivy` | [`TantivyRetriever`] | a folder of Markdown/text files, BM25 full-text, in memory (needs Rust 1.90) |
//!
//! And [`Embedder`](crate::Embedder)s for semantic deduplication:
//!
//! | Feature | Type | Runs |
//! |---|---|---|
//! | `fastembed` | `FastEmbedder` | local ONNX models on the CPU, no API key (needs Rust 1.88) |
//! | `async-openai` | [`OpenAiEmbedder`] | the OpenAI embeddings API or a compatible server |
//! | `genai` | [`GenaiEmbedder`] | any provider genai supports embeddings for |
//!
//! [`ModelWorker`]: crate::ModelWorker

#[cfg(feature = "async-openai")]
mod async_openai_worker;
#[cfg(feature = "async-openai")]
#[cfg_attr(docsrs, doc(cfg(feature = "async-openai")))]
pub use async_openai_worker::AsyncOpenAiWorker;

#[cfg(feature = "genai")]
mod genai_worker;
#[cfg(feature = "genai")]
#[cfg_attr(docsrs, doc(cfg(feature = "genai")))]
pub use genai_worker::GenaiWorker;

#[cfg(feature = "rig")]
mod rig_worker;
#[cfg(feature = "rig")]
#[cfg_attr(docsrs, doc(cfg(feature = "rig")))]
pub use rig_worker::RigWorker;

mod embedders;
#[cfg(feature = "fastembed")]
#[cfg_attr(docsrs, doc(cfg(feature = "fastembed")))]
pub use embedders::FastEmbedder;
#[cfg(feature = "genai")]
#[cfg_attr(docsrs, doc(cfg(feature = "genai")))]
pub use embedders::GenaiEmbedder;
#[cfg(feature = "async-openai")]
#[cfg_attr(docsrs, doc(cfg(feature = "async-openai")))]
pub use embedders::OpenAiEmbedder;

#[cfg(feature = "tantivy")]
mod tantivy_retriever;
#[cfg(feature = "tantivy")]
#[cfg_attr(docsrs, doc(cfg(feature = "tantivy")))]
pub use tantivy_retriever::{TantivyRetriever, INDEXED_EXTENSIONS};

#[cfg(feature = "tower")]
mod tower_adapters;
#[cfg(feature = "tower")]
#[cfg_attr(docsrs, doc(cfg(feature = "tower")))]
pub use tower_adapters::{ServiceWorker, WorkerService};

/// Map an HTTP status and message from a provider client to the error the
/// pipeline understands: 401/403 are never retried, 429 backs off, and
/// everything else counts as a (retryable) inference failure.
#[allow(dead_code)] // unused when only the tower/rig integrations are enabled
pub(crate) fn error_from_status(
    status: u16,
    retry_after_secs: Option<u64>,
    message: impl std::fmt::Display,
) -> crate::OrchestratorError {
    match status {
        401 | 403 => crate::OrchestratorError::AuthFailed(format!("HTTP {status}: {message}")),
        429 => crate::OrchestratorError::RateLimited {
            retry_after_secs: retry_after_secs.unwrap_or(1),
        },
        _ => crate::OrchestratorError::Inference(format!("HTTP {status}: {message}")),
    }
}

/// Turn a complete reply into the token list workers return: the whole text
/// as one entry, or nothing when the model answered with only whitespace
/// (the same convention as the built-in workers).
#[allow(dead_code)]
pub(crate) fn reply_tokens(text: String) -> Vec<String> {
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![text]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OrchestratorError;

    #[test]
    fn auth_errors_are_not_retryable() {
        let e = error_from_status(401, None, "bad key");
        assert!(matches!(e, OrchestratorError::AuthFailed(_)));
        assert!(!e.is_retryable());
        assert!(matches!(
            error_from_status(403, None, "forbidden"),
            OrchestratorError::AuthFailed(_)
        ));
    }

    #[test]
    fn rate_limits_keep_the_retry_after() {
        match error_from_status(429, Some(7), "slow down") {
            OrchestratorError::RateLimited { retry_after_secs } => assert_eq!(retry_after_secs, 7),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn server_errors_are_retryable_inference_failures() {
        let e = error_from_status(503, None, "overloaded");
        assert!(matches!(e, OrchestratorError::Inference(_)));
        assert!(e.is_retryable());
    }

    #[test]
    fn whitespace_replies_become_empty() {
        assert!(reply_tokens("  \n".into()).is_empty());
        assert_eq!(reply_tokens("Paris".into()), vec!["Paris".to_string()]);
    }
}
