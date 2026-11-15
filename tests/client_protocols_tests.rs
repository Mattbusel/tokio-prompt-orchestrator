//! The other client protocols the proxy speaks: the Anthropic Messages API
//! (`POST /v1/messages`, `POST /v1/messages/count_tokens`), the OpenAI
//! Responses API (`POST /v1/responses`) and embeddings (`POST /v1/embeddings`).
//!
//! The upstreams are mock HTTP servers (axum) that record every request
//! body, so the tests check what goes over the wire in both directions, with
//! the real orchestrator server and pipelines in between.
//!
//! Run with: `cargo test --features web-api --test client_protocols_tests`
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "web-api")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode as AxumStatus;
use axum::response::IntoResponse;
use axum::routing::post as post_route;
use axum::{Json, Router};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

use tokio_prompt_orchestrator::web_api::{start_server, EmbeddingsBackend, ServerConfig};
use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, Embedder, ModelRoute, ModelRoutes, ModelRoutesConfig, OrchestratorError,
    PipelineOptions, RouteProvider,
};

// ============================================================================
// Mock upstream: OpenAI chat + embeddings, Anthropic Messages
// ============================================================================

#[derive(Default)]
struct Upstream {
    chat: parking_lot::Mutex<Vec<Value>>,
    messages: parking_lot::Mutex<Vec<Value>>,
    embeddings: parking_lot::Mutex<Vec<Value>>,
    embedding_calls: AtomicUsize,
    /// Answer embeddings with this status instead of vectors (0 = succeed).
    embedding_status: std::sync::atomic::AtomicU16,
    delay_ms: u64,
}

impl Upstream {
    fn last_chat(&self) -> Value {
        self.chat.lock().last().cloned().expect("the upstream got a chat request")
    }
    fn chat_calls(&self) -> usize {
        self.chat.lock().len()
    }
    fn embedding_calls(&self) -> usize {
        self.embedding_calls.load(Ordering::SeqCst)
    }
    fn embedded_inputs(&self) -> Vec<Value> {
        self.embeddings
            .lock()
            .iter()
            .flat_map(|b| b["input"].as_array().cloned().unwrap_or_default())
            .collect()
    }
}

/// `POST /v1/chat/completions`: a tool call when tools were sent, else
/// `openai:<model>`.
async fn openai_chat(State(up): State<Arc<Upstream>>, Json(body): Json<Value>) -> axum::response::Response {
    up.chat.lock().push(body.clone());
    let model = body["model"].as_str().unwrap_or("").to_string();
    let (message, finish) = if body.get("tools").is_some() {
        (
            json!({"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_abc", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
            }]}),
            "tool_calls",
        )
    } else {
        (json!({"role": "assistant", "content": format!("openai:{model}")}), "stop")
    };
    Json(json!({
        "id": "chatcmpl-upstream",
        "object": "chat.completion",
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    }))
    .into_response()
}

/// `POST /v1/messages`: Anthropic's Messages API.
async fn anthropic_messages(State(up): State<Arc<Upstream>>, Json(body): Json<Value>) -> axum::response::Response {
    up.messages.lock().push(body.clone());
    let model = body["model"].as_str().unwrap_or("").to_string();
    if body.get("tools").is_some() {
        return Json(json!({
            "id": "msg_up", "type": "message", "role": "assistant", "model": model,
            "content": [
                {"type": "text", "text": "Checking."},
                {"type": "tool_use", "id": "toolu_up", "name": "get_weather", "input": {"city": "Paris"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 21, "output_tokens": 9}
        }))
        .into_response();
    }
    Json(json!({
        "id": "msg_up", "type": "message", "role": "assistant", "model": model,
        "content": [{"type": "text", "text": format!("anthropic:{model}")}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3}
    }))
    .into_response()
}

/// The vector the mock gives an input: `[length, kind]` where kind is 1 for
/// text and 2 for a token array.
fn mock_vector(input: &Value) -> Vec<f32> {
    match input {
        Value::String(s) => vec![s.len() as f32, 1.0],
        Value::Array(t) => vec![t.len() as f32, 2.0],
        _ => vec![0.0, 0.0],
    }
}

/// `POST /v1/embeddings`: answers in reverse index order, to check that the
/// proxy sorts by `index`.
async fn openai_embeddings(State(up): State<Arc<Upstream>>, Json(body): Json<Value>) -> axum::response::Response {
    up.embedding_calls.fetch_add(1, Ordering::SeqCst);
    up.embeddings.lock().push(body.clone());
    if up.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(up.delay_ms)).await;
    }
    let status = up.embedding_status.load(Ordering::SeqCst);
    if status != 0 {
        let code = AxumStatus::from_u16(status).expect("status");
        return (code, Json(json!({"error": {"message": format!("mock failure {status}"), "type": "x"}}))).into_response();
    }
    let inputs = body["input"].as_array().cloned().unwrap_or_default();
    let mut data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, input)| {
            let mut v = mock_vector(input);
            if let Some(d) = body["dimensions"].as_u64() {
                v.resize(d as usize, 0.5);
            }
            json!({"object": "embedding", "index": i, "embedding": v})
        })
        .collect();
    data.reverse();
    Json(json!({
        "object": "list",
        "data": data,
        "model": body["model"],
        "usage": {"prompt_tokens": inputs.len() * 3, "total_tokens": inputs.len() * 3}
    }))
    .into_response()
}

/// Start a mock upstream; returns its `/v1` base URL.
async fn spawn_upstream(delay_ms: u64) -> (String, Arc<Upstream>) {
    let up = Arc::new(Upstream {
        delay_ms,
        ..Upstream::default()
    });
    let app = Router::new()
        .route("/v1/chat/completions", post_route(openai_chat))
        .route("/v1/messages", post_route(anthropic_messages))
        .route("/v1/embeddings", post_route(openai_embeddings))
        .with_state(Arc::clone(&up));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/v1"), up)
}

// ============================================================================
// Orchestrator harness
// ============================================================================

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    l.local_addr().expect("local addr").port()
}

fn client() -> Client {
    Client::builder().timeout(Duration::from_secs(15)).build().expect("client")
}

/// Start the orchestrator with an echo default route, `routes` (if any) and
/// `tweak` applied to the config.
async fn start(routes: Option<ModelRoutesConfig>, tweak: impl FnOnce(&mut ServerConfig)) -> String {
    let handles = spawn_pipeline(Arc::new(EchoWorker::new()));
    let output_rx = handles.take_output_rx().await.expect("output receiver");
    let port = free_port();
    let model_routes = routes.map(|cfg| {
        Arc::new(ModelRoutes::spawn(&cfg, &PipelineOptions::default()).expect("routes spawn"))
    });
    let mut config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        provider: "echo".to_string(),
        model: "echo".to_string(),
        model_routes,
        ..ServerConfig::default()
    };
    tweak(&mut config);
    let (tx, dlq, cb) = (handles.input_tx.clone(), handles.dlq.clone(), handles.circuit_breaker.clone());
    tokio::spawn(async move {
        let _handles = handles;
        let _ = start_server(config, tx, output_rx, dlq, cb, None).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if client().get(format!("{base}/live")).send().await.is_ok() {
            return base;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server did not start on port {port}");
}

fn gpt_routes(url: &str) -> ModelRoutesConfig {
    ModelRoutesConfig::new(vec![ModelRoute::new("gpt-*", RouteProvider::OpenAi).with_base_url(url)])
}

async fn post(base: &str, path: &str, body: &Value) -> reqwest::Response {
    client()
        .post(format!("{base}{path}"))
        .json(body)
        .send()
        .await
        .expect("request sent")
}

/// `(event name, data)` pairs of an SSE body.
fn sse_events(body: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut name = String::new();
    for line in body.lines() {
        if let Some(n) = line.strip_prefix("event: ") {
            name = n.to_string();
        } else if let Some(d) = line.strip_prefix("data: ") {
            if let Ok(v) = serde_json::from_str::<Value>(d) {
                out.push((std::mem::take(&mut name), v));
            }
        }
    }
    out
}

fn weather_tool_anthropic() -> Value {
    json!({
        "name": "get_weather",
        "description": "Current weather",
        "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
        "cache_control": {"type": "ephemeral"}
    })
}

// ============================================================================
// Anthropic Messages API
// ============================================================================

/// A Claude Code style request: system blocks, tools, a tool_use / tool_result
/// round in the history, `?beta=true`.
fn claude_code_request(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "max_tokens": 1024,
        "stream": stream,
        "system": [{"type": "text", "text": "You are a coding agent.", "cache_control": {"type": "ephemeral"}}],
        "tools": [weather_tool_anthropic()],
        "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "metadata": {"user_id": "session-1"},
        "messages": [
            {"role": "user", "content": "Weather in Rome?"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "toolu_0", "name": "get_weather", "input": {"city": "Rome"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0", "content": [{"type": "text", "text": "21C"}]},
                {"type": "text", "text": "And Paris?"}
            ]}
        ]
    })
}

#[tokio::test]
async fn messages_with_tools_are_translated_for_an_openai_upstream_and_back() {
    let (url, up) = spawn_upstream(0).await;
    let base = start(Some(gpt_routes(&url)), |_| {}).await;

    let r = post(&base, "/v1/messages?beta=true", &claude_code_request("gpt-4o-mini", false)).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v: Value = r.json().await.expect("json");

    // What the OpenAI upstream received.
    let sent = up.last_chat();
    assert_eq!(sent["model"], "gpt-4o-mini");
    assert_eq!(sent["max_tokens"], 1024);
    assert_eq!(sent["messages"][0], json!({"role": "system", "content": "You are a coding agent."}));
    assert_eq!(sent["messages"][2]["tool_calls"][0]["id"], "toolu_0");
    assert_eq!(sent["messages"][2]["tool_calls"][0]["function"]["arguments"], "{\"city\":\"Rome\"}");
    assert_eq!(sent["messages"][3], json!({"role": "tool", "tool_call_id": "toolu_0", "content": "21C"}));
    assert_eq!(sent["messages"][4], json!({"role": "user", "content": "And Paris?"}));
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "get_weather");
    assert_eq!(sent["tools"][0]["function"]["parameters"]["required"][0], "city");
    assert_eq!(sent["tool_choice"], "auto");
    assert_eq!(sent["parallel_tool_calls"], false);

    // What the Anthropic client got back.
    assert_eq!(v["type"], "message");
    assert_eq!(v["stop_reason"], "tool_use");
    assert_eq!(v["content"][0]["type"], "tool_use");
    assert_eq!(v["content"][0]["id"], "call_abc");
    assert_eq!(v["content"][0]["name"], "get_weather");
    assert_eq!(v["content"][0]["input"], json!({"city": "Paris"}));
    assert_eq!(v["usage"]["input_tokens"], 11);
    assert_eq!(v["usage"]["output_tokens"], 7);
}

#[tokio::test]
async fn messages_round_trip_to_an_anthropic_upstream_with_tool_use() {
    let (url, up) = spawn_upstream(0).await;
    std::env::set_var("TPO_PROTOCOLS_TEST_ANTHROPIC_KEY", "sk-ant-test");
    let routes = ModelRoutesConfig::new(vec![ModelRoute::new("claude-*", RouteProvider::Anthropic)
        .with_base_url(&url)
        .with_api_key_env("TPO_PROTOCOLS_TEST_ANTHROPIC_KEY")]);
    let base = start(Some(routes), |_| {}).await;

    let mut req = claude_code_request("claude-sonnet-4-5", false);
    req["tool_choice"] = json!({"type": "tool", "name": "get_weather"});
    let v: Value = post(&base, "/v1/messages", &req).await.json().await.expect("json");

    let sent = up.messages.lock().last().cloned().expect("upstream called");
    assert_eq!(sent["model"], "claude-sonnet-4-5");
    assert_eq!(sent["system"], "You are a coding agent.");
    assert_eq!(sent["tools"][0]["name"], "get_weather");
    assert_eq!(sent["tools"][0]["input_schema"]["required"][0], "city");
    assert_eq!(sent["tool_choice"]["type"], "tool");
    assert_eq!(sent["tool_choice"]["name"], "get_weather");
    assert_eq!(sent["messages"][1]["content"][1]["type"], "tool_use");
    assert_eq!(sent["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(sent["messages"][2]["content"][0]["tool_use_id"], "toolu_0");

    assert_eq!(v["stop_reason"], "tool_use");
    assert_eq!(v["content"][0], json!({"type": "text", "text": "Checking."}));
    assert_eq!(v["content"][1]["type"], "tool_use");
    assert_eq!(v["content"][1]["id"], "toolu_up");
    assert_eq!(v["content"][1]["input"], json!({"city": "Paris"}));
    assert_eq!(v["usage"]["input_tokens"], 21);
}

#[tokio::test]
async fn messages_stream_tool_use_as_input_json_deltas() {
    let (url, _up) = spawn_upstream(0).await;
    let base = start(Some(gpt_routes(&url)), |_| {}).await;
    let r = post(&base, "/v1/messages", &claude_code_request("gpt-4o", true)).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers()["content-type"].to_str().unwrap_or("").starts_with("text/event-stream"));
    let events = sse_events(&r.text().await.expect("body"));
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.first(), Some(&"message_start"));
    assert_eq!(names.last(), Some(&"message_stop"));
    for (name, data) in &events {
        assert_eq!(data["type"], name.as_str(), "event name and data type agree");
    }
    let start = events.iter().find(|(n, _)| n == "content_block_start").expect("block start");
    assert_eq!(start.1["content_block"]["type"], "tool_use");
    assert_eq!(start.1["content_block"]["id"], "call_abc");
    assert_eq!(start.1["content_block"]["input"], json!({}));
    let json_text: String = events
        .iter()
        .filter(|(_, d)| d["delta"]["type"] == "input_json_delta")
        .map(|(_, d)| d["delta"]["partial_json"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(serde_json::from_str::<Value>(&json_text).expect("valid json"), json!({"city": "Paris"}));
    let delta = events.iter().find(|(n, _)| n == "message_delta").expect("message_delta");
    assert_eq!(delta.1["delta"]["stop_reason"], "tool_use");
}

#[tokio::test]
async fn messages_share_identical_requests_and_respect_the_spend_cap() {
    let (url, up) = spawn_upstream(0).await;
    let base = start(Some(gpt_routes(&url)), |_| {}).await;
    let req = claude_code_request("gpt-4o", false);
    assert_eq!(post(&base, "/v1/messages", &req).await.headers()["x-orchestrator-dedup"], "miss");
    assert_eq!(post(&base, "/v1/messages", &req).await.headers()["x-orchestrator-dedup"], "cached");
    assert_eq!(up.chat_calls(), 1);

    let capped = start(Some(gpt_routes(&url)), |c| c.max_spend_usd = Some(0.0)).await;
    let r = post(&capped, "/v1/messages", &req).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    let v: Value = r.json().await.expect("json");
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn messages_reject_server_tools_in_anthropic_shape() {
    let base = start(None, |_| {}).await;
    let r = post(
        &base,
        "/v1/messages",
        &json!({"model": "m", "max_tokens": 8,
                "tools": [{"type": "web_search_20250305", "name": "web_search"}],
                "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v: Value = r.json().await.expect("json");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert!(v["error"]["message"].as_str().unwrap_or("").contains("web_search_20250305"));
}

#[tokio::test]
async fn count_tokens_answers_without_calling_the_model() {
    let (url, up) = spawn_upstream(0).await;
    let base = start(Some(gpt_routes(&url)), |_| {}).await;
    let mut req = claude_code_request("gpt-4o", false);
    req.as_object_mut().expect("object").remove("max_tokens");
    let r = post(&base, "/v1/messages/count_tokens", &req).await;
    assert_eq!(r.status(), StatusCode::OK);
    let with_tools = r.json::<Value>().await.expect("json")["input_tokens"].as_u64().expect("count");
    req.as_object_mut().expect("object").remove("tools");
    let without = post(&base, "/v1/messages/count_tokens", &req).await.json::<Value>().await.expect("json")
        ["input_tokens"]
        .as_u64()
        .expect("count");
    assert!(without > 0);
    assert!(with_tools > without, "tool definitions count: {with_tools} vs {without}");
    assert_eq!(up.chat_calls(), 0);
}

#[tokio::test]
async fn count_tokens_uses_anthropic_auth_errors() {
    let base = start(None, |c| c.api_key = Some("secret".into())).await;
    let r = client()
        .post(format!("{base}/v1/messages/count_tokens"))
        .header("x-api-key", "wrong")
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let v: Value = r.json().await.expect("json");
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "authentication_error");
}

// ============================================================================
// OpenAI Responses API
// ============================================================================

fn responses_request(stream: bool) -> Value {
    json!({
        "model": "gpt-4o-mini",
        "instructions": "Be terse.",
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "Weather in Rome?"}]},
            {"type": "function_call", "call_id": "call_0", "name": "get_weather", "arguments": "{\"city\":\"Rome\"}"},
            {"type": "function_call_output", "call_id": "call_0", "output": "21C"},
            {"role": "user", "content": "And Paris?"}
        ],
        "tools": [{"type": "function", "name": "get_weather", "description": "Current weather",
                   "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}, "strict": true}],
        "tool_choice": "required",
        "parallel_tool_calls": false,
        "temperature": 0,
        "max_output_tokens": 300,
        "text": {"format": {"type": "json_schema", "name": "w", "schema": {"type": "object"}, "strict": true}},
        "metadata": {"trace": "t1"},
        "store": true,
        "stream": stream
    })
}

#[tokio::test]
async fn responses_are_translated_for_the_upstream_and_back() {
    let (url, up) = spawn_upstream(0).await;
    let base = start(Some(gpt_routes(&url)), |_| {}).await;
    let r = post(&base, "/v1/responses", &responses_request(false)).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["x-orchestrator-dedup"], "miss");
    let v: Value = r.json().await.expect("json");

    let sent = up.last_chat();
    assert_eq!(sent["model"], "gpt-4o-mini");
    assert_eq!(sent["messages"][0], json!({"role": "system", "content": "Be terse."}));
    assert_eq!(sent["messages"][1], json!({"role": "user", "content": "Weather in Rome?"}));
    assert_eq!(sent["messages"][2]["tool_calls"][0]["id"], "call_0");
    assert_eq!(sent["messages"][3], json!({"role": "tool", "tool_call_id": "call_0", "content": "21C"}));
    assert_eq!(sent["max_completion_tokens"], 300);
    assert_eq!(sent["tool_choice"], "required");
    assert_eq!(sent["parallel_tool_calls"], false);
    assert_eq!(sent["tools"][0]["function"]["strict"], true);
    assert_eq!(sent["response_format"]["type"], "json_schema");
    assert_eq!(sent["response_format"]["json_schema"]["name"], "w");

    assert_eq!(v["object"], "response");
    assert!(v["id"].as_str().is_some_and(|id| id.starts_with("resp_")));
    assert_eq!(v["status"], "completed");
    assert_eq!(v["model"], "gpt-4o-mini");
    assert_eq!(v["store"], false);
    assert_eq!(v["metadata"]["trace"], "t1");
    let call = &v["output"][0];
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["call_id"], "call_abc");
    assert_eq!(call["name"], "get_weather");
    assert_eq!(call["arguments"], "{\"city\":\"Paris\"}");
    assert_eq!(v["usage"]["input_tokens"], 11);
    assert_eq!(v["usage"]["output_tokens"], 7);
    assert_eq!(v["usage"]["total_tokens"], 18);
}

#[tokio::test]
async fn responses_plain_text_answer_and_dedup() {
    let (url, up) = spawn_upstream(0).await;
    let base = start(Some(gpt_routes(&url)), |_| {}).await;
    let req = json!({"model": "gpt-4o", "input": "Say hi", "temperature": 0});
    let r = post(&base, "/v1/responses", &req).await;
    let v: Value = r.json().await.expect("json");
    assert_eq!(v["output"][0]["type"], "message");
    assert_eq!(v["output"][0]["role"], "assistant");
    assert_eq!(v["output"][0]["content"][0]["type"], "output_text");
    assert_eq!(v["output"][0]["content"][0]["text"], "openai:gpt-4o");
    let again = post(&base, "/v1/responses", &req).await;
    assert_eq!(again.headers()["x-orchestrator-dedup"], "cached");
    assert_eq!(up.chat_calls(), 1);
}

#[tokio::test]
async fn responses_stream_follows_the_event_sequence() {
    let base = start(None, |_| {}).await;
    let r = post(&base, "/v1/responses", &json!({"model": "echo", "input": "one two three", "stream": true})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let events = sse_events(&r.text().await.expect("body"));
    for (i, (name, data)) in events.iter().enumerate() {
        assert_eq!(data["type"], name.as_str());
        assert_eq!(data["sequence_number"], i);
    }
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    let order = [
        "response.created",
        "response.in_progress",
        "response.output_item.added",
        "response.content_part.added",
        "response.output_text.delta",
        "response.output_text.done",
        "response.content_part.done",
        "response.output_item.done",
        "response.completed",
    ];
    let mut at = 0;
    for name in &names {
        let i = order.iter().position(|o| o == name).expect("known event");
        assert!(i >= at, "{name} out of order in {names:?}");
        at = i;
    }
    let text: String = events
        .iter()
        .filter(|(n, _)| n == "response.output_text.delta")
        .map(|(_, d)| d["delta"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(text, "one two three");
    let done = &events.last().expect("events").1["response"];
    assert_eq!(done["status"], "completed");
    assert_eq!(done["output"][0]["content"][0]["text"], "one two three");
}

#[tokio::test]
async fn responses_stream_function_calls() {
    let (url, _up) = spawn_upstream(0).await;
    let base = start(Some(gpt_routes(&url)), |_| {}).await;
    let r = post(&base, "/v1/responses", &responses_request(true)).await;
    let events = sse_events(&r.text().await.expect("body"));
    let added = events
        .iter()
        .find(|(n, _)| n == "response.output_item.added")
        .expect("item added");
    assert_eq!(added.1["item"]["type"], "function_call");
    assert_eq!(added.1["item"]["arguments"], "");
    let args = events
        .iter()
        .find(|(n, _)| n == "response.function_call_arguments.done")
        .expect("arguments done");
    assert_eq!(args.1["arguments"], "{\"city\":\"Paris\"}");
    assert_eq!(events.last().expect("events").0, "response.completed");
}

#[tokio::test]
async fn responses_reject_stateful_and_builtin_features() {
    let base = start(None, |_| {}).await;
    for (body, param) in [
        (json!({"model": "m", "input": "x", "previous_response_id": "resp_1"}), "previous_response_id"),
        (json!({"model": "m", "input": "x", "tools": [{"type": "web_search"}]}), "tools"),
        (json!({"model": "m", "input": [{"role": "user", "content": [{"type": "input_image", "image_url": "u"}]}]}), "input"),
        (json!({"model": "m"}), "input"),
    ] {
        let r = post(&base, "/v1/responses", &body).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{body}");
        let v: Value = r.json().await.expect("json");
        assert_eq!(v["error"]["param"], param, "{body}");
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }
    let capped = start(None, |c| c.max_spend_usd = Some(0.0)).await;
    let r = post(&capped, "/v1/responses", &json!({"model": "m", "input": "x"})).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ============================================================================
// Embeddings
// ============================================================================

async fn start_embeddings(url: &str, tweak: impl FnOnce(&mut ServerConfig)) -> String {
    let url = url.to_string();
    start(None, move |c| {
        c.embeddings = Some(EmbeddingsBackend::openai_compatible(url, Some("sk-test".into())));
        tweak(c);
    })
    .await
}

fn vectors(v: &Value) -> Vec<Vec<f64>> {
    v["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|d| d["embedding"].as_array().expect("floats").iter().map(|x| x.as_f64().expect("f")).collect())
        .collect()
}

#[tokio::test]
async fn embeddings_are_returned_in_input_order() {
    let (url, up) = spawn_upstream(0).await;
    let base = start_embeddings(&url, |_| {}).await;
    let r = post(
        &base,
        "/v1/embeddings",
        &json!({"model": "text-embedding-3-small", "input": ["a", "bbb", "cc"], "user": "u1"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["x-orchestrator-dedup"], "miss");
    let v: Value = r.json().await.expect("json");
    assert_eq!(v["object"], "list");
    assert_eq!(v["model"], "text-embedding-3-small");
    assert_eq!(vectors(&v), vec![vec![1.0, 1.0], vec![3.0, 1.0], vec![2.0, 1.0]]);
    for (i, d) in v["data"].as_array().expect("data").iter().enumerate() {
        assert_eq!(d["index"], i);
        assert_eq!(d["object"], "embedding");
    }
    assert_eq!(v["usage"]["prompt_tokens"], 9, "the upstream's own count");
    let sent = up.embeddings.lock()[0].clone();
    assert_eq!(sent["model"], "text-embedding-3-small");
    assert_eq!(sent["encoding_format"], "float");
    assert_eq!(sent["user"], "u1");
}

#[tokio::test]
async fn repeated_inputs_are_served_from_the_cache() {
    let (url, up) = spawn_upstream(0).await;
    let base = start_embeddings(&url, |_| {}).await;
    let first = post(&base, "/v1/embeddings", &json!({"model": "m", "input": ["a", "bb"]})).await;
    assert_eq!(first.headers()["x-orchestrator-dedup"], "miss");

    let again = post(&base, "/v1/embeddings", &json!({"model": "m", "input": ["a", "bb"]})).await;
    assert_eq!(again.headers()["x-orchestrator-dedup"], "cached");
    assert_eq!(again.headers()["x-orchestrator-cached-inputs"], "2");
    assert_eq!(up.embedding_calls(), 1);

    // A batch with one new input sends only that one upstream.
    let mixed = post(&base, "/v1/embeddings", &json!({"model": "m", "input": ["bb", "cccc", "a"]})).await;
    assert_eq!(mixed.headers()["x-orchestrator-dedup"], "partial");
    assert_eq!(mixed.headers()["x-orchestrator-cached-inputs"], "2");
    let v: Value = mixed.json().await.expect("json");
    assert_eq!(vectors(&v), vec![vec![2.0, 1.0], vec![4.0, 1.0], vec![1.0, 1.0]]);
    assert_eq!(up.embedding_calls(), 2);
    assert_eq!(up.embeddings.lock()[1]["input"], json!(["cccc"]));

    // A different model or dimensions is a different cache entry.
    let other = post(&base, "/v1/embeddings", &json!({"model": "m2", "input": "a"})).await;
    assert_eq!(other.headers()["x-orchestrator-dedup"], "miss");
    let dims = post(&base, "/v1/embeddings", &json!({"model": "m", "input": "a", "dimensions": 4})).await;
    assert_eq!(dims.headers()["x-orchestrator-dedup"], "miss");
    assert_eq!(vectors(&dims.json::<Value>().await.expect("json"))[0].len(), 4);
    assert_eq!(up.embeddings.lock()[3]["dimensions"], 4);
}

#[tokio::test]
async fn duplicate_inputs_in_one_batch_are_embedded_once() {
    let (url, up) = spawn_upstream(0).await;
    let base = start_embeddings(&url, |_| {}).await;
    let v: Value = post(&base, "/v1/embeddings", &json!({"model": "m", "input": ["x", "yy", "x", "x"]}))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(up.embedded_inputs(), vec![json!("x"), json!("yy")]);
    assert_eq!(vectors(&v), vec![vec![1.0, 1.0], vec![2.0, 1.0], vec![1.0, 1.0], vec![1.0, 1.0]]);
}

#[tokio::test]
async fn concurrent_identical_inputs_share_one_upstream_call() {
    let (url, up) = spawn_upstream(300).await;
    // Window 0: nothing is reused after a call completes, so any sharing
    // here is in-flight sharing.
    let base = start_embeddings(&url, |c| c.dedup_window_secs = 0).await;
    let body = json!({"model": "m", "input": ["same text", "also same"]});
    let mut tasks = Vec::new();
    for _ in 0..5 {
        let (base, body) = (base.clone(), body.clone());
        tasks.push(tokio::spawn(async move {
            let r = post(&base, "/v1/embeddings", &body).await;
            let header = r.headers()["x-orchestrator-dedup"].to_str().unwrap_or("").to_string();
            (header, r.json::<Value>().await.expect("json"))
        }));
    }
    let mut headers = Vec::new();
    for t in tasks {
        let (h, v) = t.await.expect("task");
        assert_eq!(vectors(&v), vec![vec![9.0, 1.0], vec![9.0, 1.0]]);
        headers.push(h);
    }
    assert_eq!(up.embedding_calls(), 1, "{headers:?}");
    assert_eq!(headers.iter().filter(|h| *h == "miss").count(), 1, "{headers:?}");
    assert_eq!(headers.iter().filter(|h| *h == "joined").count(), 4, "{headers:?}");

    // And with window 0 the next request calls again.
    let later = post(&base, "/v1/embeddings", &body).await;
    assert_eq!(later.headers()["x-orchestrator-dedup"], "miss");
    assert_eq!(up.embedding_calls(), 2);
}

#[tokio::test]
async fn base64_encoding_and_token_inputs() {
    let (url, up) = spawn_upstream(0).await;
    let base = start_embeddings(&url, |_| {}).await;
    let v: Value = post(
        &base,
        "/v1/embeddings",
        &json!({"model": "m", "input": [[1, 2, 3], [4, 5]], "encoding_format": "base64"}),
    )
    .await
    .json()
    .await
    .expect("json");
    // [3.0, 2.0] as little-endian f32: 00 00 40 40 00 00 00 40
    assert_eq!(v["data"][0]["embedding"], "AABAQAAAAEA=");
    // [2.0, 2.0]: 00 00 00 40 00 00 00 40
    assert_eq!(v["data"][1]["embedding"], "AAAAQAAAAEA=");
    assert_eq!(up.embeddings.lock()[0]["input"], json!([[1, 2, 3], [4, 5]]));
    assert_eq!(up.embeddings.lock()[0]["encoding_format"], "float", "always float upstream");

    // A single token array is one input.
    let one: Value = post(&base, "/v1/embeddings", &json!({"model": "m", "input": [7, 8, 9, 10]}))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(vectors(&one), vec![vec![4.0, 2.0]]);
}

#[tokio::test]
async fn embeddings_errors() {
    // No backend configured.
    let none = start(None, |_| {}).await;
    let r = post(&none, "/v1/embeddings", &json!({"model": "m", "input": "a"})).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(r.json::<Value>().await.expect("json")["error"]["code"], "embeddings_not_configured");

    let (url, up) = spawn_upstream(0).await;
    let base = start_embeddings(&url, |_| {}).await;
    for body in [
        json!({"model": "m", "input": []}),
        json!({"model": "m", "input": [""]}),
        json!({"model": "m", "input": ["a", 1]}),
        json!({"model": "m", "input": "a", "encoding_format": "hex"}),
        json!({"input": "a"}),
    ] {
        let r = post(&base, "/v1/embeddings", &body).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(up.embedding_calls(), 0);

    // An upstream 4xx comes back with its status and message and does not
    // trip the breaker.
    up.embedding_status.store(400, Ordering::SeqCst);
    for _ in 0..6 {
        let r = post(&base, "/v1/embeddings", &json!({"model": "m", "input": "bad"})).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(r.json::<Value>().await.expect("json")["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("mock failure 400"));
    }
    // 5xx failures do: after five the breaker opens and answers 503 at once.
    up.embedding_status.store(500, Ordering::SeqCst);
    for _ in 0..5 {
        let r = post(&base, "/v1/embeddings", &json!({"model": "m", "input": "down"})).await;
        assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    }
    let calls = up.embedding_calls();
    let r = post(&base, "/v1/embeddings", &json!({"model": "m", "input": "down"})).await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.headers().get("retry-after").is_some());
    assert_eq!(r.json::<Value>().await.expect("json")["error"]["code"], "circuit_open");
    assert_eq!(up.embedding_calls(), calls, "refused without calling the upstream");

    // Spend cap.
    let capped = start_embeddings(&url, |c| c.max_spend_usd = Some(0.0)).await;
    let r = post(&capped, "/v1/embeddings", &json!({"model": "m", "input": "a"})).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn embedding_models_follow_openai_routes() {
    let (url, up) = spawn_upstream(0).await;
    let routes = ModelRoutesConfig::new(vec![
        ModelRoute::new("text-embedding-*", RouteProvider::OpenAi).with_base_url(&url),
        ModelRoute::new("claude-*", RouteProvider::Echo),
    ]);
    let base = start(Some(routes), |_| {}).await;
    let r = post(&base, "/v1/embeddings", &json!({"model": "text-embedding-3-large", "input": "abc"})).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(vectors(&r.json::<Value>().await.expect("json")), vec![vec![3.0, 1.0]]);
    assert_eq!(up.embeddings.lock()[0]["model"], "text-embedding-3-large");

    // A route to a provider without embeddings, and no fallback backend.
    let r = post(&base, "/v1/embeddings", &json!({"model": "claude-x", "input": "abc"})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(r.json::<Value>().await.expect("json")["error"]["code"], "model_not_found");
}

#[derive(Debug)]
struct LenEmbedder {
    calls: AtomicUsize,
}

#[async_trait]
impl Embedder for LenEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, OrchestratorError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![text.chars().count() as f32, 0.0, 1.0])
    }
}

#[tokio::test]
async fn a_local_embedder_serves_embeddings() {
    let embedder = Arc::new(LenEmbedder { calls: AtomicUsize::new(0) });
    let backend = EmbeddingsBackend::from_embedder(embedder.clone(), "local-model");
    let base = start(None, move |c| c.embeddings = Some(backend)).await;
    let r = post(&base, "/v1/embeddings", &json!({"input": ["hello", "hi"]})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v: Value = r.json().await.expect("json");
    assert_eq!(v["model"], "local-model");
    assert_eq!(vectors(&v), vec![vec![5.0, 0.0, 1.0], vec![2.0, 0.0, 1.0]]);
    let again = post(&base, "/v1/embeddings", &json!({"input": "hello"})).await;
    assert_eq!(again.headers()["x-orchestrator-dedup"], "cached");
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 2);

    // Token arrays and dimensions need an HTTP backend.
    let r = post(&base, "/v1/embeddings", &json!({"input": [1, 2]})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = post(&base, "/v1/embeddings", &json!({"input": "x", "dimensions": 2})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn new_paths_use_openai_auth_errors() {
    let base = start(None, |c| c.api_key = Some("secret".into())).await;
    for path in ["/v1/responses", "/v1/embeddings"] {
        let r = post(&base, path, &json!({"model": "m", "input": "x"})).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{path}");
        let v: Value = r.json().await.expect("json");
        assert_eq!(v["error"]["code"], "invalid_api_key", "{path}");
    }
}
