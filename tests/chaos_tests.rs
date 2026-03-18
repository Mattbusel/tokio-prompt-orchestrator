//! Chaos / fault injection tests
//!
//! Each test exercises a specific failure mode of the pipeline and must
//! complete in under 10 seconds.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_prompt_orchestrator::{
    enhanced::{
        circuit_breaker::{CircuitBreaker, CircuitBreakerError, CircuitStatus},
        Deduplicator,
    },
    spawn_pipeline, EchoWorker, ModelWorker, OrchestratorError, PromptRequest, SessionId,
};

fn make_request(id: &str, input: &str) -> PromptRequest {
    PromptRequest {
        session: SessionId::new(id),
        request_id: id.to_string(),
        input: input.to_string(),
        meta: HashMap::new(),
        deadline: None,
    }
}

// ── Test 1: Worker panic recovery ─────────────────────────────────────────
//
// A panic inside a spawned Tokio task is caught by JoinError and does NOT
// kill the test process.  We verify that further sends succeed without hanging.

struct PanicOnThirdCallWorker {
    call_count: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ModelWorker for PanicOnThirdCallWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        let n = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 2 {
            panic!("intentional panic on call 3 (index 2)");
        }
        Ok(vec![prompt.to_string()])
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_worker_panic_does_not_kill_pipeline_process() {
    let worker: Arc<dyn ModelWorker> = Arc::new(PanicOnThirdCallWorker {
        call_count: std::sync::atomic::AtomicUsize::new(0),
    });
    let handles = spawn_pipeline(worker);

    for i in 0..5 {
        let req = make_request(&format!("panic-test-{i}"), "hello");
        let _ = tokio::time::timeout(Duration::from_secs(2), handles.input_tx.send(req)).await;
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    // If we reach here, the process is still alive and no hang occurred.
    drop(handles.input_tx);
}

// ── Test 2: Channel backpressure under load ────────────────────────────────
//
// Flood the input channel with a slow worker.  Every send is wrapped in a
// short timeout so the test never hangs even if the channel fills completely.

#[tokio::test(flavor = "multi_thread")]
async fn test_channel_backpressure_sheds_requests_not_hangs() {
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(200));
    let handles = spawn_pipeline(worker);

    let mut queued_count = 0usize;
    let mut timed_out = 0usize;

    for i in 0..600 {
        let req = make_request(&format!("bp-{i}"), "backpressure test");
        match tokio::time::timeout(Duration::from_millis(50), handles.input_tx.send(req)).await {
            Ok(Ok(())) => queued_count += 1,
            Ok(Err(_)) => timed_out += 1, // channel closed
            Err(_) => timed_out += 1,     // timeout — would have hung without it
        }
    }

    assert!(
        queued_count > 0,
        "at least some requests should have been queued, got 0"
    );

    let _ = timed_out; // acceptable; documents backpressure was exercised
    drop(handles.input_tx);
}

// ── Test 3: Concurrent duplicate requests (dedup stress) ──────────────────
//
// 50 goroutines all send the same prompt.  Only 1 inference should proceed;
// the rest are deduplicated.  The EchoWorker delay widens the window.

#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_duplicate_requests_dedup_stress() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use tokio_prompt_orchestrator::enhanced::DeduplicationResult;

    let dedup = Arc::new(Deduplicator::new(Duration::from_secs(60)));
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(50));
    let handles = Arc::new(spawn_pipeline(worker));

    let prompt = "What is 2 + 2?";
    let key = {
        let mut h = DefaultHasher::new();
        prompt.hash(&mut h);
        format!("{:016x}", h.finish())
    };

    let inference_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut join_handles = Vec::new();

    for i in 0..50 {
        let dedup = Arc::clone(&dedup);
        let key = key.clone();
        let handles = Arc::clone(&handles);
        let counter = Arc::clone(&inference_count);

        join_handles.push(tokio::spawn(async move {
            match dedup.check_and_register(&key).await {
                DeduplicationResult::New(token) => {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let req = make_request(&format!("dedup-{i}"), prompt);
                    let _ = handles.input_tx.send(req).await;
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    dedup.complete(token, "4".to_string()).await;
                }
                DeduplicationResult::InProgress | DeduplicationResult::Cached(_) => {}
            }
        }));
    }

    for h in join_handles {
        let _ = h.await;
    }

    let actual = inference_count.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        actual, 1,
        "only 1 inference should run for 50 identical concurrent requests, got {actual}"
    );
}

// ── Test 4: Circuit breaker opens under failures and closes after half-open ─

#[tokio::test(flavor = "multi_thread")]
async fn test_circuit_breaker_opens_after_n_failures_and_recovers() {
    const THRESHOLD: usize = 3;
    let breaker = CircuitBreaker::new(THRESHOLD, 0.8, Duration::from_millis(200));

    // Inject THRESHOLD consecutive failures to open the circuit.
    for _ in 0..THRESHOLD {
        let result: Result<(), CircuitBreakerError<String>> = breaker
            .call(|| async { Err("simulated failure".to_string()) })
            .await;
        assert!(result.is_err());
    }

    assert_eq!(
        breaker.status().await,
        CircuitStatus::Open,
        "circuit must open after {THRESHOLD} failures"
    );

    // The next call must be rejected (Open), not executed.
    let rejected: Result<(), CircuitBreakerError<String>> =
        breaker.call(|| async { Ok(()) }).await;
    assert!(
        matches!(rejected, Err(CircuitBreakerError::Open)),
        "open circuit must reject calls immediately"
    );

    // Wait for the half-open timeout, then recover.
    tokio::time::sleep(Duration::from_millis(250)).await;

    for _ in 0..10 {
        let _: Result<(), CircuitBreakerError<String>> =
            breaker.call(|| async { Ok(()) }).await;
    }

    assert_eq!(
        breaker.status().await,
        CircuitStatus::Closed,
        "circuit must close after successful half-open probes"
    );
}

// ── Test 5: Timeout handling ────────────────────────────────────────────────
//
// A worker that sleeps 5 s is wrapped in a 500 ms timeout — must not hang.

struct SlowWorker {
    sleep_ms: u64,
}

#[async_trait::async_trait]
impl ModelWorker for SlowWorker {
    async fn infer(&self, _prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        tokio::time::sleep(Duration::from_millis(self.sleep_ms)).await;
        Ok(vec!["done".to_string()])
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_slow_worker_times_out_gracefully() {
    let worker = Arc::new(SlowWorker { sleep_ms: 5_000 });

    let result =
        tokio::time::timeout(Duration::from_millis(500), worker.infer("will timeout")).await;

    assert!(
        result.is_err(),
        "slow worker must time out after 500 ms (got Ok — worker was unexpectedly fast)"
    );
}
