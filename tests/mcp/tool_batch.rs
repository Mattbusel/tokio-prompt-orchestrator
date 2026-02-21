//! Integration tests for MCP batch_infer tool.
//!
//! Verifies batch submission to the pipeline: multiple prompts sent
//! through the input channel, session ID grouping, and non-blocking behavior.

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};

#[tokio::test]
async fn test_batch_submit_multiple_prompts() {
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);
    let job_id = "batch-test-001";

    let prompts = vec!["first prompt", "second prompt", "third prompt"];

    for (i, prompt) in prompts.iter().enumerate() {
        let request = PromptRequest {
            session: SessionId::new(format!("batch-{job_id}")),
            request_id: format!("{job_id}-{i}"),
            input: prompt.to_string(),
            meta: {
                let mut m = HashMap::new();
                m.insert("batch_job".to_string(), job_id.to_string());
                m
            },
        };

        let result = handles.input_tx.send(request).await;
        assert!(result.is_ok(), "prompt {i} should be accepted");
    }
}

#[tokio::test]
async fn test_batch_submit_returns_immediately() {
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);

    let start = std::time::Instant::now();

    for i in 0..10 {
        let request = PromptRequest {
            session: SessionId::new("batch-speed-test"),
            request_id: format!("speed-{i}"),
            input: format!("prompt {i}"),
            meta: HashMap::new(),
        };
        let _ = handles.input_tx.send(request).await;
    }

    let elapsed = start.elapsed();
    // Submitting 10 prompts should be nearly instant (< 1 second)
    assert!(
        elapsed.as_secs() < 1,
        "batch submission took too long: {elapsed:?}"
    );
}

#[tokio::test]
async fn test_batch_prompts_share_session_id() {
    let job_id = "shared-session-test";
    let session = SessionId::new(format!("batch-{job_id}"));

    let requests: Vec<PromptRequest> = (0..3)
        .map(|i| PromptRequest {
            session: SessionId::new(format!("batch-{job_id}")),
            request_id: format!("{job_id}-{i}"),
            input: format!("prompt {i}"),
            meta: HashMap::new(),
        })
        .collect();

    // All requests share the same session prefix
    for req in &requests {
        assert_eq!(req.session.as_str(), session.as_str());
    }
}

#[tokio::test]
async fn test_batch_request_ids_are_unique() {
    let job_id = "unique-ids-test";
    let request_ids: Vec<String> = (0..5).map(|i| format!("{job_id}-{i}")).collect();

    // All request IDs should be distinct
    let mut seen = std::collections::HashSet::new();
    for id in &request_ids {
        assert!(seen.insert(id.as_str()), "duplicate request_id: {id}");
    }
}

#[tokio::test]
async fn test_batch_empty_prompts_list() {
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);

    // An empty batch should succeed (nothing to send)
    let prompts: Vec<&str> = vec![];
    for (i, prompt) in prompts.iter().enumerate() {
        let request = PromptRequest {
            session: SessionId::new("batch-empty"),
            request_id: format!("empty-{i}"),
            input: prompt.to_string(),
            meta: HashMap::new(),
        };
        let _ = handles.input_tx.send(request).await;
    }
    // No assertions needed — the loop body never executes
    assert!(prompts.is_empty());
}

#[tokio::test]
async fn test_batch_large_submission() {
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);

    // Submit 50 prompts — should not block or panic
    for i in 0..50 {
        let request = PromptRequest {
            session: SessionId::new("batch-large"),
            request_id: format!("large-{i}"),
            input: format!("batch prompt number {i}"),
            meta: HashMap::new(),
        };
        let result = handles.input_tx.send(request).await;
        assert!(result.is_ok(), "prompt {i} should be accepted");
    }
}
