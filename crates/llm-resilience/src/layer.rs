//! A tower [`Layer`] that adds single-flight deduplication and a circuit
//! breaker to any [`Service`]: an HTTP client, an LLM client, a gRPC channel.
//!
//! Use it when you already have a tower stack and want identical in-flight
//! requests to share one call, and a failing dependency to fail fast.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tower_layer::Layer;
use tower_service::Service;

use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerError};
use crate::single_flight::{Dedup, DedupStats};

type KeyFn<Req, K> = Arc<dyn Fn(&Req) -> Option<K> + Send + Sync>;
type FailureFn<Resp> = Arc<dyn Fn(&Resp) -> bool + Send + Sync>;

/// The error a [`ResilienceService`] returns.
///
/// The inner service's error is held in an [`Arc`] because a deduplicated
/// call hands the same error to every caller that joined it.
#[non_exhaustive]
pub enum ResilienceError<E> {
    /// The circuit breaker is open: the inner service was not called.
    CircuitOpen {
        /// Roughly how long until the breaker lets a probe through, when
        /// known.
        retry_after: Option<Duration>,
    },
    /// The inner service failed (for this call, or for the deduplicated call
    /// this one joined).
    Upstream(Arc<E>),
}

impl<E> ResilienceError<E> {
    /// Whether this is [`ResilienceError::CircuitOpen`].
    pub fn is_circuit_open(&self) -> bool {
        matches!(self, ResilienceError::CircuitOpen { .. })
    }

    /// The inner service's error, if that is what this is.
    pub fn upstream(&self) -> Option<&E> {
        match self {
            ResilienceError::Upstream(e) => Some(e),
            ResilienceError::CircuitOpen { .. } => None,
        }
    }
}

impl<E> Clone for ResilienceError<E> {
    fn clone(&self) -> Self {
        match self {
            ResilienceError::CircuitOpen { retry_after } => ResilienceError::CircuitOpen {
                retry_after: *retry_after,
            },
            ResilienceError::Upstream(e) => ResilienceError::Upstream(Arc::clone(e)),
        }
    }
}

impl<E: fmt::Debug> fmt::Debug for ResilienceError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResilienceError::CircuitOpen { retry_after } => f
                .debug_struct("CircuitOpen")
                .field("retry_after", retry_after)
                .finish(),
            ResilienceError::Upstream(e) => f.debug_tuple("Upstream").field(e).finish(),
        }
    }
}

impl<E: fmt::Display> fmt::Display for ResilienceError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResilienceError::CircuitOpen {
                retry_after: Some(d),
            } => write!(f, "circuit breaker open, retry in {}s", d.as_secs().max(1)),
            ResilienceError::CircuitOpen { retry_after: None } => {
                f.write_str("circuit breaker open")
            }
            ResilienceError::Upstream(e) => write!(f, "{e}"),
        }
    }
}

impl<E> std::error::Error for ResilienceError<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResilienceError::Upstream(e) => Some(&**e as &(dyn std::error::Error + 'static)),
            ResilienceError::CircuitOpen { .. } => None,
        }
    }
}

/// A tower [`Layer`] that deduplicates concurrent identical requests and
/// fails fast through a circuit breaker.
///
/// - **Deduplication** is opt-in with [`dedup_by`](Self::dedup_by): give it a
///   function from a request to a key (or `None` to skip dedup for that
///   request). Concurrent requests with the same key make one call to the
///   inner service and all get its response (so the response type must be
///   `Clone`; for HTTP bodies, buffer them first, or use the
///   `reqwest-middleware` integration, which does that for you).
/// - **Circuit breaker** is on by default (opens after 5 consecutive
///   failures, probes again after 30 s). Errors from the inner service count
///   as failures; [`failure_if`](Self::failure_if) also counts responses you
///   consider failed (an HTTP 5xx, say) without turning them into errors.
///
/// The breaker sits inside the dedup, so ten deduplicated callers count as
/// one call. Each service the layer wraps gets its own in-flight table, and
/// clones of that service share it. The circuit breaker is shared by every
/// service the layer wraps (clones of a [`CircuitBreaker`] share state).
///
/// ```
/// use std::time::Duration;
/// use llm_resilience::CircuitBreaker;
/// use llm_resilience::ResilienceLayer;
/// use tower::{ServiceBuilder, ServiceExt};
///
/// # #[tokio::main]
/// # async fn main() {
/// // Any Service works; here a fake LLM client that takes a prompt.
/// let llm = tower::service_fn(|prompt: String| async move {
///     Ok::<_, std::io::Error>(format!("answer to {prompt}"))
/// });
///
/// let svc = ServiceBuilder::new()
///     .layer(
///         ResilienceLayer::new()
///             .dedup_by(|prompt: &String| Some(prompt.clone()))
///             .with_circuit_breaker(CircuitBreaker::new(3, 0.8, Duration::from_secs(10))),
///     )
///     .service(llm);
///
/// let reply = svc.oneshot("2+2".to_string()).await.unwrap();
/// assert_eq!(reply, "answer to 2+2");
/// # }
/// ```
///
/// Requires the `tower` feature.
pub struct ResilienceLayer<Req, Resp, K = NoDedup> {
    key_fn: Option<KeyFn<Req, K>>,
    breaker: Option<CircuitBreaker>,
    failure_if: Option<FailureFn<Resp>>,
}

impl<Req, Resp, K> Clone for ResilienceLayer<Req, Resp, K> {
    fn clone(&self) -> Self {
        Self {
            key_fn: self.key_fn.clone(),
            breaker: self.breaker.clone(),
            failure_if: self.failure_if.clone(),
        }
    }
}

impl<Req, Resp, K> fmt::Debug for ResilienceLayer<Req, Resp, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResilienceLayer")
            .field("dedup", &self.key_fn.is_some())
            .field("circuit_breaker", &self.breaker.is_some())
            .field("failure_if", &self.failure_if.is_some())
            .finish()
    }
}

/// The key type of a [`ResilienceLayer`] or [`ResilienceService`] that does
/// not deduplicate (before [`ResilienceLayer::dedup_by`] is called).
///
/// Without deduplication the response type does not need to be `Clone`, so a
/// circuit-breaker-only layer works over any service, `http::Response`
/// bodies included.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDedup;

impl<Req, Resp> Default for ResilienceLayer<Req, Resp, NoDedup> {
    fn default() -> Self {
        Self::new()
    }
}

/// Default breaker: open after 5 consecutive failures, require an 80 %
/// success rate to close, probe after 30 s.
fn default_breaker() -> CircuitBreaker {
    CircuitBreaker::new(5, 0.8, Duration::from_secs(30))
}

impl<Req, Resp> ResilienceLayer<Req, Resp, NoDedup> {
    /// A layer with the default circuit breaker (5 consecutive failures
    /// open it, a probe goes through after 30 s) and no deduplication.
    pub fn new() -> Self {
        Self {
            key_fn: None,
            breaker: Some(default_breaker()),
            failure_if: None,
        }
    }
}

impl<Req, Resp, K> ResilienceLayer<Req, Resp, K> {
    /// Deduplicate concurrent requests that map to the same key. Return
    /// `None` from `key` for requests that must always run (non-idempotent
    /// calls, streaming calls).
    ///
    /// The key should capture everything that changes the response,
    /// including the credentials when one service serves several tenants.
    pub fn dedup_by<K2, F>(self, key: F) -> ResilienceLayer<Req, Resp, K2>
    where
        K2: std::hash::Hash + Eq + Clone,
        F: Fn(&Req) -> Option<K2> + Send + Sync + 'static,
    {
        ResilienceLayer {
            key_fn: Some(Arc::new(key)),
            breaker: self.breaker,
            failure_if: self.failure_if,
        }
    }

    /// Use `breaker` instead of the default. Clones of a [`CircuitBreaker`]
    /// share state, so passing a clone lets you watch or reset it.
    #[must_use]
    pub fn with_circuit_breaker(mut self, breaker: CircuitBreaker) -> Self {
        self.breaker = Some(breaker);
        self
    }

    /// Turn the circuit breaker off (deduplication only).
    #[must_use]
    pub fn without_circuit_breaker(mut self) -> Self {
        self.breaker = None;
        self
    }

    /// Count a successful response as a failure for the circuit breaker when
    /// `is_failure` returns `true`. The response is still returned to the
    /// caller unchanged. Typical use: `|resp| resp.status().is_server_error()`.
    #[must_use]
    pub fn failure_if<F>(mut self, is_failure: F) -> Self
    where
        F: Fn(&Resp) -> bool + Send + Sync + 'static,
    {
        self.failure_if = Some(Arc::new(is_failure));
        self
    }

    /// The circuit breaker, if one is configured.
    pub fn circuit_breaker(&self) -> Option<&CircuitBreaker> {
        self.breaker.as_ref()
    }
}

impl<S, Req, K> Layer<S> for ResilienceLayer<Req, S::Response, K>
where
    S: Service<Req>,
{
    type Service = ResilienceService<S, Req, K>;

    fn layer(&self, inner: S) -> Self::Service {
        ResilienceService {
            inner,
            dedup: self.key_fn.as_ref().map(|f| (Dedup::new(), Arc::clone(f))),
            breaker: self.breaker.clone(),
            failure_if: self.failure_if.clone(),
        }
    }
}

/// What one deduplicated call hands to everyone who joined it.
type SharedResult<S, Req> =
    Result<<S as Service<Req>>::Response, ResilienceError<<S as Service<Req>>::Error>>;

/// A service's in-flight table and the function that keys its requests.
type DedupSlot<S, Req, K> = Option<(Dedup<K, SharedResult<S, Req>>, KeyFn<Req, K>)>;

/// The [`Service`] produced by [`ResilienceLayer`].
///
/// Requires the `tower` feature.
pub struct ResilienceService<S, Req, K = NoDedup>
where
    S: Service<Req>,
{
    inner: S,
    dedup: DedupSlot<S, Req, K>,
    breaker: Option<CircuitBreaker>,
    failure_if: Option<FailureFn<S::Response>>,
}

impl<S, Req, K> Clone for ResilienceService<S, Req, K>
where
    S: Service<Req> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            dedup: self.dedup.as_ref().map(|(d, f)| (d.clone(), Arc::clone(f))),
            breaker: self.breaker.clone(),
            failure_if: self.failure_if.clone(),
        }
    }
}

impl<S, Req, K> fmt::Debug for ResilienceService<S, Req, K>
where
    S: Service<Req> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResilienceService")
            .field("inner", &self.inner)
            .field("dedup", &self.dedup.is_some())
            .field("circuit_breaker", &self.breaker.is_some())
            .finish()
    }
}

impl<S, Req, K> ResilienceService<S, Req, K>
where
    S: Service<Req>,
{
    /// The wrapped service.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// The circuit breaker, if one is configured.
    pub fn circuit_breaker(&self) -> Option<&CircuitBreaker> {
        self.breaker.as_ref()
    }

    /// Deduplication counters (shared by clones of this service), or `None`
    /// when deduplication is off.
    pub fn dedup_stats(&self) -> Option<DedupStats> {
        self.dedup.as_ref().map(|(d, _)| d.stats())
    }
}

/// What a breaker-wrapped call produced, before mapping back.
enum Outcome<Resp, E> {
    BadResponse(Resp),
    Err(E),
}

async fn call_through_breaker<S, Req>(
    mut inner: S,
    req: Req,
    breaker: Option<CircuitBreaker>,
    failure_if: Option<FailureFn<S::Response>>,
) -> Result<S::Response, ResilienceError<S::Error>>
where
    S: Service<Req>,
{
    let Some(breaker) = breaker else {
        return inner
            .call(req)
            .await
            .map_err(|e| ResilienceError::Upstream(Arc::new(e)));
    };
    let result = breaker
        .call(|| async move {
            match inner.call(req).await {
                Ok(resp) if failure_if.as_ref().is_some_and(|f| f(&resp)) => {
                    Err(Outcome::BadResponse(resp))
                }
                Ok(resp) => Ok(resp),
                Err(e) => Err(Outcome::Err(e)),
            }
        })
        .await;
    match result {
        Ok(resp) => Ok(resp),
        Err(CircuitBreakerError::Failed(Outcome::BadResponse(resp))) => Ok(resp),
        Err(CircuitBreakerError::Failed(Outcome::Err(e))) => {
            Err(ResilienceError::Upstream(Arc::new(e)))
        }
        Err(CircuitBreakerError::Open) => Err(ResilienceError::CircuitOpen {
            retry_after: breaker.retry_after_sync(),
        }),
    }
}

/// Circuit breaker only: the response need not be `Clone`.
impl<S, Req> Service<Req> for ResilienceService<S, Req, NoDedup>
where
    S: Service<Req> + Clone + Send + 'static,
    S::Future: Send,
    S::Response: Send + 'static,
    S::Error: Send + Sync + 'static,
    Req: Send + 'static,
{
    type Response = S::Response;
    type Error = ResilienceError<S::Error>;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner
            .poll_ready(cx)
            .map_err(|e| ResilienceError::Upstream(Arc::new(e)))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let clone = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(call_through_breaker(
            inner,
            req,
            self.breaker.clone(),
            self.failure_if.clone(),
        ))
    }
}

/// Deduplication and circuit breaker: the response must be `Clone`.
impl<S, Req, K> Service<Req> for ResilienceService<S, Req, K>
where
    S: Service<Req> + Clone + Send + 'static,
    S::Future: Send,
    S::Response: Clone + Send + Sync + 'static,
    S::Error: Send + Sync + 'static,
    Req: Send + 'static,
    K: std::hash::Hash + Eq + Clone + Send + Sync + 'static,
{
    type Response = S::Response;
    type Error = ResilienceError<S::Error>;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner
            .poll_ready(cx)
            .map_err(|e| ResilienceError::Upstream(Arc::new(e)))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        // Take the service that was driven to readiness and leave a clone.
        let clone = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, clone);
        let breaker = self.breaker.clone();
        let failure_if = self.failure_if.clone();
        let keyed = self
            .dedup
            .as_ref()
            .and_then(|(dedup, key_fn)| key_fn(&req).map(|k| (dedup.clone(), k)));

        Box::pin(async move {
            match keyed {
                None => call_through_breaker(inner, req, breaker, failure_if).await,
                Some((dedup, key)) => {
                    dedup
                        .run(key, || {
                            call_through_breaker(inner, req, breaker, failure_if)
                        })
                        .await
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[derive(Debug)]
    struct Boom;
    impl fmt::Display for Boom {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("boom")
        }
    }
    impl std::error::Error for Boom {}

    fn counting_service(
        calls: Arc<AtomicUsize>,
        fail: bool,
    ) -> impl Service<String, Response = String, Error = Boom, Future = impl Send> + Clone + Send
    {
        tower::service_fn(move |prompt: String| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(40)).await;
                if fail {
                    Err(Boom)
                } else {
                    Ok(format!("re: {prompt}"))
                }
            }
        })
    }

    #[tokio::test]
    async fn concurrent_identical_requests_call_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let svc = ResilienceLayer::new()
            .dedup_by(|p: &String| Some(p.clone()))
            .layer(counting_service(calls.clone(), false));
        let mut joins = Vec::new();
        for _ in 0..8 {
            joins.push(tokio::spawn(svc.clone().oneshot("same".to_string())));
        }
        joins.push(tokio::spawn(svc.clone().oneshot("other".to_string())));
        let mut replies = Vec::new();
        for j in joins {
            replies.push(j.await.unwrap().unwrap());
        }
        assert_eq!(replies.iter().filter(|r| *r == "re: same").count(), 8);
        assert_eq!(replies.last().map(String::as_str), Some("re: other"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let stats = svc.dedup_stats().unwrap();
        assert_eq!(stats.leaders, 2);
        assert_eq!(stats.coalesced, 7);
    }

    #[tokio::test]
    async fn none_key_skips_dedup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let svc = ResilienceLayer::new()
            .dedup_by(|_: &String| None::<String>)
            .layer(counting_service(calls.clone(), false));
        let (a, b) = tokio::join!(
            svc.clone().oneshot("x".to_string()),
            svc.clone().oneshot("x".to_string())
        );
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn errors_are_shared_with_joined_callers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let svc = ResilienceLayer::new()
            .dedup_by(|p: &String| Some(p.clone()))
            .layer(counting_service(calls.clone(), true));
        let (a, b) = tokio::join!(
            svc.clone().oneshot("x".to_string()),
            svc.clone().oneshot("x".to_string())
        );
        let (a, b) = (a.unwrap_err(), b.unwrap_err());
        assert_eq!(a.to_string(), "boom");
        assert!(b.upstream().is_some());
        assert!(std::error::Error::source(&b).is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn breaker_opens_and_fails_fast() {
        let calls = Arc::new(AtomicUsize::new(0));
        let breaker = CircuitBreaker::new(2, 0.8, Duration::from_secs(60));
        let svc = ResilienceLayer::new()
            .with_circuit_breaker(breaker.clone())
            .layer(counting_service(calls.clone(), true));
        for _ in 0..2 {
            let e = svc.clone().oneshot("x".to_string()).await.unwrap_err();
            assert!(!e.is_circuit_open());
        }
        let e = svc.clone().oneshot("x".to_string()).await.unwrap_err();
        assert!(e.is_circuit_open(), "got {e}");
        assert!(e.to_string().contains("circuit breaker open"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "open breaker must not call"
        );
        assert!(breaker.is_open_sync());
    }

    #[tokio::test]
    async fn failure_if_counts_bad_responses_but_returns_them() {
        let breaker = CircuitBreaker::new(1, 0.8, Duration::from_secs(60));
        let svc = ResilienceLayer::new()
            .with_circuit_breaker(breaker.clone())
            .failure_if(|r: &u16| *r >= 500)
            .layer(tower::service_fn(|_: ()| async { Ok::<u16, Boom>(503) }));
        assert_eq!(svc.clone().oneshot(()).await.unwrap(), 503);
        assert!(svc.clone().oneshot(()).await.unwrap_err().is_circuit_open());
    }

    #[tokio::test]
    async fn without_breaker_never_opens() {
        let calls = Arc::new(AtomicUsize::new(0));
        let svc = ResilienceLayer::new()
            .without_circuit_breaker()
            .layer(counting_service(calls.clone(), true));
        assert!(svc.circuit_breaker().is_none());
        assert!(svc.dedup_stats().is_none());
        assert!(svc.get_ref().clone().ready().await.is_ok());
        for _ in 0..8 {
            assert!(!svc
                .clone()
                .oneshot("x".to_string())
                .await
                .unwrap_err()
                .is_circuit_open());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 8);
    }
}

#[cfg(test)]
mod non_clone_tests {
    use super::*;
    use tower::ServiceExt;

    /// A response type that is not Clone, like `http::Response`.
    struct NotClone(u16);

    #[tokio::test]
    async fn breaker_only_layer_accepts_non_clone_responses() {
        let breaker = CircuitBreaker::new(1, 0.8, Duration::from_secs(60));
        let layer = ResilienceLayer::new()
            .with_circuit_breaker(breaker)
            .failure_if(|r: &NotClone| r.0 >= 500);
        let svc = layer.layer(tower::service_fn(|_: ()| async {
            Ok::<_, std::io::Error>(NotClone(502))
        }));
        assert_eq!(svc.clone().oneshot(()).await.unwrap().0, 502);
        assert!(svc
            .clone()
            .oneshot(())
            .await
            .err()
            .unwrap()
            .is_circuit_open());
    }
}
