//! End-to-end integration tests for request deadline expiry in the pipeline.
//!
//! These tests exercise the deadline-checking paths in the RAG and inference
//! stages, verifying that:
//! - Requests with already-expired deadlines are dropped and never reach output.
//! - The `requests_expired_total` and `rag_requests_expired_total` Prometheus
//!   counters increment correctly.
//! - Requests with future deadlines complete normally.
//! - The `.with_deadline()` builder convenience method works end-to-end.
//! - Mixed batches of expired and valid requests are split correctly.
//!
//! ## Notes on metric isolation
//!
//! The global `METRICS` `OnceLock` is shared across all tests in a process.
//! Rather than asserting absolute counter values (which would be flaky when
//! tests run in parallel), each test records the counter values *before* the
//! pipeline run and asserts that the relevant counters have *increased* by the
//! expected delta.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tokio_prompt_orchestrator::{metrics, spawn_pipeline, EchoWorker, PromptRequest, SessionId};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Build a minimal `PromptRequest` with a unique `request_id`.
fn make_request(id: &str) -> PromptRequest {
    PromptRequest {
        session: SessionId::new(format!("deadline-test-session-{id}")),
        request_id: id.to_string(),
        input: format!("test input for {id}"),
        meta: HashMap::new(),
        deadline: None,
    }
}

/// Build a request whose deadline is already in the past.
fn make_expired_request(id: &str) -> PromptRequest {
    PromptRequest {
        session: SessionId::new(format!("deadline-test-session-{id}")),
        request_id: id.to_string(),
        input: format!("test input for {id}"),
        meta: HashMap::new(),
        // Set deadline 1 second in the past so it is guaranteed to have
        // expired before the pipeline stage dequeues and checks it.
        deadline: Some(Instant::now() - Duration::from_secs(1)),
    }
}

/// Read the current `orchestrator_requests_expired_total` counter value from
/// the Prometheus text output.  Returns 0.0 if the metric is not yet present.
fn read_expired_total() -> f64 {
    read_counter_from_output("orchestrator_requests_expired_total")
}

/// Read the current `orchestrator_rag_requests_expired_total` counter value.
fn read_rag_expired_total() -> f64 {
    read_counter_from_output("orchestrator_rag_requests_expired_total")
}

/// Parse a specific counter value from the Prometheus text exposition format.
///
/// Handles both label-free lines (`metric_name VALUE`) and lines where the
/// prometheus crate emits an empty label set (`metric_name{} VALUE`).
/// Returns 0.0 when the metric line is absent (counter has never been
/// incremented and was therefore never emitted by the registry).
fn read_counter_from_output(metric_name: &str) -> f64 {
    let output = metrics::gather_metrics();
    for line in output.lines() {
        // Skip HELP / TYPE comment lines.
        if line.starts_with('#') {
            continue;
        }
        // Strip the metric name prefix; what remains is either:
        //   " VALUE"          (no labels)
        //   "{} VALUE"        (empty label set emitted by some prometheus crates)
        if let Some(rest) = line.strip_prefix(metric_name) {
            // Strip optional empty label braces `{}` before parsing the value.
            let rest = rest.trim_start_matches("{}").trim();
            if let Ok(v) = rest.parse::<f64>() {
                return v;
            }
        }
    }
    0.0
}

// ── Test 1: Expired deadline dropped at inference stage ───────────────────

/// A request with a deadline already in the past must never reach the output
/// channel.  The inference stage drops it and increments
/// `orchestrator_requests_expired_total`.
#[tokio::test]
async fn test_expired_deadline_dropped_in_inference() {
    let _ = metrics::init_metrics();

    // Snapshot counter value before the run.
    // NOTE: because RAG also checks deadlines and drops before reaching
    // inference, we measure both counters and assert at least one was
    // incremented.
    let expired_before = read_expired_total();
    let rag_expired_before = read_rag_expired_total();

    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    // Take ownership of the output receiver before sending the request.
    let mut output_rx = handles
        .output_rx
        .lock()
        .await
        .take()
        .expect("output_rx must be present");

    let req = make_expired_request("expired-inference-001");
    handles
        .input_tx
        .send(req)
        .await
        .expect("input channel must accept the request");

    // Close the input so the pipeline drains cleanly.
    drop(handles.input_tx);

    // Allow the pipeline enough time to process the single request.
    // We expect NO output to arrive because the request is dropped.
    let received = timeout(Duration::from_millis(500), output_rx.recv()).await;

    // A timeout means no output was produced — that is the expected outcome.
    // If a `PostOutput` arrived the request was NOT dropped as required.
    match received {
        Err(_elapsed) => {
            // Correct: timed out waiting for output — request was dropped.
        }
        Ok(None) => {
            // Channel closed without output — also acceptable (pipeline shut down).
        }
        Ok(Some(output)) => {
            panic!(
                "Expected expired request to be dropped but received output for request_id={}",
                output.request_id
            );
        }
    }

    // Allow a brief moment for metrics to be flushed by the pipeline task.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let expired_after = read_expired_total();
    let rag_expired_after = read_rag_expired_total();

    // At least one of the two expiry counters must have been incremented,
    // proving the request was detected as expired somewhere in the pipeline.
    assert!(
        expired_after > expired_before || rag_expired_after > rag_expired_before,
        "Expected at least one expiry counter to increment. \
         requests_expired_total: {expired_before} -> {expired_after}, \
         rag_requests_expired_total: {rag_expired_before} -> {rag_expired_after}"
    );
}

// ── Test 2: Valid deadline completes normally ─────────────────────────────

/// A request with a future deadline (30 seconds from now) must flow through
/// the entire pipeline and produce a `PostOutput`.
#[tokio::test]
async fn test_valid_deadline_completes_normally() {
    let _ = metrics::init_metrics();

    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    let mut output_rx = handles
        .output_rx
        .lock()
        .await
        .take()
        .expect("output_rx must be present");

    let req = PromptRequest {
        session: SessionId::new("valid-deadline-session-001"),
        request_id: "valid-deadline-001".to_string(),
        input: "hello from valid deadline test".to_string(),
        meta: HashMap::new(),
        deadline: Some(Instant::now() + Duration::from_secs(30)),
    };

    handles
        .input_tx
        .send(req)
        .await
        .expect("input channel must accept the request");
    drop(handles.input_tx);

    // The pipeline should produce exactly one output within a reasonable window.
    let received = timeout(Duration::from_secs(5), output_rx.recv())
        .await
        .expect("pipeline must produce output within 5 seconds")
        .expect("output channel must not be closed prematurely");

    assert_eq!(
        received.request_id, "valid-deadline-001",
        "output request_id must match the submitted request"
    );
    assert!(
        !received.text.is_empty(),
        "output text must be non-empty for a successful request"
    );
}

// ── Test 3: Expired deadline dropped at RAG stage ────────────────────────

/// When a request with a past deadline arrives at the RAG stage, it must be
/// dropped immediately (before RAG work is performed) and
/// `orchestrator_rag_requests_expired_total` must increment.
#[tokio::test]
async fn test_expired_deadline_dropped_in_rag() {
    let _ = metrics::init_metrics();

    let rag_expired_before = read_rag_expired_total();

    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    let mut output_rx = handles
        .output_rx
        .lock()
        .await
        .take()
        .expect("output_rx must be present");

    // Use a deadline 2 seconds in the past for extra margin.
    let req = PromptRequest {
        session: SessionId::new("rag-expired-session-001"),
        request_id: "rag-expired-001".to_string(),
        input: "should be dropped at rag".to_string(),
        meta: HashMap::new(),
        deadline: Some(Instant::now() - Duration::from_secs(2)),
    };

    handles
        .input_tx
        .send(req)
        .await
        .expect("input channel must accept the request");
    drop(handles.input_tx);

    // Expect no output — the request should have been dropped at RAG.
    let received = timeout(Duration::from_millis(500), output_rx.recv()).await;

    match received {
        Err(_elapsed) => { /* Correct: no output produced. */ }
        Ok(None) => { /* Channel closed without output — also acceptable. */ }
        Ok(Some(output)) => {
            panic!(
                "Expected RAG-expired request to be dropped but received output for {}",
                output.request_id
            );
        }
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    let rag_expired_after = read_rag_expired_total();
    assert!(
        rag_expired_after > rag_expired_before,
        "rag_requests_expired_total must increment when a request expires at RAG. \
         Before: {rag_expired_before}, After: {rag_expired_after}"
    );
}

// ── Test 4: .with_deadline() builder expires immediately ─────────────────

/// The `.with_deadline(Duration::from_millis(1))` builder must create a
/// request whose deadline expires almost immediately.  The pipeline must drop
/// it without producing output.
#[tokio::test]
async fn test_deadline_with_deadline_builder() {
    let _ = metrics::init_metrics();

    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    let mut output_rx = handles
        .output_rx
        .lock()
        .await
        .take()
        .expect("output_rx must be present");

    // Build the request, then sleep briefly so the 1 ms deadline has expired
    // by the time the pipeline dequeues the message.
    let req = make_request("builder-deadline-001").with_deadline(Duration::from_millis(1));

    // Give the deadline time to elapse before the request is dequeued.
    tokio::time::sleep(Duration::from_millis(50)).await;

    handles
        .input_tx
        .send(req)
        .await
        .expect("input channel must accept the request");
    drop(handles.input_tx);

    let received = timeout(Duration::from_millis(500), output_rx.recv()).await;

    match received {
        Err(_elapsed) => { /* Correct: timed out — request was dropped. */ }
        Ok(None) => { /* Channel closed without output — also acceptable. */ }
        Ok(Some(output)) => {
            panic!(
                "Expected with_deadline(1ms) request to be dropped but received output for {}",
                output.request_id
            );
        }
    }
}

// ── Test 5: Concurrent mix of expired and valid requests ──────────────────

/// Send 10 requests: 5 with past deadlines and 5 with a 30-second future
/// deadline.  Exactly 5 outputs must be produced (the valid ones) and no
/// output from the 5 expired ones.
#[tokio::test]
async fn test_concurrent_mix_expired_and_valid() {
    let _ = metrics::init_metrics();

    let expired_before = read_expired_total();
    let rag_expired_before = read_rag_expired_total();

    let worker = Arc::new(EchoWorker::with_delay(0));
    let handles = spawn_pipeline(worker);

    let mut output_rx = handles
        .output_rx
        .lock()
        .await
        .take()
        .expect("output_rx must be present");

    // Send 5 expired + 5 valid requests.
    // Use unique request IDs prefixed so we can distinguish them in output.
    for i in 0..5u32 {
        let req = make_expired_request(&format!("mix-expired-{i:02}"));
        handles
            .input_tx
            .send(req)
            .await
            .expect("input channel must accept expired request");
    }

    for i in 0..5u32 {
        let req = PromptRequest {
            session: SessionId::new(format!("mix-valid-session-{i}")),
            request_id: format!("mix-valid-{i:02}"),
            input: format!("valid input {i}"),
            meta: HashMap::new(),
            deadline: Some(Instant::now() + Duration::from_secs(30)),
        };
        handles
            .input_tx
            .send(req)
            .await
            .expect("input channel must accept valid request");
    }

    drop(handles.input_tx);

    // Collect all outputs that arrive within a generous window.
    let mut outputs = Vec::new();
    let collection_deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    loop {
        let remaining = collection_deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_default();

        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, output_rx.recv()).await {
            Ok(Some(post)) => {
                outputs.push(post);
                // Once we have all 5 valid outputs, stop waiting early.
                if outputs.len() == 5 {
                    break;
                }
            }
            Ok(None) => break,      // Channel closed.
            Err(_elapsed) => break, // Overall timeout reached.
        }
    }

    // Exactly 5 valid requests must have produced output.
    assert_eq!(
        outputs.len(),
        5,
        "Expected exactly 5 outputs (one per valid request), got {}. \
         Outputs: {:?}",
        outputs.len(),
        outputs.iter().map(|o| &o.request_id).collect::<Vec<_>>()
    );

    // Every output must come from a valid request, not an expired one.
    for output in &outputs {
        assert!(
            output.request_id.starts_with("mix-valid-"),
            "Output request_id '{}' must belong to a valid request",
            output.request_id
        );
    }

    // Allow pipeline tasks to flush metrics.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let expired_after = read_expired_total();
    let rag_expired_after = read_rag_expired_total();

    // The 5 expired requests must have incremented at least one expiry counter
    // by a combined total of at least 5.
    let total_expired_delta =
        (expired_after - expired_before) + (rag_expired_after - rag_expired_before);

    assert!(
        total_expired_delta >= 5.0,
        "Expected at least 5 expiry counter increments for the 5 expired requests. \
         requests_expired_total delta: {}, rag_requests_expired_total delta: {}",
        expired_after - expired_before,
        rag_expired_after - rag_expired_before
    );
}
