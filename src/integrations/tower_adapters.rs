//! Adapters between [`ModelWorker`] and [`tower::Service`](tower::Service).
//!
//! tower is the middleware layer most of async Rust is built on (axum,
//! tonic, reqwest's ecosystem). These two types let the orchestrator and
//! tower stacks use each other:
//!
//! - [`ServiceWorker`] turns any `Service<String, Response = String>` into a
//!   worker, so a model client wrapped in tower layers (timeouts, concurrency
//!   limits, load shedding, your own middleware) runs inside the pipeline.
//! - [`WorkerService`] turns any worker into a `Service<String>`, so the
//!   orchestrator's workers can be wrapped in tower layers or served by
//!   anything that accepts a service.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tower::{Service, ServiceExt};

use crate::worker::ModelWorker;
use crate::OrchestratorError;

/// A [`ModelWorker`] that calls a tower [`Service`] with the prompt and
/// returns its reply.
///
/// The service is cloned for each call (the usual tower pattern), and
/// readiness is awaited first, so backpressure from layers such as
/// `ConcurrencyLimit` is respected.
///
/// ```
/// use std::sync::Arc;
/// use tokio_prompt_orchestrator::{integrations::ServiceWorker, ModelWorker};
///
/// // Any Service<String, Response = String> works; service_fn is the simplest.
/// let svc = tower::service_fn(|prompt: String| async move {
///     Ok::<_, std::convert::Infallible>(prompt.to_uppercase())
/// });
/// let worker = ServiceWorker::new(svc);
/// # tokio_test::block_on(async {
/// assert_eq!(worker.infer("hi").await.unwrap(), vec!["HI".to_string()]);
/// # });
/// ```
///
/// Requires the `tower` feature.
#[derive(Clone, Debug)]
pub struct ServiceWorker<S> {
    service: S,
}

impl<S> ServiceWorker<S> {
    /// Wrap `service`.
    pub fn new(service: S) -> Self {
        Self { service }
    }

    /// The wrapped service.
    pub fn into_inner(self) -> S {
        self.service
    }
}

#[async_trait]
impl<S> ModelWorker for ServiceWorker<S>
where
    S: Service<String, Response = String> + Clone + Send + Sync + 'static,
    S::Future: Send,
    S::Error: std::fmt::Display + Send,
{
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let reply = self
            .service
            .clone()
            .oneshot(prompt.to_string())
            .await
            .map_err(|e| OrchestratorError::Inference(e.to_string()))?;
        Ok(super::reply_tokens(reply))
    }
}

/// A tower [`Service`] that answers a prompt with the worker's full reply.
///
/// Tokens are joined into one string. Errors are the worker's
/// [`OrchestratorError`], so a retry layer can still tell a bad API key from
/// a timeout with [`OrchestratorError::is_retryable`].
///
/// ```
/// use std::sync::Arc;
/// use tower::ServiceExt;
/// use tokio_prompt_orchestrator::{integrations::WorkerService, EchoWorker};
///
/// let svc = WorkerService::new(Arc::new(EchoWorker::new()));
/// # tokio_test::block_on(async {
/// let reply = svc.oneshot("hello world".to_string()).await.unwrap();
/// assert_eq!(reply, "hello world");
/// # });
/// ```
///
/// Requires the `tower` feature.
#[derive(Clone)]
pub struct WorkerService {
    worker: Arc<dyn ModelWorker>,
}

impl std::fmt::Debug for WorkerService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerService").finish_non_exhaustive()
    }
}

impl WorkerService {
    /// Wrap `worker`.
    pub fn new(worker: Arc<dyn ModelWorker>) -> Self {
        Self { worker }
    }
}

impl Service<String> for WorkerService {
    type Response = String;
    type Error = OrchestratorError;
    type Future = Pin<Box<dyn Future<Output = Result<String, OrchestratorError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // A worker has no readiness of its own; put a tower layer in front
        // of this service to limit concurrency or rate.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, prompt: String) -> Self::Future {
        let worker = Arc::clone(&self.worker);
        Box::pin(async move {
            let tokens = worker.infer(&prompt).await?;
            Ok(join_tokens(&tokens))
        })
    }
}

/// Join worker tokens into a reply. Workers that return the whole reply as
/// one token are passed through; word-split workers (EchoWorker) are
/// re-joined with spaces.
fn join_tokens(tokens: &[String]) -> String {
    match tokens {
        [] => String::new(),
        [one] => one.clone(),
        many => many.join(" "),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::EchoWorker;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn service_worker_passes_errors_through_as_retryable_inference() {
        let svc =
            tower::service_fn(|_prompt: String| async move { Err::<String, _>("provider down") });
        let err = ServiceWorker::new(svc).infer("x").await.unwrap_err();
        assert!(matches!(err, OrchestratorError::Inference(ref m) if m == "provider down"));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn service_worker_blank_reply_is_empty() {
        let svc = tower::service_fn(|_p: String| async move {
            Ok::<_, std::convert::Infallible>("   ".to_string())
        });
        assert!(ServiceWorker::new(svc).infer("x").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn service_worker_respects_a_tower_timeout_layer() {
        // A tower layer applied outside the orchestrator still governs calls.
        let slow = tower::service_fn(|p: String| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok::<_, tower::BoxError>(p)
        });
        let svc = tower::ServiceBuilder::new()
            .layer(tower::timeout::TimeoutLayer::new(Duration::from_millis(20)))
            .service(slow);
        let err = ServiceWorker::new(svc).infer("x").await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn worker_service_round_trips_through_the_pipeline_worker() {
        let svc = WorkerService::new(Arc::new(EchoWorker::new()));
        let reply = svc.oneshot("one two three".to_string()).await.unwrap();
        assert_eq!(reply, "one two three");
    }

    #[tokio::test]
    async fn worker_service_and_service_worker_compose() {
        // worker -> service -> worker: the reply survives both conversions.
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = {
            let calls = Arc::clone(&calls);
            tower::service_fn(move |p: String| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move { Ok::<_, std::convert::Infallible>(format!("<{p}>")) }
            })
        };
        let inner: Arc<dyn ModelWorker> = Arc::new(ServiceWorker::new(counted));
        let outer = ServiceWorker::new(WorkerService::new(inner));
        assert_eq!(outer.infer("q").await.unwrap(), vec!["<q>".to_string()]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
