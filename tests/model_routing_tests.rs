//! Per-request parameters and model routing on the OpenAI-compatible
//! endpoint (`POST /v1/chat/completions`, `GET /v1/models`).
//!
//! The upstream providers are mock HTTP servers (axum) that record every
//! request body, so the tests check what actually goes over the wire: the
//! real orchestrator server and pipelines run in between.
//!
//! Run with: `cargo test --features web-api --test model_routing_tests`
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "web-api")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode as AxumStatus;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

use tokio_prompt_orchestrator::web_api::{start_server, ServerConfig};
use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, ModelRoute, ModelRoutes, ModelRoutesConfig, ModelWorker,
    OpenAiWorker, PipelineOptions, RouteProvider,
};

// ============================================================================
// Mock upstream: OpenAI-compatible and Anthropic, recording request bodies
// ============================================================================

#[derive(Default)]
struct Upstream {
    bodies: parking_lot::Mutex<Vec<Value>>,
    calls: AtomicUsize,
    delay_ms: u64,
}

impl Upstream {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn last(&self) -> Value {
        self.bodies.lock().last().cloned().expect("the upstream got a request")
    }
}

async fn record(up: &Upstream, body: &Value) {
    up.calls.fetch_add(1, Ordering::SeqCst);
    up.bodies.lock().push(body.clone());
    if up.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(up.delay_ms)).await;
    }
}

/// `true` when `tool_choice` or `parallel_tool_calls` came without tools,
/// which OpenAI and Anthropic both refuse with 400.
fn tool_settings_without_tools(body: &Value) -> bool {
    let no_tools = body["tools"].as_array().is_none_or(|t| t.is_empty());
    no_tools && (body.get("tool_choice").is_some() || body.get("parallel_tool_calls").is_some())
}

/// `POST /v1/chat/completions`: answers with a tool call when tools were
/// sent, else `openai:<model>`. A temperature above 2, or tool settings
/// without tools, is refused with 400, like OpenAI does.
async fn openai_upstream(State(up): State<Arc<Upstream>>, Json(body): Json<Value>) -> axum::response::Response {
    record(&up, &body).await;
    if tool_settings_without_tools(&body) {
        return (
            AxumStatus::BAD_REQUEST,
            Json(json!({"error": {"message": "'tool_choice' is only allowed when 'tools' are specified", "type": "invalid_request_error"}})),
        )
            .into_response();
    }
    if body["temperature"].as_f64().is_some_and(|t| t > 2.0) {
        return (
            AxumStatus::BAD_REQUEST,
            Json(json!({"error": {"message": "temperature must be at most 2", "type": "invalid_request_error"}})),
        )
            .into_response();
    }
    let model = body["model"].as_str().unwrap_or("").to_string();
    let message = if body.get("tools").is_some() {
        json!({"role": "assistant", "content": null, "tool_calls": [{
            "id": "call_abc", "type": "function",
            "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
        }]})
    } else {
        json!({"role": "assistant", "content": format!("openai:{model}")})
    };
    let finish = if body.get("tools").is_some() { "tool_calls" } else { "stop" };
    Json(json!({
        "id": "chatcmpl-upstream",
        "object": "chat.completion",
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    }))
    .into_response()
}

/// `POST /v1/messages`: the Anthropic Messages API (400 for `tool_choice`
/// without tools).
async fn anthropic_upstream(State(up): State<Arc<Upstream>>, Json(body): Json<Value>) -> axum::response::Response {
    record(&up, &body).await;
    if tool_settings_without_tools(&body) {
        return (
            AxumStatus::BAD_REQUEST,
            Json(json!({"type": "error", "error": {"type": "invalid_request_error", "message": "tool_choice may only be specified while providing tools"}})),
        )
            .into_response();
    }
    let model = body["model"].as_str().unwrap_or("").to_string();
    if body.get("tools").is_some() {
        return Json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": model,
            "content": [
                {"type": "text", "text": "Checking."},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 21, "output_tokens": 9}
        }))
        .into_response();
    }
    Json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": model,
        "content": [{"type": "text", "text": format!("anthropic:{model}")}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3}
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
        .route("/v1/chat/completions", post(openai_upstream))
        .route("/v1/messages", post(anthropic_upstream))
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

/// Start the orchestrator: `worker` serves the default route, `routes` (if
/// any) the `[[models]]` table.
async fn start(worker: Arc<dyn ModelWorker>, routes: Option<ModelRoutesConfig>) -> String {
    let handles = spawn_pipeline(worker);
    let output_rx = handles.take_output_rx().await.expect("output receiver");
    let port = free_port();
    let model_routes = routes.map(|cfg| {
        Arc::new(ModelRoutes::spawn(&cfg, &PipelineOptions::default()).expect("routes spawn"))
    });
    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        provider: "echo".to_string(),
        model: "echo".to_string(),
        model_routes,
        ..ServerConfig::default()
    };
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

async fn post_chat(base: &str, body: &Value) -> reqwest::Response {
    client()
        .post(format!("{base}/v1/chat/completions"))
        .json(body)
        .send()
        .await
        .expect("request sent")
}

fn ask(model: &str, text: &str) -> Value {
    json!({"model": model, "temperature": 0, "messages": [{"role": "user", "content": text}]})
}

fn openai_route(name: &str, base_url: &str) -> ModelRoute {
    ModelRoute::new(name, RouteProvider::OpenAi).with_base_url(base_url)
}

// ============================================================================
// Per-request parameters reach the upstream
// ============================================================================

#[tokio::test]
async fn every_per_request_parameter_reaches_the_upstream_and_tool_calls_come_back() {
    let (url, up) = spawn_upstream(0).await;
    let base = start(
        Arc::new(EchoWorker::new()),
        Some(ModelRoutesConfig::new(vec![openai_route("gpt-*", &url)])),
    )
    .await;

    let messages = json!([
        {"role": "system", "content": "You are terse."},
        {"role": "user", "content": "Weather in Paris?"},
        {"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_0", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Rome\"}"}}
        ]},
        {"role": "tool", "tool_call_id": "call_0", "content": "{\"temp\":21}"},
        {"role": "user", "content": "And Paris?"}
    ]);
    let tools = json!([{"type": "function", "function": {
        "name": "get_weather", "description": "Current weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
    }}]);
    let response_format = json!({"type": "json_schema", "json_schema": {"name": "w", "schema": {"type": "object"}}});
    let body = json!({
        "model": "gpt-4o-mini",
        "messages": messages,
        "temperature": 0.3,
        "top_p": 0.9,
        "max_tokens": 77,
        "max_completion_tokens": 88,
        "stop": ["END", "STOP"],
        "seed": 42,
        "response_format": response_format,
        "tools": tools,
        "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
        "parallel_tool_calls": false,
        "user": "u-1",
        "some_future_field": {"ignored": true}
    });
    let resp = post_chat(&base, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.expect("json");

    let sent = up.last();
    assert_eq!(sent["model"], "gpt-4o-mini", "a glob route sends the client's model");
    assert_eq!(sent["messages"], messages, "messages go upstream exactly as written");
    assert_eq!(sent["temperature"], 0.3);
    assert_eq!(sent["top_p"], 0.9);
    assert_eq!(sent["max_tokens"], 77);
    assert_eq!(sent["max_completion_tokens"], 88);
    assert_eq!(sent["stop"], json!(["END", "STOP"]));
    assert_eq!(sent["seed"], 42);
    assert_eq!(sent["response_format"], response_format);
    assert_eq!(sent["tools"], tools);
    assert_eq!(sent["tool_choice"], json!({"type": "function", "function": {"name": "get_weather"}}));
    assert_eq!(sent["parallel_tool_calls"], false);
    assert!(sent.get("some_future_field").is_none(), "unknown fields are not forwarded: {sent}");
    assert!(sent.get("user").is_none() && sent.get("stream").is_none(), "{sent}");

    assert_eq!(v["model"], "gpt-4o-mini");
    let choice = &v["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert_eq!(choice["message"]["content"], Value::Null);
    assert_eq!(choice["message"]["tool_calls"][0]["id"], "call_abc");
    assert_eq!(choice["message"]["tool_calls"][0]["function"]["name"], "get_weather");
    assert_eq!(choice["message"]["tool_calls"][0]["function"]["arguments"], "{\"city\":\"Paris\"}");
    assert_eq!(v["usage"], json!({"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}));
}

#[tokio::test]
async fn streamed_tool_calls_arrive_as_a_tool_calls_delta() {
    let (url, _up) = spawn_upstream(0).await;
    let base = start(
        Arc::new(EchoWorker::new()),
        Some(ModelRoutesConfig::new(vec![openai_route("gpt-*", &url)])),
    )
    .await;
    let body = json!({
        "model": "gpt-4o", "stream": true,
        "messages": [{"role": "user", "content": "Weather?"}],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}]
    });
    let raw = post_chat(&base, &body).await.text().await.expect("body");
    let chunks: Vec<Value> = raw
        .lines()
        .filter_map(|l| l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:")))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).expect("json chunk"))
        .collect();
    let calls: Vec<&Value> = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"].get("tool_calls"))
        .collect();
    assert_eq!(calls.len(), 1, "{raw}");
    assert_eq!(calls[0][0]["index"], 0);
    assert_eq!(calls[0][0]["function"]["name"], "get_weather");
    assert!(chunks.iter().any(|c| c["choices"][0]["finish_reason"] == "tool_calls"), "{raw}");
}

#[tokio::test]
async fn default_route_sends_the_clients_settings_with_its_own_model_and_no_cap() {
    let (url, up) = spawn_upstream(0).await;
    // Today's single-provider setup: one OpenAI-compatible worker, no table.
    let worker = OpenAiWorker::with_key("configured-model", "sk-test").with_base_url(&url);
    let base = start(Arc::new(worker), None).await;

    let v: Value = post_chat(
        &base,
        &json!({"model": "whatever-the-sdk-sends", "temperature": 0.5, "max_tokens": 33, "seed": 9,
                "messages": [{"role": "user", "content": "one"}]}),
    )
    .await
    .json()
    .await
    .expect("json");
    let sent = up.last();
    assert_eq!(sent["model"], "configured-model", "the default route keeps its configured model");
    assert_eq!(sent["temperature"], 0.5);
    assert_eq!(sent["max_tokens"], 33);
    assert_eq!(sent["seed"], 9);
    assert_eq!(v["model"], "echo", "and reports the server's configured model, as before");
    assert_eq!(v["choices"][0]["message"]["content"], "openai:configured-model");

    // Nothing set by the client: nothing invented by the orchestrator.
    post_chat(&base, &json!({"messages": [{"role": "user", "content": "two"}]})).await;
    let sent = up.last();
    assert!(sent.get("max_tokens").is_none(), "no default reply cap: {sent}");
    assert!(sent.get("max_completion_tokens").is_none(), "{sent}");
    assert!(sent.get("temperature").is_none(), "no default temperature: {sent}");
    assert_eq!(up.calls(), 2);
}

// ============================================================================
// Routing
// ============================================================================

#[tokio::test]
async fn routing_sends_each_model_to_its_own_upstream() {
    let (openai_url, openai_up) = spawn_upstream(0).await;
    let (anthropic_url, anthropic_up) = spawn_upstream(0).await;
    // Only this test reads this variable.
    std::env::set_var("TPO_ROUTING_TEST_ANTHROPIC_KEY", "sk-ant-test");
    let table = ModelRoutesConfig::new(vec![
        openai_route("gpt-*", &openai_url),
        ModelRoute::new("claude-*", RouteProvider::Anthropic)
            .with_base_url(&anthropic_url)
            .with_api_key_env("TPO_ROUTING_TEST_ANTHROPIC_KEY"),
        ModelRoute::new("llama-*", RouteProvider::Echo),
        openai_route("fast", &openai_url).with_model("gpt-4o-mini"),
    ]);
    let base = start(Arc::new(EchoWorker::new()), Some(table)).await;

    let v: Value = post_chat(&base, &ask("gpt-4.1", "hi gpt")).await.json().await.expect("json");
    assert_eq!(v["choices"][0]["message"]["content"], "openai:gpt-4.1");
    assert_eq!((openai_up.calls(), anthropic_up.calls()), (1, 0));

    let v: Value = post_chat(
        &base,
        &json!({"model": "claude-sonnet-4-5", "temperature": 0, "stop": "END",
                "messages": [{"role": "system", "content": "Be brief."}, {"role": "user", "content": "hi claude"}]}),
    )
    .await
    .json()
    .await
    .expect("json");
    assert_eq!(v["choices"][0]["message"]["content"], "anthropic:claude-sonnet-4-5");
    assert_eq!(v["model"], "claude-sonnet-4-5");
    assert_eq!(v["usage"]["prompt_tokens"], 5, "the provider's own usage is reported");
    assert_eq!((openai_up.calls(), anthropic_up.calls()), (1, 1));
    let sent = anthropic_up.last();
    assert_eq!(sent["model"], "claude-sonnet-4-5");
    assert_eq!(sent["system"], "Be brief.");
    assert_eq!(sent["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "hi claude"}]}]));
    assert_eq!(sent["max_tokens"], 4096, "Anthropic needs a cap; the route default is 4096");
    assert_eq!(sent["stop_sequences"], json!(["END"]));
    assert_eq!(sent["temperature"], 0.0);

    let v: Value = post_chat(&base, &ask("llama-3.1-8b", "hi llama")).await.json().await.expect("json");
    assert_eq!(v["choices"][0]["message"]["content"], "hi llama", "echo route");
    assert_eq!(v["model"], "llama-3.1-8b");

    let v: Value = post_chat(&base, &ask("FAST", "hi alias")).await.json().await.expect("json");
    assert_eq!(v["choices"][0]["message"]["content"], "openai:gpt-4o-mini", "a route's model overrides");
    assert_eq!(openai_up.last()["model"], "gpt-4o-mini");

    // No route matches: the default route (the server's own pipeline).
    let v: Value = post_chat(&base, &ask("mistral-large", "hi default")).await.json().await.expect("json");
    assert_eq!(v["choices"][0]["message"]["content"], "hi default");
    assert_eq!(v["model"], "echo");
    assert_eq!((openai_up.calls(), anthropic_up.calls()), (2, 1));
}

#[tokio::test]
async fn tool_settings_without_tools_are_not_forwarded() {
    // SDK wrappers often send tools: [] with tool_choice and
    // parallel_tool_calls; OpenAI rejects either one without tools.
    let (url, up) = spawn_upstream(0).await;
    std::env::set_var("TPO_ROUTING_TEST_ANTHROPIC_KEY_3", "sk-ant-test");
    let table = ModelRoutesConfig::new(vec![
        openai_route("gpt-*", &url),
        ModelRoute::new("claude-*", RouteProvider::Anthropic)
            .with_base_url(&url)
            .with_api_key_env("TPO_ROUTING_TEST_ANTHROPIC_KEY_3"),
    ]);
    let base = start(Arc::new(EchoWorker::new()), Some(table)).await;
    for model in ["gpt-4o-mini", "claude-sonnet-4"] {
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": format!("hi {model}")}],
            "tools": [],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        });
        let r = post_chat(&base, &body).await;
        assert_eq!(r.status(), StatusCode::OK, "{model}");
        let sent = up.last();
        for key in ["tools", "tool_choice", "parallel_tool_calls"] {
            assert!(sent.get(key).is_none(), "{model}: {key} forwarded without tools: {sent}");
        }
    }
}

#[tokio::test]
async fn anthropic_route_translates_tools_both_ways() {
    let (url, up) = spawn_upstream(0).await;
    std::env::set_var("TPO_ROUTING_TEST_ANTHROPIC_KEY_2", "sk-ant-test");
    let table = ModelRoutesConfig::new(vec![ModelRoute::new("claude-*", RouteProvider::Anthropic)
        .with_base_url(&url)
        .with_api_key_env("TPO_ROUTING_TEST_ANTHROPIC_KEY_2")
        .with_max_tokens(1000)]);
    let base = start(Arc::new(EchoWorker::new()), Some(table)).await;
    let body = json!({
        "model": "claude-opus-4",
        "max_completion_tokens": 300,
        "messages": [
            {"role": "user", "content": "Weather in Rome?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "toolu_0", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Rome\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "toolu_0", "content": "21C"},
            {"role": "user", "content": "And Paris?"}
        ],
        "tools": [{"type": "function", "function": {"name": "get_weather", "description": "Weather",
                   "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}],
        "tool_choice": "required",
        "parallel_tool_calls": false
    });
    let v: Value = post_chat(&base, &body).await.json().await.expect("json");
    let sent = up.last();
    assert_eq!(sent["max_tokens"], 300, "the client's cap wins over the route's");
    assert_eq!(sent["tools"][0]["name"], "get_weather");
    assert_eq!(sent["tools"][0]["input_schema"]["properties"]["city"]["type"], "string");
    assert_eq!(sent["tool_choice"], json!({"type": "any", "disable_parallel_tool_use": true}));
    let msgs = sent["messages"].as_array().expect("messages");
    assert_eq!(msgs[1]["content"][0], json!({"type": "tool_use", "id": "toolu_0", "name": "get_weather", "input": {"city": "Rome"}}));
    assert_eq!(msgs[2], json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_0", "content": "21C"}]}));

    let choice = &v["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert_eq!(choice["message"]["content"], "Checking.");
    assert_eq!(choice["message"]["tool_calls"][0]["id"], "toolu_1");
    assert_eq!(choice["message"]["tool_calls"][0]["type"], "function");
    let args: Value = serde_json::from_str(
        choice["message"]["tool_calls"][0]["function"]["arguments"].as_str().expect("arguments string"),
    )
    .expect("arguments are JSON");
    assert_eq!(args, json!({"city": "Paris"}));

    // response_format cannot be honoured by the Messages API: 400 up front.
    let mut bad = body.clone();
    bad["response_format"] = json!({"type": "json_object"});
    let calls_before = up.calls();
    let r = post_chat(&base, &bad).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let e: Value = r.json().await.expect("json");
    assert_eq!(e["error"]["param"], "response_format");
    assert_eq!(up.calls(), calls_before);
}

#[tokio::test]
async fn unknown_model_is_a_400_listing_the_configured_models_without_a_default_route() {
    let (url, up) = spawn_upstream(0).await;
    let mut table = ModelRoutesConfig::new(vec![
        openai_route("gpt-*", &url),
        ModelRoute::new("llama-*", RouteProvider::Echo),
    ]);
    table.default_route = false;
    let base = start(Arc::new(EchoWorker::new()), Some(table)).await;

    let r = post_chat(&base, &ask("claude-sonnet-4-5", "hi")).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v: Value = r.json().await.expect("json");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "model_not_found");
    assert_eq!(v["error"]["param"], "model");
    let msg = v["error"]["message"].as_str().expect("message");
    assert!(msg.contains("claude-sonnet-4-5") && msg.contains("gpt-*") && msg.contains("llama-*"), "{msg}");

    // No model at all is the same 400.
    let r = post_chat(&base, &json!({"messages": [{"role": "user", "content": "hi"}]})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // The Anthropic endpoint answers in Anthropic's shape.
    let r = client()
        .post(format!("{base}/v1/messages"))
        .json(&json!({"model": "claude-x", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("sent");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v: Value = r.json().await.expect("json");
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(up.calls(), 0);

    // A configured model still works.
    assert_eq!(post_chat(&base, &ask("gpt-4o", "hi")).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn models_endpoint_lists_the_configured_routes() {
    let (url, _up) = spawn_upstream(0).await;
    let table = ModelRoutesConfig::new(vec![
        openai_route("gpt-*", &url),
        ModelRoute::new("llama-3.1-8b", RouteProvider::LlamaCpp).with_base_url("http://127.0.0.1:9"),
    ]);
    let base = start(Arc::new(EchoWorker::new()), Some(table.clone())).await;
    let v: Value = client().get(format!("{base}/v1/models")).send().await.expect("sent").json().await.expect("json");
    let ids: Vec<(&str, &str)> = v["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|m| (m["id"].as_str().unwrap_or(""), m["owned_by"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(ids, vec![("echo", "echo"), ("gpt-*", "openai"), ("llama-3.1-8b", "llama_cpp")]);

    let mut no_default = table;
    no_default.default_route = false;
    let base = start(Arc::new(EchoWorker::new()), Some(no_default)).await;
    let v: Value = client().get(format!("{base}/v1/models")).send().await.expect("sent").json().await.expect("json");
    assert_eq!(v["data"].as_array().map(Vec::len), Some(2), "{v}");
}

// ============================================================================
// Deduplication over the full request
// ============================================================================

#[tokio::test]
async fn different_settings_never_share_an_answer() {
    let (url, up) = spawn_upstream(0).await;
    let base = start(
        Arc::new(EchoWorker::new()),
        Some(ModelRoutesConfig::new(vec![openai_route("gpt-*", &url)])),
    )
    .await;
    let msgs = json!([{"role": "user", "content": "same words"}]);
    let variants = [
        json!({"model": "gpt-4o", "messages": msgs, "temperature": 0}),
        json!({"model": "gpt-4o-mini", "messages": msgs, "temperature": 0}),
        json!({"model": "gpt-4o", "messages": msgs, "temperature": 0, "max_tokens": 10}),
        json!({"model": "gpt-4o", "messages": msgs, "temperature": 0, "top_p": 0.5}),
        json!({"model": "gpt-4o", "messages": msgs, "temperature": 0, "stop": "x"}),
        json!({"model": "gpt-4o", "messages": msgs, "temperature": 0, "seed": 1}),
        json!({"model": "gpt-4o", "messages": msgs, "temperature": 0, "response_format": {"type": "json_object"}}),
        json!({"model": "gpt-4o", "messages": msgs, "temperature": 0,
               "tools": [{"type": "function", "function": {"name": "f"}}]}),
        json!({"model": "gpt-4o", "messages": msgs, "temperature": 0,
               "tools": [{"type": "function", "function": {"name": "f"}}], "tool_choice": "required"}),
    ];
    for (i, body) in variants.iter().enumerate() {
        let r = post_chat(&base, body).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["x-orchestrator-dedup"], "miss", "variant {i} must not reuse an answer");
    }
    assert_eq!(up.calls(), variants.len());

    // The same request with its fields in another order is the same request.
    let reordered = json!({"temperature": 0, "response_format": {"type": "json_object"}, "messages": msgs, "model": "gpt-4o"});
    let r = post_chat(&base, &reordered).await;
    assert_eq!(r.headers()["x-orchestrator-dedup"], "cached");
    assert_eq!(up.calls(), variants.len());
}

#[tokio::test]
async fn the_same_sampled_prompt_twice_is_paid_for_once_unless_the_client_asks_for_fresh() {
    let (url, up) = spawn_upstream(300).await;
    let base = start(
        Arc::new(EchoWorker::new()),
        Some(ModelRoutesConfig::new(vec![openai_route("gpt-*", &url)])),
    )
    .await;
    let sampled = json!({"model": "gpt-4o", "temperature": 0.8, "messages": [{"role": "user", "content": "a poem"}]});

    let resps = futures::future::join_all((0..4).map(|_| post_chat(&base, &sampled))).await;
    let mut outcomes: Vec<String> = resps
        .iter()
        .map(|r| r.headers()["x-orchestrator-dedup"].to_str().expect("ascii").to_string())
        .collect();
    outcomes.sort();
    assert_eq!(outcomes, vec!["joined", "joined", "joined", "miss"]);
    assert_eq!(up.calls(), 1, "identical requests in flight share one call");

    // Finished: the same sampled request again is a duplicate, not a new
    // question, so it is answered from the cache (3.0 default).
    let again = post_chat(&base, &sampled).await;
    assert_eq!(again.headers()["x-orchestrator-dedup"], "cached");
    assert_eq!(up.calls(), 1);

    // A fresh sample is an explicit choice, per request.
    for (name, value) in [("x-orchestrator-fresh", "1"), ("cache-control", "no-cache")] {
        let fresh = client()
            .post(format!("{base}/v1/chat/completions"))
            .header(name, value)
            .json(&sampled)
            .send()
            .await
            .expect("request sent");
        assert_eq!(fresh.headers()["x-orchestrator-dedup"], "miss", "{name}");
    }
    assert_eq!(up.calls(), 3);
}

#[tokio::test]
async fn cache_sampled_false_keeps_the_old_rule() {
    let (url, up) = spawn_upstream(0).await;
    let handles = spawn_pipeline(Arc::new(EchoWorker::new()));
    let output_rx = handles.take_output_rx().await.expect("output receiver");
    let port = free_port();
    let cfg = ModelRoutesConfig::new(vec![openai_route("gpt-*", &url)]);
    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        provider: "echo".to_string(),
        model: "echo".to_string(),
        model_routes: Some(Arc::new(
            ModelRoutes::spawn(&cfg, &PipelineOptions::default()).expect("routes"),
        )),
        cache_sampled: false,
        ..ServerConfig::default()
    };
    let (tx, dlq, cb) = (
        handles.input_tx.clone(),
        handles.dlq.clone(),
        handles.circuit_breaker.clone(),
    );
    tokio::spawn(async move {
        let _handles = handles;
        let _ = start_server(config, tx, output_rx, dlq, cb, None).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if client().get(format!("{base}/live")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let sampled = json!({"model": "gpt-4o", "temperature": 0.8, "messages": [{"role": "user", "content": "a poem"}]});
    assert_eq!(
        post_chat(&base, &sampled).await.headers()["x-orchestrator-dedup"],
        "miss"
    );
    assert_eq!(
        post_chat(&base, &sampled).await.headers()["x-orchestrator-dedup"],
        "miss"
    );
    assert_eq!(up.calls(), 2);

    // With a seed the answer is reproducible, so it is cached either way.
    let mut seeded = sampled.clone();
    seeded["seed"] = json!(5);
    assert_eq!(post_chat(&base, &seeded).await.headers()["x-orchestrator-dedup"], "miss");
    assert_eq!(post_chat(&base, &seeded).await.headers()["x-orchestrator-dedup"], "cached");
    assert_eq!(up.calls(), 3);
}

// ============================================================================
// Provider rejections
// ============================================================================

#[tokio::test]
async fn a_provider_400_comes_back_as_400_and_does_not_open_the_breaker() {
    let (url, up) = spawn_upstream(0).await;
    let base = start(
        Arc::new(EchoWorker::new()),
        Some(ModelRoutesConfig::new(vec![openai_route("gpt-*", &url)])),
    )
    .await;
    // More rejections than the breaker's failure threshold.
    for i in 0..8 {
        let body = json!({"model": "gpt-4o", "temperature": 5, "seed": i,
                          "messages": [{"role": "user", "content": "x"}]});
        let r = post_chat(&base, &body).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(r.headers()["x-should-retry"], "false");
        let v: Value = r.json().await.expect("json");
        assert_eq!(v["error"]["code"], "upstream_rejected");
        assert!(v["error"]["message"].as_str().is_some_and(|m| m.contains("temperature must be at most 2")), "{v}");
    }
    assert_eq!(up.calls(), 8, "a rejected request is not retried");
    let ok = post_chat(&base, &ask("gpt-4o", "fine")).await;
    assert_eq!(ok.status(), StatusCode::OK, "the breaker stayed closed");
}
