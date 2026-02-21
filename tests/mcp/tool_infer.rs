//! Integration tests for MCP infer tool pipeline interaction.
//!
//! Verifies that inference through the pipeline produces correct output
//! with the EchoWorker and that session IDs flow through properly.

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};

#[tokio::test]
async fn test_echo_worker_inference_produces_tokens() {
    let worker = EchoWorker::new();
    let result = worker.infer("hello world test").await;
    assert!(result.is_ok());
    let tokens = result.unwrap_or_default();
    assert!(!tokens.is_empty());
    // EchoWorker splits on whitespace
    assert_eq!(tokens.len(), 3);
}

#[tokio::test]
async fn test_pipeline_accepts_prompt_request() {
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);

    let request = PromptRequest {
        session: SessionId::new("mcp-test-sess"),
        request_id: "mcp-req-001".to_string(),
        input: "test prompt for mcp".to_string(),
        meta: HashMap::new(),
    };

    let result = handles.input_tx.send(request).await;
    assert!(result.is_ok(), "pipeline should accept prompt request");
}

#[tokio::test]
async fn test_pipeline_accepts_multiple_sequential_requests() {
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);

    for i in 0..5 {
        let request = PromptRequest {
            session: SessionId::new(format!("sess-{i}")),
            request_id: format!("req-{i}"),
            input: format!("prompt number {i}"),
            meta: HashMap::new(),
        };
        let result = handles.input_tx.send(request).await;
        assert!(result.is_ok(), "request {i} should be accepted");
    }
}

#[tokio::test]
async fn test_session_id_preserved_through_request() {
    let session = SessionId::new("my-unique-session");
    assert_eq!(session.as_str(), "my-unique-session");

    let request = PromptRequest {
        session: session.clone(),
        request_id: "req-test".to_string(),
        input: "test".to_string(),
        meta: HashMap::new(),
    };

    assert_eq!(request.session.as_str(), "my-unique-session");
}

#[tokio::test]
async fn test_echo_worker_handles_empty_prompt() {
    let worker = EchoWorker::new();
    let result = worker.infer("").await;
    assert!(result.is_ok());
    // Empty prompt produces empty or single-element split
    let tokens = result.unwrap_or_default();
    // Splitting empty string on whitespace yields one empty string
    assert!(tokens.len() <= 1);
}

#[tokio::test]
async fn test_echo_worker_handles_long_prompt() {
    let worker = EchoWorker::new();
    let long_prompt = "word ".repeat(100);
    let result = worker.infer(&long_prompt).await;
    assert!(result.is_ok());
    let tokens = result.unwrap_or_default();
    assert!(
        tokens.len() >= 90,
        "should have many tokens from long prompt"
    );
}

#[tokio::test]
async fn test_request_meta_carries_batch_info() {
    let mut meta = HashMap::new();
    meta.insert("batch_job".to_string(), "job-123".to_string());
    meta.insert("source".to_string(), "mcp".to_string());

    let request = PromptRequest {
        session: SessionId::new("batch-sess"),
        request_id: "batch-req-0".to_string(),
        input: "batch prompt".to_string(),
        meta: meta.clone(),
    };

    assert_eq!(
        request.meta.get("batch_job").map(|s| s.as_str()),
        Some("job-123")
    );
    assert_eq!(request.meta.get("source").map(|s| s.as_str()), Some("mcp"));
}
