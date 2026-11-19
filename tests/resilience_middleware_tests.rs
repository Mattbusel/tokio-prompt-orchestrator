//! End-to-end tests for `ResilienceMiddleware` against a local mock server.
#![cfg(feature = "reqwest-middleware")]

use std::time::Duration;

use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, Error};
use tokio_prompt_orchestrator::enhanced::CircuitBreaker;
use tokio_prompt_orchestrator::integrations::{
    CircuitOpenError, JoinedRequestFailed, ResilienceMiddleware, SkipDedup,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(mw: ResilienceMiddleware) -> ClientWithMiddleware {
    ClientBuilder::new(reqwest::Client::new()).with(mw).build()
}

async fn slow_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-upstream", "yes")
                .set_body_string(r#"{"choices":[{"message":{"content":"Paris"}}]}"#)
                .set_delay(Duration::from_millis(300)),
        )
        .mount(server)
        .await;
}

fn chat_body(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": "gpt-4o-mini",
        "stream": stream,
        "messages": [{"role": "user", "content": "Capital of France?"}]
    })
}

#[tokio::test]
async fn identical_concurrent_posts_share_one_upstream_call() {
    let server = MockServer::start().await;
    slow_ok(&server).await;
    let client = client(ResilienceMiddleware::new());
    let url = format!("{}/v1/chat/completions", server.uri());

    let sends = (0..10).map(|i| {
        client
            .post(&url)
            .bearer_auth("sk-test")
            .header("x-request-id", format!("req-{i}")) // ignored in the key
            .json(&chat_body(false))
            .send()
    });
    let responses = futures::future::join_all(sends).await;
    for r in responses {
        let r = r.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-upstream"], "yes");
        assert_eq!(r.url().path(), "/v1/chat/completions");
        assert!(r.text().await.unwrap().contains("Paris"));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn different_credentials_streams_and_skip_dedup_are_not_shared() {
    let server = MockServer::start().await;
    slow_ok(&server).await;
    let mw = ResilienceMiddleware::new();
    let client = client(mw);
    let url = format!("{}/v1/chat/completions", server.uri());

    let (a, b, c, d, e) = tokio::join!(
        client
            .post(&url)
            .bearer_auth("k1")
            .json(&chat_body(false))
            .send(),
        client
            .post(&url)
            .bearer_auth("k2")
            .json(&chat_body(false))
            .send(),
        client
            .post(&url)
            .bearer_auth("k1")
            .json(&chat_body(true))
            .send(),
        client
            .post(&url)
            .bearer_auth("k1")
            .json(&chat_body(true))
            .send(),
        client
            .post(&url)
            .bearer_auth("k1")
            .json(&chat_body(false))
            .with_extension(SkipDedup)
            .send(),
    );
    for r in [a, b, c, d, e] {
        assert_eq!(r.unwrap().status(), 200);
    }
    // k1, k2, two streams, one SkipDedup: five requests reach the server.
    assert_eq!(server.received_requests().await.unwrap().len(), 5);
}

#[tokio::test]
async fn server_errors_open_the_breaker_for_that_host_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let mw = ResilienceMiddleware::new()
        .without_dedup()
        .with_circuit_breaker_per_host(2, 0.8, Duration::from_secs(60));
    let client = client(mw);
    let url = format!("{}/models", server.uri());

    // The 503s come back as responses, and count against the breaker.
    for _ in 0..2 {
        assert_eq!(client.get(&url).send().await.unwrap().status(), 503);
    }
    let err = client.get(&url).send().await.unwrap_err();
    let Error::Middleware(inner) = &err else {
        panic!("expected a middleware error, got {err:?}");
    };
    let open = inner
        .downcast_ref::<CircuitOpenError>()
        .expect("CircuitOpenError");
    assert!(open.host().starts_with("127.0.0.1:"));
    assert!(open.retry_after().is_some());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    // Another host has its own breaker.
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&other)
        .await;
    let ok = client.get(format!("{}/models", other.uri())).send().await;
    assert_eq!(ok.unwrap().status(), 200);
}

#[tokio::test]
async fn shared_breaker_and_custom_failure_statuses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let breaker = CircuitBreaker::new(1, 0.8, Duration::from_secs(60));
    let mw = ResilienceMiddleware::new()
        .with_circuit_breaker(breaker.clone())
        .failure_if_status(|s| s.as_u16() == 429 || s.is_server_error());
    let client = client(mw);
    assert_eq!(client.get(server.uri()).send().await.unwrap().status(), 429);
    assert!(breaker.is_open_sync());
    assert!(client.get(server.uri()).send().await.is_err());
}

#[tokio::test]
async fn joined_callers_get_the_leaders_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let client = ClientBuilder::new(http)
        .with(ResilienceMiddleware::new().without_circuit_breaker())
        .build();
    let url = format!("{}/v1/chat/completions", server.uri());

    let leader = client.post(&url).json(&chat_body(false)).send();
    let joiner = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.post(&url).json(&chat_body(false)).send().await
    };
    let (a, b) = tokio::join!(leader, joiner);
    assert!(
        matches!(a, Err(Error::Reqwest(ref e)) if e.is_timeout()),
        "{a:?}"
    );
    let Err(Error::Middleware(inner)) = b else {
        panic!("joined caller should get a middleware error, got {b:?}");
    };
    let joined = inner
        .downcast_ref::<JoinedRequestFailed>()
        .expect("JoinedRequestFailed");
    assert!(!joined.message().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn stats_count_leaders_and_coalesced_requests() {
    let server = MockServer::start().await;
    slow_ok(&server).await;
    let mw = std::sync::Arc::new(ResilienceMiddleware::new());
    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(mw.clone())
        .build();
    let url = format!("{}/v1/chat/completions", server.uri());
    let sends = (0..4).map(|_| client.post(&url).json(&chat_body(false)).send());
    for r in futures::future::join_all(sends).await {
        r.unwrap();
    }
    let stats = mw.dedup_stats().unwrap();
    assert_eq!(stats.leaders, 1);
    assert_eq!(stats.coalesced, 3);
    assert_eq!(stats.in_flight, 0);
    assert!(mw
        .circuit_breaker_for(&server.address().ip().to_string())
        .is_some());
}

#[tokio::test]
async fn the_same_prompt_twice_is_paid_for_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"choices":[{"message":{"content":"Paris"}}]}"#),
        )
        .mount(&server)
        .await;
    let mw = std::sync::Arc::new(ResilienceMiddleware::new());
    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(mw.clone())
        .build();
    let url = format!("{}/v1/chat/completions", server.uri());
    // A sampled request (temperature 1) sent twice, one after the other.
    let body = serde_json::json!({
        "model": "gpt-4o-mini",
        "temperature": 1.0,
        "messages": [{"role": "user", "content": "Capital of France?"}]
    });

    let first = client.post(&url).json(&body).send().await.unwrap();
    assert!(first.headers().get(llm_resilience::REPLAY_HEADER).is_none());
    let second = client.post(&url).json(&body).send().await.unwrap();
    assert_eq!(second.headers()[llm_resilience::REPLAY_HEADER], "replayed");
    assert!(second.text().await.unwrap().contains("Paris"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(mw.replayed(), Some(1));

    // Asking for a fresh sample is an explicit choice.
    let fresh = client
        .post(&url)
        .header("cache-control", "no-cache")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(fresh.headers().get(llm_resilience::REPLAY_HEADER).is_none());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn failures_are_not_replayed_and_the_window_can_be_turned_off() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    let client400 = client(ResilienceMiddleware::new());
    let url = format!("{}/v1/chat/completions", server.uri());
    for _ in 0..2 {
        let r = client400.post(&url).json(&chat_body(false)).send().await;
        assert_eq!(r.unwrap().status(), 400);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    let ok = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&ok)
        .await;
    let mw = ResilienceMiddleware::new().with_replay_window(Duration::ZERO);
    assert_eq!(mw.replayed(), None);
    let plain = client(mw);
    let url = format!("{}/v1/chat/completions", ok.uri());
    for _ in 0..2 {
        plain
            .post(&url)
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
    }
    assert_eq!(ok.received_requests().await.unwrap().len(), 2);
}
