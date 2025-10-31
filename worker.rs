//! Model worker abstraction
//!
//! Defines the trait for pluggable inference backends.
//!
//! Future implementations:
//! - LlamaCppWorker (llama.cpp via FFI or gRPC)
//! - VllmWorker (vLLM via HTTP or gRPC)
//! - OpenAiWorker (OpenAI API)
//! - AnthropicWorker (Anthropic API)

use crate::OrchestratorError;
use async_trait::async_trait;

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
