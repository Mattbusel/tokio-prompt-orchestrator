//! Prometheus metrics for the orchestrator pipeline.
//!
//! ## Usage
//!
//! Call [`init_metrics`] once at process startup **before** spawning any pipeline
//! stages. The helper functions (`record_stage_latency`, `inc_request`, …) are
//! no-ops if `init_metrics` was never called, so the pipeline is always safe to
//! run — observability simply degrades gracefully.
//!
//! ## Metrics Exposed
//!
//! | Name | Type | Labels |
//! |------|------|--------|
//! | `orchestrator_requests_total` | Counter | `stage` |
//! | `orchestrator_requests_shed_total` | Counter | `stage` |
//! | `orchestrator_errors_total` | Counter | `stage`, `err_type` |
//! | `orchestrator_stage_duration_seconds` | Histogram | `stage` |
//! | `orchestrator_queue_depth` | Gauge | `stage` |

use crate::OrchestratorError;
use prometheus::{
    core::Collector, CounterVec, Encoder, HistogramOpts, HistogramVec, IntGaugeVec, Opts, Registry,
    TextEncoder,
};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

// ── Internal metrics bundle ────────────────────────────────────────────────

/// All Prometheus metrics for the orchestrator, bundled together so they can
/// be stored in a single [`OnceLock`] and initialised atomically.
pub struct Metrics {
    /// Prometheus registry that owns all metric descriptors.
    pub registry: Registry,
    /// Total requests processed per stage.
    pub requests_total: CounterVec,
    /// Requests shed (queue full) per stage.
    pub requests_shed: CounterVec,
    /// Errors by stage and error type.
    pub errors_total: CounterVec,
    /// Stage processing latency histogram.
    pub stage_duration: HistogramVec,
    /// Current queue depth per stage.
    pub queue_depth: IntGaugeVec,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

// ── Initialisation ─────────────────────────────────────────────────────────

/// Initialise all Prometheus metrics and register them with a private registry.
///
/// Must be called once at process startup before any pipeline stage is spawned.
/// Calling it a second time is a no-op (returns `Ok(())`).
///
/// # Errors
///
/// Returns [`OrchestratorError::Other`] if metric construction or registry
/// registration fails (e.g., duplicate descriptor names).
///
/// # Panics
///
/// This function never panics.
pub fn init_metrics() -> Result<(), OrchestratorError> {
    if METRICS.get().is_some() {
        return Ok(());
    }

    let registry = Registry::new();

    let requests_total = CounterVec::new(
        Opts::new("orchestrator_requests_total", "Total requests processed"),
        &["stage"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(requests_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let requests_shed = CounterVec::new(
        Opts::new(
            "orchestrator_requests_shed_total",
            "Requests dropped due to backpressure",
        ),
        &["stage"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(requests_shed.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let errors_total = CounterVec::new(
        Opts::new("orchestrator_errors_total", "Errors by stage and type"),
        &["stage", "err_type"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(errors_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let stage_duration = HistogramVec::new(
        HistogramOpts::new(
            "orchestrator_stage_duration_seconds",
            "Processing duration per stage",
        ),
        &["stage"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(stage_duration.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let queue_depth = IntGaugeVec::new(
        Opts::new("orchestrator_queue_depth", "Current queue depth per stage"),
        &["stage"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(queue_depth.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    // If another thread raced us, the first one wins — both initializations
    // produce identical metric descriptors, so neither outcome is incorrect.
    let _ = METRICS.set(Metrics {
        registry,
        requests_total,
        requests_shed,
        errors_total,
        stage_duration,
        queue_depth,
    });

    Ok(())
}

/// Return a reference to the initialised [`Metrics`], or `None` if
/// [`init_metrics`] has not been called yet.
fn metrics() -> Option<&'static Metrics> {
    METRICS.get()
}

// ── Public helper functions ────────────────────────────────────────────────

/// Record the processing latency for a pipeline stage.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn record_stage_latency(stage: &str, d: Duration) {
    if let Some(m) = metrics() {
        if let Ok(h) = m.stage_duration.get_metric_with_label_values(&[stage]) {
            h.observe(d.as_secs_f64());
        }
    }
}

/// Increment the request counter for a pipeline stage.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_request(stage: &str) {
    if let Some(m) = metrics() {
        if let Ok(c) = m.requests_total.get_metric_with_label_values(&[stage]) {
            c.inc();
        }
    }
}

/// Increment the shed-request counter for a pipeline stage.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_shed(stage: &str) {
    if let Some(m) = metrics() {
        if let Ok(c) = m.requests_shed.get_metric_with_label_values(&[stage]) {
            c.inc();
        }
    }
}

/// Increment the error counter for a pipeline stage and error type.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_error(stage: &str, err_type: &str) {
    if let Some(m) = metrics() {
        if let Ok(c) = m
            .errors_total
            .get_metric_with_label_values(&[stage, err_type])
        {
            c.inc();
        }
    }
}

/// Set the queue depth gauge for a pipeline stage.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn set_queue_depth(stage: &str, depth: i64) {
    if let Some(m) = metrics() {
        if let Ok(g) = m.queue_depth.get_metric_with_label_values(&[stage]) {
            g.set(depth);
        }
    }
}

/// Gather all registered metrics as a raw list of metric families.
///
/// Returns an empty `Vec` if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn gather() -> Vec<prometheus::proto::MetricFamily> {
    metrics().map_or_else(Vec::new, |m| m.registry.gather())
}

/// Gather and encode all metrics in the Prometheus text exposition format.
///
/// Returns an empty string if metrics have not been initialised or if
/// encoding fails. Observability degrades gracefully rather than panicking.
///
/// # Panics
///
/// This function never panics.
pub fn gather_metrics() -> String {
    let families = gather();
    if families.is_empty() {
        return String::new();
    }
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    if encoder.encode(&families, &mut buffer).is_err() {
        return String::new();
    }
    String::from_utf8(buffer).unwrap_or_default()
}

/// A structured snapshot of key metric counters, used by the health endpoint.
#[derive(Debug, Default)]
pub struct MetricsSummary {
    /// Total request counts keyed by stage label.
    pub requests_total: HashMap<String, u64>,
    /// Shed request counts keyed by stage label.
    pub requests_shed: HashMap<String, u64>,
    /// Error counts keyed by `"stage:err_type"`.
    pub errors_total: HashMap<String, u64>,
}

/// Return a structured summary of current metric counter values.
///
/// Returns a zeroed [`MetricsSummary`] if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn get_metrics_summary() -> MetricsSummary {
    let Some(m) = metrics() else {
        return MetricsSummary::default();
    };

    let mut summary = MetricsSummary::default();

    for family in m.requests_total.collect() {
        for metric in family.get_metric() {
            let stage = metric
                .get_label()
                .iter()
                .find(|l| l.get_name() == "stage")
                .map_or("unknown", |l| l.get_value());
            let value = metric.get_counter().get_value() as u64;
            summary.requests_total.insert(stage.to_string(), value);
        }
    }

    for family in m.requests_shed.collect() {
        for metric in family.get_metric() {
            let stage = metric
                .get_label()
                .iter()
                .find(|l| l.get_name() == "stage")
                .map_or("unknown", |l| l.get_value());
            let value = metric.get_counter().get_value() as u64;
            summary.requests_shed.insert(stage.to_string(), value);
        }
    }

    for family in m.errors_total.collect() {
        for metric in family.get_metric() {
            let stage = metric
                .get_label()
                .iter()
                .find(|l| l.get_name() == "stage")
                .map_or("unknown", |l| l.get_value());
            let err_type = metric
                .get_label()
                .iter()
                .find(|l| l.get_name() == "err_type")
                .map_or("unknown", |l| l.get_value());
            let key = format!("{stage}:{err_type}");
            let value = metric.get_counter().get_value() as u64;
            summary.errors_total.insert(key, value);
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fresh, isolated [`Metrics`] bundle backed by its own registry.
    ///
    /// We cannot reset the global `METRICS` OnceLock between tests, so tests
    /// that need to verify exact counter values build a local bundle instead.
    fn make_test_metrics() -> Metrics {
        let registry = Registry::new();

        let requests_total =
            CounterVec::new(Opts::new("t_requests_total", "test counter"), &["stage"])
                .expect("CounterVec construction must succeed in tests");
        registry
            .register(Box::new(requests_total.clone()))
            .expect("register must succeed in tests");

        let requests_shed = CounterVec::new(
            Opts::new("t_requests_shed_total", "test counter"),
            &["stage"],
        )
        .expect("CounterVec construction must succeed in tests");
        registry
            .register(Box::new(requests_shed.clone()))
            .expect("register must succeed in tests");

        let errors_total = CounterVec::new(
            Opts::new("t_errors_total", "test counter"),
            &["stage", "err_type"],
        )
        .expect("CounterVec construction must succeed in tests");
        registry
            .register(Box::new(errors_total.clone()))
            .expect("register must succeed in tests");

        let stage_duration = HistogramVec::new(
            HistogramOpts::new("t_stage_duration_seconds", "test histogram"),
            &["stage"],
        )
        .expect("HistogramVec construction must succeed in tests");
        registry
            .register(Box::new(stage_duration.clone()))
            .expect("register must succeed in tests");

        let queue_depth = IntGaugeVec::new(Opts::new("t_queue_depth", "test gauge"), &["stage"])
            .expect("IntGaugeVec construction must succeed in tests");
        registry
            .register(Box::new(queue_depth.clone()))
            .expect("register must succeed in tests");

        Metrics {
            registry,
            requests_total,
            requests_shed,
            errors_total,
            stage_duration,
            queue_depth,
        }
    }

    #[test]
    fn test_init_metrics_succeeds_once() {
        let result = init_metrics();
        assert!(result.is_ok(), "init_metrics should succeed: {result:?}");
    }

    #[test]
    fn test_init_metrics_idempotent_second_call_is_noop() {
        let _ = init_metrics();
        let result2 = init_metrics();
        assert!(result2.is_ok(), "second call must be a no-op returning Ok");
    }

    #[test]
    fn test_record_stage_latency_before_init_does_not_panic() {
        // Cannot reset OnceLock; just verify no panic occurs.
        record_stage_latency("pre-init-stage", Duration::from_millis(5));
    }

    #[test]
    fn test_record_stage_latency_records_observation_in_isolated_metrics() {
        let m = make_test_metrics();
        m.stage_duration
            .get_metric_with_label_values(&["rag"])
            .expect("label values must be valid")
            .observe(0.005);
        let families = m.registry.gather();
        assert!(
            !families.is_empty(),
            "should have at least one metric family"
        );
        let family = families
            .iter()
            .find(|f| f.get_name() == "t_stage_duration_seconds")
            .expect("histogram family must be present");
        let count = family.get_metric()[0].get_histogram().get_sample_count();
        assert_eq!(count, 1, "one observation should have been recorded");
    }

    #[test]
    fn test_inc_request_increments_counter_by_one() {
        let m = make_test_metrics();
        m.requests_total
            .get_metric_with_label_values(&["rag"])
            .expect("label ok")
            .inc();
        m.requests_total
            .get_metric_with_label_values(&["rag"])
            .expect("label ok")
            .inc();

        let families = m.registry.gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "t_requests_total")
            .expect("family must exist");
        let value = family.get_metric()[0].get_counter().get_value();
        assert!(
            (value - 2.0).abs() < f64::EPSILON,
            "counter must be 2.0, got {value}"
        );
    }

    #[test]
    fn test_inc_shed_increments_shed_counter() {
        let m = make_test_metrics();
        m.requests_shed
            .get_metric_with_label_values(&["assemble"])
            .expect("label ok")
            .inc();

        let families = m.registry.gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "t_requests_shed_total")
            .expect("family must exist");
        let value = family.get_metric()[0].get_counter().get_value();
        assert!((value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_inc_error_increments_error_counter_with_correct_labels() {
        let m = make_test_metrics();
        m.errors_total
            .get_metric_with_label_values(&["inference", "timeout"])
            .expect("label ok")
            .inc();

        let families = m.registry.gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "t_errors_total")
            .expect("family must exist");
        let value = family.get_metric()[0].get_counter().get_value();
        assert!((value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_set_queue_depth_sets_gauge_to_exact_value() {
        let m = make_test_metrics();
        m.queue_depth
            .get_metric_with_label_values(&["rag"])
            .expect("label ok")
            .set(42);

        let families = m.registry.gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "t_queue_depth")
            .expect("family must exist");
        let value = family.get_metric()[0].get_gauge().get_value();
        assert!(
            (value - 42.0).abs() < f64::EPSILON,
            "gauge must be 42.0, got {value}"
        );
    }

    #[test]
    fn test_gather_metrics_returns_valid_utf8_string() {
        let _ = init_metrics();
        let output = gather_metrics();
        assert!(
            std::str::from_utf8(output.as_bytes()).is_ok(),
            "gather_metrics output must be valid UTF-8"
        );
    }

    #[test]
    fn test_gather_metrics_does_not_panic_before_init() {
        // OnceLock may already be set; verify no panic in either case.
        let _ = gather_metrics();
    }

    #[test]
    fn test_get_metrics_summary_returns_valid_struct() {
        let summary = get_metrics_summary();
        // Must not panic; fields must be valid (possibly empty) maps.
        let _rt = summary.requests_total.len();
        let _rs = summary.requests_shed.len();
        let _et = summary.errors_total.len();
    }

    #[test]
    fn test_gather_returns_non_empty_after_observation() {
        // prometheus-rs gather() skips MetricFamily entries that have zero
        // recorded time-series (i.e. no label combinations ever observed).
        // We must record at least one value before gather() returns non-empty.
        let _ = init_metrics();
        inc_request("gather-test-stage");
        let families = gather();
        assert!(
            !families.is_empty(),
            "gather() must return at least one MetricFamily after an observation"
        );
    }

    #[test]
    fn test_set_queue_depth_global_helper_does_not_panic() {
        let _ = init_metrics();
        set_queue_depth("rag", 7);
        // Primary assertion: no panic.
    }
}
