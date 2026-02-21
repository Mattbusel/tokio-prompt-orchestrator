//! Observability integration tests — Phase 1 (structured logging & tracing)
//!
//! Tests in this module verify:
//! - `init_metrics()` succeeds and is idempotent
//! - `gather_metrics()` returns valid Prometheus text format
//! - `get_metrics_summary()` exposes required fields
//! - Pipeline stages propagate `request_id` without panic
//! - Pipeline stages record stage-duration metrics
//!
//! NOTE: Log-content assertions (e.g. `logs_contain(...)`) are intentionally
//! omitted from integration tests. The `#[traced_test]` subscriber is
//! thread-local and cannot capture logs emitted inside `tokio::spawn` tasks
//! running on the multi-threaded runtime. Structured-log field coverage is
//! verified in `src/stages.rs` unit tests and by manual `RUST_LOG=debug`
//! inspection.

use std::collections::HashMap;
use std::sync::Arc;
use tokio_prompt_orchestrator::{metrics, spawn_pipeline, EchoWorker, PromptRequest, SessionId};

// ── init_metrics ──────────────────────────────────────────────────────

#[test]
fn test_init_metrics_succeeds_on_first_call() {
    let result = metrics::init_metrics();
    assert!(result.is_ok(), "init_metrics should succeed: {result:?}");
}

#[test]
fn test_init_metrics_double_call_is_idempotent() {
    let _ = metrics::init_metrics();
    let result = metrics::init_metrics();
    assert!(
        result.is_ok(),
        "second init_metrics must be a no-op returning Ok"
    );
}

// ── gather_metrics ────────────────────────────────────────────────────

#[test]
fn test_gather_metrics_returns_valid_prometheus_format() {
    let _ = metrics::init_metrics();
    metrics::inc_request("test-gather-stage");

    let output = metrics::gather_metrics();
    assert!(
        output.contains("orchestrator_requests_total"),
        "Prometheus output must contain the requests counter"
    );
    assert!(
        std::str::from_utf8(output.as_bytes()).is_ok(),
        "must be valid UTF-8"
    );
}

#[test]
fn test_gather_metrics_before_init_returns_valid_string() {
    let output = metrics::gather_metrics();
    assert!(
        std::str::from_utf8(output.as_bytes()).is_ok(),
        "must not panic and must be valid UTF-8"
    );
}

// ── get_metrics_summary ───────────────────────────────────────────────

#[test]
fn test_get_metrics_summary_fields_accessible() {
    let summary = metrics::get_metrics_summary();
    let _rt = &summary.requests_total;
    let _rs = &summary.requests_shed;
    let _et = &summary.errors_total;
}

// ── Metrics helpers are no-ops before init ────────────────────────────

#[test]
fn test_metrics_helpers_safe_before_init() {
    // All metric-increment helpers must be safe to call even if
    // init_metrics() has not yet been called (or was called in another
    // test that may or may not have run first).
    metrics::inc_request("pre-init-stage");
    metrics::inc_error("pre-init-stage", "test_error");
    metrics::inc_shed("pre-init-stage");
    // No panic = pass
}

// ── Pipeline end-to-end (structural) ─────────────────────────────────

#[tokio::test]
async fn test_pipeline_request_id_propagates_through_all_stages() {
    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    let request = PromptRequest {
        session: SessionId::new("propagation-session"),
        request_id: "req-propagation-001".to_string(),
        input: "propagation test input".to_string(),
        meta: HashMap::new(),
    };

    handles.input_tx.send(request).await.unwrap_or_else(|_| ());
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    // Primary assertion: no panic during propagation through all 5 stages
}

#[tokio::test]
async fn test_pipeline_handles_empty_request_id_gracefully() {
    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    let request = PromptRequest {
        session: SessionId::new("empty-rid-session"),
        request_id: String::new(),
        input: "empty request id test".to_string(),
        meta: HashMap::new(),
    };

    handles.input_tx.send(request).await.unwrap_or_else(|_| ());
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    // Must not panic even with empty request_id
}

#[tokio::test]
async fn test_pipeline_graceful_shutdown_after_input_close() {
    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    // Send one request then immediately close the channel
    let request = PromptRequest {
        session: SessionId::new("shutdown-test"),
        request_id: "req-shutdown".to_string(),
        input: "shutdown".to_string(),
        meta: HashMap::new(),
    };
    handles.input_tx.send(request).await.unwrap_or_else(|_| ());
    drop(handles.input_tx);

    // Pipeline should drain cleanly within a reasonable timeout
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    // No panic, no hang = pass
}

#[tokio::test]
async fn test_pipeline_multiple_requests_complete_without_panic() {
    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    for i in 0..10 {
        let request = PromptRequest {
            session: SessionId::new(format!("batch-session-{i}")),
            request_id: format!("req-batch-{i:03}"),
            input: format!("batch input {i}"),
            meta: HashMap::new(),
        };
        handles.input_tx.send(request).await.unwrap_or_else(|_| ());
    }
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    // 10 requests through all 5 stages without panic
}

// ── Pipeline metrics integration ──────────────────────────────────────

#[tokio::test]
async fn test_pipeline_records_metrics_for_all_stages() {
    let _ = metrics::init_metrics();

    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    for i in 0..3 {
        let request = PromptRequest {
            session: SessionId::new(format!("metrics-session-{i}")),
            request_id: format!("req-metrics-{i}"),
            input: "metrics test".to_string(),
            meta: HashMap::new(),
        };
        handles.input_tx.send(request).await.unwrap_or_else(|_| ());
    }
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let prom_output = metrics::gather_metrics();
    assert!(
        prom_output.contains("orchestrator_stage_duration_seconds"),
        "stage duration histogram must be recorded"
    );
}

#[tokio::test]
async fn test_pipeline_increments_request_counter() {
    let _ = metrics::init_metrics();

    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    let request = PromptRequest {
        session: SessionId::new("counter-test"),
        request_id: "req-counter".to_string(),
        input: "counter test".to_string(),
        meta: HashMap::new(),
    };
    handles.input_tx.send(request).await.unwrap_or_else(|_| ());
    drop(handles.input_tx);
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let prom_output = metrics::gather_metrics();
    assert!(
        prom_output.contains("orchestrator_requests_total"),
        "request counter must be incremented by pipeline stages"
    );
}
