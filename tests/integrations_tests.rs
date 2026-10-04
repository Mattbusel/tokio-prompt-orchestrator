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
