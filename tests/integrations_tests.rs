//! End-to-end tests for the `integrations` workers against a local mock of
//! the OpenAI Chat Completions API (wiremock). No network, no API key.
//!
//! Each client crate is exercised through its real HTTP stack: a normal
//! reply, a streamed reply, and the error mapping the pipeline depends on
//! (401 must not be retried, 429 must back off).

#![cfg(any(feature = "async-openai", feature = "genai", feature = "rig"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use serde_json::json;
use tokio_prompt_orchestrator::{ModelWorker, OrchestratorError};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHAT_PATH: &str = "/v1/chat/completions";

fn completion(content: &str) -> serde_json::Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1_790_000_000,
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    })
}

/// A streamed reply split into `parts`, in OpenAI's SSE wire format.
fn sse(parts: &[&str]) -> String {
    let mut body = String::new();
    for (i, part) in parts.iter().enumerate() {
        let mut delta = json!({"content": part});
        if i == 0 {
            delta["role"] = json!("assistant");
        }
        let chunk = json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 1_790_000_000,
            "model": "gpt-4o-mini",
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
        });
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    let done = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1_790_000_000,
        "model": "gpt-4o-mini",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    body.push_str(&format!("data: {done}\n\ndata: [DONE]\n\n"));
    body
}

fn api_error(status: u16, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "error": {"message": message, "type": "error", "param": null, "code": null}
    }))
}

async fn reply_server(prompt: &str, content: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .and(body_partial_json(
            json!({"messages": [{"role": "user", "content": prompt}]}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion(content)))
        .expect(1)
        .mount(&server)
        .await;
    server
}

async fn stream_server(parts: &[&str]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            // set_body_raw keeps the SSE content type; set_body_string would
            // reset it to text/plain, which strict clients (rig) reject.
            ResponseTemplate::new(200).set_body_raw(sse(parts), "text/event-stream"),
        )
        .mount(&server)
        .await;
    server
}

async fn error_server(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

async fn collect(worker: &dyn ModelWorker, prompt: &str) -> String {
    let mut stream = worker.infer_stream(prompt).await.expect("stream opens");
    let mut out = String::new();
    while let Some(token) = stream.next().await {
        out.push_str(&token.expect("token"));
    }
    out
}

/// The shared assertions every integration must pass.
async fn check_worker(make: impl Fn(&MockServer) -> Box<dyn ModelWorker>) {
    // A normal reply comes back as one token.
    let server = reply_server("Capital of France?", "Paris.").await;
    let tokens = make(&server)
        .infer("Capital of France?")
        .await
        .expect("reply");
    assert_eq!(tokens, vec!["Paris.".to_string()]);

    // A streamed reply arrives in pieces and reassembles exactly.
    let server = stream_server(&["Bounded ", "queues ", "stay calm."]).await;
    assert_eq!(
        collect(make(&server).as_ref(), "haiku").await,
        "Bounded queues stay calm."
    );

    // A rejected key is an auth failure, which the pipeline never retries.
    let server = error_server(api_error(401, "Incorrect API key provided")).await;
    let err = make(&server).infer("x").await.unwrap_err();
    assert!(
        matches!(err, OrchestratorError::AuthFailed(_)),
        "got {err:?}"
    );
    assert!(!err.is_retryable());

    // A 429 is a rate limit, which backs off and retries.
    let server = error_server(api_error(429, "Rate limit reached")).await;
    let err = make(&server).infer("x").await.unwrap_err();
    assert!(
        matches!(err, OrchestratorError::RateLimited { .. }),
        "got {err:?}"
    );
    assert!(err.is_retryable());

    // A 500 is a retryable inference failure carrying the provider's message.
    let server = error_server(api_error(500, "The server had an error")).await;
    let err = make(&server).infer("x").await.unwrap_err();
    assert!(
        matches!(err, OrchestratorError::Inference(_)),
        "got {err:?}"
    );
    assert!(err.is_retryable());
}

#[cfg(feature = "async-openai")]
mod async_openai_worker {
    use super::*;
    use async_openai::{config::OpenAIConfig, Client};
    use tokio_prompt_orchestrator::integrations::AsyncOpenAiWorker;

    fn worker(server: &MockServer) -> Box<dyn ModelWorker> {
        let config = OpenAIConfig::new()
            .with_api_base(format!("{}/v1", server.uri()))
            .with_api_key("test-key");
        Box::new(AsyncOpenAiWorker::new(
            Client::with_config(config),
            "gpt-4o-mini",
        ))
    }

    #[tokio::test]
    async fn replies_streams_and_maps_errors() {
        check_worker(worker).await;
    }

    #[tokio::test]
    async fn sends_the_configured_model_and_limits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(CHAT_PATH))
            .and(body_partial_json(json!({
                "model": "gpt-4o-mini",
                "max_completion_tokens": 64,
                "temperature": 0.25
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")))
            .expect(1)
            .mount(&server)
            .await;
        let config = OpenAIConfig::new()
            .with_api_base(format!("{}/v1", server.uri()))
            .with_api_key("k");
        let worker = AsyncOpenAiWorker::new(Client::with_config(config), "gpt-4o-mini")
            .with_max_tokens(64)
            .with_temperature(0.25);
        assert_eq!(worker.infer("hi").await.unwrap(), vec!["ok".to_string()]);
    }
}

#[cfg(feature = "genai")]
mod genai_worker {
    use super::*;
    use genai::resolver::{AuthData, Endpoint};
    use genai::{Client, ServiceTarget};
    use tokio_prompt_orchestrator::integrations::GenaiWorker;

    fn worker(server: &MockServer) -> Box<dyn ModelWorker> {
        let base = format!("{}/v1/", server.uri());
        let client = Client::builder()
            .with_service_target_resolver_fn(move |mut target: ServiceTarget| {
                target.endpoint = Endpoint::from_owned(base.clone());
                target.auth = AuthData::from_single("test-key");
                Ok(target)
            })
            .build();
        // "gpt-" model names route through genai's OpenAI adapter.
        Box::new(GenaiWorker::new(client, "gpt-4o-mini"))
    }

    #[tokio::test]
    async fn replies_streams_and_maps_errors() {
        check_worker(worker).await;
    }
}

#[cfg(feature = "rig")]
mod rig_worker {
    use super::*;
    use rig_core::providers::openai::OpenAIConfig;
    use tokio_prompt_orchestrator::integrations::RigWorker;

    fn worker(server: &MockServer) -> Box<dyn ModelWorker> {
        let model = OpenAIConfig::new("test-key")
            .with_base_url(format!("{}/v1", server.uri()))
            .client()
            .chat("gpt-4o-mini");
        Box::new(RigWorker::new(model))
    }

    #[tokio::test]
    async fn replies_streams_and_maps_errors() {
        check_worker(worker).await;
    }
}

/// The point of the integrations: the pipeline's own features work on top.
/// Two identical prompts through the deduplicator cost one provider call.
#[cfg(feature = "async-openai")]
#[tokio::test]
async fn dedup_in_front_of_an_integration_saves_the_second_call() {
    use async_openai::{config::OpenAIConfig, Client};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_prompt_orchestrator::enhanced::{DeduplicationResult, Deduplicator};
    use tokio_prompt_orchestrator::integrations::AsyncOpenAiWorker;

    let server = reply_server("Summarize this ticket", "A login bug.").await; // expects 1 call
    let config = OpenAIConfig::new()
        .with_api_base(format!("{}/v1", server.uri()))
        .with_api_key("k");
    let worker: Arc<dyn ModelWorker> = Arc::new(AsyncOpenAiWorker::new(
        Client::with_config(config),
        "gpt-4o-mini",
    ));
    let dedup = Deduplicator::new(Duration::from_secs(60));

    let mut answers = Vec::new();
    for _ in 0..2 {
        let answer = match dedup.check_and_register("Summarize this ticket").await {
            DeduplicationResult::Cached(cached) => cached,
            DeduplicationResult::New(token) => {
                let reply = worker
                    .infer("Summarize this ticket")
                    .await
                    .unwrap()
                    .join("");
                dedup.complete(token, reply.clone()).await;
                reply
            }
            DeduplicationResult::InProgress => unreachable!("calls are sequential"),
        };
        answers.push(answer);
    }
    assert_eq!(answers, vec!["A login bug.", "A login bug."]);
    // MockServer verifies `.expect(1)` on drop: the second answer cost nothing.
}

/// `OrchestratedTransport` under a rig 0.46 model: the rig request goes
/// through unchanged, and dedup, the breaker and retries act on the sends.
#[cfg(feature = "rig")]
mod rig_transport {
    use super::*;
    use rig_core::completion::CompletionRequest;
    use rig_core::driver::{Exchange, Model, Opened, Opening, Transport};
    use rig_core::http_client::framing::WireFrame;
    use rig_core::http_client::StatusCode;
    use rig_core::providers::openai::wire::Chat;
    use rig_core::providers::openai::OpenAIConfig;
    use rig_core::wire::{Body, Encoded, Wire};
    use rig_core::ProviderError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio_prompt_orchestrator::enhanced::{CircuitBreaker, RetryPolicy};
    use tokio_prompt_orchestrator::integrations::OrchestratedTransport;

    /// A transport that counts sends and answers each one the same way.
    #[derive(Clone)]
    struct MockTransport {
        calls: Arc<AtomicUsize>,
        bodies: Arc<Mutex<Vec<serde_json::Value>>>,
        reply: Result<String, u16>,
        delay: Duration,
    }

    impl MockTransport {
        fn replying(content: &str) -> Self {
            Self {
                calls: Arc::default(),
                bodies: Arc::default(),
                reply: Ok(completion(content).to_string()),
                delay: Duration::from_millis(50),
            }
        }

        fn failing(status: u16) -> Self {
            Self {
                reply: Err(status),
                delay: Duration::ZERO,
                ..Self::replying("")
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl<W: Wire<Payload = Encoded, Frame = WireFrame>> Transport<W> for MockTransport {
        fn send(&self, payload: Encoded, _exchange: Exchange) -> Opening<WireFrame> {
            let this = self.clone();
            Opening::new(async move {
                this.calls.fetch_add(1, Ordering::SeqCst);
                if let Body::Bytes(body) = payload.request.body() {
                    let json = serde_json::from_slice(body).unwrap_or_default();
                    this.bodies.lock().unwrap().push(json);
                }
                tokio::time::sleep(this.delay).await;
                Ok(match this.reply {
                    Ok(body) => Opened::new(futures::stream::iter([Ok(WireFrame::Text(body))])),
                    Err(status) => Opened::failed(ProviderError::from_http_response(
                        StatusCode::from_u16(status).unwrap(),
                        json!({"error": {"message": "upstream says no", "type": "error"}})
                            .to_string(),
                    )),
                })
            })
        }
    }

    fn model<T>(transport: OrchestratedTransport<T>) -> Model<Chat, OrchestratedTransport<T>> {
        let wire = OpenAIConfig::new("test-key")
            .client()
            .chat("gpt-4o-mini")
            .wire;
        Model::new(wire, transport)
    }

    fn quick_retry(attempts: usize) -> RetryPolicy {
        RetryPolicy::fixed(attempts, Duration::from_millis(1))
    }

    #[tokio::test]
    async fn concurrent_identical_requests_cost_one_upstream_call() {
        let mock = MockTransport::replying("Paris.");
        let model = model(OrchestratedTransport::new(mock.clone()));

        let request = || {
            CompletionRequest::new("Capital of France?")
                .max_tokens(Some(77))
                .temperature(Some(0.3))
        };
        let calls = (0..8).map(|_| model.call(request()));
        let replies = futures::future::join_all(calls).await;

        for reply in replies {
            assert_eq!(reply.expect("reply").text(), "Paris.");
        }
        assert_eq!(mock.calls(), 1, "8 identical requests, 1 upstream call");

        // Rig's request reached the transport as rig encoded it.
        let body = mock.bodies.lock().unwrap()[0].clone();
        assert_eq!(body["model"], "gpt-4o-mini");
        assert_eq!(body["temperature"], 0.3);
        assert!(
            body.to_string().contains("77"),
            "max_tokens passed through: {body}"
        );

        // A different prompt is a different request.
        model
            .call(CompletionRequest::new("Capital of Spain?"))
            .await
            .unwrap();
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn without_dedup_every_request_is_sent() {
        let mock = MockTransport::replying("Paris.");
        let model = model(
            OrchestratedTransport::builder(mock.clone())
                .without_dedup()
                .build(),
        );
        let calls = (0..3).map(|_| model.call(CompletionRequest::new("Capital of France?")));
        for reply in futures::future::join_all(calls).await {
            reply.unwrap();
        }
        assert_eq!(mock.calls(), 3);
    }

    #[tokio::test]
    async fn breaker_opens_after_k_failures() {
        const K: usize = 3;
        let mock = MockTransport::failing(503);
        let transport = OrchestratedTransport::builder(mock.clone())
            .breaker(Arc::new(CircuitBreaker::new(
                K,
                0.8,
                Duration::from_secs(60),
            )))
            .retry(quick_retry(1))
            .build();
        let model = model(transport.clone());

        for i in 0..K {
            let err = model
                .call(CompletionRequest::new(format!("q{i}")))
                .await
                .unwrap_err();
            assert_eq!(
                err.provider_response_status(),
                Some(StatusCode::SERVICE_UNAVAILABLE)
            );
        }
        assert_eq!(mock.calls(), K);
        assert!(transport.breaker().is_open_sync());

        // Open: further requests fail fast and never reach the provider.
        for i in 0..5 {
            let err = model
                .call(CompletionRequest::new(format!("more{i}")))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("circuit breaker open"), "{err}");
            assert!(!err.is_retryable());
        }
        assert_eq!(
            mock.calls(),
            K,
            "no upstream calls while the breaker is open"
        );
    }

    #[tokio::test]
    async fn retries_stop_at_max_attempts() {
        let mock = MockTransport::failing(503);
        let model = model(
            OrchestratedTransport::builder(mock.clone())
                .retry(quick_retry(4))
                .build(),
        );
        let err = model.call(CompletionRequest::new("hi")).await.unwrap_err();
        assert_eq!(
            err.provider_response_status(),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(mock.calls(), 4, "1 try + 3 retries, then the last error");
    }

    #[tokio::test]
    async fn non_retryable_errors_are_not_retried_or_counted() {
        let mock = MockTransport::failing(400);
        let transport = OrchestratedTransport::builder(mock.clone())
            .breaker(Arc::new(CircuitBreaker::new(
                1,
                0.8,
                Duration::from_secs(60),
            )))
            .retry(quick_retry(4))
            .build();
        let model = model(transport.clone());
        let err = model.call(CompletionRequest::new("hi")).await.unwrap_err();
        assert_eq!(
            err.provider_response_status(),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(mock.calls(), 1, "a 400 is not retried");
        assert!(
            transport.breaker().is_closed_sync(),
            "a 400 says nothing about provider health"
        );
    }

    /// The same through rig's real HTTP client against a mock server.
    #[tokio::test]
    async fn real_http_client_concurrent_calls_hit_the_server_once() {
        let server = reply_server("Capital of France?", "Paris.").await; // expects 1 call
        let base = OpenAIConfig::new("test-key")
            .with_base_url(format!("{}/v1", server.uri()))
            .client()
            .chat("gpt-4o-mini");
        let model = Model::new(base.wire, OrchestratedTransport::new(base.transport));

        let calls = (0..5).map(|_| model.call(CompletionRequest::new("Capital of France?")));
        for reply in futures::future::join_all(calls).await {
            assert_eq!(reply.unwrap().text(), "Paris.");
        }
        // MockServer verifies `.expect(1)` on drop.
    }

    #[tokio::test]
    async fn streams_are_not_deduplicated() {
        let server = stream_server(&["Hel", "lo"]).await;
        let base = OpenAIConfig::new("test-key")
            .with_base_url(format!("{}/v1", server.uri()))
            .client()
            .chat("gpt-4o-mini");
        let model = Model::new(base.wire, OrchestratedTransport::new(base.transport));

        let a = model.stream(CompletionRequest::new("hi")).unwrap();
        let b = model.stream(CompletionRequest::new("hi")).unwrap();
        let (a, b) = tokio::join!(a.collect::<Vec<_>>(), b.collect::<Vec<_>>());
        for items in [a, b] {
            assert!(!items.is_empty());
            assert!(items.iter().all(Result::is_ok), "a stream item failed");
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
