//! Metrics module
//!
//! Prometheus counters + histograms for:
//! - Requests per stage
//! - Stage durations
//! - Errors
//! - Queue shed
//!
//! Expose via /metrics if metrics-server feature enabled

use prometheus::{
    CounterVec, HistogramOpts, HistogramVec, IntGaugeVec, Opts, Registry,
};
use lazy_static::lazy_static;
use std::time::Duration;
use tracing::debug;

lazy_static! {
    /// Registry
    pub static ref REGISTRY: Registry = Registry::new();

    /// Total requests processed per stage
    pub static ref REQUESTS_TOTAL: CounterVec = {
        let c = CounterVec::new(
            Opts::new("orchestrator_requests_total", "Total requests processed"),
            &["stage"],
        ).unwrap();
        REGISTRY.register(Box::new(c.clone())).unwrap();
        c
    };

    /// Requests shed (queue full)
    pub static ref REQUESTS_SHED: CounterVec = {
        let c = CounterVec::new(
            Opts::new("orchestrator_requests_shed_total", "Requests dropped due to backpressure"),
            &["stage"],
        ).unwrap();
        REGISTRY.register(Box::new(c.clone())).unwrap();
        c
    };

    /// Errors by stage + type
    pub static ref ERRORS_TOTAL: CounterVec = {
        let c = CounterVec::new(
            Opts::new("orchestrator_errors_total", "Errors"),
            &["stage", "err_type"],
        ).unwrap();
        REGISTRY.register(Box::new(c.clone())).unwrap();
        c
    };

    /// Histogram per stage
    pub static ref STAGE_DURATION: HistogramVec = {
        let h = HistogramVec::new(
            HistogramOpts::new("orchestrator_stage_duration_seconds", "Duration per stage"),
            &["stage"],
        ).unwrap();
        REGISTRY.register(Box::new(h.clone())).unwrap();
        h
    };

    /// Queue depth gauge
    pub static ref QUEUE_DEPTH: IntGaugeVec = {
        let g = IntGaugeVec::new(
            Opts::new("orchestrator_queue_depth", "Queue depth per stage"),
            &["stage"],
        ).unwrap();
        REGISTRY.register(Box::new(g.clone())).unwrap();
        g
    };
}

/// Record stage latency
pub fn record_stage_latency(stage: &str, d: Duration) {
    let secs = d.as_secs_f64();
    if let Err(_) = STAGE_DURATION.get_metric_with_label_values(&[stage]) {
        return;
    }
    STAGE_DURATION
        .get_metric_with_label_values(&[stage])
        .unwrap()
        .observe(secs);
}

/// Bump request count
pub fn inc_request(stage: &str) {
    REQUESTS_TOTAL
        .get_metric_with_label_values(&[stage])
        .unwrap()
        .inc();
}

/// Bump shed count
pub fn inc_shed(stage: &str) {
    REQUESTS_SHED
        .get_metric_with_label_values(&[stage])
        .unwrap()
        .inc();
}

/// Record an error
pub fn inc_error(stage: &str, err_type: &str) {
    ERRORS_TOTAL
        .get_metric_with_label_values(&[stage, err_type])
        .unwrap()
        .inc();
}

/// Update queue depth gauge
pub fn set_queue_depth(stage: &str, depth: i64) {
    QUEUE_DEPTH
        .get_metric_with_label_values(&[stage])
        .unwrap()
        .set(depth);
}

/// Export all metrics
pub fn gather() -> Vec<prometheus::proto::MetricFamily> {
    REGISTRY.gather()
}

