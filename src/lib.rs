//! # tokio-prompt-orchestrator
//!
//! A production-leaning orchestrator for multi-stage LLM pipelines over Tokio.
//!
//! ## Architecture
//!
//! Five-stage pipeline with bounded channels and backpressure:
//! ```text
//! PromptRequest → RAG(512) → Assemble(512) → Inference(1024) → Post(512) → Stream(256)
//! ```
//!
//! ## Roadmap
//!
//! - TODO: gRPC worker protocol (streaming tokens, cancel, deadlines)
//! - TODO: distributed mesh (NATS/Kafka) for cross-node queues
//! - TODO: declarative DAG config (.toml / .yaml)
//! - TODO: per-core runtime pinning with session affinity
//! - TODO: OTLP/Prometheus metrics instead of tracing logs

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

pub mod metrics;
pub mod stages;
pub mod worker;

#[cfg(feature = "metrics-server")]
pub mod metrics_server;

// Re-exports for convenience
pub use stages::{spawn_pipeline, PipelineHandles};
pub use worker::{AnthropicWorker, EchoWorker, LlamaCppWorker, ModelWorker, OpenAiWorker, VllmWorker};

/// Orchestrator-specific errors
#[derive(Error, Debug)]
pub enum OrchestratorError {
    #[error("channel closed unexpectedly")]
    ChannelClosed,

    #[error("inference failed: {0}")]
    Inference(String),

    #[error("{0}")]
    Other(String),
}

/// Unique session identifier for request tracking and affinity
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    pub input: String,
    pub meta: HashMap<String, String>,
}

/// Output from RAG stage (retrieval-augmented generation)
#[derive(Debug, Clone)]
pub struct RagOutput {
    pub session: SessionId,
    pub context: String,
    pub original: PromptRequest,
}

/// Output from assembly stage (prompt construction)
#[derive(Debug, Clone)]
pub struct AssembleOutput {
    pub session: SessionId,
    pub prompt: String,
}

/// Output from inference stage (model generation)
#[derive(Debug, Clone)]
pub struct InferenceOutput {
    pub session: SessionId,
    pub tokens: Vec<String>,
}

/// Output from post-processing stage
#[derive(Debug, Clone)]
pub struct PostOutput {
    pub session: SessionId,
    pub text: String,
}

/// Session affinity sharding helper
///
/// Returns shard index [0, shards) for the given session.
/// This enables per-core runtime pinning in future iterations.
///
/// TODO: Use consistent hashing (jump hash, rendezvous) for better distribution
pub fn shard_session(session: &SessionId, shards: usize) -> usize {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    session.0.hash(&mut hasher);
    (hasher.finish() as usize) % shards
}

/// Helper for sending with graceful shedding on backpressure
///
/// Strategy: try_send → if full, log and drop (oldest in queue stays)
/// This prevents cascading delays under load spikes.
pub async fn send_with_shed<T>(
    tx: &tokio::sync::mpsc::Sender<T>,
    item: T,
    stage: &str,
) -> Result<(), OrchestratorError> {
    match tx.try_send(item) {
        Ok(_) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(stage = stage, "queue full, shedding request");
            // Drop the item - this is our graceful degradation
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
            .map(|i| SessionId::new(format!("session-{}", i)))
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

        // Should have reasonable distribution (not all in one shard)
        assert!(counts.iter().all(|&c| c > 0));
    }
}
