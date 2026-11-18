//! A [`reqwest_middleware::Middleware`] that adds single-flight
//! deduplication and a per-host circuit breaker to raw reqwest calls.
//!
//! For apps that call an LLM API (or any API) with reqwest directly: attach
//! [`ResilienceMiddleware`] to a `ClientWithMiddleware` and identical
//! requests that are in flight at the same time share one upstream call,
//! while a provider that keeps failing is cut off quickly instead of every
//! request waiting for its own timeout.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use http::Extensions;
use reqwest::header::{HeaderMap, HeaderName};
use reqwest::{Method, Request, Response, ResponseBuilderExt, StatusCode, Url, Version};
use reqwest_middleware::{Error, Middleware, Next, Result};
use sha2::{Digest, Sha256};

use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerError};
use crate::single_flight::{Claim, Dedup, DedupStats};

/// Returned (inside [`reqwest_middleware::Error::Middleware`]) when the
/// circuit breaker for a host is open and the request was not sent.
///
/// Find it with `err.downcast_ref::<CircuitOpenError>()` on the
/// `anyhow::Error` inside `Error::Middleware`.
#[derive(Debug, Clone)]
pub struct CircuitOpenError {
    host: String,
    retry_after: Option<Duration>,
}

impl CircuitOpenError {
    /// The host (`host:port`) whose breaker is open.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Roughly how long until the breaker lets a probe request through.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

impl fmt::Display for CircuitOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "circuit breaker open for {}", self.host)?;
        if let Some(d) = self.retry_after {
            write!(f, ", retry in {}s", d.as_secs().max(1))?;
        }
        Ok(())
    }
}

impl std::error::Error for CircuitOpenError {}

/// Returned (inside [`reqwest_middleware::Error::Middleware`]) to a request
/// that joined an identical in-flight request which then failed. The request
/// that actually went out gets the original error.
#[derive(Debug, Clone)]
pub struct JoinedRequestFailed {
    message: String,
}

impl JoinedRequestFailed {
    /// The failed request's error, as text.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for JoinedRequestFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the identical in-flight request this call joined failed: {}",
            self.message
        )
    }
}

impl std::error::Error for JoinedRequestFailed {}

/// Put this in a request's extensions to send it even if an identical
/// request is in flight:
/// `client.post(url).with_extension(SkipDedup).send()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SkipDedup;

/// A fully read response that can be handed to several callers.
#[derive(Clone)]
struct Buffered {
    status: StatusCode,
    version: Version,
    headers: HeaderMap,
    url: Url,
    body: Bytes,
}

impl Buffered {
    async fn read(resp: Response) -> std::result::Result<Self, reqwest::Error> {
        let status = resp.status();
        let version = resp.version();
        let headers = resp.headers().clone();
        let url = resp.url().clone();
        let body = resp.bytes().await?;
        Ok(Self {
            status,
            version,
            headers,
            url,
            body,
        })
    }

    fn to_response(&self) -> Result<Response> {
        let mut builder = http::Response::builder()
            .status(self.status)
            .version(self.version)
            .url(self.url.clone());
        if let Some(h) = builder.headers_mut() {
            h.extend(self.headers.clone());
        }
        let resp = builder.body(self.body.clone()).map_err(Error::middleware)?;
        Ok(Response::from(resp))
    }
}

#[derive(Clone)]
enum SharedFailure {
    Open(CircuitOpenError),
    Other(String),
}

type Shared = std::result::Result<Buffered, SharedFailure>;

/// What a breaker-wrapped call produced, before mapping back.
enum Outcome {
    BadStatus(Response),
    Err(Error),
}

enum Breakers {
    None,
    Shared(CircuitBreaker),
    PerHost {
        failure_threshold: usize,
        success_threshold: f64,
        timeout: Duration,
        map: DashMap<String, CircuitBreaker>,
    },
}

/// Headers left out of the dedup key by default: tracing and request-id
/// headers that differ on every request without changing the response.
const DEFAULT_IGNORED_HEADERS: &[&str] = &[
    "traceparent",
    "tracestate",
    "baggage",
    "sentry-trace",
    "x-request-id",
    "x-correlation-id",
    "x-amzn-trace-id",
    "x-cloud-trace-context",
    "x-b3-traceid",
    "x-b3-spanid",
    "x-b3-parentspanid",
    "x-b3-sampled",
    "b3",
    "date",
];

/// Deduplication and a circuit breaker for a
/// [`reqwest_middleware::ClientWithMiddleware`].
///
/// **Deduplication.** A `GET`, `HEAD` or `POST` whose body is in memory is
/// keyed by its method, URL, body and headers (minus tracing and request-id
/// headers, see [`ignore_header`](Self::ignore_header)). While one request
/// with a key is in flight, identical requests wait for it and get a copy of
/// its response. The shared response is read into memory first, so
/// `Response::remote_addr()` is `None` on it; status, version, headers, URL
/// and body are kept. Streaming requests (a JSON body with `"stream": true`,
/// or `Accept: text/event-stream`) and requests carrying [`SkipDedup`] are
/// never deduplicated, so streams keep streaming. Credentials are part of the
/// key, so two API keys never share a response.
///
/// **Circuit breaker.** One breaker per host by default: 5 consecutive
/// failures open it, it lets a probe through after 30 s, and while open,
/// requests to that host fail at once with [`CircuitOpenError`]. Transport
/// errors and 5xx responses count as failures (change with
/// [`failure_if_status`](Self::failure_if_status)); 5xx responses are still
/// returned to the caller as responses.
///
/// Attach it before a retry middleware so that a deduplicated group shares
/// the final result after retries: `ClientBuilder::new(client)
/// .with(ResilienceMiddleware::new()).with(retry)`. Use one middleware per
/// client: clients with different default headers (such as different API
/// keys set with `default_headers`) must not share one, since default
/// headers are added after the middleware runs.
///
/// ```no_run
/// use reqwest_middleware::ClientBuilder;
/// use llm_resilience::ResilienceMiddleware;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let client = ClientBuilder::new(reqwest::Client::new())
///     .with(ResilienceMiddleware::new())
///     .build();
///
/// let reply = client
///     .post("https://api.openai.com/v1/chat/completions")
///     .bearer_auth(std::env::var("OPENAI_API_KEY")?)
///     .json(&serde_json::json!({
///         "model": "gpt-4o-mini",
///         "messages": [{"role": "user", "content": "Say hi"}]
///     }))
///     .send()
///     .await?;
/// println!("{}", reply.text().await?);
/// # Ok(())
/// # }
/// ```
///
/// Requires the `reqwest-middleware` feature (reqwest-middleware 0.4,
/// reqwest 0.12).
pub struct ResilienceMiddleware {
    dedup: Option<Dedup<[u8; 32], Shared>>,
    breakers: Breakers,
    is_failure: Arc<dyn Fn(StatusCode) -> bool + Send + Sync>,
    ignored_headers: Vec<HeaderName>,
}

impl fmt::Debug for ResilienceMiddleware {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let breakers = match &self.breakers {
            Breakers::None => "none",
            Breakers::Shared(_) => "shared",
            Breakers::PerHost { .. } => "per-host",
        };
        f.debug_struct("ResilienceMiddleware")
            .field("dedup", &self.dedup)
            .field("circuit_breakers", &breakers)
            .finish()
    }
}

impl Default for ResilienceMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

impl ResilienceMiddleware {
    /// Deduplication on, and a circuit breaker per host (5 consecutive
    /// failures open it, a probe goes through after 30 s).
    pub fn new() -> Self {
        Self {
            dedup: Some(Dedup::new()),
            breakers: Breakers::PerHost {
                failure_threshold: 5,
                success_threshold: 0.8,
                timeout: Duration::from_secs(30),
                map: DashMap::new(),
            },
            is_failure: Arc::new(|s: StatusCode| s.is_server_error()),
            ignored_headers: DEFAULT_IGNORED_HEADERS
                .iter()
                .map(|h| HeaderName::from_static(h))
                .collect(),
        }
    }

    /// Turn deduplication off (circuit breaker only).
    #[must_use]
    pub fn without_dedup(mut self) -> Self {
        self.dedup = None;
        self
    }

    /// Turn the circuit breaker off (deduplication only).
    #[must_use]
    pub fn without_circuit_breaker(mut self) -> Self {
        self.breakers = Breakers::None;
        self
    }

    /// One breaker per host (`host:port`), each created with these settings
    /// on first use. See [`CircuitBreaker::new`] for what they mean.
    #[must_use]
    pub fn with_circuit_breaker_per_host(
        mut self,
        failure_threshold: usize,
        success_threshold: f64,
        timeout: Duration,
    ) -> Self {
        self.breakers = Breakers::PerHost {
            failure_threshold,
            success_threshold,
            timeout,
            map: DashMap::new(),
        };
        self
    }

    /// One breaker for every host. Clones of a [`CircuitBreaker`] share
    /// state, so keep a clone to watch or reset it.
    #[must_use]
    pub fn with_circuit_breaker(mut self, breaker: CircuitBreaker) -> Self {
        self.breakers = Breakers::Shared(breaker);
        self
    }

    /// Decide which response statuses count as failures for the breaker
    /// (default: 5xx). Such responses are still returned to the caller.
    #[must_use]
    pub fn failure_if_status<F>(mut self, is_failure: F) -> Self
    where
        F: Fn(StatusCode) -> bool + Send + Sync + 'static,
    {
        self.is_failure = Arc::new(is_failure);
        self
    }

    /// Leave `header` out of the dedup key, for a header that differs on
    /// every request without changing the response (your own request-id
    /// header, say). Tracing and request-id headers are left out already.
    #[must_use]
    pub fn ignore_header(mut self, header: HeaderName) -> Self {
        self.ignored_headers.push(header);
        self
    }

    /// Deduplication counters, or `None` when deduplication is off.
    pub fn dedup_stats(&self) -> Option<DedupStats> {
        self.dedup.as_ref().map(Dedup::stats)
    }

    /// The breaker that guards `host` (as `host:port`, or `host` for the
    /// scheme's default port), if one exists yet.
    pub fn circuit_breaker_for(&self, host: &str) -> Option<CircuitBreaker> {
        match &self.breakers {
            Breakers::None => None,
            Breakers::Shared(b) => Some(b.clone()),
            Breakers::PerHost { map, .. } => {
                if let Some(b) = map.get(host) {
                    return Some(b.clone());
                }
                // A bare host: match it on any port.
                let prefix = format!("{host}:");
                map.iter()
                    .find(|e| e.key().starts_with(&prefix))
                    .map(|e| e.value().clone())
            }
        }
    }

    fn breaker(&self, url: &Url) -> Option<(String, CircuitBreaker)> {
        let host = host_key(url);
        match &self.breakers {
            Breakers::None => None,
            Breakers::Shared(b) => Some((host, b.clone())),
            Breakers::PerHost {
                failure_threshold,
                success_threshold,
                timeout,
                map,
            } => {
                let b = map
                    .entry(host.clone())
                    .or_insert_with(|| {
                        CircuitBreaker::new(*failure_threshold, *success_threshold, *timeout)
                    })
                    .clone();
                Some((host, b))
            }
        }
    }

    /// The dedup key, or `None` if this request must go out on its own.
    fn dedup_key(&self, req: &Request, extensions: &Extensions) -> Option<[u8; 32]> {
        if extensions.get::<SkipDedup>().is_some() {
            return None;
        }
        if !matches!(*req.method(), Method::GET | Method::HEAD | Method::POST) {
            return None;
        }
        let body: &[u8] = match req.body() {
            None => &[],
            Some(b) => b.as_bytes()?, // a streaming body cannot be compared
        };
        if wants_stream(req.headers(), body) {
            return None;
        }

        let mut hasher = Sha256::new();
        hasher.update(req.method().as_str().as_bytes());
        hasher.update([0]);
        hasher.update(req.url().as_str().as_bytes());
        hasher.update([0]);
        let mut names: Vec<&HeaderName> = req
            .headers()
            .keys()
            .filter(|n| !self.ignored_headers.contains(n))
            .collect();
        names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        names.dedup();
        for name in names {
            for value in req.headers().get_all(name) {
                hasher.update(name.as_str().as_bytes());
                hasher.update(b":");
                hasher.update(value.as_bytes());
                hasher.update([0]);
            }
        }
        hasher.update([1]);
        hasher.update(body);
        Some(hasher.finalize().into())
    }

    async fn send(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> Result<Response> {
        let Some((host, breaker)) = self.breaker(req.url()) else {
            return next.run(req, extensions).await;
        };
        let is_failure = Arc::clone(&self.is_failure);
        let result = breaker
            .call(|| async move {
                match next.run(req, extensions).await {
                    Ok(resp) if is_failure(resp.status()) => Err(Outcome::BadStatus(resp)),
                    Ok(resp) => Ok(resp),
                    Err(e) => Err(Outcome::Err(e)),
                }
            })
            .await;
        match result {
            Ok(resp) => Ok(resp),
            Err(CircuitBreakerError::Failed(Outcome::BadStatus(resp))) => Ok(resp),
            Err(CircuitBreakerError::Failed(Outcome::Err(e))) => Err(e),
            Err(CircuitBreakerError::Open) => Err(Error::middleware(CircuitOpenError {
                host,
                retry_after: breaker.retry_after_sync(),
            })),
        }
    }
}

#[async_trait::async_trait]
impl Middleware for ResilienceMiddleware {
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> Result<Response> {
        let (Some(dedup), Some(key)) = (&self.dedup, self.dedup_key(&req, extensions)) else {
            return self.send(req, extensions, next).await;
        };
        let leader = match dedup.claim(key).await {
            Claim::Joined(Ok(buffered)) => return buffered.to_response(),
            Claim::Joined(Err(SharedFailure::Open(e))) => return Err(Error::middleware(e)),
            Claim::Joined(Err(SharedFailure::Other(message))) => {
                return Err(Error::middleware(JoinedRequestFailed { message }))
            }
            Claim::Leader(leader) => leader,
        };
        match self.send(req, extensions, next).await {
            Ok(resp) => match Buffered::read(resp).await {
                Ok(buffered) => {
                    let out = buffered.to_response();
                    leader.complete(Ok(buffered));
                    out
                }
                Err(e) => {
                    leader.complete(Err(SharedFailure::Other(e.to_string())));
                    Err(Error::Reqwest(e))
                }
            },
            Err(e) => {
                let shared = match &e {
                    Error::Middleware(inner) => match inner.downcast_ref::<CircuitOpenError>() {
                        Some(open) => SharedFailure::Open(open.clone()),
                        None => SharedFailure::Other(e.to_string()),
                    },
                    Error::Reqwest(_) => SharedFailure::Other(e.to_string()),
                };
                leader.complete(Err(shared));
                Err(e)
            }
        }
    }
}

/// `host:port` with the port always present, so `a.com` and `a.com:443`
/// share a breaker.
fn host_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port_or_known_default() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// Whether the request asks for a streamed reply.
fn wants_stream(headers: &HeaderMap, body: &[u8]) -> bool {
    let sse = headers
        .get_all(reqwest::header::ACCEPT)
        .iter()
        .any(|v| v.to_str().is_ok_and(|s| s.contains("text/event-stream")));
    if sse {
        return true;
    }
    // Cheap check before parsing: only JSON bodies that mention "stream".
    if !body.windows(8).any(|w| w == b"\"stream\"") {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_detection() {
        let h = HeaderMap::new();
        assert!(wants_stream(&h, br#"{"model":"x","stream":true}"#));
        assert!(!wants_stream(&h, br#"{"model":"x","stream":false}"#));
        assert!(!wants_stream(&h, br#"{"model":"x"}"#));
        assert!(!wants_stream(&h, b"not json \"stream\""));
        let mut sse = HeaderMap::new();
        sse.insert(
            reqwest::header::ACCEPT,
            "text/event-stream".parse().unwrap(),
        );
        assert!(wants_stream(&sse, b""));
    }

    #[test]
    fn host_keys_include_default_ports() {
        let a = Url::parse("https://api.openai.com/v1/chat").unwrap();
        let b = Url::parse("https://api.openai.com:443/v1/models").unwrap();
        assert_eq!(host_key(&a), host_key(&b));
        assert_eq!(host_key(&a), "api.openai.com:443");
    }

    fn key_of(mw: &ResilienceMiddleware, req: &Request) -> Option<[u8; 32]> {
        mw.dedup_key(req, &Extensions::new())
    }

    #[test]
    fn keys_cover_credentials_and_skip_trace_headers() {
        let mw = ResilienceMiddleware::new();
        let c = reqwest::Client::new();
        let base = || {
            c.post("http://h/v1")
                .header("content-type", "application/json")
                .body(r#"{"a":1}"#)
        };
        let a = base().bearer_auth("k1").build().unwrap();
        let b = base().bearer_auth("k2").build().unwrap();
        let a_traced = base()
            .bearer_auth("k1")
            .header("traceparent", "00-abc-def-01")
            .header("x-request-id", "r-1")
            .build()
            .unwrap();
        assert_ne!(key_of(&mw, &a), key_of(&mw, &b));
        assert_eq!(key_of(&mw, &a), key_of(&mw, &a_traced));
        assert!(key_of(&mw, &a).is_some());

        let custom = mw.ignore_header(HeaderName::from_static("x-mine"));
        let x1 = base().header("x-mine", "1").build().unwrap();
        let x2 = base().header("x-mine", "2").build().unwrap();
        assert_eq!(key_of(&custom, &x1), key_of(&custom, &x2));
    }

    #[test]
    fn only_safe_methods_and_post_are_keyed() {
        let mw = ResilienceMiddleware::new();
        let c = reqwest::Client::new();
        assert!(key_of(&mw, &c.get("http://h/").build().unwrap()).is_some());
        assert!(key_of(&mw, &c.put("http://h/").build().unwrap()).is_none());
        assert!(key_of(&mw, &c.delete("http://h/").build().unwrap()).is_none());
        let mut ext = Extensions::new();
        ext.insert(SkipDedup);
        assert!(mw
            .dedup_key(&c.get("http://h/").build().unwrap(), &ext)
            .is_none());
    }
}
