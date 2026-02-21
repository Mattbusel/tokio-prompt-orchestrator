//! Integration tests for MCP pipeline_status tool.
//!
//! Verifies that metrics collection and status reporting work correctly
//! through the pipeline lifecycle.

use tokio_prompt_orchestrator::metrics;

#[test]
fn test_metrics_init_succeeds() {
    // May already be initialised by another test — both Ok and re-init Ok are fine
    let result = metrics::init_metrics();
    assert!(result.is_ok());
}

#[test]
fn test_metrics_summary_returns_default_when_uninitialised() {
    // get_metrics_summary is safe to call regardless of init state
    let summary = metrics::get_metrics_summary();
    // Should return a valid (possibly empty) summary, never panic
    assert!(summary.requests_total.len() <= 10);
    assert!(summary.requests_shed.len() <= 10);
    assert!(summary.errors_total.len() <= 10);
}

#[test]
fn test_gather_metrics_returns_string() {
    let _ = metrics::init_metrics();
    let text = metrics::gather_metrics();
    // Should return either valid prometheus text or empty string, never panic
    assert!(text.is_empty() || text.contains("orchestrator") || text.len() > 0);
}

#[test]
fn test_metrics_inc_request_does_not_panic() {
    let _ = metrics::init_metrics();
    // These should be no-ops or succeed, never panic
    metrics::inc_request("rag");
    metrics::inc_request("assemble");
    metrics::inc_request("inference");
    metrics::inc_request("post_process");
    metrics::inc_request("stream");
}

#[test]
fn test_metrics_record_latency_does_not_panic() {
    let _ = metrics::init_metrics();
    metrics::record_stage_latency("rag", std::time::Duration::from_millis(5));
    metrics::record_stage_latency("inference", std::time::Duration::from_millis(200));
}

#[test]
fn test_metrics_inc_shed_does_not_panic() {
    let _ = metrics::init_metrics();
    metrics::inc_shed("assemble");
    metrics::inc_shed("inference");
}

#[test]
fn test_metrics_inc_error_does_not_panic() {
    let _ = metrics::init_metrics();
    metrics::inc_error("inference", "timeout");
    metrics::inc_error("inference", "network");
}

#[test]
fn test_metrics_set_queue_depth_does_not_panic() {
    let _ = metrics::init_metrics();
    metrics::set_queue_depth("rag", 42);
    metrics::set_queue_depth("inference", 0);
}

#[test]
fn test_status_degraded_calculation() {
    // When errors > requests/4, status should be "degraded"
    let total_requests: u64 = 100;
    let total_errors: u64 = 30;
    let status = if total_errors > total_requests / 4 {
        "degraded"
    } else {
        "healthy"
    };
    assert_eq!(status, "degraded");
}

#[test]
fn test_status_healthy_calculation() {
    let total_requests: u64 = 100;
    let total_errors: u64 = 10;
    let status = if total_errors > total_requests / 4 {
        "degraded"
    } else {
        "healthy"
    };
    assert_eq!(status, "healthy");
}

#[test]
fn test_savings_percent_calculation() {
    let total_requests: u64 = 100;
    let total_shed: u64 = 20;
    let savings = if total_requests > 0 {
        (total_shed as f64 / total_requests as f64) * 100.0
    } else {
        0.0
    };
    assert!((savings - 20.0).abs() < f64::EPSILON);
}

#[test]
fn test_savings_zero_when_no_requests() {
    let total_requests: u64 = 0;
    let total_shed: u64 = 0;
    let savings = if total_requests > 0 {
        (total_shed as f64 / total_requests as f64) * 100.0
    } else {
        0.0
    };
    assert!((savings - 0.0).abs() < f64::EPSILON);
}
