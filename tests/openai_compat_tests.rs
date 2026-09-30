//! Integration tests for the OpenAI-compatible endpoints
//! (`POST /v1/chat/completions`, `GET /v1/models`).
//!
//! Each test starts the real pipeline (`spawn_pipeline`) with an echo-style
//! worker and the real HTTP server on a free local port, then talks to it
//! over HTTP the way an OpenAI client would.
//!
//! Run with: `cargo test --features web-api --test openai_compat_tests`

#![cfg(feature = "web-api")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

use tokio_prompt_orchestrator::stages::PipelineHandles;
use tokio_prompt_orchestrator::web_api::{start_server, ServerConfig};
use tokio_prompt_orchestrator::{spawn_pipeline, EchoWorker, ModelWorker, OrchestratorError};

// ============================================================================
// Harness
// ============================================================================

/// Echo worker that counts how many times the "provider" is called.
struct CountingEcho {
    inner: EchoWorker,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ModelWorker for CountingEcho {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.infer(prompt).await
    }
}

/// Worker whose provider is down.
struct FailingWorker;

#[async_trait]
impl ModelWorker for FailingWorker {
    async fn infer(&self, _prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        Err(OrchestratorError::Inference("503 Service Unavailable (simulated outage)".into()))
    }
}

struct Server {
    base: String,
    handles: PipelineHandles,
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    l.local_addr().expect("local addr").port()
}

async fn start(worker: Arc<dyn ModelWorker>, tweak: impl FnOnce(&mut ServerConfig)) -> Server {
    let handles = spawn_pipeline(worker);
    let output_rx = handles.take_output_rx().await.expect("output receiver");
    let port = free_port();
    let mut config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        provider: "echo".to_string(),
        model: "echo".to_string(),
        ..ServerConfig::default()
    };
    tweak(&mut config);
    let (tx, dlq, cb) = (
        handles.input_tx.clone(),
        handles.dlq.clone(),
        handles.circuit_breaker.clone(),
    );
    tokio::spawn(async move {
        let _ = start_server(config, tx, output_rx, dlq, cb, None).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if client().get(format!("{base}/live")).send().await.is_ok() {
            return Server { base, handles };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server did not start on port {port}");
}

async fn start_echo() -> Server {
    start(Arc::new(EchoWorker::new()), |_| {}).await
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("reqwest client")
}

fn chat_body(text: &str) -> Value {
    json!({
        "model": "gpt-4o-mini",
        "messages": [{"role": "user", "content": text}],
        "temperature": 0.2,
        "max_tokens": 50,
        "some_future_field": {"ignored": true}
    })
}

async fn post_chat(base: &str, body: &Value) -> reqwest::Response {
    client()
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", "Bearer anything")
        .json(body)
        .send()
        .await
        .expect("request sent")
}

fn assert_openai_error(body: &Value, code: &str) {
    let err = &body["error"];
    assert!(err.is_object(), "error object missing: {body}");
    assert!(err["message"].as_str().is_some_and(|m| !m.is_empty()), "message: {body}");
    assert!(err["type"].is_string(), "type: {body}");
    assert!(err.get("param").is_some(), "param key: {body}");
    assert_eq!(err["code"], code, "code: {body}");
}

// ============================================================================
// Non-streaming
// ============================================================================

#[tokio::test]
async fn non_streaming_returns_a_chat_completion() {
    let s = start_echo().await;
    let resp = post_chat(&s.base, &chat_body("Say hello to the proxy")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-orchestrator-dedup"], "miss");
    let v: Value = resp.json().await.expect("json");

    assert_eq!(v["object"], "chat.completion");
    assert!(v["id"].as_str().is_some_and(|id| id.starts_with("chatcmpl-")));
    assert!(v["created"].as_u64().is_some_and(|c| c > 1_600_000_000));
    assert_eq!(v["model"], "echo");
    let choice = &v["choices"][0];
    assert_eq!(choice["index"], 0);
    assert_eq!(choice["message"]["role"], "assistant");
    // The echo worker returns the prompt; raw mode means no template around it.
    assert_eq!(choice["message"]["content"], "Say hello to the proxy");
    assert_eq!(choice["finish_reason"], "stop");

    let u = &v["usage"];
    let (p, c, t) = (
        u["prompt_tokens"].as_u64().expect("prompt_tokens"),
        u["completion_tokens"].as_u64().expect("completion_tokens"),
        u["total_tokens"].as_u64().expect("total_tokens"),
    );
    assert!(p > 0 && c > 0);
    assert_eq!(t, p + c);
}

#[tokio::test]
async fn system_and_multi_turn_messages_are_flattened() {
    let s = start_echo().await;
    let body = json!({
        "model": "x",
        "messages": [
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": [{"type": "text", "text": "Hi"}]}
        ]
    });
    let v: Value = post_chat(&s.base, &body).await.json().await.expect("json");
    // Echo splits on whitespace and the post stage joins with single spaces.
    assert_eq!(v["choices"][0]["message"]["content"], "System: Be brief. User: Hi");
}

// ============================================================================
// Streaming
// ============================================================================

#[tokio::test]
async fn streaming_returns_sse_chunks_ending_with_done() {
    let s = start_echo().await;
    let mut body = chat_body("stream these five words");
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage": true});
    let resp = post_chat(&s.base, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp.headers()["content-type"].to_str().expect("ascii").to_string();
    assert!(ctype.starts_with("text/event-stream"), "content-type {ctype}");
    let raw = resp.text().await.expect("body");

    let data: Vec<&str> = raw
        .lines()
        .filter_map(|l| l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:")))
        .collect();
    assert_eq!(data.last().copied(), Some("[DONE]"), "raw: {raw}");

    let chunks: Vec<Value> = data[..data.len() - 1]
        .iter()
        .map(|d| serde_json::from_str(d).expect("each data line is JSON"))
        .collect();
    assert!(chunks.len() >= 3);
    let id = chunks[0]["id"].as_str().expect("id").to_string();
    for c in &chunks {
        assert_eq!(c["object"], "chat.completion.chunk");
        assert_eq!(c["id"], id.as_str());
        assert_eq!(c["model"], "echo");
    }
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");

    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "stream these five words");

    let finish: Vec<&Value> = chunks
        .iter()
        .filter(|c| c["choices"][0]["finish_reason"] == "stop")
        .collect();
    assert_eq!(finish.len(), 1);

    let usage = chunks.last().expect("usage chunk");
    assert_eq!(usage["choices"], json!([]));
    assert!(usage["usage"]["total_tokens"].as_u64().is_some_and(|t| t > 0));
}

// ============================================================================
// Deduplication
// ============================================================================

#[tokio::test]
async fn identical_concurrent_requests_make_one_upstream_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = Arc::new(CountingEcho {
        inner: EchoWorker::with_delay(400),
        calls: Arc::clone(&calls),
    });
    let s = start(worker, |_| {}).await;

    let body = chat_body("the same question five times");
    let futs = (0..5).map(|_| post_chat(&s.base, &body));
    let resps = futures::future::join_all(futs).await;

    let mut outcomes = Vec::new();
    for r in resps {
        assert_eq!(r.status(), StatusCode::OK);
        outcomes.push(r.headers()["x-orchestrator-dedup"].to_str().expect("ascii").to_string());
        let v: Value = r.json().await.expect("json");
        assert_eq!(v["choices"][0]["message"]["content"], "the same question five times");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one upstream call for 5 identical requests");
    assert_eq!(outcomes.iter().filter(|o| *o == "miss").count(), 1, "{outcomes:?}");
    assert_eq!(outcomes.iter().filter(|o| *o == "joined").count(), 4, "{outcomes:?}");

    // Asked again after it finished: served from the dedup window.
    let again = post_chat(&s.base, &body).await;
    assert_eq!(again.headers()["x-orchestrator-dedup"], "cached");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A different question is a new call.
    let other = post_chat(&s.base, &chat_body("a different question")).await;
    assert_eq!(other.headers()["x-orchestrator-dedup"], "miss");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn dedup_window_zero_only_shares_in_flight_calls() {
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = Arc::new(CountingEcho {
        inner: EchoWorker::with_delay(10),
        calls: Arc::clone(&calls),
    });
    let s = start(worker, |c| c.dedup_window_secs = 0).await;
    let body = chat_body("ask twice in a row");
    assert_eq!(post_chat(&s.base, &body).await.status(), StatusCode::OK);
    assert_eq!(post_chat(&s.base, &body).await.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// ============================================================================
// /v1/models
// ============================================================================

#[tokio::test]
async fn models_lists_the_configured_model() {
    let s = start(Arc::new(EchoWorker::new()), |c| {
        c.provider = "openai".into();
        c.model = "gpt-4o-mini".into();
    })
    .await;
    let resp = client().get(format!("{}/v1/models", s.base)).send().await.expect("sent");
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.expect("json");
    assert_eq!(v["object"], "list");
    let data = v["data"].as_array().expect("data array");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["id"], "gpt-4o-mini");
    assert_eq!(data[0]["object"], "model");
    assert_eq!(data[0]["owned_by"], "openai");
    assert!(data[0]["created"].as_u64().is_some());
}

// ============================================================================
// Auth
// ============================================================================

#[tokio::test]
async fn auth_on_requires_the_bearer_key() {
    let s = start(Arc::new(EchoWorker::new()), |c| c.api_key = Some("sk-orch-test".into())).await;
    let url = format!("{}/v1/chat/completions", s.base);
    let body = chat_body("auth check");

    let none = client().post(&url).json(&body).send().await.expect("sent");
    assert_eq!(none.status(), StatusCode::UNAUTHORIZED);
    assert_openai_error(&none.json().await.expect("json"), "invalid_api_key");

    let wrong = client().post(&url).bearer_auth("sk-wrong").json(&body).send().await.expect("sent");
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let right = client().post(&url).bearer_auth("sk-orch-test").json(&body).send().await.expect("sent");
    assert_eq!(right.status(), StatusCode::OK);

    let models = client()
        .get(format!("{}/v1/models", s.base))
        .send()
        .await
        .expect("sent");
    assert_eq!(models.status(), StatusCode::UNAUTHORIZED);
    let models = client()
        .get(format!("{}/v1/models", s.base))
        .bearer_auth("sk-orch-test")
        .send()
        .await
        .expect("sent");
    assert_eq!(models.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_off_accepts_any_or_no_key() {
    let s = start_echo().await;
    let url = format!("{}/v1/chat/completions", s.base);
    let body = chat_body("open server");
    let none = client().post(&url).json(&body).send().await.expect("sent");
    assert_eq!(none.status(), StatusCode::OK);
    let any = client().post(&url).bearer_auth("sk-whatever").json(&body).send().await.expect("sent");
    assert_eq!(any.status(), StatusCode::OK);
}

// ============================================================================
// Errors
// ============================================================================

#[tokio::test]
async fn bad_requests_get_openai_shaped_400s() {
    let s = start_echo().await;
    let url = format!("{}/v1/chat/completions", s.base);

    let bad_json = client()
        .post(&url)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .expect("sent");
    assert_eq!(bad_json.status(), StatusCode::BAD_REQUEST);
    let v: Value = bad_json.json().await.expect("json");
    assert_openai_error(&v, "invalid_request");
    assert_eq!(v["error"]["type"], "invalid_request_error");

    let no_messages = post_chat(&s.base, &json!({"model": "x"})).await;
    assert_eq!(no_messages.status(), StatusCode::BAD_REQUEST);

    let empty = post_chat(&s.base, &json!({"model": "x", "messages": []})).await;
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
    assert_eq!(empty.json::<Value>().await.expect("json")["error"]["param"], "messages");

    let mut n2 = chat_body("two please");
    n2["n"] = json!(2);
    let n2 = post_chat(&s.base, &n2).await;
    assert_eq!(n2.status(), StatusCode::BAD_REQUEST);
    assert_eq!(n2.json::<Value>().await.expect("json")["error"]["param"], "n");
}

#[tokio::test]
async fn open_circuit_breaker_returns_503() {
    let s = start_echo().await;
    s.handles.circuit_breaker.trip().await;
    let resp = post_chat(&s.base, &chat_body("is anyone there")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_openai_error(&resp.json().await.expect("json"), "circuit_open");
    let dropped = s.handles.dlq.peek();
    assert!(dropped.iter().any(|d| d.reason == "circuit_breaker_open"), "{dropped:?}");
}

#[tokio::test]
async fn provider_failures_return_502_then_the_breaker_opens() {
    let s = start(Arc::new(FailingWorker), |_| {}).await;
    // The breaker opens after 5 consecutive failures.
    for i in 0..5 {
        let resp = post_chat(&s.base, &chat_body(&format!("outage {i}"))).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_openai_error(&resp.json().await.expect("json"), "upstream_error");
    }
    let resp = post_chat(&s.base, &chat_body("outage 5")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_openai_error(&resp.json().await.expect("json"), "circuit_open");
}

#[tokio::test]
async fn joined_requests_get_the_same_error() {
    struct SlowFail;
    #[async_trait]
    impl ModelWorker for SlowFail {
        async fn infer(&self, _p: &str) -> Result<Vec<String>, OrchestratorError> {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Err(OrchestratorError::Inference("boom".into()))
        }
    }
    let s = start(Arc::new(SlowFail), |_| {}).await;
    let body = chat_body("fails for everyone");
    let resps = futures::future::join_all((0..3).map(|_| post_chat(&s.base, &body))).await;
    for r in resps {
        assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    }
}

#[tokio::test]
async fn spend_cap_reached_returns_429() {
    let s = start(Arc::new(EchoWorker::new()), |c| c.max_spend_usd = Some(0.0)).await;
    let resp = post_chat(&s.base, &chat_body("one more please")).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let v: Value = resp.json().await.expect("json");
    assert_openai_error(&v, "insufficient_quota");
    assert_eq!(v["error"]["type"], "insufficient_quota");
}

#[tokio::test]
async fn slow_upstream_returns_504() {
    let s = start(Arc::new(EchoWorker::with_delay(3_000)), |c| c.timeout_seconds = 1).await;
    let resp = post_chat(&s.base, &chat_body("take your time")).await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_openai_error(&resp.json().await.expect("json"), "timeout");
}

// ============================================================================
// Side effect on the existing API: dropped requests now show as failed
// ============================================================================

#[tokio::test]
async fn infer_status_reports_failed_when_the_pipeline_drops_the_request() {
    let s = start_echo().await;
    s.handles.circuit_breaker.trip().await;
    let v: Value = client()
        .post(format!("{}/api/v1/infer", s.base))
        .json(&json!({"prompt": "hello"}))
        .send()
        .await
        .expect("sent")
        .json()
        .await
        .expect("json");
    let id = v["request_id"].as_str().expect("request_id").to_string();
    let mut last = Value::Null;
    for _ in 0..40 {
        last = client()
            .get(format!("{}/api/v1/status/{id}", s.base))
            .send()
            .await
            .expect("sent")
            .json()
            .await
            .expect("json");
        if last["status"] == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(last["status"], "failed", "{last}");
    assert_eq!(last["error"], "circuit_breaker_open");
}
