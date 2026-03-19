//! Explicit circuit breaker state machine transition tests.
//!
//! Covers each edge of the state graph:
//! - CLOSED → OPEN (failure threshold reached)
//! - OPEN → HALF-OPEN (after recovery timeout)
//! - HALF-OPEN → CLOSED (probe succeeds with rate above threshold)
//! - HALF-OPEN → OPEN (probe fails)
//! - Boundary: exactly at the success-rate threshold

use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::circuit_breaker::{
    CircuitBreaker, CircuitBreakerError, CircuitStatus,
};

// ── CLOSED → OPEN ──────────────────────────────────────────────────────────

/// The circuit should transition from CLOSED to OPEN exactly when the number
/// of consecutive failures reaches the configured `failure_threshold`.
#[tokio::test]
async fn test_closed_to_open_on_failure_threshold() {
    let threshold = 3;
    let breaker = CircuitBreaker::new(threshold, 0.8, Duration::from_secs(60));

    // One fewer failure than the threshold must keep the circuit CLOSED.
    for _ in 0..(threshold - 1) {
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
    }
    assert_eq!(
        breaker.status().await,
        CircuitStatus::Closed,
        "circuit must remain CLOSED below the failure threshold"
    );

    // The threshold-th failure must flip it to OPEN.
    let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
    assert_eq!(
        breaker.status().await,
        CircuitStatus::Open,
        "circuit must transition to OPEN once the failure threshold is reached"
    );

    // Subsequent calls must be rejected without executing the operation.
    let result: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
    assert!(
        matches!(result, Err(CircuitBreakerError::Open)),
        "OPEN circuit must reject requests immediately"
    );
}

// ── OPEN → HALF-OPEN ───────────────────────────────────────────────────────

/// After the recovery timeout elapses the circuit must transition from OPEN
/// to HALF-OPEN and allow a probe request through.
#[tokio::test]
async fn test_open_to_half_open_after_timeout() {
    let recovery = Duration::from_millis(80);
    let breaker = CircuitBreaker::new(1, 0.8, recovery);

    // Trip the circuit.
    let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
    assert_eq!(breaker.status().await, CircuitStatus::Open);

    // Still within the timeout — requests must be rejected.
    let result: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
    assert!(
        matches!(result, Err(CircuitBreakerError::Open)),
        "requests within the timeout window must still be rejected"
    );

    // Wait for the timeout to elapse.
    tokio::time::sleep(Duration::from_millis(120)).await;

    // The next call triggers the OPEN → HALF-OPEN transition.  The operation
    // itself succeeds, so after this call the circuit is HALF-OPEN (or already
    // CLOSED if the single probe was enough — but with 0.8 threshold and one
    // result we expect HALF-OPEN still given window math).
    let probe: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
    assert!(
        probe.is_ok(),
        "probe request through HALF-OPEN circuit must execute successfully"
    );

    // Status must now be either HALF-OPEN or CLOSED (never OPEN).
    let status = breaker.status().await;
    assert_ne!(
        status,
        CircuitStatus::Open,
        "circuit must not be OPEN after a successful probe past the timeout"
    );
}

// ── HALF-OPEN → CLOSED ─────────────────────────────────────────────────────

/// Once in HALF-OPEN, enough successful probes must close the circuit.
#[tokio::test]
async fn test_half_open_to_closed_on_success() {
    // Use a low success threshold (50%) so a few probes are enough.
    let breaker = CircuitBreaker::new(1, 0.5, Duration::from_millis(50));

    // Trip the circuit.
    let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
    assert_eq!(breaker.status().await, CircuitStatus::Open);

    // Wait for recovery.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Send several successful probes — the circuit should close.
    for _ in 0..5 {
        let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
    }

    assert_eq!(
        breaker.status().await,
        CircuitStatus::Closed,
        "circuit must transition to CLOSED after sufficient successes in HALF-OPEN"
    );
}

// ── HALF-OPEN → OPEN ───────────────────────────────────────────────────────

/// A failure during a HALF-OPEN probe must re-open the circuit immediately.
#[tokio::test]
async fn test_half_open_to_open_on_probe_failure() {
    let breaker = CircuitBreaker::new(1, 0.8, Duration::from_millis(40));

    // Trip the circuit.
    let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
    assert_eq!(breaker.status().await, CircuitStatus::Open);

    // Wait for the timeout.
    tokio::time::sleep(Duration::from_millis(70)).await;

    // Send a failing probe — must flip back to OPEN.
    let probe: Result<(), CircuitBreakerError<String>> =
        breaker.call(|| async { Err("probe failure".to_string()) }).await;
    assert!(probe.is_err(), "failing probe must propagate the error");

    assert_eq!(
        breaker.status().await,
        CircuitStatus::Open,
        "circuit must re-open after a failing probe in HALF-OPEN state"
    );

    // Requests must now be rejected again.
    let result: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;
    assert!(
        matches!(result, Err(CircuitBreakerError::Open)),
        "re-opened circuit must reject subsequent requests"
    );
}

// ── Boundary: exactly at the success-rate threshold ────────────────────────

/// When the success rate in HALF-OPEN equals exactly the configured threshold,
/// the circuit must close (>= comparison).
#[tokio::test]
async fn test_half_open_closes_at_exact_success_rate_threshold() {
    // threshold = 1.0 means every probe must succeed.
    let breaker = CircuitBreaker::new(1, 1.0, Duration::from_millis(40));

    // Trip the circuit.
    let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Err(()) }).await;
    assert_eq!(breaker.status().await, CircuitStatus::Open);

    // Wait for recovery.
    tokio::time::sleep(Duration::from_millis(70)).await;

    // A single successful probe achieves a 100% success rate — exactly the
    // threshold — so the circuit must close.
    let _: Result<(), CircuitBreakerError<()>> = breaker.call(|| async { Ok(()) }).await;

    assert_eq!(
        breaker.status().await,
        CircuitStatus::Closed,
        "circuit must close when the success rate equals exactly the configured threshold"
    );
}
