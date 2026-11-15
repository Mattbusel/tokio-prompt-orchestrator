//! [`OrchestratedTransport`]: a rig `Transport` with dedup, a circuit
//! breaker and retry.

// rig's `ProviderError` (and its `Encoded` request) are large by design, and
// the `Transport` contract returns them by value, so this module does too.
#![allow(clippy::result_large_err)]

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use rig_core::driver::{Exchange, Opened, Opening, Transport};
use rig_core::http_client::framing::WireFrame;
use rig_core::http_client::{HeaderMap, Request};
use rig_core::wire::{Body, Encoded, Mode, Wire};
use rig_core::{ErrorReport, ProviderError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::enhanced::circuit_breaker::CircuitBreakerError;
use crate::enhanced::dedup::is_cancelled_result;
use crate::enhanced::{retry_if, CircuitBreaker, DeduplicationResult, Deduplicator, RetryPolicy};

/// Request headers that identify the caller. They are part of the dedup key,
/// so callers with different credentials never share a reply.
const CREDENTIAL_HEADERS: [&str; 4] = ["authorization", "x-api-key", "api-key", "x-goog-api-key"];

/// A rig [`Transport`] that puts deduplication, a circuit breaker and
/// retries in front of any other transport.
///
/// Rig 0.45 and later split a model into a wire (what to send, how to read the reply)
/// and a transport (how the bytes travel). [`OrchestratedTransport`] wraps
/// the transport, so every rig feature keeps working: messages, tools,
/// temperature, `max_tokens`, structured output and streaming all reach the
/// provider exactly as rig encoded them. The orchestrator only decides
/// whether, and how many times, the request is sent.
///
/// * **Deduplication.** Identical unary requests in flight at the same time
///   share one upstream call. The key is a SHA-256 of the method, URI,
///   credential headers and serialized request body, so two different
///   prompts, models or API keys never share an answer. Streaming requests
///   are never deduplicated. The first caller's outcome is shared either
///   way: if it fails (a 400, a rejected key, retries used up, the breaker
///   open), the callers waiting on it get the same error, as
///   [`ProviderError::Relayed`] with the original's status, code, retry
///   verdict and provider reply, instead of all sending the request again
///   at once. Only when the first caller is dropped before it finishes do
///   the waiters send their own.
/// * **Circuit breaker.** Every failed *attempt* with a retryable error
///   (connection errors, 5xx, 429, truncated replies) counts against a
///   [`CircuitBreaker`], so retries count too: with the defaults, two user
///   requests that each fail all 3 attempts open it. While it is open, sends
///   fail at once with [`ProviderError::Provider`] instead of waiting on a
///   provider that is down. Non-retryable failures (a 400, a 401) are not
///   counted: the provider answered.
/// * **Retry.** Retryable failures are retried under a [`RetryPolicy`].
///   A streamed reply is retried only until its first frame arrives; after
///   that the caller already holds part of the answer.
///
/// # What rig's observation sees
///
/// The first caller's reply is the inner transport's own, passed through
/// with its HTTP status, headers, request id, route and observation slot.
/// Three things are lost, because rig gives a wrapping transport no way
/// to read or set them:
///
/// * A deduplicated caller's reply is rebuilt from the first caller's
///   frames and JSON document. It has the request path as its route, but no
///   HTTP status, headers or provider request id, and no observation slot,
///   so `call_observed` records no attempt for it (no request was sent).
///   Without a slot rig never consults the wire's `analysis_only`
///   predicate, which only exempts frames from observation's frame count;
///   decoding is the same, so every wire decodes the reply as it would the
///   original.
/// * The opening of a reply does not carry the inner transport's slot. Rig
///   uses that slot only when a reply fails or stalls before it opens; such
///   a failure is still returned, but not recorded on the observation.
/// * A send refused by the open breaker never reaches the inner transport,
///   so it is not observed either.
///
/// ```no_run
/// use rig_core::completion::CompletionRequest;
/// use rig_core::driver::Model;
/// use rig_core::providers::openai::OpenAIConfig;
/// use tokio_prompt_orchestrator::integrations::OrchestratedTransport;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// // Build the model exactly as you do today...
/// let model = OpenAIConfig::from_env()?.client().chat("gpt-4o-mini");
/// // ...then swap its transport for the orchestrated one.
/// let model = Model::new(model.wire, OrchestratedTransport::new(model.transport));
///
/// let reply = model.call(CompletionRequest::new("Capital of France?")).await?;
/// println!("{}", reply.text());
/// # Ok(())
/// # }
/// ```
///
/// The defaults are listed on [`OrchestratedTransport::new`]. Use
/// [`OrchestratedTransport::builder`] to change them, or to share one
/// breaker or deduplicator between several models:
///
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::{CircuitBreaker, Deduplicator, RetryPolicy};
/// use tokio_prompt_orchestrator::integrations::OrchestratedTransport;
///
/// # fn wrap<T: Clone>(http: T) -> OrchestratedTransport<T> {
/// let breaker = Arc::new(CircuitBreaker::new(3, 0.8, Duration::from_secs(10)));
/// let transport = OrchestratedTransport::builder(http)
///     .breaker(breaker)
///     .retry(RetryPolicy::exponential(4, Duration::from_millis(250)))
///     // Also reuse answers to identical requests for 30 seconds.
///     .deduplicator(Arc::new(Deduplicator::new(Duration::from_secs(30))))
///     .build();
/// # transport
/// # }
/// ```
///
/// Requires the `rig-transport` feature and rig-core 0.45 or 0.46 (run
/// `cargo update -p rig-core` if your lockfile holds an older one). This is
/// not the same thing as
/// [`RigWorker`](super::RigWorker): the worker turns a rig model into a
/// prompt-in, text-out [`ModelWorker`](crate::ModelWorker) for the pipeline,
/// which drops everything but the prompt. The transport keeps the whole rig
/// request.
///
/// It is `Clone`, and clones share one deduplicator and one breaker, so the
/// same protection covers every model that uses a clone.
pub struct OrchestratedTransport<T> {
    inner: T,
    dedup: Option<Arc<Deduplicator>>,
    breaker: Arc<CircuitBreaker>,
    retry: RetryPolicy,
}

impl<T: Clone> Clone for OrchestratedTransport<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            dedup: self.dedup.clone(),
            breaker: Arc::clone(&self.breaker),
            retry: self.retry.clone(),
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for OrchestratedTransport<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrchestratedTransport")
            .field("inner", &self.inner)
            .field("dedup", &self.dedup.is_some())
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl<T> OrchestratedTransport<T> {
    /// Wrap `inner` with the default protection:
    ///
    /// * deduplication of identical unary requests while they are in flight
    ///   (no reuse of finished answers),
    /// * a circuit breaker that opens after 5 consecutive failed attempts
    ///   (each retry is an attempt, so two requests that fail all their
    ///   attempts are enough) and probes again after 30 seconds,
    /// * up to 3 attempts per request, with exponential backoff from 200 ms.
    pub fn new(inner: T) -> Self {
        Self::builder(inner).build()
    }

    /// Start configuring a transport around `inner`.
    pub fn builder(inner: T) -> OrchestratedTransportBuilder<T> {
        OrchestratedTransportBuilder {
            inner,
            dedup: Some(Arc::new(Deduplicator::new(Duration::ZERO))),
            breaker: Arc::new(CircuitBreaker::new(5, 0.8, Duration::from_secs(30))),
            retry: RetryPolicy::exponential(3, Duration::from_millis(200)),
        }
    }

    /// The wrapped transport.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// The circuit breaker, for reading its state or resetting it.
    pub fn breaker(&self) -> &Arc<CircuitBreaker> {
        &self.breaker
    }

    /// The deduplicator, unless deduplication is off.
    pub fn deduplicator(&self) -> Option<&Arc<Deduplicator>> {
        self.dedup.as_ref()
    }

    /// The retry policy.
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry
    }
}

/// Configures an [`OrchestratedTransport`]. Made by
/// [`OrchestratedTransport::builder`].
#[must_use = "call .build() to get the transport"]
pub struct OrchestratedTransportBuilder<T> {
    inner: T,
    dedup: Option<Arc<Deduplicator>>,
    breaker: Arc<CircuitBreaker>,
    retry: RetryPolicy,
}

impl<T: fmt::Debug> fmt::Debug for OrchestratedTransportBuilder<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrchestratedTransportBuilder")
            .field("inner", &self.inner)
            .field("dedup", &self.dedup.is_some())
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl<T> OrchestratedTransportBuilder<T> {
    /// Use this deduplicator. Its cache duration decides whether finished
    /// answers are reused: `Duration::ZERO` shares only calls in flight.
    pub fn deduplicator(mut self, dedup: Arc<Deduplicator>) -> Self {
        self.dedup = Some(dedup);
        self
    }

    /// Send every request, even identical concurrent ones.
    pub fn without_dedup(mut self) -> Self {
        self.dedup = None;
        self
    }

    /// Use this circuit breaker. Pass the same `Arc` to several transports
    /// to trip them together.
    pub fn breaker(mut self, breaker: Arc<CircuitBreaker>) -> Self {
        self.breaker = breaker;
        self
    }

    /// Use this retry policy. `max_attempts` counts the first try, so `1`
    /// turns retries off.
    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Finish the transport.
    pub fn build(self) -> OrchestratedTransport<T> {
        OrchestratedTransport {
            inner: self.inner,
            dedup: self.dedup,
            breaker: self.breaker,
            retry: self.retry,
        }
    }
}

impl<W, T> Transport<W> for OrchestratedTransport<T>
where
    W: Wire<Payload = Encoded, Frame = WireFrame>,
    T: Transport<W>,
{
    fn send(&self, payload: Encoded, exchange: Exchange) -> Opening<WireFrame> {
        let this = self.clone();
        Opening::new(async move { this.run::<W>(payload, exchange).await })
    }
}

impl<T> OrchestratedTransport<T> {
    async fn run<W>(
        &self,
        payload: Encoded,
        exchange: Exchange,
    ) -> Result<Opened<WireFrame>, ProviderError>
    where
        W: Wire<Payload = Encoded, Frame = WireFrame>,
        T: Transport<W>,
    {
        let dedup = match (&self.dedup, exchange.mode) {
            (Some(dedup), Mode::Unary) => dedup,
            _ => return self.resilient::<W>(payload, exchange).await.into_result(),
        };
        let Some(key) = dedup_key(&payload) else {
            return self.resilient::<W>(payload, exchange).await.into_result();
        };

        let route = payload.request.uri().path().to_owned();
        match dedup.check_and_register(&key).await {
            DeduplicationResult::New(token) => {
                let reply = self.resilient::<W>(payload, exchange).await;
                match (&reply.frames, &reply.error) {
                    (Some(frames), _) => dedup.complete(token, Shared::frames(frames)).await,
                    // Waiters get the same failure instead of all sending
                    // the request again; nothing is cached.
                    (None, Some(error)) => dedup.fail_with(token, Shared::error(error)).await,
                    (None, None) => dedup.fail(token).await,
                }
                reply.into_result()
            }
            DeduplicationResult::Cached(shared) => shared_reply(&shared, route),
            DeduplicationResult::InProgress => match dedup.wait_for_result(&key).await {
                Some(shared) if !is_cancelled_result(&shared) => shared_reply(&shared, route),
                // The first caller was dropped before it finished (or its
                // entry was already gone): send our own.
                _ => self.resilient::<W>(payload, exchange).await.into_result(),
            },
        }
    }

    /// Send `payload` through the breaker, retrying what is retryable.
    async fn resilient<W>(&self, payload: Encoded, exchange: Exchange) -> Reply
    where
        W: Wire<Payload = Encoded, Frame = WireFrame>,
        T: Transport<W>,
    {
        let template = match Template::new(payload) {
            Ok(template) => template,
            // A multipart upload cannot be rebuilt, so it gets one attempt.
            Err(payload) => {
                return self
                    .guarded::<W>(payload, exchange)
                    .await
                    .unwrap_or_else(Failure::into_reply)
            }
        };
        let exchange = &exchange;
        let template = &template;
        retry_if(
            &self.retry,
            move || self.guarded::<W>(template.build(), exchange.clone()),
            |failure: &Failure| failure.retryable,
        )
        .await
        .unwrap_or_else(Failure::into_reply)
    }

    /// One attempt, through the circuit breaker. `Err` is a failure the
    /// breaker counts; a reply that failed for a reason retrying cannot fix
    /// (a 400, a rejected key) is `Ok`, since the provider is up.
    async fn guarded<W>(&self, payload: Encoded, exchange: Exchange) -> Result<Reply, Failure>
    where
        W: Wire<Payload = Encoded, Frame = WireFrame>,
        T: Transport<W>,
    {
        let mode = exchange.mode;
        let outcome = self
            .breaker
            .call(|| async move {
                let opened = match self.inner.send(payload, exchange).await {
                    Ok(opened) => opened,
                    Err(error) => return classify(error, Outcome::Refused),
                };
                match mode {
                    Mode::Unary => unary(opened).await,
                    Mode::Streaming => streaming(opened).await,
                }
            })
            .await;
        match outcome {
            Ok(reply) => Ok(reply),
            Err(CircuitBreakerError::Failed(failure)) => Err(failure),
            Err(CircuitBreakerError::Open) => {
                let wait = self
                    .breaker
                    .retry_after_sync()
                    .map(|d| format!("; it probes again in {} ms", d.as_millis()))
                    .unwrap_or_default();
                let error = ProviderError::Provider(format!(
                    "circuit breaker open after repeated upstream failures{wait}"
                ));
                Err(Failure {
                    retryable: false,
                    error: error.clone(),
                    outcome: Outcome::Refused(error),
                })
            }
        }
    }
}

/// A unary reply, read whole: its frames are buffered so a failure anywhere
/// in it can be retried, and a success can be shared with waiting callers.
async fn unary(opened: Opened<WireFrame>) -> Result<Reply, Failure> {
    let (opened, frames) = take_frames(opened);
    let items: Vec<Result<WireFrame, ProviderError>> = frames.collect().await;
    let error = items.iter().find_map(|item| item.as_ref().err().cloned());
    match error {
        Some(error) => {
            let opened = opened.map_frames(move |_| futures::stream::iter(items));
            classify(error, |_| Outcome::Opened(opened))
        }
        None => {
            let frames: Vec<WireFrame> = items
                .iter()
                .filter_map(|i| i.as_ref().ok().cloned())
                .collect();
            let opened = opened.map_frames(move |_| futures::stream::iter(items));
            Ok(Reply {
                outcome: Outcome::Opened(opened),
                frames: Some(frames),
                error: None,
            })
        }
    }
}

/// A streamed reply: wait for its first frame. A failure before it can be
/// retried; after it the stream belongs to the caller.
async fn streaming(opened: Opened<WireFrame>) -> Result<Reply, Failure> {
    let (opened, mut frames) = take_frames(opened);
    let first = frames.next().await;
    if let Some(Err(error)) = &first {
        let error = error.clone();
        let opened = opened.map_frames(move |_| futures::stream::iter(first).chain(frames));
        return classify(error, |_| Outcome::Opened(opened));
    }
    let opened = opened.map_frames(move |_| futures::stream::iter(first).chain(frames));
    Ok(Reply {
        outcome: Outcome::Opened(opened),
        frames: None,
        error: None,
    })
}

type Frames = rig_core::wasm_compat::WasmBoxedStream<'static, Result<WireFrame, ProviderError>>;

/// Take the frame stream out of a reply, keeping the reply's status,
/// headers, request id and document for when the frames are put back.
fn take_frames(opened: Opened<WireFrame>) -> (Opened<WireFrame>, Frames) {
    let mut taken: Option<Frames> = None;
    let opened = opened.map_frames(|frames| {
        taken = Some(frames);
        futures::stream::empty()
    });
    let frames = taken.unwrap_or_else(|| Box::pin(futures::stream::empty()));
    (opened, frames)
}

/// Sort a failed attempt: retryable errors are breaker failures, the rest
/// are handed back as they are.
fn classify(
    error: ProviderError,
    outcome: impl FnOnce(ProviderError) -> Outcome,
) -> Result<Reply, Failure> {
    let retryable = error.is_retryable();
    let outcome = outcome(error.clone());
    if retryable {
        Err(Failure {
            retryable,
            error,
            outcome,
        })
    } else {
        Ok(Reply {
            outcome,
            frames: None,
            error: Some(error),
        })
    }
}

/// What a send ends with: a reply (which may carry a failure as its last
/// frame) or an opening that failed outright.
enum Outcome {
    Opened(Opened<WireFrame>),
    Refused(ProviderError),
}

struct Reply {
    outcome: Outcome,
    /// The frames of a successful unary reply, to share with waiters.
    frames: Option<Vec<WireFrame>>,
    /// The error a failed reply ends with, to share with waiters.
    error: Option<ProviderError>,
}

impl Reply {
    fn into_result(self) -> Result<Opened<WireFrame>, ProviderError> {
        match self.outcome {
            Outcome::Opened(opened) => Ok(opened),
            Outcome::Refused(error) => Err(error),
        }
    }
}

/// A failed attempt. Holds what to hand back if it is the last one.
struct Failure {
    retryable: bool,
    error: ProviderError,
    outcome: Outcome,
}

impl Failure {
    fn into_reply(self) -> Reply {
        Reply {
            outcome: self.outcome,
            frames: None,
            error: Some(self.error),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)
    }
}

/// An [`Encoded`] request with a byte body, which can be rebuilt for every
/// attempt.
struct Template {
    head: Request<()>,
    body: Vec<u8>,
    rest: Encoded,
}

impl Template {
    /// `Err` gives back a request whose body cannot be copied (multipart).
    fn new(payload: Encoded) -> Result<Self, Encoded> {
        if !matches!(payload.request.body(), Body::Bytes(_)) {
            return Err(payload);
        }
        let Encoded {
            request,
            framing,
            request_id_header,
            relaxed_content_type,
            route,
            project,
            analysis_only,
        } = payload;
        let (parts, body) = request.into_parts();
        let body = match body {
            Body::Bytes(bytes) => bytes,
            Body::Multipart(_) => Vec::new(), // excluded above
        };
        Ok(Self {
            head: Request::from_parts(parts, ()),
            body,
            rest: Encoded {
                request: Request::new(Body::empty()),
                framing,
                request_id_header,
                relaxed_content_type,
                route,
                project,
                analysis_only,
            },
        })
    }

    fn build(&self) -> Encoded {
        Encoded {
            request: self.head.clone().map(|()| Body::Bytes(self.body.clone())),
            framing: self.rest.framing,
            request_id_header: self.rest.request_id_header,
            relaxed_content_type: self.rest.relaxed_content_type,
            route: self.rest.route,
            project: self.rest.project,
            analysis_only: self.rest.analysis_only,
        }
    }
}

/// The dedup key of a request: a SHA-256 of what decides its answer. `None`
/// for a multipart body, which is never deduplicated.
fn dedup_key(payload: &Encoded) -> Option<String> {
    let Body::Bytes(body) = payload.request.body() else {
        return None;
    };
    let mut hash = Sha256::new();
    hash.update(payload.request.method().as_str());
    hash.update([0]);
    hash.update(payload.request.uri().to_string());
    hash.update([0]);
    let headers: &HeaderMap = payload.request.headers();
    for name in CREDENTIAL_HEADERS {
        for value in headers.get_all(name) {
            hash.update(name);
            hash.update(b":");
            hash.update(value.as_bytes());
            hash.update([0]);
        }
    }
    hash.update(body);
    Some(format!("rig-transport:{}", hex::encode(hash.finalize())))
}

/// What the deduplicator hands the callers waiting on a request: the first
/// caller's reply frames, or the error its request ended with.
#[derive(Serialize, Deserialize)]
enum Shared {
    #[serde(rename = "ok")]
    Frames(Vec<SharedFrame>),
    #[serde(rename = "err")]
    Error(Box<ErrorReport>),
}

impl Shared {
    fn frames(frames: &[WireFrame]) -> String {
        let frames = frames
            .iter()
            .map(|frame| match frame {
                WireFrame::Text(text) => SharedFrame::Text(text.clone()),
                WireFrame::Bytes(bytes) => SharedFrame::Bytes(hex::encode(bytes)),
            })
            .collect();
        serde_json::to_string(&Shared::Frames(frames)).unwrap_or_default()
    }

    fn error(error: &ProviderError) -> String {
        serde_json::to_string(&Shared::Error(Box::new(error.report()))).unwrap_or_default()
    }
}

/// A frame as the deduplicator stores it.
#[derive(Serialize, Deserialize)]
enum SharedFrame {
    #[serde(rename = "t")]
    Text(String),
    #[serde(rename = "b")]
    Bytes(String),
}

impl SharedFrame {
    fn decode(self) -> Result<WireFrame, ProviderError> {
        match self {
            SharedFrame::Text(text) => Ok(WireFrame::Text(text)),
            SharedFrame::Bytes(hex) => hex::decode(hex)
                .map(WireFrame::Bytes)
                .map_err(|e| ProviderError::Provider(format!("shared reply unreadable: {e}"))),
        }
    }
}

/// The reply a waiting caller gets: the first caller's frames, under this
/// caller's request path, or its error. A whole-body reply also carries its
/// JSON document, as the HTTP transport would set it. The first caller's
/// HTTP status, headers and request id are not available to a wrapping
/// transport, so they are left unset rather than made up.
fn shared_reply(shared: &str, route: String) -> Result<Opened<WireFrame>, ProviderError> {
    let shared: Shared = serde_json::from_str(shared)
        .map_err(|e| ProviderError::Provider(format!("shared reply unreadable: {e}")))?;
    let frames = match shared {
        Shared::Frames(frames) => frames
            .into_iter()
            .map(SharedFrame::decode)
            .collect::<Result<Vec<_>, _>>()?,
        Shared::Error(report) => return Err(ProviderError::Relayed(report)),
    };
    let document = match frames.as_slice() {
        [only] => serde_json::from_slice::<serde_json::Value>(only.as_bytes()).ok(),
        _ => None,
    };
    let opened = Opened::new(futures::stream::iter(frames.into_iter().map(Ok))).with_route(route);
    Ok(match document {
        Some(document) => opened.with_document(document),
        None => opened,
    })
}
