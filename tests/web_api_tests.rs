//! Integration tests for `src/web_api.rs`
//!
//! Tests the REST API endpoints and public type serialization for the web API
//! module. Integration tests spawn a real HTTP server on a unique port and
//! exercise it via `reqwest`. The pipeline channel is captured so tests can
//! verify requests are forwarded without needing a full model backend.
//!
//! All tests require the `web-api` Cargo feature.

#![cfg(feature = "web-api")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use tokio_prompt_orchestrator::web_api::{
    InferRequest, InferResponse, RequestStatus, ServerConfig,
};
use tokio_prompt_orchestrator::PromptRequest;

// ============================================================================
// Test Infrastructure
// ============================================================================

/// Atomic counter for unique per-test port allocation.
/// Starts high to avoid collisions with common services.
static PORT_COUNTER: AtomicU16 = AtomicU16::new(29200);

fn next_port() -> u16 {
    PORT_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Spawn a web API server in the background and return its base URL plus the
/// pipeline receiver so tests can verify forwarded requests.
async fn spawn_server() -> (String, mpsc::Receiver<PromptRequest>) {
    let port = next_port();
    let (tx, rx) = mpsc::channel::<PromptRequest>(64);
    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        max_request_size: 1024 * 1024,
        timeout_seconds: 2,
    };
    tokio::spawn(async move {
        let _ = tokio_prompt_orchestrator::web_api::start_server(config, tx).await;
    });
    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (format!("http://127.0.0.1:{port}"), rx)
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client must build in tests")
}

// ============================================================================
// ServerConfig — Default Values
// ============================================================================

#[test]
fn test_server_config_default_host_is_all_interfaces() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.host, "0.0.0.0");
}

#[test]
fn test_server_config_default_port_is_8080() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.port, 8080);
}

#[test]
fn test_server_config_default_max_request_size_is_10mb() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.max_request_size, 10 * 1024 * 1024);
}

#[test]
fn test_server_config_default_timeout_is_300_seconds() {
    let cfg = ServerConfig::default();
    assert_eq!(cfg.timeout_seconds, 300);
}

// ============================================================================
// ServerConfig — Serialization
// ============================================================================

#[test]
fn test_server_config_serializes_to_json() {
    let cfg = ServerConfig::default();
    let json = serde_json::to_value(&cfg).expect("ServerConfig must serialize");
    assert_eq!(json["host"], "0.0.0.0");
    assert_eq!(json["port"], 8080);
    assert_eq!(json["max_request_size"], 10 * 1024 * 1024);
    assert_eq!(json["timeout_seconds"], 300);
}

#[test]
fn test_server_config_deserializes_from_json() {
    let json = json!({
        "host": "127.0.0.1",
        "port": 3000,
        "max_request_size": 1024,
        "timeout_seconds": 60
    });
    let cfg: ServerConfig = serde_json::from_value(json).expect("must deserialize");
    assert_eq!(cfg.host, "127.0.0.1");
    assert_eq!(cfg.port, 3000);
    assert_eq!(cfg.max_request_size, 1024);
    assert_eq!(cfg.timeout_seconds, 60);
}

#[test]
fn test_server_config_round_trip_serialization() {
    let original = ServerConfig {
        host: "10.0.0.1".to_string(),
        port: 9090,
        max_request_size: 2048,
        timeout_seconds: 120,
    };
    let json = serde_json::to_value(&original).expect("serialize");
    let restored: ServerConfig = serde_json::from_value(json).expect("deserialize");
    assert_eq!(restored.host, original.host);
    assert_eq!(restored.port, original.port);
    assert_eq!(restored.max_request_size, original.max_request_size);
    assert_eq!(restored.timeout_seconds, original.timeout_seconds);
}

#[test]
fn test_server_config_clone_produces_equal_values() {
    let cfg = ServerConfig::default();
    let cloned = cfg.clone();
    assert_eq!(cloned.host, cfg.host);
    assert_eq!(cloned.port, cfg.port);
    assert_eq!(cloned.max_request_size, cfg.max_request_size);
    assert_eq!(cloned.timeout_seconds, cfg.timeout_seconds);
}

#[test]
fn test_server_config_debug_format_is_non_empty() {
    let cfg = ServerConfig::default();
    let debug = format!("{:?}", cfg);
    assert!(!debug.is_empty(), "Debug format should produce output");
    assert!(
        debug.contains("ServerConfig"),
        "Debug should include type name"
    );
}

// ============================================================================
// InferRequest — Deserialization
// ============================================================================

#[test]
fn test_infer_request_minimal_deserializes() {
    let json = json!({"prompt": "hello"});
    let req: InferRequest = serde_json::from_value(json).expect("must deserialize");
    assert_eq!(req.prompt, "hello");
    assert!(req.session_id.is_none());
    assert!(req.metadata.is_empty());
    assert!(!req.stream);
}

#[test]
fn test_infer_request_full_deserializes() {
    let json = json!({
        "prompt": "hello world",
        "session_id": "sess-1",
        "metadata": {"key": "value"},
        "stream": true
    });
    let req: InferRequest = serde_json::from_value(json).expect("must deserialize");
    assert_eq!(req.prompt, "hello world");
    assert_eq!(req.session_id.as_deref(), Some("sess-1"));
    assert_eq!(req.metadata.get("key").map(|s| s.as_str()), Some("value"));
    assert!(req.stream);
}

#[test]
fn test_infer_request_missing_prompt_fails() {
    let json = json!({"session_id": "s1"});
    let result = serde_json::from_value::<InferRequest>(json);
    assert!(
        result.is_err(),
        "Missing prompt should fail deserialization"
    );
}

#[test]
fn test_infer_request_empty_prompt_deserializes() {
    let json = json!({"prompt": ""});
    let req: InferRequest = serde_json::from_value(json).expect("empty prompt is valid");
    assert_eq!(req.prompt, "");
}

#[test]
fn test_infer_request_serializes_round_trip() {
    let req = InferRequest {
        prompt: "test prompt".to_string(),
        session_id: Some("sess-42".to_string()),
        metadata: {
            let mut m = HashMap::new();
            m.insert("client".to_string(), "test".to_string());
            m
        },
        stream: false,
    };
    let json = serde_json::to_value(&req).expect("must serialize");
    let deserialized: InferRequest = serde_json::from_value(json).expect("must deserialize");
    assert_eq!(deserialized.prompt, req.prompt);
    assert_eq!(deserialized.session_id, req.session_id);
}

#[test]
fn test_infer_request_multiple_metadata_keys() {
    let json = json!({
        "prompt": "test",
        "metadata": {"a": "1", "b": "2", "c": "3"}
    });
    let req: InferRequest = serde_json::from_value(json).expect("must deserialize");
    assert_eq!(req.metadata.len(), 3);
    assert_eq!(req.metadata["a"], "1");
    assert_eq!(req.metadata["b"], "2");
    assert_eq!(req.metadata["c"], "3");
}

#[test]
fn test_infer_request_stream_defaults_to_false() {
    let json = json!({"prompt": "test"});
    let req: InferRequest = serde_json::from_value(json).expect("must deserialize");
    assert!(!req.stream, "stream should default to false");
}

#[test]
fn test_infer_request_clone_preserves_all_fields() {
    let req = InferRequest {
        prompt: "cloneable".to_string(),
        session_id: Some("s1".to_string()),
        metadata: {
            let mut m = HashMap::new();
            m.insert("k".to_string(), "v".to_string());
            m
        },
        stream: true,
    };
    let cloned = req.clone();
    assert_eq!(cloned.prompt, req.prompt);
    assert_eq!(cloned.session_id, req.session_id);
    assert_eq!(cloned.metadata, req.metadata);
    assert_eq!(cloned.stream, req.stream);
}

// ============================================================================
// InferResponse — Serialization
// ============================================================================

#[test]
fn test_infer_response_completed_includes_result() {
    let resp = InferResponse {
        request_id: "req-1".to_string(),
        status: RequestStatus::Completed,
        result: Some("output text".to_string()),
        error: None,
    };
    let json = serde_json::to_value(&resp).expect("must serialize");
    assert_eq!(json["result"], "output text");
    assert!(json.get("error").is_none(), "None error should be skipped");
}

#[test]
fn test_infer_response_failed_includes_error() {
    let resp = InferResponse {
        request_id: "req-2".to_string(),
        status: RequestStatus::Failed,
        result: None,
        error: Some("something broke".to_string()),
    };
    let json = serde_json::to_value(&resp).expect("must serialize");
    assert_eq!(json["error"], "something broke");
    assert!(
        json.get("result").is_none(),
        "None result should be skipped"
    );
}

#[test]
fn test_infer_response_pending_skips_both_optionals() {
    let resp = InferResponse {
        request_id: "req-3".to_string(),
        status: RequestStatus::Pending,
        result: None,
        error: None,
    };
    let json = serde_json::to_value(&resp).expect("must serialize");
    assert!(json.get("result").is_none());
    assert!(json.get("error").is_none());
}

#[test]
fn test_infer_response_deserializes_from_json() {
    let json = json!({
        "request_id": "abc",
        "status": "completed",
        "result": "hello"
    });
    let resp: InferResponse = serde_json::from_value(json).expect("must deserialize");
    assert_eq!(resp.request_id, "abc");
    assert_eq!(resp.status, RequestStatus::Completed);
    assert_eq!(resp.result.as_deref(), Some("hello"));
}

#[test]
fn test_infer_response_processing_has_no_result_or_error() {
    let resp = InferResponse {
        request_id: "req-4".to_string(),
        status: RequestStatus::Processing,
        result: None,
        error: None,
    };
    let json = serde_json::to_value(&resp).expect("must serialize");
    assert_eq!(json["status"], "processing");
    assert_eq!(json["request_id"], "req-4");
    assert!(json.get("result").is_none());
    assert!(json.get("error").is_none());
}

#[test]
fn test_infer_response_clone_preserves_all_fields() {
    let resp = InferResponse {
        request_id: "req-clone".to_string(),
        status: RequestStatus::Completed,
        result: Some("result".to_string()),
        error: None,
    };
    let cloned = resp.clone();
    assert_eq!(cloned.request_id, resp.request_id);
    assert_eq!(cloned.status, resp.status);
    assert_eq!(cloned.result, resp.result);
    assert_eq!(cloned.error, resp.error);
}

// ============================================================================
// RequestStatus — Serialization
// ============================================================================

#[test]
fn test_request_status_serializes_as_lowercase() {
    let cases = vec![
        (RequestStatus::Pending, "pending"),
        (RequestStatus::Processing, "processing"),
        (RequestStatus::Completed, "completed"),
        (RequestStatus::Failed, "failed"),
        (RequestStatus::Timeout, "timeout"),
    ];
    for (status, expected) in cases {
        let json = serde_json::to_value(&status).expect("must serialize");
        assert_eq!(
            json, expected,
            "RequestStatus::{status:?} should serialize to {expected}"
        );
    }
}

#[test]
fn test_request_status_deserializes_from_lowercase() {
    let cases = vec![
        ("pending", RequestStatus::Pending),
        ("processing", RequestStatus::Processing),
        ("completed", RequestStatus::Completed),
        ("failed", RequestStatus::Failed),
        ("timeout", RequestStatus::Timeout),
    ];
    for (input, expected) in cases {
        let status: RequestStatus = serde_json::from_value(json!(input)).expect("must deserialize");
        assert_eq!(status, expected);
    }
}

#[test]
fn test_request_status_invalid_value_fails_deserialization() {
    let result = serde_json::from_value::<RequestStatus>(json!("INVALID"));
    assert!(result.is_err());
}

#[test]
fn test_request_status_round_trips_all_variants() {
    let variants = vec![
        RequestStatus::Pending,
        RequestStatus::Processing,
        RequestStatus::Completed,
        RequestStatus::Failed,
        RequestStatus::Timeout,
    ];
    for variant in variants {
        let json = serde_json::to_value(&variant).expect("serialize");
        let back: RequestStatus = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, variant);
    }
}

#[test]
fn test_request_status_partial_eq_same_variant() {
    assert_eq!(RequestStatus::Pending, RequestStatus::Pending);
    assert_eq!(RequestStatus::Completed, RequestStatus::Completed);
}

#[test]
fn test_request_status_partial_eq_different_variant() {
    assert_ne!(RequestStatus::Pending, RequestStatus::Completed);
    assert_ne!(RequestStatus::Processing, RequestStatus::Failed);
    assert_ne!(RequestStatus::Timeout, RequestStatus::Pending);
}

// ============================================================================
// Health Endpoint
// ============================================================================

#[tokio::test]
async fn test_health_endpoint_returns_200() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("request must succeed");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_health_endpoint_returns_healthy_status() {
    let (base, _rx) = spawn_server().await;
    let body: Value = client()
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(body["status"], "healthy");
}

#[tokio::test]
async fn test_health_endpoint_includes_version() {
    let (base, _rx) = spawn_server().await;
    let body: Value = client()
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert!(
        body.get("version").is_some(),
        "Health response must include version"
    );
    assert_eq!(body["version"], "0.1.0");
}

#[tokio::test]
async fn test_health_endpoint_response_is_json_content_type() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("send");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("application/json"),
        "Health endpoint should return JSON, got content-type: {ct}"
    );
}

// ============================================================================
// Metrics Endpoint
// ============================================================================

#[tokio::test]
async fn test_metrics_endpoint_returns_200() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .get(format!("{base}/metrics"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_metrics_endpoint_returns_valid_utf8() {
    let (base, _rx) = spawn_server().await;
    let text = client()
        .get(format!("{base}/metrics"))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");
    assert!(
        std::str::from_utf8(text.as_bytes()).is_ok(),
        "Metrics output must be valid UTF-8"
    );
}

// ============================================================================
// Infer Endpoint — Happy Path
// ============================================================================

#[tokio::test]
async fn test_infer_endpoint_returns_200() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "hello"}))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_infer_endpoint_returns_request_id() {
    let (base, _rx) = spawn_server().await;
    let body: InferResponse = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test"}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert!(!body.request_id.is_empty(), "request_id must be present");
}

#[tokio::test]
async fn test_infer_endpoint_request_id_is_valid_uuid() {
    let (base, _rx) = spawn_server().await;
    let body: InferResponse = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test"}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert!(
        uuid::Uuid::parse_str(&body.request_id).is_ok(),
        "request_id '{}' must be a valid UUID",
        body.request_id
    );
}

#[tokio::test]
async fn test_infer_endpoint_returns_processing_status() {
    let (base, _rx) = spawn_server().await;
    let body: InferResponse = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test"}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(body.status, RequestStatus::Processing);
}

#[tokio::test]
async fn test_infer_endpoint_result_is_none_when_processing() {
    let (base, _rx) = spawn_server().await;
    let body: InferResponse = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test"}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert!(
        body.result.is_none(),
        "result should be None while processing"
    );
}

#[tokio::test]
async fn test_infer_endpoint_error_is_none_when_processing() {
    let (base, _rx) = spawn_server().await;
    let body: InferResponse = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test"}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert!(
        body.error.is_none(),
        "error should be None while processing"
    );
}

#[tokio::test]
async fn test_infer_endpoint_response_is_json_content_type() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test"}))
        .send()
        .await
        .expect("send");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("application/json"),
        "Infer response should be JSON, got: {ct}"
    );
}

// ============================================================================
// Infer Endpoint — Pipeline Forwarding
// ============================================================================

#[tokio::test]
async fn test_infer_endpoint_forwards_request_to_pipeline() {
    let (base, mut rx) = spawn_server().await;
    let _ = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "pipeline test"}))
        .send()
        .await
        .expect("send");

    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("should receive within timeout")
        .expect("channel should not be closed");
    assert_eq!(received.input, "pipeline test");
}

#[tokio::test]
async fn test_infer_endpoint_with_session_id_uses_provided_session() {
    let (base, mut rx) = spawn_server().await;
    let _ = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test", "session_id": "my-session"}))
        .send()
        .await
        .expect("send");

    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("channel open");
    assert_eq!(received.session.as_str(), "my-session");
}

#[tokio::test]
async fn test_infer_endpoint_without_session_id_generates_web_prefixed_session() {
    let (base, mut rx) = spawn_server().await;
    let _ = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test"}))
        .send()
        .await
        .expect("send");

    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("channel open");
    assert!(
        received.session.as_str().starts_with("web-"),
        "Auto-generated session should start with 'web-', got '{}'",
        received.session.as_str()
    );
}

#[tokio::test]
async fn test_infer_endpoint_with_metadata_forwards_metadata() {
    let (base, mut rx) = spawn_server().await;
    let _ = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({
            "prompt": "test",
            "metadata": {"client": "test-suite", "version": "1.0"}
        }))
        .send()
        .await
        .expect("send");

    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("channel open");
    assert_eq!(
        received.meta.get("client").map(|s| s.as_str()),
        Some("test-suite")
    );
    assert_eq!(
        received.meta.get("version").map(|s| s.as_str()),
        Some("1.0")
    );
}

#[tokio::test]
async fn test_infer_endpoint_empty_metadata_forwards_empty_map() {
    let (base, mut rx) = spawn_server().await;
    let _ = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test", "metadata": {}}))
        .send()
        .await
        .expect("send");

    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("channel open");
    assert!(received.meta.is_empty());
}

// ============================================================================
// Infer Endpoint — Error Cases
// ============================================================================

#[tokio::test]
async fn test_infer_endpoint_invalid_json_returns_client_error() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .post(format!("{base}/api/v1/infer"))
        .header("content-type", "application/json")
        .body("not valid json {{{")
        .send()
        .await
        .expect("send");
    assert!(
        resp.status().is_client_error(),
        "Invalid JSON should return 4xx, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_infer_endpoint_missing_prompt_field_returns_client_error() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"session_id": "s1"}))
        .send()
        .await
        .expect("send");
    assert!(
        resp.status().is_client_error(),
        "Missing prompt should return 4xx, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_infer_endpoint_wrong_content_type_returns_client_error() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .post(format!("{base}/api/v1/infer"))
        .header("content-type", "text/plain")
        .body("prompt=hello")
        .send()
        .await
        .expect("send");
    assert!(
        resp.status().is_client_error(),
        "Wrong content type should return 4xx, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_infer_endpoint_empty_body_returns_client_error() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .post(format!("{base}/api/v1/infer"))
        .header("content-type", "application/json")
        .body("")
        .send()
        .await
        .expect("send");
    assert!(
        resp.status().is_client_error(),
        "Empty body should return 4xx, got {}",
        resp.status()
    );
}

// ============================================================================
// Status Endpoint
// ============================================================================

#[tokio::test]
async fn test_status_endpoint_unknown_id_returns_404() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .get(format!("{base}/api/v1/status/nonexistent-id"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_status_endpoint_unknown_id_returns_error_json() {
    let (base, _rx) = spawn_server().await;
    let body: Value = client()
        .get(format!("{base}/api/v1/status/nonexistent-id"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert!(
        body.get("error").is_some(),
        "404 should include error field"
    );
}

// ============================================================================
// Result Endpoint
// ============================================================================

#[tokio::test]
async fn test_result_endpoint_unknown_id_returns_404() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .get(format!("{base}/api/v1/result/nonexistent-id"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================================
// Concurrent Request Tests
// ============================================================================

#[tokio::test]
async fn test_concurrent_requests_do_not_deadlock() {
    let (base, _rx) = spawn_server().await;
    let c = client();

    let mut handles = Vec::new();
    for i in 0..10 {
        let url = format!("{base}/api/v1/infer");
        let http = c.clone();
        handles.push(tokio::spawn(async move {
            http.post(&url)
                .json(&json!({"prompt": format!("concurrent request {i}")}))
                .send()
                .await
        }));
    }

    for handle in handles {
        let result = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("task should complete within 10s")
            .expect("task should not panic");
        let resp = result.expect("request should succeed");
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn test_concurrent_requests_get_unique_request_ids() {
    let (base, _rx) = spawn_server().await;
    let c = client();

    let mut ids = Vec::new();
    for _ in 0..5 {
        let body: InferResponse = c
            .post(format!("{base}/api/v1/infer"))
            .json(&json!({"prompt": "test"}))
            .send()
            .await
            .expect("send")
            .json()
            .await
            .expect("json");
        ids.push(body.request_id);
    }

    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "All request IDs must be unique: {ids:?}"
    );
}

// ============================================================================
// Method Not Allowed Tests
// ============================================================================

#[tokio::test]
async fn test_get_to_infer_endpoint_returns_method_not_allowed() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .get(format!("{base}/api/v1/infer"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_post_to_health_endpoint_returns_method_not_allowed() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .post(format!("{base}/health"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_post_to_metrics_endpoint_returns_method_not_allowed() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .post(format!("{base}/metrics"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_post_to_status_endpoint_returns_method_not_allowed() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .post(format!("{base}/api/v1/status/some-id"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ============================================================================
// Unknown Route
// ============================================================================

#[tokio::test]
async fn test_unknown_route_returns_404() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .get(format!("{base}/api/v1/nonexistent"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_root_path_returns_404() {
    let (base, _rx) = spawn_server().await;
    let resp = client().get(format!("{base}/")).send().await.expect("send");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================================
// Pipeline Closed (503)
// ============================================================================

#[tokio::test]
async fn test_infer_endpoint_pipeline_closed_returns_503() {
    let port = next_port();
    let (tx, rx) = mpsc::channel::<PromptRequest>(1);
    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        max_request_size: 1024 * 1024,
        timeout_seconds: 2,
    };
    tokio::spawn(async move {
        let _ = tokio_prompt_orchestrator::web_api::start_server(config, tx).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Drop the receiver to close the pipeline channel.
    drop(rx);

    let base = format!("http://127.0.0.1:{port}");
    let resp = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test"}))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "Closed pipeline should return 503"
    );
}

#[tokio::test]
async fn test_pipeline_closed_error_body_contains_error_field() {
    let port = next_port();
    let (tx, rx) = mpsc::channel::<PromptRequest>(1);
    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        max_request_size: 1024 * 1024,
        timeout_seconds: 2,
    };
    tokio::spawn(async move {
        let _ = tokio_prompt_orchestrator::web_api::start_server(config, tx).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(rx);

    let base = format!("http://127.0.0.1:{port}");
    let body: Value = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "test"}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert!(
        body.get("error").is_some(),
        "503 response body should contain an error field"
    );
}

// ============================================================================
// CORS
// ============================================================================

#[tokio::test]
async fn test_cors_allows_any_origin() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .get(format!("{base}/health"))
        .header("origin", "http://example.com")
        .send()
        .await
        .expect("send");
    let cors = resp.headers().get("access-control-allow-origin");
    assert!(cors.is_some(), "CORS allow-origin header should be present");
}

#[tokio::test]
async fn test_cors_preflight_returns_200() {
    let (base, _rx) = spawn_server().await;
    let resp = client()
        .request(reqwest::Method::OPTIONS, format!("{base}/api/v1/infer"))
        .header("origin", "http://example.com")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .expect("send");
    assert!(
        resp.status().is_success(),
        "CORS preflight should return 2xx, got {}",
        resp.status()
    );
}

// ============================================================================
// Large Prompt
// ============================================================================

#[tokio::test]
async fn test_infer_endpoint_large_prompt_succeeds() {
    let (base, mut rx) = spawn_server().await;
    let large_prompt = "x".repeat(50_000);
    let resp = client()
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": large_prompt}))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::OK);

    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("channel open");
    assert_eq!(received.input.len(), 50_000);
}

// ============================================================================
// Health Endpoint — Idempotent
// ============================================================================

#[tokio::test]
async fn test_health_endpoint_multiple_calls_return_same_result() {
    let (base, _rx) = spawn_server().await;
    let c = client();

    for _ in 0..3 {
        let body: Value = c
            .get(format!("{base}/health"))
            .send()
            .await
            .expect("send")
            .json()
            .await
            .expect("json");
        assert_eq!(body["status"], "healthy");
    }
}
