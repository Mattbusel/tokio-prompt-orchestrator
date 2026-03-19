//! TUI live mode failure recovery tests (test 28).
//!
//! Tests:
//! - Network error sets `metrics_fetch_error` in app state (via error return)
//! - Successful fetch after error clears the error state
//! - Error does NOT update live metrics values (last known values preserved)
//! - App state remains consistent after 5 consecutive failures (no panics, no NaN)
//!
//! Because the `App` struct does not have a `metrics_fetch_error` field we track
//! error state through the `Result` returned by `LiveMetrics::tick` and simulate
//! the caller's error-tracking pattern with a local boolean.
//!
//! `wiremock` (already in dev-dependencies) is used to simulate HTTP failures.

#![cfg(feature = "tui")]

use std::time::Duration;

use tokio_prompt_orchestrator::tui::app::App;
use tokio_prompt_orchestrator::tui::metrics::LiveMetrics;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ============================================================================
// Helper: build an App with sensible initial live metrics so we can detect
// whether error paths preserve or clobber values.
// ============================================================================

fn app_with_known_metrics() -> App {
    let mut app = App::new(Duration::from_secs(1));
    // Pre-populate known values by parsing a small Prometheus body.
    let mut live = LiveMetrics::new("unused".into());
    let body = "orchestrator_requests_total 42\n\
                orchestrator_inferences_total 10\n\
                orchestrator_stage_duration_seconds{stage=\"inference\"} 0.250\n";
    live.parse_metrics(body, &mut app);
    app
}

// ============================================================================
// Test 28a – Network error causes tick() to return Err (error is detectable)
// ============================================================================

/// Simulates the scenario where the metrics endpoint is unreachable.
/// `LiveMetrics::tick` must return `Err(...)`, which the caller would use to
/// set `metrics_fetch_error = true`.
#[tokio::test]
async fn test_live_mode_network_error_tick_returns_err() {
    // Point at a port where nothing is listening.
    let url = "http://127.0.0.1:1"; // port 1 is privileged and never open
    let mut live = LiveMetrics::new(url.to_string());
    let mut app = App::new(Duration::from_secs(1));

    let result = live.tick(&mut app).await;
    assert!(
        result.is_err(),
        "tick() must return Err when the endpoint is unreachable"
    );
}

// ============================================================================
// Test 28b – Caller can track error state and clear it on success
// ============================================================================

/// Simulates the typical caller pattern:
/// 1. First tick fails → set `metrics_fetch_error = true`
/// 2. Second tick succeeds → reset `metrics_fetch_error = false`
#[tokio::test]
async fn test_live_mode_error_cleared_on_successful_fetch() {
    let mock_server = MockServer::start().await;

    // First request: simulate connection refused by mounting a 500 handler,
    // then checking Err. We'll use the server itself but return a non-2xx so
    // the body parse still reaches reqwest (reqwest does not error on 5xx).
    // Instead we test with a truly broken URL for the first call.
    let broken_url = "http://127.0.0.1:1/metrics";
    let mut live_broken = LiveMetrics::new(broken_url.to_string());
    let mut app = App::new(Duration::from_secs(1));

    // Simulate caller error-tracking pattern
    let mut metrics_fetch_error = false;

    let result = live_broken.tick(&mut app).await;
    if result.is_err() {
        metrics_fetch_error = true;
    }
    assert!(
        metrics_fetch_error,
        "metrics_fetch_error should be true after network failure"
    );

    // Now mount a successful response on the mock server.
    Mock::given(method("GET"))
        .and(path("/metrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "orchestrator_requests_total 99\norchestrator_inferences_total 20\n",
        ))
        .mount(&mock_server)
        .await;

    let mut live_ok = LiveMetrics::new(format!("{}/metrics", mock_server.uri()));
    let result2 = live_ok.tick(&mut app).await;
    if result2.is_ok() {
        metrics_fetch_error = false;
    }
    assert!(
        !metrics_fetch_error,
        "metrics_fetch_error should be false after successful fetch"
    );
}

// ============================================================================
// Test 28c – Error does NOT update live metric values
// ============================================================================

/// After a successful fetch populates metrics, a subsequent network failure
/// must leave the metric values unchanged (last-known values are preserved).
#[tokio::test]
async fn test_live_mode_error_preserves_last_known_values() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/metrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "orchestrator_requests_total 77\norchestrator_inferences_total 15\n",
        ))
        .mount(&mock_server)
        .await;

    let mut live = LiveMetrics::new(format!("{}/metrics", mock_server.uri()));
    let mut app = App::new(Duration::from_secs(1));

    // First tick succeeds — populate values.
    live.tick(&mut app).await.expect("first tick must succeed");
    let requests_after_success = app.requests_total;
    let inferences_after_success = app.inferences_total;

    assert_eq!(
        requests_after_success, 77,
        "requests_total should be 77 after successful fetch"
    );
    assert_eq!(
        inferences_after_success, 15,
        "inferences_total should be 15 after successful fetch"
    );

    // Second tick: use a broken URL so it fails.
    let mut live_broken = LiveMetrics::new("http://127.0.0.1:1/metrics".to_string());
    let result = live_broken.tick(&mut app).await;
    assert!(result.is_err(), "broken tick must return Err");

    // Values must be unchanged.
    assert_eq!(
        app.requests_total, requests_after_success,
        "requests_total must be preserved after error (was {}, now {})",
        requests_after_success, app.requests_total
    );
    assert_eq!(
        app.inferences_total, inferences_after_success,
        "inferences_total must be preserved after error (was {}, now {})",
        inferences_after_success, app.inferences_total
    );
}

// ============================================================================
// Test 28d – 5 consecutive failures leave app state consistent (no NaN/panics)
// ============================================================================

/// After 5 consecutive network errors the app state must be consistent:
/// no NaN values, no panics, metrics remain at their last-known good values.
#[tokio::test]
async fn test_live_mode_five_consecutive_failures_no_nan_no_panic() {
    // Start with known good state.
    let mut app = app_with_known_metrics();
    let requests_before = app.requests_total;
    let inferences_before = app.inferences_total;

    let broken_url = "http://127.0.0.1:1/metrics";
    let mut live = LiveMetrics::new(broken_url.to_string());

    let mut consecutive_errors = 0u32;
    for _ in 0..5 {
        let r = live.tick(&mut app).await;
        if r.is_err() {
            consecutive_errors += 1;
        }
    }

    assert_eq!(consecutive_errors, 5, "All 5 ticks must fail");

    // Verify no NaN in stage latencies.
    for (i, &lat) in app.stage_latencies.iter().enumerate() {
        assert!(
            !lat.is_nan(),
            "stage_latencies[{i}] is NaN after 5 consecutive errors"
        );
        assert!(
            !lat.is_infinite(),
            "stage_latencies[{i}] is infinite after 5 consecutive errors"
        );
    }

    // Verify no NaN in health metrics.
    assert!(
        !app.cpu_percent.is_nan(),
        "cpu_percent is NaN after 5 consecutive errors"
    );
    assert!(
        !app.mem_percent.is_nan(),
        "mem_percent is NaN after 5 consecutive errors"
    );
    assert!(
        !app.cost_saved_usd.is_nan(),
        "cost_saved_usd is NaN after 5 consecutive errors"
    );

    // Metric totals must be unchanged (errors must not zero them out).
    assert_eq!(
        app.requests_total, requests_before,
        "requests_total must be unchanged after 5 failures"
    );
    assert_eq!(
        app.inferences_total, inferences_before,
        "inferences_total must be unchanged after 5 failures"
    );
}

// ============================================================================
// Test 28e – Tick count does NOT increment on error
// ============================================================================

/// When `LiveMetrics::tick` returns an error the tick_count must not be
/// incremented (it is only incremented on success inside `tick()`).
#[tokio::test]
async fn test_live_mode_tick_count_not_incremented_on_error() {
    let mut app = App::new(Duration::from_secs(1));
    assert_eq!(app.tick_count, 0);

    let mut live = LiveMetrics::new("http://127.0.0.1:1/metrics".to_string());
    let _ = live.tick(&mut app).await;

    assert_eq!(
        app.tick_count, 0,
        "tick_count must not be incremented when tick() fails"
    );
}

// ============================================================================
// Test 28f – Recovery: successful tick after 3 errors resumes metric updates
// ============================================================================

/// After several consecutive failures a successful tick should update metrics
/// correctly, demonstrating full recovery.
#[tokio::test]
async fn test_live_mode_recovery_after_errors_resumes_metrics() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/metrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "orchestrator_requests_total 500\norchestrator_inferences_total 100\n",
        ))
        .mount(&mock_server)
        .await;

    let mut app = App::new(Duration::from_secs(1));

    // Simulate 3 consecutive failures (different instance pointing at broken URL).
    {
        let mut broken = LiveMetrics::new("http://127.0.0.1:1/metrics".to_string());
        for _ in 0..3 {
            let _ = broken.tick(&mut app).await;
        }
    }
    assert_eq!(app.requests_total, 0, "no metrics yet");

    // Now recover with a successful tick.
    let mut live = LiveMetrics::new(format!("{}/metrics", mock_server.uri()));
    live.tick(&mut app).await.expect("recovery tick must succeed");

    assert_eq!(
        app.requests_total, 500,
        "After recovery requests_total should be updated"
    );
    assert_eq!(
        app.inferences_total, 100,
        "After recovery inferences_total should be updated"
    );
}
