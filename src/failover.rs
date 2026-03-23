//! # Provider Failover Chain
//!
//! Wraps multiple [`ModelWorker`] implementations in a priority-ordered failover
//! chain.  On failure the chain automatically retries with the next provider,
//! skipping any provider that the health monitor currently marks as unusable.
//!
//! ## Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use tokio_prompt_orchestrator::{EchoWorker, ModelWorker};
//! use tokio_prompt_orchestrator::provider_health::ProviderHealthMonitor;
//! use tokio_prompt_orchestrator::failover::FailoverChain;
//!
//! #[tokio::main]
//! async fn main() {
//!     let health = ProviderHealthMonitor::new(50);
//!     let chain = FailoverChain::new(health)
//!         .add_worker("primary", Arc::new(EchoWorker::new()))
//!         .add_worker("backup", Arc::new(EchoWorker::new()))
//!         .with_max_attempts(3);
//!
//!     let (provider_used, response) = chain.infer("Hello!").await.unwrap();
//!     println!("Answered by {}: {}", provider_used, response);
//! }
//! ```

use std::sync::Arc;
use std::time::Instant;

use crate::provider_health::ProviderHealthMonitor;
use crate::worker::ModelWorker;
use crate::OrchestratorError;

/// A priority-ordered chain of model workers with automatic failover.
///
/// Workers are tried in insertion order.  Unhealthy workers (as determined by
/// the [`ProviderHealthMonitor`]) are skipped without consuming an attempt
/// slot.  The `max_attempts` limit counts only *actual* inference attempts
/// (i.e. workers that were healthy and returned a result — success or failure).
pub struct FailoverChain {
    /// Ordered list of `(provider_id, worker)` pairs.
    workers: Vec<(String, Arc<dyn ModelWorker>)>,
    health: ProviderHealthMonitor,
    /// Maximum number of inference attempts before giving up.
    max_attempts: usize,
}

impl FailoverChain {
    /// Create a new, empty failover chain.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn new(health: ProviderHealthMonitor) -> Self {
        Self {
            workers: Vec::new(),
            health,
            max_attempts: 3,
        }
    }

    /// Append a worker to the end of the chain.
    ///
    /// Workers are tried in the order they are added.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn add_worker(
        mut self,
        provider_id: impl Into<String>,
        worker: Arc<dyn ModelWorker>,
    ) -> Self {
        self.workers.push((provider_id.into(), worker));
        self
    }

    /// Override the maximum number of *attempted* inferences before the chain
    /// gives up.  Defaults to `3`.  A value of `0` is silently raised to `1`.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn with_max_attempts(mut self, n: usize) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    /// Run inference through the chain.
    ///
    /// Workers are tried in priority order.  For each candidate:
    ///
    /// 1. The health monitor is consulted; unhealthy providers are skipped
    ///    (they do **not** count against `max_attempts`).
    /// 2. The worker's [`ModelWorker::infer`] method is called.
    /// 3. On success the outcome is recorded with the health monitor and the
    ///    `(provider_id, response_text)` tuple is returned.
    /// 4. On failure the outcome is recorded and the next candidate is tried
    ///    (unless `max_attempts` has been exhausted).
    ///
    /// Returns `Err(OrchestratorError::Inference)` if all candidates fail or
    /// the chain is empty.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub async fn infer(&self, prompt: &str) -> Result<(String, String), OrchestratorError> {
        if self.workers.is_empty() {
            return Err(OrchestratorError::Inference(
                "failover chain is empty — no workers configured".to_string(),
            ));
        }

        let mut attempts = 0usize;
        let mut last_error: Option<OrchestratorError> = None;

        for (provider_id, worker) in &self.workers {
            if attempts >= self.max_attempts {
                break;
            }

            // Skip providers currently deemed unhealthy.
            if !self.health.is_usable(provider_id).await {
                tracing::debug!(
                    provider = %provider_id,
                    "failover chain skipping unhealthy provider"
                );
                continue;
            }

            attempts += 1;
            let start = Instant::now();

            match worker.infer(prompt).await {
                Ok(tokens) => {
                    let latency_ms = start.elapsed().as_millis() as u64;
                    self.health.record(provider_id, latency_ms, true).await;

                    let response = tokens.join("");
                    tracing::debug!(
                        provider = %provider_id,
                        attempt = attempts,
                        latency_ms = latency_ms,
                        "failover chain: inference succeeded"
                    );
                    return Ok((provider_id.clone(), response));
                }
                Err(err) => {
                    let latency_ms = start.elapsed().as_millis() as u64;
                    self.health.record(provider_id, latency_ms, false).await;

                    tracing::warn!(
                        provider = %provider_id,
                        attempt = attempts,
                        error = %err,
                        "failover chain: inference failed, trying next provider"
                    );
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            OrchestratorError::Inference(
                "all providers in failover chain were unhealthy or exhausted".to_string(),
            )
        }))
    }

    /// Return the number of workers currently in the chain.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// Return `true` if the chain contains no workers.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_health::ProviderHealthMonitor;
    use crate::worker::EchoWorker;
    use async_trait::async_trait;

    // A worker that always returns Err.
    struct FailingWorker {
        message: String,
    }

    #[async_trait]
    impl ModelWorker for FailingWorker {
        async fn infer(&self, _prompt: &str) -> Result<Vec<String>, OrchestratorError> {
            Err(OrchestratorError::Inference(self.message.clone()))
        }
    }

    // A worker that returns a fixed string.
    struct FixedWorker {
        response: String,
    }

    #[async_trait]
    impl ModelWorker for FixedWorker {
        async fn infer(&self, _prompt: &str) -> Result<Vec<String>, OrchestratorError> {
            Ok(vec![self.response.clone()])
        }
    }

    fn health() -> ProviderHealthMonitor {
        ProviderHealthMonitor::new(20)
    }

    #[tokio::test]
    async fn test_empty_chain_returns_error() {
        let chain = FailoverChain::new(health());
        let result = chain.infer("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_single_echo_worker_succeeds() {
        let chain = FailoverChain::new(health())
            .add_worker("echo", Arc::new(EchoWorker::new()));
        let (provider, response) = chain.infer("hello").await.unwrap();
        assert_eq!(provider, "echo");
        assert!(!response.is_empty());
    }

    #[tokio::test]
    async fn test_primary_failure_falls_back_to_secondary() {
        let chain = FailoverChain::new(health())
            .add_worker("bad", Arc::new(FailingWorker { message: "oops".into() }))
            .add_worker("good", Arc::new(FixedWorker { response: "ok".into() }));

        let (provider, response) = chain.infer("prompt").await.unwrap();
        assert_eq!(provider, "good");
        assert_eq!(response, "ok");
    }

    #[tokio::test]
    async fn test_all_failing_returns_last_error() {
        let chain = FailoverChain::new(health())
            .add_worker("w1", Arc::new(FailingWorker { message: "err1".into() }))
            .add_worker("w2", Arc::new(FailingWorker { message: "err2".into() }))
            .with_max_attempts(5);

        let result = chain.infer("prompt").await;
        assert!(result.is_err());
        // Last error should be from w2.
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("err2"), "expected last error message, got: {err_str}");
    }

    #[tokio::test]
    async fn test_max_attempts_limits_tries() {
        // Three failing workers but max_attempts = 1.
        let chain = FailoverChain::new(health())
            .add_worker("w1", Arc::new(FailingWorker { message: "err1".into() }))
            .add_worker("w2", Arc::new(FailingWorker { message: "err2".into() }))
            .add_worker("w3", Arc::new(FixedWorker { response: "ok".into() }))
            .with_max_attempts(1);

        // Only w1 is attempted (1 attempt), w3 is never reached.
        let result = chain.infer("prompt").await;
        assert!(result.is_err(), "should fail because w3 was never reached");
    }

    #[tokio::test]
    async fn test_unhealthy_provider_is_skipped() {
        let h = health();
        // Poison p1 with enough consecutive failures to mark it unreachable.
        for _ in 0..5 {
            h.record("p1", 0, false).await;
        }

        let chain = FailoverChain::new(h)
            .add_worker("p1", Arc::new(FixedWorker { response: "from-p1".into() }))
            .add_worker("p2", Arc::new(FixedWorker { response: "from-p2".into() }));

        let (provider, response) = chain.infer("prompt").await.unwrap();
        // p1 should be skipped (unhealthy), p2 should answer.
        assert_eq!(provider, "p2");
        assert_eq!(response, "from-p2");
    }

    #[tokio::test]
    async fn test_health_monitor_updated_on_success() {
        let h = health();
        let chain = FailoverChain::new(h.clone())
            .add_worker("p1", Arc::new(FixedWorker { response: "hi".into() }));

        chain.infer("prompt").await.unwrap();

        let snap = h.get_health("p1").await.unwrap();
        assert_eq!(snap.total_requests, 1);
        assert_eq!(snap.total_errors, 0);
    }

    #[tokio::test]
    async fn test_health_monitor_updated_on_failure() {
        let h = health();
        let chain = FailoverChain::new(h.clone())
            .add_worker("p1", Arc::new(FailingWorker { message: "boom".into() }));

        let _ = chain.infer("prompt").await;

        let snap = h.get_health("p1").await.unwrap();
        assert_eq!(snap.total_requests, 1);
        assert_eq!(snap.total_errors, 1);
        assert_eq!(snap.consecutive_failures, 1);
    }

    #[test]
    fn test_len_and_is_empty() {
        let chain = FailoverChain::new(health());
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);

        let chain = chain.add_worker("p1", Arc::new(EchoWorker::new()));
        assert!(!chain.is_empty());
        assert_eq!(chain.len(), 1);
    }
}
