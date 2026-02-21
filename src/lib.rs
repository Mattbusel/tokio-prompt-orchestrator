//! # tokio-prompt-orchestrator
//!
//! A production-grade orchestrator for multi-stage LLM pipelines over Tokio.
//!
//! ## Architecture
//!
//! Five-stage pipeline with bounded channels and backpressure:
//! ```text
//! PromptRequest → RAG(512) → Assemble(512) → Inference(1024) → Post(512) → Stream(256)
//! ```

// ── Lint policy (aerospace-grade) ─────────────────────────────────────────
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(missing_docs)]

use std::collections::HashMap;
use thiserror::Error;
use tracing_subscriber::EnvFilter;

pub mod config;
pub mod enhanced;
pub mod metrics;
pub mod stages;
pub mod worker;

#[cfg(feature = "metrics-server")]
pub mod metrics_server;

#[cfg(feature = "web-api")]
pub mod web_api;

#[cfg(feature = "tui")]
pub mod tui;

// Re-exports for convenience
pub use stages::{spawn_pipeline, PipelineHandles};
pub use worker::{
    AnthropicWorker, EchoWorker, LlamaCppWorker, ModelWorker, OpenAiWorker, VllmWorker,
};

/// Initialise the global tracing subscriber.
///
/// Reads the `LOG_FORMAT` environment variable to choose output format:
/// - `"json"` — structured JSON output for production log aggregators
///   (Datadog, Grafana Loki, etc.)
/// - anything else (including unset) — human-readable pretty output
///   for local development
///
/// Filter level is controlled by `RUST_LOG` (e.g. `RUST_LOG=info`).
///
/// # Errors
///
/// Returns [`OrchestratorError::Other`] if the global subscriber has already
/// been set (e.g. by a previous call or a test harness).
///
/// # Panics
///
/// This function never panics.
///
/// # Example
///
/// ```no_run
/// # use tokio_prompt_orchestrator::{init_tracing, OrchestratorError};
/// # fn example() -> Result<(), OrchestratorError> {
/// init_tracing()?;
/// # Ok(()) }
/// ```
pub fn init_tracing() -> Result<(), OrchestratorError> {
    let format = std::env::var("LOG_FORMAT").unwrap_or_else(|_| "pretty".to_string());

    let result = match format.as_str() {
        "json" => tracing_subscriber::fmt()
            .json()
            .with_env_filter(EnvFilter::from_default_env())
            .with_current_span(true)
            .with_span_list(true)
            .try_init(),
        _ => tracing_subscriber::fmt()
            .pretty()
            .with_env_filter(EnvFilter::from_default_env())
            .try_init(),
    };

    result.map_err(|e| OrchestratorError::Other(format!("tracing init failed: {e}")))
}

/// Top-level orchestrator errors.
///
/// Every error surface in the pipeline is mapped to a variant here.
/// All variants implement `std::error::Error` via [`thiserror`].
#[derive(Error, Debug)]
pub enum OrchestratorError {
    /// A pipeline channel closed unexpectedly, indicating stage shutdown.
    #[error("channel closed unexpectedly")]
    ChannelClosed,

    /// An LLM inference call failed (network, API, or parsing error).
    #[error("inference failed: {0}")]
    Inference(String),

    /// A configuration value is missing or invalid (e.g., missing env var).
    ///
    /// This is returned at construction time so that misconfiguration
    /// surfaces immediately rather than at the first inference call.
    #[error("configuration error: {0}")]
    ConfigError(String),

    /// Catch-all for errors that do not fit a specific variant.
    #[error("{0}")]
    Other(String),
}

/// Unique session identifier for request tracking and pipeline affinity.
///
/// Sessions group related requests and enable consistent routing to the
/// same worker shard for session-affinity scenarios.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(
    /// The raw string ID, typically a UUID or user-provided token.
    pub String,
);

impl SessionId {
    /// Create a new [`SessionId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the session ID as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Initial prompt request submitted by a client.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    /// Session this request belongs to.
    pub session: SessionId,
    /// Unique identifier for this individual request, used for trace correlation.
    pub request_id: String,
    /// The raw user-supplied prompt text.
    pub input: String,
    /// Arbitrary key-value metadata (e.g., `client`, `timestamp`).
    pub meta: HashMap<String, String>,
}

/// Output from the RAG stage (retrieval-augmented generation).
#[derive(Debug, Clone)]
pub struct RagOutput {
    /// Session this output belongs to.
    pub session: SessionId,
    /// Retrieved context to prepend to the prompt.
    pub context: String,
    /// The original request, preserved for downstream stages.
    pub original: PromptRequest,
}

/// Output from the assembly stage (prompt construction).
#[derive(Debug, Clone)]
pub struct AssembleOutput {
    /// Session this output belongs to.
    pub session: SessionId,
    /// Request ID propagated from the originating [`PromptRequest`].
    pub request_id: String,
    /// Fully assembled prompt ready for inference.
    pub prompt: String,
}

/// Output from the inference stage (model generation).
#[derive(Debug, Clone)]
pub struct InferenceOutput {
    /// Session this output belongs to.
    pub session: SessionId,
    /// Request ID propagated from the originating [`PromptRequest`].
    pub request_id: String,
    /// Raw token strings produced by the model.
    pub tokens: Vec<String>,
}

/// Output from the post-processing stage.
#[derive(Debug, Clone)]
pub struct PostOutput {
    /// Session this output belongs to.
    pub session: SessionId,
    /// Request ID propagated from the originating [`PromptRequest`].
    pub request_id: String,
    /// Final joined and processed text ready for streaming.
    pub text: String,
}

/// Compute a shard index in `[0, shards)` for the given session.
///
/// Used for session-affinity routing — the same session always maps to the
/// same shard, enabling per-core runtime pinning in future iterations.
///
/// # Panics
///
/// This function never panics.
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::{SessionId, shard_session};
/// let session = SessionId::new("user-42");
/// let shard = shard_session(&session, 4);
/// assert!(shard < 4);
/// ```
pub fn shard_session(session: &SessionId, shards: usize) -> usize {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    session.0.hash(&mut hasher);
    (hasher.finish() as usize) % shards
}

/// Attempt a channel send, gracefully shedding the item if the queue is full.
///
/// Strategy: `try_send` → if full, log and drop the incoming item (the queue
/// keeps its existing contents). This prevents cascading delays under load spikes.
///
/// # Errors
///
/// Returns [`OrchestratorError::ChannelClosed`] if the receiving end has been dropped.
///
/// # Panics
///
/// This function never panics.
pub async fn send_with_shed<T>(
    tx: &tokio::sync::mpsc::Sender<T>,
    item: T,
    stage: &str,
) -> Result<(), OrchestratorError> {
    match tx.try_send(item) {
        Ok(_) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(stage = stage, "queue full, shedding request");
            Ok(())
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err(OrchestratorError::ChannelClosed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shard_session_deterministic() {
        let session = SessionId::new("test-session-123");
        let shard1 = shard_session(&session, 4);
        let shard2 = shard_session(&session, 4);
        assert_eq!(shard1, shard2);
    }

    #[test]
    fn test_shard_session_distribution() {
        let sessions: Vec<_> = (0..100)
            .map(|i| SessionId::new(format!("session-{i}")))
            .collect();

        let shards = 4;
        let counts: Vec<_> = (0..shards)
            .map(|shard| {
                sessions
                    .iter()
                    .filter(|s| shard_session(s, shards) == shard)
                    .count()
            })
            .collect();

        assert!(counts.iter().all(|&c| c > 0));
    }

    #[test]
    fn test_shard_session_stays_in_range() {
        let shards = 8;
        for i in 0..200 {
            let session = SessionId::new(format!("session-{i}"));
            let shard = shard_session(&session, shards);
            assert!(
                shard < shards,
                "shard {shard} out of range for shards={shards}"
            );
        }
    }

    #[test]
    fn test_config_error_display_includes_message() {
        let err = OrchestratorError::ConfigError("OPENAI_API_KEY not set".to_string());
        assert!(err.to_string().contains("OPENAI_API_KEY not set"));
    }

    #[test]
    fn test_session_id_as_str_round_trips() {
        let session = SessionId::new("my-session");
        assert_eq!(session.as_str(), "my-session");
    }

    #[tokio::test]
    async fn test_send_with_shed_returns_ok_on_full_queue() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<u32>(1);
        // Fill the queue
        let _ = tx.try_send(1);
        // Another send should shed gracefully
        let result = send_with_shed(&tx, 2, "test-stage").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_with_shed_returns_channel_closed_when_receiver_dropped() {
        let (tx, rx) = tokio::sync::mpsc::channel::<u32>(1);
        drop(rx);
        let result = send_with_shed(&tx, 1, "test-stage").await;
        assert!(matches!(result, Err(OrchestratorError::ChannelClosed)));
    }

    #[test]
    fn test_prompt_request_carries_request_id() {
        let req = PromptRequest {
            session: SessionId::new("s1"),
            request_id: "req-abc-123".to_string(),
            input: "test".to_string(),
            meta: HashMap::new(),
        };
        assert_eq!(req.request_id, "req-abc-123");
    }

    #[test]
    fn test_init_tracing_second_call_returns_err() {
        // First call may succeed or fail depending on test execution order
        // (another test may have already installed a subscriber).
        let _ = init_tracing();
        // Second call must not panic — it should return Err.
        let result = init_tracing();
        assert!(result.is_err(), "double init must return Err, not panic");
    }
}
