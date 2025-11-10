//! Metrics recording with Prometheus
//!
//! Provides production-grade metrics collection using Prometheus.
//! Metrics are exposed via HTTP endpoint on port 9090.
//!
//! ## Available Metrics
//!
//! ### Counters
//! - `orchestrator_requests_total` - Total requests processed per stage
//! - `orchestrator_requests_shed_total` - Requests dropped due to backpressure
//! - `orchestrator_errors_total` - Errors per stage
//!
//! ### Gauges
//! - `orchestrator_queue_depth` - Current queue depth per stage
//!
//! ### Histograms
//! - `orchestrator_stage_duration_seconds` - Processing latency per stage
//! - `orchestrator_request_duration_seconds` - End-to-end request latency
//!
//! ## Usage
//!
//! ```rust
//! use tokio_prompt_orchestrator::metrics;
//!
//! // Record stage latency
//! metrics::record_stage_latency("inference", duration);
//!
//! // Record queue depth
//! metrics::record_queue_depth("rag", 42);
//!
//! // Record throughput
//! metrics::record_throughput("post", 1);
//! ```

use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge_vec, register_histogram_vec, CounterVec, GaugeVec,
    HistogramVec, TextEncoder, Encoder,
};
use std::time::Duration;

lazy_static! {
    /// Total requests processed per stage
    pub static ref REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "orchestrator_requests_total",
        "Total number of requests processed",
        &["stage"]
    )
    .unwrap();

    /// Requests shed due to backpressure per stage
    pub static ref REQUESTS_SHED: CounterVec = register_counter_vec!(
        "orchestrator_requests_shed_total",
        "Total number of requests shed due to backpressure",
        &["stage"]
    )
    .unwrap();

    /// Errors per stage
    pub static ref ERRORS_TOTAL: CounterVec = register_counter_vec!(
        "orchestrator_errors_total",
        "Total number of errors",
        &["stage", "error_type"]
    )
    .unwrap();

    /// Current queue depth per stage
    pub static ref QUEUE_DEPTH: GaugeVec = register_gauge_vec!(
        "orchestrator_queue_depth",
        "Current queue depth",
        &["stage"]
    )
    .unwrap();

    /// Stage processing duration
    pub static ref STAGE_DURATION: HistogramVec = register_histogram_vec!(
        "orchestrator_stage_duration_seconds",
        "Stage processing duration in seconds",
        &["stage"],
        vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
    )
    .unwrap();

    /// End-to-end request duration
    pub static ref REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "orchestrator_request_duration_seconds",
        "End-to-end request duration in seconds",
        &["session"]
    )
    .unwrap();

    /// Model inference token count
    pub static ref TOKENS_GENERATED: CounterVec = register_counter_vec!(
        "orchestrator_tokens_generated_total",
        "Total tokens generated",
        &["worker_type"]
    )
    .unwrap();
}

/// Record current queue depth for a stage
pub fn record_queue_depth(stage: &str, depth: usize) {
    QUEUE_DEPTH.with_label_values(&[stage]).set(depth as f64);
    
    // Also log for backwards compatibility
    tracing::debug!(
        metric = "queue_depth",
        stage = stage,
        depth = depth,
        "queue depth sampled"
    );
}

/// Record stage processing latency
pub fn record_stage_latency(stage: &str, dur: Duration) {
    STAGE_DURATION
        .with_label_values(&[stage])
        .observe(dur.as_secs_f64());

    // Also log for backwards compatibility
    tracing::debug!(
        metric = "stage_latency_ms",
        stage = stage,
        latency_ms = dur.as_millis() as u64,
        "stage processing completed"
    );
}

/// Record request shed event
pub fn record_shed(stage: &str) {
    REQUESTS_SHED.with_label_values(&[stage]).inc();
    
    tracing::warn!(
        metric = "requests_shed",
        stage = stage,
        "request shed due to backpressure"
    );
}

/// Record stage throughput (requests processed)
pub fn record_throughput(stage: &str, count: u64) {
    REQUESTS_TOTAL
        .with_label_values(&[stage])
        .inc_by(count as f64);
    
    tracing::debug!(
        metric = "requests_processed",
        stage = stage,
        count = count,
        "requests processed"
    );
}

/// Record error
pub fn record_error(stage: &str, error_type: &str) {
    ERRORS_TOTAL
        .with_label_values(&[stage, error_type])
        .inc();
    
    tracing::error!(
        metric = "error",
        stage = stage,
        error_type = error_type,
        "error occurred"
    );
}

/// Record end-to-end request duration
pub fn record_request_duration(session: &str, dur: Duration) {
    REQUEST_DURATION
        .with_label_values(&[session])
        .observe(dur.as_secs_f64());
}

/// Record tokens generated
pub fn record_tokens_generated(worker_type: &str, count: usize) {
    TOKENS_GENERATED
        .with_label_values(&[worker_type])
        .inc_by(count as f64);
}

/// Gather all metrics in Prometheus text format
pub fn gather_metrics() -> String {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    
    String::from_utf8(buffer).unwrap()
}

/// Get metrics summary for logging/debugging
pub fn get_metrics_summary() -> MetricsSummary {
    MetricsSummary {
        requests_total: REQUESTS_TOTAL
            .collect()
            .into_iter()
            .map(|m| {
                let labels = m.get_label();
                let stage = labels.iter().find(|l| l.get_name() == "stage")
                    .map(|l| l.get_value().to_string())
                    .unwrap_or_default();
                (stage, m.get_counter().get_value() as u64)
            })
            .collect(),
        requests_shed: REQUESTS_SHED
            .collect()
            .into_iter()
            .map(|m| {
                let labels = m.get_label();
                let stage = labels.iter().find(|l| l.get_name() == "stage")
                    .map(|l| l.get_value().to_string())
                    .unwrap_or_default();
                (stage, m.get_counter().get_value() as u64)
            })
            .collect(),
        errors_total: ERRORS_TOTAL
            .collect()
            .into_iter()
            .map(|m| {
                let labels = m.get_label();
                let stage = labels.iter().find(|l| l.get_name() == "stage")
                    .map(|l| l.get_value().to_string())
                    .unwrap_or_default();
                let error_type = labels.iter().find(|l| l.get_name() == "error_type")
                    .map(|l| l.get_value().to_string())
                    .unwrap_or_default();
                ((stage, error_type), m.get_counter().get_value() as u64)
            })
            .collect(),
    }
}

/// Metrics summary for debugging
#[derive(Debug)]
pub struct MetricsSummary {
    pub requests_total: Vec<(String, u64)>,
    pub requests_shed: Vec<(String, u64)>,
    pub errors_total: Vec<((String, String), u64)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_recording() {
        record_throughput("test_stage", 1);
        record_stage_latency("test_stage", Duration::from_millis(100));
        record_queue_depth("test_stage", 42);
        
        let metrics = gather_metrics();
        assert!(metrics.contains("orchestrator_requests_total"));
        assert!(metrics.contains("orchestrator_stage_duration_seconds"));
    }

    #[test]
    fn test_metrics_summary() {
        record_throughput("summary_test", 5);
        let summary = get_metrics_summary();
        
        // Should have at least one entry
        assert!(!summary.requests_total.is_empty());
    }
}
