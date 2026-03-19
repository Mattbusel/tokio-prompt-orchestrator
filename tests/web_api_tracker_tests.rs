//! Result tracker eviction tests (test 30).
//!
//! Tests the lifetime of tracked requests in the web API:
//! - Submit a request and verify it is tracked
//! - After eviction (simulated via completing + the tracker's DashMap retaining
//!   only fresh entries), GET /result and GET /status return 404
//! - Server does NOT panic when a client polls an evicted request ID
//!
//! Because `RequestTracker` is an implementation detail (private) and its
//! background cleanup task runs every 5 minutes against a 1-hour TTL, we
//! simulate eviction by:
//!   1. Submitting a request through the public API.
//!   2. Sending a `PostOutput` over the output channel so the server marks the
//!      request Completed.
//!   3. Probing that the ID is present immediately after completion.
//!   4. Verifying 404 for a completely unknown ID (which is the same code path
//!      that runs after eviction).
//!   5. Verifying the server stays healthy after repeated 404 polls (no panic).

#![cfg(feature = "web-api")]

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use tokio_prompt_orchestrator::web_api::{InferResponse, ServerConfig};
use tokio_prompt_orchestrator::{PostOutput, PromptRequest};

// ============================================================================
// Port allocation (separate range to avoid collisions)
// ============================================================================

static TRACKER_PORT_COUNTER: AtomicU16 = AtomicU16::new(30500);

fn next_port() -> u16 {
    TRACKER_PORT_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ============================================================================
// Helpers
// ============================================================================

/// Spawn the server with a short `timeout_seconds` (2s) for fast test runs.
/// Returns `(base_url, pipeline_rx, output_tx)` so tests can drive completion.
async fn spawn_server_with_output() -> (
    String,
    mpsc::Receiver<PromptRequest>,
    mpsc::Sender<PostOutput>,
) {
    let port = next_port();
    let (pipeline_tx, pipeline_rx) = mpsc::channel::<PromptRequest>(64);
    let (output_tx, output_rx) = mpsc::channel::<PostOutput>(64);

    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        max_request_size: 1024 * 1024,
        timeout_seconds: 2,
    };

    tokio::spawn(async move {
        let _ =
            tokio_prompt_orchestrator::web_api::start_server(config, pipeline_tx, output_rx).await;
    });

    tokio::time::sleep(Duration::from_millis(300)).await;
    (format!("http://127.0.0.1:{port}"), pipeline_rx, output_tx)
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client must build in tests")
}

// ============================================================================
// Test 30a – Submit and complete a request; verify it is tracked
// ============================================================================

/// After submitting a request and signalling completion via PostOutput, the
/// status endpoint must return `completed` while the entry exists in the tracker.
#[tokio::test]
async fn test_tracker_completed_request_is_visible() {
    let (base, mut pipeline_rx, output_tx) = spawn_server_with_output().await;
    let c = client();

    // Submit a request.
    let resp: InferResponse = c
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "echo me"}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    let req_id = resp.request_id.clone();
    assert!(!req_id.is_empty(), "request_id must be present");

    // Receive the forwarded request so we know the session.
    let forwarded = tokio::time::timeout(Duration::from_secs(2), pipeline_rx.recv())
        .await
        .expect("timeout")
        .expect("channel open");

    // Complete the request via the output channel.
    output_tx
        .send(PostOutput {
            session: forwarded.session.clone(),
            request_id: req_id.clone(),
            text: "echo result".to_string(),
        })
        .await
        .expect("output channel must accept send");

    // Give the collector task time to process the output.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Status should now be `completed`.
    let status_body: Value = c
        .get(format!("{base}/api/v1/status/{req_id}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert_eq!(
        status_body["status"], "completed",
        "Status should be 'completed' after PostOutput delivered, got: {status_body}"
    );
}

// ============================================================================
// Test 30b – Unknown (evicted) ID returns 404 from /result
// ============================================================================

/// Polling `/api/v1/result/{id}` with an ID that was never tracked (or has
/// been evicted from the DashMap) must return HTTP 404.
#[tokio::test]
async fn test_tracker_evicted_result_id_returns_404() {
    let (base, _rx, _out) = spawn_server_with_output().await;

    let evicted_id = "00000000-dead-beef-0000-000000000000";
    let resp = client()
        .get(format!("{base}/api/v1/result/{evicted_id}"))
        .send()
        .await
        .expect("send must not fail at transport level");

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "Evicted/unknown result ID must return 404, got {}",
        resp.status()
    );
}

// ============================================================================
// Test 30c – Unknown (evicted) ID returns 404 from /status
// ============================================================================

#[tokio::test]
async fn test_tracker_evicted_status_id_returns_404() {
    let (base, _rx, _out) = spawn_server_with_output().await;

    let evicted_id = "11111111-dead-beef-0000-000000000001";
    let resp = client()
        .get(format!("{base}/api/v1/status/{evicted_id}"))
        .send()
        .await
        .expect("send");

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "Evicted/unknown status ID must return 404, got {}",
        resp.status()
    );
}

// ============================================================================
// Test 30d – 404 for evicted ID includes an `error` field in the body
// ============================================================================

#[tokio::test]
async fn test_tracker_evicted_status_response_includes_error_field() {
    let (base, _rx, _out) = spawn_server_with_output().await;

    let evicted_id = "22222222-dead-beef-0000-000000000002";
    let body: Value = client()
        .get(format!("{base}/api/v1/status/{evicted_id}"))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert!(
        body.get("error").is_some(),
        "404 response for evicted ID must include 'error' field, got: {body}"
    );
}

// ============================================================================
// Test 30e – Server does NOT panic when client repeatedly polls evicted ID
// ============================================================================

/// Simulates a client retrying an evicted/unknown request ID many times.
/// The server must keep returning 404 without crashing.
#[tokio::test]
async fn test_tracker_repeated_polls_of_evicted_id_no_panic() {
    let (base, _rx, _out) = spawn_server_with_output().await;
    let c = client();

    let evicted_id = "33333333-dead-beef-0000-000000000003";

    // Poll 20 times in quick succession.
    for attempt in 0..20 {
        let resp = c
            .get(format!("{base}/api/v1/status/{evicted_id}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("attempt {attempt}: send failed: {e}"));
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "attempt {attempt}: expected 404, got {}",
            resp.status()
        );
    }

    // Verify server is still alive.
    let health = c
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("health check send");
    assert_eq!(
        health.status(),
        StatusCode::OK,
        "Server must still be healthy after repeated evicted-ID polls"
    );
}

// ============================================================================
// Test 30f – Completed result is accessible immediately after PostOutput
// ============================================================================

#[tokio::test]
async fn test_tracker_result_accessible_after_completion() {
    let (base, mut pipeline_rx, output_tx) = spawn_server_with_output().await;
    let c = client();

    // Submit.
    let resp: InferResponse = c
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "hello tracker"}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let req_id = resp.request_id.clone();

    // Receive forwarded request.
    let forwarded = tokio::time::timeout(Duration::from_secs(2), pipeline_rx.recv())
        .await
        .expect("timeout")
        .expect("channel open");

    // Complete.
    output_tx
        .send(PostOutput {
            session: forwarded.session.clone(),
            request_id: req_id.clone(),
            text: "hello back".to_string(),
        })
        .await
        .expect("send output");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // GET /api/v1/result should return the completed result.
    // result_handler polls until completed or timeout — it should resolve
    // immediately since we already completed the request.
    let result_resp = c
        .get(format!("{base}/api/v1/result/{req_id}"))
        .send()
        .await
        .expect("send");

    assert_eq!(
        result_resp.status(),
        StatusCode::OK,
        "Result endpoint must return 200 for a completed request, got {}",
        result_resp.status()
    );

    let result_body: Value = result_resp.json().await.expect("json");
    assert_eq!(result_body["status"], "completed");
    assert_eq!(
        result_body["result"], "hello back",
        "Result body must contain the PostOutput text"
    );
}

// ============================================================================
// Test 30g – Fresh request is NOT evicted (still returns non-404 while in-flight)
// ============================================================================

/// An in-flight request (status = Processing) must be present in the tracker
/// and must not return 404 until it is explicitly evicted.
#[tokio::test]
async fn test_tracker_in_flight_request_not_evicted() {
    let (base, _rx, _out) = spawn_server_with_output().await;
    let c = client();

    // Submit a request but do NOT complete it — tracker entry should remain.
    let resp: InferResponse = c
        .post(format!("{base}/api/v1/infer"))
        .json(&json!({"prompt": "still in flight"}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let req_id = resp.request_id.clone();

    // Status endpoint should find the entry (Processing or Pending, not 404).
    let status_resp = c
        .get(format!("{base}/api/v1/status/{req_id}"))
        .send()
        .await
        .expect("send");

    assert_ne!(
        status_resp.status(),
        StatusCode::NOT_FOUND,
        "In-flight request must not be 404 in the tracker"
    );
}

// ============================================================================
// Test 30h – Multiple completed requests are all individually retrievable
// ============================================================================

#[tokio::test]
async fn test_tracker_multiple_completed_requests_individually_accessible() {
    let (base, mut pipeline_rx, output_tx) = spawn_server_with_output().await;
    let c = client();

    let mut ids_and_texts = Vec::new();

    // Submit 3 requests.
    for i in 0..3 {
        let resp: InferResponse = c
            .post(format!("{base}/api/v1/infer"))
            .json(&json!({"prompt": format!("prompt {i}")}))
            .send()
            .await
            .expect("send")
            .json()
            .await
            .expect("json");
        ids_and_texts.push((resp.request_id.clone(), format!("result {i}")));
    }

    // Complete all 3 by draining the pipeline channel.
    for (req_id, result_text) in &ids_and_texts {
        let forwarded = tokio::time::timeout(Duration::from_secs(2), pipeline_rx.recv())
            .await
            .expect("timeout")
            .expect("channel open");
        output_tx
            .send(PostOutput {
                session: forwarded.session.clone(),
                request_id: req_id.clone(),
                text: result_text.clone(),
            })
            .await
            .expect("send output");
    }

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Verify each is accessible.
    for (req_id, expected_text) in &ids_and_texts {
        let body: Value = c
            .get(format!("{base}/api/v1/status/{req_id}"))
            .send()
            .await
            .expect("send")
            .json()
            .await
            .expect("json");
        assert_eq!(
            body["status"], "completed",
            "Request {req_id} should be completed, got: {body}"
        );

        let result_body: Value = c
            .get(format!("{base}/api/v1/result/{req_id}"))
            .send()
            .await
            .expect("send")
            .json()
            .await
            .expect("json");
        assert_eq!(
            result_body["result"], expected_text.as_str(),
            "Request {req_id} result mismatch"
        );
    }
}
