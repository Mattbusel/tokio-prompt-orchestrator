//! # Circuit Breaker Behavior Under 50 Rapid Requests
//!
//! Integration test that sends 50 rapid requests through the circuit breaker,
//! logging state transitions after every batch of 10.
//!
//! This test documents the exact threshold at which the circuit opens and the
//! state machine transitions (Closed → Open → HalfOpen → Closed).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_prompt_orchestrator::enhanced::circuit_breaker::{
    CircuitBreaker, CircuitBreakerError, CircuitStatus,
};

/// Simulated worker that fails after `fail_after` consecutive calls,
/// then recovers after `recover_after` total calls.
struct SimulatedWorker {
    call_count: AtomicUsize,
    fail_after: usize,
    recover_after: usize,
}

impl SimulatedWorker {
    fn new(fail_after: usize, recover_after: usize) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            fail_after,
            recover_after,
        }
    }

    async fn infer(&self) -> Result<String, String> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        if n >= self.fail_after && n < self.recover_after {
            Err(format!("simulated failure at call {n}"))
        } else {
            Ok(format!("response for call {n}"))
        }
    }
}

/// Result log entry for each request.
#[derive(Debug)]
struct RequestLog {
    index: usize,
    outcome: String,
    circuit_status: CircuitStatus,
    failures: usize,
    successes: usize,
    success_rate: f64,
}

/// Status snapshot after each batch of 10.
#[derive(Debug)]
struct BatchSnapshot {
    batch: usize,
    requests_completed: usize,
    circuit_status: CircuitStatus,
    failures: usize,
    successes: usize,
    success_rate: f64,
    elapsed_ms: u128,
}

#[tokio::test]
async fn test_50_rapid_requests_circuit_breaker_behavior() {
    // Configuration: threshold=5 failures, 80% success to close, 200ms recovery timeout
    let breaker = CircuitBreaker::new(5, 0.8, Duration::from_millis(200));

    // Worker fails on calls 8..28 (requests index 8-27 inclusive → 20 failures)
    // This means: first 8 succeed, next 20 fail, remaining succeed
    let worker = Arc::new(SimulatedWorker::new(8, 28));

    let start = Instant::now();
    let mut logs: Vec<RequestLog> = Vec::with_capacity(50);
    let mut snapshots: Vec<BatchSnapshot> = Vec::new();
    let mut open_detected_at: Option<usize> = None;

    for i in 0..50 {
        let worker_ref = Arc::clone(&worker);
        let result: Result<String, CircuitBreakerError<String>> =
            breaker.call(|| worker_ref.infer()).await;

        let outcome = match &result {
            Ok(msg) => format!("OK: {msg}"),
            Err(CircuitBreakerError::Open) => "REJECTED: circuit open".to_string(),
            Err(CircuitBreakerError::Failed(e)) => format!("FAILED: {e}"),
        };

        let stats = breaker.stats().await;

        if stats.status == CircuitStatus::Open && open_detected_at.is_none() {
            open_detected_at = Some(i);
        }

        logs.push(RequestLog {
            index: i,
            outcome,
            circuit_status: stats.status.clone(),
            failures: stats.failures,
            successes: stats.successes,
            success_rate: stats.success_rate,
        });

        // After every batch of 10, snapshot the state
        if (i + 1) % 10 == 0 {
            let stats = breaker.stats().await;
            snapshots.push(BatchSnapshot {
                batch: (i + 1) / 10,
                requests_completed: i + 1,
                circuit_status: stats.status.clone(),
                failures: stats.failures,
                successes: stats.successes,
                success_rate: stats.success_rate,
                elapsed_ms: start.elapsed().as_millis(),
            });
        }

        // After batch 3 (request 30), if circuit is open, sleep to trigger half-open
        if i == 29 && breaker.status().await == CircuitStatus::Open {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    let total_elapsed = start.elapsed();

    // ─── Assertions ─────────────────────────────────────────────────────
    // Circuit must have opened at some point
    assert!(
        open_detected_at.is_some(),
        "circuit breaker never opened during 50 requests"
    );

    // It should open at request index 12 (after 5 consecutive failures: calls 8..12)
    // (first 8 succeed, then failures at 8,9,10,11,12 = 5 failures → opens)
    let opened_at = open_detected_at.expect("already checked");
    assert!(
        opened_at <= 15,
        "circuit opened too late: request {opened_at}"
    );

    // ─── Print report ───────────────────────────────────────────────────
    println!("\n========================================================================");
    println!("CIRCUIT BREAKER BEHAVIOR REPORT - 50 RAPID REQUESTS");
    println!("========================================================================\n");

    println!("Configuration:");
    println!("  failure_threshold: 5");
    println!("  success_threshold: 80%");
    println!("  recovery_timeout:  200ms");
    println!("  worker fails on calls 8..27 (20 failures injected)\n");

    println!("Circuit opened at: request #{opened_at}");
    println!(
        "Total elapsed: {:.1}ms\n",
        total_elapsed.as_secs_f64() * 1000.0
    );

    println!("─── Batch Snapshots (every 10 requests) ───\n");
    for snap in &snapshots {
        println!(
            "  Batch {}: requests={}, status={:?}, failures={}, successes={}, \
             success_rate={:.1}%, elapsed={}ms",
            snap.batch,
            snap.requests_completed,
            snap.circuit_status,
            snap.failures,
            snap.successes,
            snap.success_rate * 100.0,
            snap.elapsed_ms,
        );
    }

    println!("\n─── Per-Request Detail ───\n");
    for log in &logs {
        let marker = match log.circuit_status {
            CircuitStatus::Closed => "CLOSED   ",
            CircuitStatus::Open => "OPEN     ",
            CircuitStatus::HalfOpen => "HALF-OPEN",
        };
        println!(
            "  [{:02}] {marker} | failures={} success_rate={:.0}% | {}",
            log.index,
            log.failures,
            log.success_rate * 100.0,
            log.outcome,
        );
    }
}
