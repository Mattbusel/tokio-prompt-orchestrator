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

use std::collections::HashMap;
use thiserror::Error;

pub mod config;
pub mod coordination;
// pub mod momentum; // module removed — C++ SIMD momentum is in crates/
#[cfg(feature = "distributed")]
pub mod distributed;
pub mod enhanced;
pub mod metrics;
pub mod routing;
pub mod stages;
pub mod worker;

#[cfg(feature = "metrics-server")]
pub mod metrics_server;

#[cfg(feature = "web-api")]
pub mod web_api;

#[cfg(feature = "self-tune")]
pub mod self_tune;

#[cfg(feature = "self-modify")]
pub mod self_modify;

#[cfg(feature = "intelligence")]
pub mod intelligence;

#[cfg(feature = "evolution")]
pub mod evolution;

#[cfg(all(
    feature = "self-tune",
    feature = "self-modify",
    feature = "intelligence"
))]
pub mod self_improve;

#[cfg(all(feature = "self-tune", feature = "self-modify"))]
pub mod self_improve_loop;

#[cfg(feature = "tui")]
pub mod tui;

// Re-exports
pub use stages::{spawn_pipeline, LogSink, OutputSink, PipelineHandles, SinkError};
pub use worker::{
    AnthropicWorker, EchoWorker, LlamaCppWorker, LoadBalancedWorker, LoadBalanceStrategy,
    ModelWorker, OpenAiWorker, TokenStream, VllmWorker,
};

/// Orchestrator-specific errors
#[derive(Error, Debug)]
pub enum OrchestratorError {
    #[error("channel closed unexpectedly")]
    ChannelClosed,

    #[error("inference failed: {0}")]
    Inference(String),

    #[error("configuration error: {0}")]
    ConfigError(String),

    /// Provider returned HTTP 429 — callers should back off for `retry_after`.
    #[error("rate limited by provider (retry after {retry_after_secs}s)")]
    RateLimited {
        /// Seconds to wait before retrying, parsed from `Retry-After` header.
        retry_after_secs: u64,
    },

    /// Spending cap reached — no further inference allowed this session.
    #[error("budget exceeded: spent ${spent:.4} of ${limit:.4} limit")]
    BudgetExceeded { spent: f64, limit: f64 },

    #[error("{0}")]
    Other(String),
}

/// Unique session identifier for request tracking and affinity
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Initial prompt request from client
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub session: SessionId,
    /// Unique ID for distributed trace correlation across all pipeline stages.
    pub request_id: String,
    pub input: String,
    pub meta: HashMap<String, String>,
}

/// Output from RAG stage
#[derive(Debug, Clone)]
pub struct RagOutput {
    pub session: SessionId,
    pub context: String,
    pub original: PromptRequest,
}

/// Output from assembly stage
#[derive(Debug, Clone)]
pub struct AssembleOutput {
    pub session: SessionId,
    pub request_id: String,
    pub prompt: String,
}

/// Output from inference stage
#[derive(Debug, Clone)]
pub struct InferenceOutput {
    pub session: SessionId,
    pub request_id: String,
    pub tokens: Vec<String>,
}

/// Output from post-processing stage
#[derive(Debug, Clone)]
pub struct PostOutput {
    pub session: SessionId,
    pub request_id: String,
    pub text: String,
}

/// Session affinity sharding helper
pub fn shard_session(session: &SessionId, shards: usize) -> usize {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    session.0.hash(&mut hasher);
    (hasher.finish() as usize) % shards
}

/// Initialise tracing with env-filter support. Call once at binary startup.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let _ = fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .try_init();
}

/// Outcome of a [`send_with_shed`] call.
///
/// Distinguishes between successful delivery and a graceful shed so callers
/// can log/metric them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The item was successfully placed in the channel.
    Queued,
    /// The channel was full; the item was dropped to shed load.
    Shed,
}

/// Send with graceful shedding on backpressure.
///
/// # Returns
/// - `Ok(SendOutcome::Queued)` — item was accepted by the channel.
/// - `Ok(SendOutcome::Shed)` — channel was full; item was dropped gracefully.
/// - `Err(OrchestratorError::ChannelClosed)` — the receiver has been dropped.
///
/// # Panics
///
/// This function never panics.
pub async fn send_with_shed<T>(
    tx: &tokio::sync::mpsc::Sender<T>,
    item: T,
    stage: &str,
) -> Result<SendOutcome, OrchestratorError> {
    match tx.try_send(item) {
        Ok(_) => Ok(SendOutcome::Queued),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(stage = stage, "queue full, shedding request");
            Ok(SendOutcome::Shed)
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err(OrchestratorError::ChannelClosed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_with_shed_returns_shed_outcome_when_channel_full() {
        // Channel of capacity 1 — fill it, then send again to trigger shed
        let (tx, _rx) = tokio::sync::mpsc::channel::<u32>(1);
        // Fill the channel
        let first = send_with_shed(&tx, 1u32, "test").await.unwrap();
        assert_eq!(first, SendOutcome::Queued, "first send should be Queued");
        // Channel is now full — next send must be Shed
        let second = send_with_shed(&tx, 2u32, "test").await.unwrap();
        assert_eq!(
            second,
            SendOutcome::Shed,
            "second send on full channel should be Shed"
        );
    }

    #[tokio::test]
    async fn test_send_with_shed_returns_queued_when_space_available() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<u32>(10);
        let outcome = send_with_shed(&tx, 42u32, "test").await.unwrap();
        assert_eq!(outcome, SendOutcome::Queued);
    }

    #[tokio::test]
    async fn test_send_with_shed_returns_error_when_channel_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel::<u32>(10);
        drop(rx);
        let result = send_with_shed(&tx, 1u32, "test").await;
        assert!(matches!(result, Err(OrchestratorError::ChannelClosed)));
    }

    #[test]
    fn test_shard_session_deterministic() {
        let s = SessionId::new("test-session-123");
        assert_eq!(shard_session(&s, 4), shard_session(&s, 4));
    }

    #[test]
    fn test_shard_session_distribution() {
        let sessions: Vec<_> = (0..100).map(|i| SessionId::new(format!("s-{i}"))).collect();
        let counts: Vec<_> = (0..4usize)
            .map(|sh| {
                sessions
                    .iter()
                    .filter(|s| shard_session(s, 4) == sh)
                    .count()
            })
            .collect();
        assert!(counts.iter().all(|&c| c > 0));
    }
}
