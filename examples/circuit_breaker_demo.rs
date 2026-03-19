//! # Example: Circuit Breaker Demo
//!
//! Demonstrates: Circuit breaker state transitions — CLOSED → OPEN → HALF-OPEN → CLOSED
//!
//! Run with: `cargo run --example circuit_breaker_demo`
//! Features needed: none

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::{
    circuit_breaker::CircuitBreakerError, CircuitBreaker, CircuitStatus,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Circuit Breaker Demo");
    info!("====================");
    info!("");
    info!("This demo walks a circuit breaker through its full lifecycle:");
    info!("  CLOSED (working) -> OPEN (failing) -> HALF-OPEN (probe) -> CLOSED (recovered)");
    info!("");

    // Create a circuit breaker that:
    //   - opens after 3 consecutive failures
    //   - requires 80% success rate to close
    //   - waits 2 seconds before probing (HALF-OPEN)
    let cb = CircuitBreaker::new(
        3,                       // failure threshold
        0.8,                     // success rate required to re-close
        Duration::from_secs(2),  // timeout before HALF-OPEN probe
    );

    // Track how many calls the "backend" actually received so we can show
    // that OPEN state fast-fails without touching the service.
    let backend_calls = Arc::new(AtomicUsize::new(0));

    // Simulate a worker that fails the first N calls then recovers.
    let fail_until = Arc::new(AtomicUsize::new(4)); // fail calls 1-4

    // ── Phase 1: CLOSED — happy path ────────────────────────────────────
    info!("Phase 1: CLOSED state — requests flowing normally");
    info!("---------------------------------------------------");

    for i in 1..=2 {
        let backend_calls = backend_calls.clone();
        let result = cb
            .call(|| async move {
                backend_calls.fetch_add(1, Ordering::Relaxed);
                Ok::<&str, &str>("ok")
            })
            .await;

        let stats = cb.stats().await;
        info!(
            "  Call {i}: {:?} | CB state: {:?}",
            result.map(|_| "success"),
            stats.status
        );
    }

    // ── Phase 2: Drive the circuit OPEN ─────────────────────────────────
    info!("");
    info!("Phase 2: Injecting failures to trip the circuit OPEN");
    info!("------------------------------------------------------");

    let failure_counter = Arc::new(AtomicUsize::new(0));

    for i in 3..=7 {
        let backend_calls_c = backend_calls.clone();
        let fail_until_c = fail_until.clone();
        let failure_counter_c = failure_counter.clone();

        let result = cb
            .call(|| async move {
                let call_num = backend_calls_c.fetch_add(1, Ordering::Relaxed) + 1;
                let threshold = fail_until_c.load(Ordering::Relaxed);
                if call_num <= threshold {
                    failure_counter_c.fetch_add(1, Ordering::Relaxed);
                    Err("service unavailable")
                } else {
                    Ok("ok")
                }
            })
            .await;

        let stats = cb.stats().await;
        match &result {
            Ok(_) => info!("  Call {i}: SUCCESS | CB state: {:?}", stats.status),
            Err(CircuitBreakerError::Open) => {
                info!("  Call {i}: FAST-FAIL (circuit OPEN — backend not called) | CB state: {:?}", stats.status);
            }
            Err(CircuitBreakerError::Failed(e)) => {
                info!("  Call {i}: FAILURE ({e}) | CB state: {:?}", stats.status);
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let total_backend = backend_calls.load(Ordering::Relaxed);
    info!("");
    info!(
        "  Backend received {} calls; {} fast-failed by the open circuit",
        total_backend,
        7 - total_backend
    );

    // ── Phase 3: Wait for HALF-OPEN timeout ─────────────────────────────
    info!("");
    info!("Phase 3: Waiting 2s for the open-timeout to expire -> HALF-OPEN");
    info!("-----------------------------------------------------------------");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let stats = cb.stats().await;
    info!("  CB state after wait: {:?}", stats.status);

    // ── Phase 4: HALF-OPEN probe succeeds -> CLOSED ──────────────────────
    info!("");
    info!("Phase 4: HALF-OPEN probe — a single request tests recovery");
    info!("------------------------------------------------------------");

    // Make the backend healthy now
    fail_until.store(0, Ordering::Relaxed);
    backend_calls.store(0, Ordering::Relaxed);

    let backend_calls_probe = backend_calls.clone();
    let result = cb
        .call(|| async move {
            // Backend is now healthy
            backend_calls_probe.fetch_add(1, Ordering::Relaxed);
            Ok::<&str, &str>("recovered!")
        })
        .await;

    let stats = cb.stats().await;
    match result {
        Ok(v) => info!("  Probe result: {v} | CB state: {:?}", stats.status),
        Err(e) => info!("  Probe error: {e:?} | CB state: {:?}", stats.status),
    }

    // ── Phase 5: Confirm CLOSED — normal operation resumed ──────────────
    info!("");
    info!("Phase 5: Confirming CLOSED state — normal operation");
    info!("----------------------------------------------------");

    for i in 1..=3 {
        let backend_calls_c = backend_calls.clone();
        let result = cb
            .call(|| async move {
                backend_calls_c.fetch_add(1, Ordering::Relaxed);
                Ok::<&str, &str>("all good")
            })
            .await;

        let stats = cb.stats().await;
        info!(
            "  Post-recovery call {i}: {:?} | CB state: {:?}",
            result.map(|_| "success"),
            stats.status
        );
    }

    // ── Summary ──────────────────────────────────────────────────────────
    info!("");
    info!("Summary");
    info!("=======");
    let final_stats = cb.stats().await;
    info!("  Final CB state:    {:?}", final_stats.status);
    info!("  Total successes:   {}", final_stats.successes);
    info!("  Total failures:    {}", final_stats.failures);
    info!("  Success rate:      {:.0}%", final_stats.success_rate * 100.0);
    info!("");

    let expected_final = CircuitStatus::Closed;
    if final_stats.status == expected_final {
        info!("  Circuit breaker successfully completed CLOSED -> OPEN -> HALF-OPEN -> CLOSED cycle.");
    }

    Ok(())
}
