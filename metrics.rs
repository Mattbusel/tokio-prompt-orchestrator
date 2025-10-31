//! Metrics recording hooks
//!
//! Currently implemented via tracing::info! for zero-dependency MVP.
//!
//! TODO: Add feature flags for:
//! - `metrics-prometheus` → use prometheus-client crate
//! - `metrics-otlp` → OpenTelemetry exporters
//! - `metrics-statsd` → StatsD protocol

use std::time::Duration;

/// Record current queue depth for a stage
///
/// Future: histogram metric with stage label
pub fn record_queue_depth(stage: &str, depth: usize) {
    tracing::info!(
        metric = "queue_depth",
        stage = stage,
        depth = depth,
        "queue depth sampled"
    );
}

/// Record stage processing latency
///
/// Future: histogram metric with stage label and percentiles
pub fn record_stage_latency(stage: &str, dur: Duration) {
    tracing::info!(
        metric = "stage_latency_ms",
        stage = stage,
        latency_ms = dur.as_millis() as u64,
        "stage processing completed"
    );
}

/// Record request shed event
///
/// Future: counter metric with stage label
pub fn record_shed(stage: &str) {
    tracing::warn!(
        metric = "requests_shed",
        stage = stage,
        "request shed due to backpressure"
    );
}

/// Record stage throughput (requests processed)
///
/// Future: counter metric with stage label
pub fn record_throughput(stage: &str, count: u64) {
    tracing::debug!(
        metric = "requests_processed",
        stage = stage,
        count = count,
        "requests processed"
    );
}
