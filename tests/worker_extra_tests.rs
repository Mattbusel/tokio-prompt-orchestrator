//! Additional integration tests for `src/worker.rs`
//!
//! Covers test cases not addressed by the in-module unit tests:
//! - HTTP 429 (rate limit) responses for all HTTP-backed workers
//! - Request timeout behaviour for all HTTP-backed workers
//! - HTTP 401 (authentication) error handling
//! - Empty API key construction
//! - Multiple sequential requests through mocked backends
//! - Builder pattern edge cases (Default trait, temperature, top_p)

use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tokio_prompt_orchestrator::{
    AnthropicWorker, LlamaCppWorker, ModelWorker, OpenAiWorker, OrchestratorError, VllmWorker,
};

/// Serialise tests that read/write environment variables so they don't race
/// against each other within this integration test binary.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

// ============================================================================
// Helpers
// ============================================================================

/// Create an `OpenAiWorker` pointed at `base_url`.
/// Must be called while `ENV_MUTEX` is held.
fn make_openai_worker(base_url: &str) -> OpenAiWorker {
    std::env::set_var("OPENAI_API_KEY", "test-key-openai");
    let w = OpenAiWorker::new("gpt-4")
        .expect("must succeed with key set")
        .with_base_url(base_url);
    std::env::remove_var("OPENAI_API_KEY");
    w
}

/// Create an `AnthropicWorker` pointed at `base_url`.
/// Must be called while `ENV_MUTEX` is held.
fn make_anthropic_worker(base_url: &str) -> AnthropicWorker {
    std::env::set_var("ANTHROPIC_API_KEY", "test-key-anthropic");
    let w = AnthropicWorker::new("claude-instant-1-2")
        .expect("must succeed with key set")
        .with_base_url(base_url);
    std::env::remove_var("ANTHROPIC_API_KEY");
    w
}

fn openai_success_body() -> serde_json::Value {
    json!({"choices": [{"text": "hello world response"}]})
}

fn anthropic_success_body() -> serde_json::Value {
    json!({"completion": "hello world response"})
}

fn llamacpp_success_body() -> serde_json::Value {
    json!({"content": "hello world response"})
}

fn vllm_success_body() -> serde_json::Value {
    json!({"text": ["hello world response"]})
}

// ============================================================================
// OpenAI — HTTP 429 Rate Limit
// ============================================================================

#[tokio::test]
async fn test_openai_infer_http_429_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(
            json!({"error": {"type": "rate_limit_exceeded", "message": "Rate limit reached"}}),
        ))
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_openai_worker(&server.uri())
    };
    let result = worker.infer("test").await;
    assert!(result.is_err(), "429 should return Err");
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(
                msg.contains("429"),
                "Error should include status code 429, got: {msg}"
            );
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// OpenAI — Timeout
// ============================================================================

#[tokio::test]
async fn test_openai_infer_timeout_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openai_success_body())
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_openai_worker(&server.uri())
    };
    let worker = worker.with_timeout(Duration::from_millis(100));

    let result = worker.infer("test").await;
    assert!(result.is_err(), "Timeout should return Err");
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            // reqwest timeout errors include "timed out" or "timeout" in the message
            let lower = msg.to_lowercase();
            assert!(
                lower.contains("timed out")
                    || lower.contains("timeout")
                    || lower.contains("request failed"),
                "Error should indicate timeout, got: {msg}"
            );
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// OpenAI — HTTP 401 Unauthorized
// ============================================================================

#[tokio::test]
async fn test_openai_infer_http_401_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"error": {"message": "Invalid API key"}})),
        )
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_openai_worker(&server.uri())
    };
    let result = worker.infer("test").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("401"), "Error should include 401, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// OpenAI — Empty API Key
// ============================================================================

#[test]
fn test_openai_worker_empty_api_key_succeeds_construction() {
    // The current implementation accepts any non-missing env var, including empty.
    // This test documents that behaviour.
    let _g = ENV_MUTEX.lock().unwrap();
    std::env::set_var("OPENAI_API_KEY", "");
    let result = OpenAiWorker::new("gpt-4");
    std::env::remove_var("OPENAI_API_KEY");
    assert!(
        result.is_ok(),
        "Empty OPENAI_API_KEY is accepted (env var is set, just empty)"
    );
}

// ============================================================================
// OpenAI — Multiple Sequential Requests
// ============================================================================

#[tokio::test]
async fn test_openai_infer_multiple_sequential_requests_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_success_body()))
        .expect(3)
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_openai_worker(&server.uri())
    };
    for i in 0..3 {
        let result = worker.infer(&format!("prompt {i}")).await;
        assert!(result.is_ok(), "Request {i} should succeed");
    }
}

// ============================================================================
// Anthropic — HTTP 429 Rate Limit
// ============================================================================

#[tokio::test]
async fn test_anthropic_infer_http_429_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/complete"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(json!({"error": {"type": "rate_limit_error"}})),
        )
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_anthropic_worker(&server.uri())
    };
    let result = worker.infer("test").await;
    assert!(result.is_err(), "429 should return Err");
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(
                msg.contains("429"),
                "Error should include status code 429, got: {msg}"
            );
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// Anthropic — Timeout
// ============================================================================

#[tokio::test]
async fn test_anthropic_infer_timeout_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/complete"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(anthropic_success_body())
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_anthropic_worker(&server.uri())
    };
    let worker = worker.with_timeout(Duration::from_millis(100));

    let result = worker.infer("test").await;
    assert!(result.is_err(), "Timeout should return Err");
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            let lower = msg.to_lowercase();
            assert!(
                lower.contains("timed out")
                    || lower.contains("timeout")
                    || lower.contains("request failed"),
                "Error should indicate timeout, got: {msg}"
            );
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// Anthropic — HTTP 401 Unauthorized
// ============================================================================

#[tokio::test]
async fn test_anthropic_infer_http_401_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/complete"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"error": {"message": "Invalid x-api-key"}})),
        )
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_anthropic_worker(&server.uri())
    };
    let result = worker.infer("test").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("401"), "Error should include 401, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// Anthropic — Empty API Key
// ============================================================================

#[test]
fn test_anthropic_worker_empty_api_key_succeeds_construction() {
    let _g = ENV_MUTEX.lock().unwrap();
    std::env::set_var("ANTHROPIC_API_KEY", "");
    let result = AnthropicWorker::new("claude-3-5-sonnet-20241022");
    std::env::remove_var("ANTHROPIC_API_KEY");
    assert!(
        result.is_ok(),
        "Empty ANTHROPIC_API_KEY is accepted (env var is set, just empty)"
    );
}

// ============================================================================
// Anthropic — Multiple Sequential Requests
// ============================================================================

#[tokio::test]
async fn test_anthropic_infer_multiple_sequential_requests_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_success_body()))
        .expect(3)
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_anthropic_worker(&server.uri())
    };
    for i in 0..3 {
        let result = worker.infer(&format!("prompt {i}")).await;
        assert!(result.is_ok(), "Request {i} should succeed");
    }
}

// ============================================================================
// LlamaCpp — HTTP 429 Rate Limit
// ============================================================================

#[tokio::test]
async fn test_llamacpp_infer_http_429_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completion"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let worker = LlamaCppWorker::new().with_url(server.uri());
    let result = worker.infer("test").await;
    assert!(result.is_err(), "429 should return Err");
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("429"), "Error should include 429, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// LlamaCpp — Timeout
// ============================================================================

#[tokio::test]
async fn test_llamacpp_infer_timeout_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completion"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(llamacpp_success_body())
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&server)
        .await;

    let worker = LlamaCppWorker::new()
        .with_url(server.uri())
        .with_timeout(Duration::from_millis(100));

    let result = worker.infer("test").await;
    assert!(result.is_err(), "Timeout should return Err");
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            let lower = msg.to_lowercase();
            assert!(
                lower.contains("timed out")
                    || lower.contains("timeout")
                    || lower.contains("request failed"),
                "Error should indicate timeout, got: {msg}"
            );
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// LlamaCpp — HTTP 401
// ============================================================================

#[tokio::test]
async fn test_llamacpp_infer_http_401_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completion"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let worker = LlamaCppWorker::new().with_url(server.uri());
    let result = worker.infer("test").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("401"), "Error should include 401, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// LlamaCpp — Multiple Sequential Requests
// ============================================================================

#[tokio::test]
async fn test_llamacpp_infer_multiple_sequential_requests_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completion"))
        .respond_with(ResponseTemplate::new(200).set_body_json(llamacpp_success_body()))
        .expect(3)
        .mount(&server)
        .await;

    let worker = LlamaCppWorker::new().with_url(server.uri());
    for i in 0..3 {
        let result = worker.infer(&format!("prompt {i}")).await;
        assert!(result.is_ok(), "Request {i} should succeed");
    }
}

// ============================================================================
// LlamaCpp — Builder Pattern
// ============================================================================

#[test]
fn test_llamacpp_default_trait_produces_valid_worker() {
    let worker = LlamaCppWorker::default();
    // Must not panic; URL should have a default value.
    let _ = worker;
}

#[test]
fn test_llamacpp_with_temperature_does_not_panic() {
    let _worker = LlamaCppWorker::new().with_temperature(0.5);
}

#[tokio::test]
async fn test_llamacpp_with_temperature_sends_correct_value() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completion"))
        .respond_with(ResponseTemplate::new(200).set_body_json(llamacpp_success_body()))
        .mount(&server)
        .await;

    let worker = LlamaCppWorker::new()
        .with_url(server.uri())
        .with_temperature(0.42);
    let _ = worker.infer("test").await;

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let temp = body["temperature"].as_f64().unwrap();
    assert!(
        (temp - 0.42_f64).abs() < 0.01,
        "Temperature should be ~0.42, got {temp}"
    );
}

// ============================================================================
// Vllm — HTTP 429 Rate Limit
// ============================================================================

#[tokio::test]
async fn test_vllm_infer_http_429_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generate"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let worker = VllmWorker::new().with_url(server.uri());
    let result = worker.infer("test").await;
    assert!(result.is_err(), "429 should return Err");
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("429"), "Error should include 429, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// Vllm — Timeout
// ============================================================================

#[tokio::test]
async fn test_vllm_infer_timeout_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generate"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(vllm_success_body())
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&server)
        .await;

    let worker = VllmWorker::new()
        .with_url(server.uri())
        .with_timeout(Duration::from_millis(100));

    let result = worker.infer("test").await;
    assert!(result.is_err(), "Timeout should return Err");
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            let lower = msg.to_lowercase();
            assert!(
                lower.contains("timed out")
                    || lower.contains("timeout")
                    || lower.contains("request failed"),
                "Error should indicate timeout, got: {msg}"
            );
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// Vllm — HTTP 401
// ============================================================================

#[tokio::test]
async fn test_vllm_infer_http_401_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generate"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let worker = VllmWorker::new().with_url(server.uri());
    let result = worker.infer("test").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("401"), "Error should include 401, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// Vllm — Multiple Sequential Requests
// ============================================================================

#[tokio::test]
async fn test_vllm_infer_multiple_sequential_requests_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vllm_success_body()))
        .expect(3)
        .mount(&server)
        .await;

    let worker = VllmWorker::new().with_url(server.uri());
    for i in 0..3 {
        let result = worker.infer(&format!("prompt {i}")).await;
        assert!(result.is_ok(), "Request {i} should succeed");
    }
}

// ============================================================================
// Vllm — Builder Pattern
// ============================================================================

#[test]
fn test_vllm_default_trait_produces_valid_worker() {
    let worker = VllmWorker::default();
    let _ = worker;
}

#[test]
fn test_vllm_with_temperature_does_not_panic() {
    let _worker = VllmWorker::new().with_temperature(0.5);
}

#[tokio::test]
async fn test_vllm_with_temperature_sends_correct_value() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vllm_success_body()))
        .mount(&server)
        .await;

    let worker = VllmWorker::new()
        .with_url(server.uri())
        .with_temperature(0.42);
    let _ = worker.infer("test").await;

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let temp = body["temperature"].as_f64().unwrap();
    assert!(
        (temp - 0.42_f64).abs() < 0.01,
        "Temperature should be ~0.42, got {temp}"
    );
}

// ============================================================================
// OpenAI — Builder Chain
// ============================================================================

#[test]
fn test_openai_worker_full_builder_chain() {
    let _g = ENV_MUTEX.lock().unwrap();
    std::env::set_var("OPENAI_API_KEY", "test-key");
    let result = OpenAiWorker::new("gpt-4").map(|w| {
        w.with_max_tokens(2048)
            .with_temperature(0.5)
            .with_timeout(Duration::from_secs(120))
            .with_base_url("http://localhost:9999")
    });
    std::env::remove_var("OPENAI_API_KEY");
    assert!(result.is_ok(), "Full builder chain should succeed");
}

// ============================================================================
// Anthropic — Builder Chain
// ============================================================================

#[test]
fn test_anthropic_worker_full_builder_chain() {
    let _g = ENV_MUTEX.lock().unwrap();
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let result = AnthropicWorker::new("claude-3-5-sonnet-20241022").map(|w| {
        w.with_max_tokens(4096)
            .with_temperature(0.8)
            .with_timeout(Duration::from_secs(90))
            .with_base_url("http://localhost:9999")
    });
    std::env::remove_var("ANTHROPIC_API_KEY");
    assert!(result.is_ok(), "Full builder chain should succeed");
}

// ============================================================================
// LlamaCpp — Builder Chain
// ============================================================================

#[test]
fn test_llamacpp_worker_full_builder_chain() {
    let _worker = LlamaCppWorker::new()
        .with_url("http://localhost:9999")
        .with_max_tokens(1024)
        .with_temperature(0.6)
        .with_timeout(Duration::from_secs(45));
}

// ============================================================================
// Vllm — Builder Chain
// ============================================================================

#[test]
fn test_vllm_worker_full_builder_chain() {
    let _worker = VllmWorker::new()
        .with_url("http://localhost:9999")
        .with_max_tokens(2048)
        .with_temperature(0.7)
        .with_top_p(0.9)
        .with_timeout(Duration::from_secs(120));
}

// ============================================================================
// OpenAI — HTTP 503 Service Unavailable
// ============================================================================

#[tokio::test]
async fn test_openai_infer_http_503_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("service temporarily unavailable"))
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_openai_worker(&server.uri())
    };
    let result = worker.infer("test").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("503"), "Error should include 503, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// Anthropic — HTTP 503 Service Unavailable
// ============================================================================

#[tokio::test]
async fn test_anthropic_infer_http_503_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/complete"))
        .respond_with(ResponseTemplate::new(503).set_body_string("service temporarily unavailable"))
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_anthropic_worker(&server.uri())
    };
    let result = worker.infer("test").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("503"), "Error should include 503, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// LlamaCpp — HTTP 503 Service Unavailable
// ============================================================================

#[tokio::test]
async fn test_llamacpp_infer_http_503_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completion"))
        .respond_with(ResponseTemplate::new(503).set_body_string("service temporarily unavailable"))
        .mount(&server)
        .await;

    let worker = LlamaCppWorker::new().with_url(server.uri());
    let result = worker.infer("test").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("503"), "Error should include 503, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// Vllm — HTTP 503 Service Unavailable
// ============================================================================

#[tokio::test]
async fn test_vllm_infer_http_503_returns_inference_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generate"))
        .respond_with(ResponseTemplate::new(503).set_body_string("service temporarily unavailable"))
        .mount(&server)
        .await;

    let worker = VllmWorker::new().with_url(server.uri());
    let result = worker.infer("test").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        OrchestratorError::Inference(msg) => {
            assert!(msg.contains("503"), "Error should include 503, got: {msg}");
        }
        other => panic!("Expected Inference error, got: {other:?}"),
    }
}

// ============================================================================
// OpenAI — Response Whitespace Tokenization
// ============================================================================

#[tokio::test]
async fn test_openai_infer_whitespace_only_response_returns_empty_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"choices": [{"text": "   \t  \n  "}]})),
        )
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_openai_worker(&server.uri())
    };
    let tokens = worker.infer("test").await.unwrap();
    assert!(
        tokens.is_empty(),
        "Whitespace-only response should produce no tokens"
    );
}

// ============================================================================
// Anthropic — Response Whitespace Tokenization
// ============================================================================

#[tokio::test]
async fn test_anthropic_infer_whitespace_only_response_returns_empty_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/complete"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"completion": "   \t  \n  "})),
        )
        .mount(&server)
        .await;

    let worker = {
        let _g = ENV_MUTEX.lock().unwrap();
        make_anthropic_worker(&server.uri())
    };
    let tokens = worker.infer("test").await.unwrap();
    assert!(
        tokens.is_empty(),
        "Whitespace-only response should produce no tokens"
    );
}
