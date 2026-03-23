//! # Observability — OpenTelemetry-Compatible Tracing and Metrics
//!
//! Implements an OpenTelemetry-compatible data model from scratch (no external
//! OTel crate) including spans, exporters, and a Prometheus-format metrics
//! registry.
//!
//! ## Example
//!
//! ```rust
//! use std::collections::HashMap;
//! use std::sync::Arc;
//! use tokio_prompt_orchestrator::observability::{
//!     InMemoryExporter, ObservabilityRegistry, Tracer,
//! };
//!
//! let exporter = Arc::new(InMemoryExporter::new());
//! let tracer = Arc::new(Tracer::new(exporter.clone()));
//! let registry = ObservabilityRegistry::new(tracer);
//!
//! let mut span = registry.tracer.start_span("my-operation");
//! span.set_attribute("http.method", "GET".into());
//! span.finish();
//!
//! registry.record_metric("requests_total", 1.0, HashMap::new());
//! println!("{}", registry.to_prometheus());
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

// ── ID Generation ─────────────────────────────────────────────────────────────

/// Global monotonic counter used to contribute uniqueness to generated IDs.
static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Returns the current time in nanoseconds since UNIX epoch.
fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Generates a pseudo-random u64 using an LCG seeded from the counter and
/// current time.  Not cryptographically secure — suitable for trace IDs only.
fn gen_id64() -> u64 {
    let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let time = now_ns();
    // Mix with a simple LCG step.
    let mixed = seq
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(time ^ 1_442_695_040_888_963_407);
    mixed
}

/// Generates a pseudo-random u128 trace ID.
fn gen_trace_id() -> u128 {
    let hi = gen_id64() as u128;
    let lo = gen_id64() as u128;
    (hi << 64) | lo
}

// ── SpanContext ───────────────────────────────────────────────────────────────

/// Immutable identity and sampling context for a span, equivalent to the
/// OpenTelemetry `SpanContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanContext {
    /// 128-bit trace identifier shared by all spans in a trace.
    pub trace_id: u128,
    /// 64-bit identifier unique within the trace.
    pub span_id: u64,
    /// `span_id` of the parent span, or `None` for a root span.
    pub parent_span_id: Option<u64>,
    /// Whether this span is sampled (i.e. should be exported).
    pub sampled: bool,
}

impl SpanContext {
    /// Creates a new root-level span context with freshly generated IDs.
    pub fn new_root(sampled: bool) -> Self {
        Self {
            trace_id: gen_trace_id(),
            span_id: gen_id64(),
            parent_span_id: None,
            sampled,
        }
    }

    /// Creates a child span context that inherits `trace_id` and records this
    /// span as the parent.
    pub fn child(&self) -> Self {
        Self {
            trace_id: self.trace_id,
            span_id: gen_id64(),
            parent_span_id: Some(self.span_id),
            sampled: self.sampled,
        }
    }
}

// ── AttributeValue ────────────────────────────────────────────────────────────

/// A typed attribute value, matching the OpenTelemetry attribute type system.
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    /// UTF-8 string attribute.
    String(String),
    /// 64-bit signed integer attribute.
    Int(i64),
    /// 64-bit floating-point attribute.
    Float(f64),
    /// Boolean attribute.
    Bool(bool),
}

impl From<&str> for AttributeValue {
    fn from(s: &str) -> Self {
        AttributeValue::String(s.to_owned())
    }
}

impl From<String> for AttributeValue {
    fn from(s: String) -> Self {
        AttributeValue::String(s)
    }
}

impl From<i64> for AttributeValue {
    fn from(i: i64) -> Self {
        AttributeValue::Int(i)
    }
}

impl From<f64> for AttributeValue {
    fn from(f: f64) -> Self {
        AttributeValue::Float(f)
    }
}

impl From<bool> for AttributeValue {
    fn from(b: bool) -> Self {
        AttributeValue::Bool(b)
    }
}

// ── SpanEvent ─────────────────────────────────────────────────────────────────

/// A timestamped event recorded within a span's lifetime.
#[derive(Debug, Clone)]
pub struct SpanEvent {
    /// Human-readable event name.
    pub name: String,
    /// Nanoseconds since UNIX epoch at which the event occurred.
    pub timestamp_ns: u64,
    /// Arbitrary key-value attributes attached to this event.
    pub attributes: HashMap<String, AttributeValue>,
}

// ── SpanStatus ────────────────────────────────────────────────────────────────

/// The completion status of a span, mirroring the OpenTelemetry `StatusCode`.
#[derive(Debug, Clone, PartialEq)]
pub enum SpanStatus {
    /// No explicit status has been set.
    Unset,
    /// The operation completed successfully.
    Ok,
    /// The operation failed with the given description.
    Error(String),
}

impl Default for SpanStatus {
    fn default() -> Self {
        SpanStatus::Unset
    }
}

// ── Span ──────────────────────────────────────────────────────────────────────

/// A single unit of work in a distributed trace.
#[derive(Debug)]
pub struct Span {
    /// Sampling and identity context.
    pub context: SpanContext,
    /// Human-readable operation name.
    pub name: String,
    /// Nanoseconds since UNIX epoch when the span started.
    pub start_ns: u64,
    /// Nanoseconds since UNIX epoch when the span ended, or `None` if still
    /// in progress.
    pub end_ns: Option<u64>,
    /// Key-value attributes describing the operation.
    pub attributes: HashMap<String, AttributeValue>,
    /// Timestamped events that occurred during this span.
    pub events: Vec<SpanEvent>,
    /// Completion status of this span.
    pub status: SpanStatus,
}

impl Span {
    /// Creates a new in-progress span with the given context and name.
    fn new(context: SpanContext, name: impl Into<String>) -> Self {
        Self {
            context,
            name: name.into(),
            start_ns: now_ns(),
            end_ns: None,
            attributes: HashMap::new(),
            events: Vec::new(),
            status: SpanStatus::Unset,
        }
    }

    /// Sets a key-value attribute on this span.
    pub fn set_attribute(&mut self, key: impl Into<String>, value: AttributeValue) {
        self.attributes.insert(key.into(), value);
    }

    /// Appends a named event with optional attributes to this span.
    pub fn add_event(&mut self, name: impl Into<String>, attributes: HashMap<String, AttributeValue>) {
        self.events.push(SpanEvent {
            name: name.into(),
            timestamp_ns: now_ns(),
            attributes,
        });
    }

    /// Marks this span as finished by recording the current time as `end_ns`.
    pub fn finish(&mut self) {
        self.end_ns = Some(now_ns());
    }

    /// Returns the duration of this span in nanoseconds, or `None` if it has
    /// not yet finished.
    pub fn duration_ns(&self) -> Option<u64> {
        self.end_ns.map(|end| end.saturating_sub(self.start_ns))
    }
}

// ── SpanExporter ──────────────────────────────────────────────────────────────

/// A sink that receives completed spans for storage or forwarding.
pub trait SpanExporter: Send + Sync {
    /// Called when a span is ready to be exported.
    fn export(&self, span: Span);
}

// ── InMemoryExporter ─────────────────────────────────────────────────────────

/// A [`SpanExporter`] that accumulates all exported spans in memory.
///
/// Useful for testing and introspection.
#[derive(Debug, Default)]
pub struct InMemoryExporter {
    spans: Arc<Mutex<Vec<Span>>>,
}

impl InMemoryExporter {
    /// Creates a new, empty in-memory exporter.
    pub fn new() -> Self {
        Self {
            spans: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns a snapshot of all spans that have been exported so far.
    pub fn get_spans(&self) -> Vec<String> {
        // Return span names for easy inspection in tests.
        self.spans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    /// Returns a clone of every exported [`Span`].
    pub fn drain_spans(&self) -> Vec<String> {
        let mut guard = self.spans.lock().unwrap_or_else(|e| e.into_inner());
        let names: Vec<String> = guard.iter().map(|s| s.name.clone()).collect();
        guard.clear();
        names
    }

    /// Returns the number of spans collected so far.
    pub fn span_count(&self) -> usize {
        self.spans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

impl SpanExporter for InMemoryExporter {
    fn export(&self, span: Span) {
        let mut guard = self.spans.lock().unwrap_or_else(|e| e.into_inner());
        guard.push(span);
    }
}

// ── Tracer ────────────────────────────────────────────────────────────────────

/// Creates and manages spans, routing finished spans to the configured exporter.
#[derive(Clone)]
pub struct Tracer {
    exporter: Arc<dyn SpanExporter>,
}

impl std::fmt::Debug for Tracer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracer").field("exporter", &"<dyn SpanExporter>").finish()
    }
}

impl Tracer {
    /// Creates a tracer that will send completed spans to `exporter`.
    pub fn new(exporter: Arc<dyn SpanExporter>) -> Self {
        Self { exporter }
    }

    /// Starts a new root span with the given name.  The span is sampled by
    /// default.
    pub fn start_span(&self, name: &str) -> Span {
        let ctx = SpanContext::new_root(true);
        Span::new(ctx, name)
    }

    /// Starts a child span whose `parent_span_id` is set to the `parent`
    /// span's `span_id`.
    pub fn start_child_span(&self, name: &str, parent: &Span) -> Span {
        let ctx = parent.context.child();
        Span::new(ctx, name)
    }

    /// Finishes and exports `span`.
    pub fn finish_span(&self, mut span: Span) {
        span.finish();
        self.exporter.export(span);
    }
}

// ── MetricPoint ───────────────────────────────────────────────────────────────

/// A single timestamped metric observation.
#[derive(Debug, Clone)]
pub struct MetricPoint {
    /// Metric name (e.g. `"http_requests_total"`).
    pub name: String,
    /// Numeric value.
    pub value: f64,
    /// Prometheus-style label key-value pairs.
    pub labels: HashMap<String, String>,
    /// Nanoseconds since UNIX epoch.
    pub timestamp_ns: u64,
}

// ── ObservabilityRegistry ─────────────────────────────────────────────────────

/// Central registry that combines a [`Tracer`] with an in-process metrics
/// store and Prometheus export.
pub struct ObservabilityRegistry {
    /// The tracer used to create spans.
    pub tracer: Arc<Tracer>,
    /// Accumulated metric observations.
    pub metrics: Arc<Mutex<Vec<MetricPoint>>>,
}

impl ObservabilityRegistry {
    /// Creates a new registry wrapping the given [`Tracer`].
    pub fn new(tracer: Arc<Tracer>) -> Self {
        Self {
            tracer,
            metrics: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Records a metric observation.
    pub fn record_metric(
        &self,
        name: &str,
        value: f64,
        labels: HashMap<String, String>,
    ) {
        let point = MetricPoint {
            name: name.to_owned(),
            value,
            labels,
            timestamp_ns: now_ns(),
        };
        let mut guard = self.metrics.lock().unwrap_or_else(|e| e.into_inner());
        guard.push(point);
    }

    /// Serializes all recorded metrics in the
    /// [Prometheus exposition format](https://prometheus.io/docs/instrumenting/exposition_formats/).
    ///
    /// Each metric is emitted as:
    /// ```text
    /// metric_name{label="value",...} <value> <timestamp_ms>
    /// ```
    pub fn to_prometheus(&self) -> String {
        let guard = self.metrics.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();

        for point in guard.iter() {
            // Build label set string.
            let label_str = if point.labels.is_empty() {
                String::new()
            } else {
                let pairs: Vec<String> = point
                    .labels
                    .iter()
                    .map(|(k, v)| format!("{}=\"{}\"", k, v))
                    .collect();
                format!("{{{}}}", pairs.join(","))
            };

            let ts_ms = point.timestamp_ns / 1_000_000;
            out.push_str(&format!(
                "{}{} {} {}\n",
                point.name, label_str, point.value, ts_ms
            ));
        }

        out
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn span_duration_calculation() {
        let ctx = SpanContext::new_root(true);
        let mut span = Span::new(ctx, "test-span");
        assert!(span.duration_ns().is_none(), "span should have no duration before finish");
        span.finish();
        let dur = span.duration_ns();
        assert!(dur.is_some(), "finished span should have a duration");
        // Duration should be non-negative.
        assert!(dur.unwrap() < u64::MAX);
    }

    #[test]
    fn child_span_inherits_trace_id() {
        let root_ctx = SpanContext::new_root(true);
        let child_ctx = root_ctx.child();

        assert_eq!(
            root_ctx.trace_id, child_ctx.trace_id,
            "child should inherit trace_id"
        );
        assert_ne!(
            root_ctx.span_id, child_ctx.span_id,
            "child should have a distinct span_id"
        );
        assert_eq!(
            child_ctx.parent_span_id,
            Some(root_ctx.span_id),
            "child parent_span_id should be root span_id"
        );
    }

    #[test]
    fn in_memory_exporter_collects_spans() {
        let exporter = Arc::new(InMemoryExporter::new());
        let tracer = Arc::new(Tracer::new(exporter.clone()));

        let span1 = tracer.start_span("op-one");
        tracer.finish_span(span1);

        let span2 = tracer.start_span("op-two");
        tracer.finish_span(span2);

        assert_eq!(exporter.span_count(), 2);
        let names = exporter.get_spans();
        assert!(names.contains(&"op-one".to_string()));
        assert!(names.contains(&"op-two".to_string()));
    }

    #[test]
    fn child_span_via_tracer() {
        let exporter = Arc::new(InMemoryExporter::new());
        let tracer = Arc::new(Tracer::new(exporter.clone()));

        let parent = tracer.start_span("parent");
        let child = tracer.start_child_span("child", &parent);

        assert_eq!(parent.context.trace_id, child.context.trace_id);
        assert_eq!(child.context.parent_span_id, Some(parent.context.span_id));

        tracer.finish_span(parent);
        tracer.finish_span(child);
        assert_eq!(exporter.span_count(), 2);
    }

    #[test]
    fn prometheus_output_format() {
        let exporter = Arc::new(InMemoryExporter::new());
        let tracer = Arc::new(Tracer::new(exporter));
        let registry = ObservabilityRegistry::new(tracer);

        let mut labels = HashMap::new();
        labels.insert("model".to_string(), "gpt-4".to_string());
        registry.record_metric("tokens_total", 42.0, labels);
        registry.record_metric("latency_ms", 123.5, HashMap::new());

        let prom = registry.to_prometheus();
        assert!(prom.contains("tokens_total{"), "should contain metric with labels");
        assert!(prom.contains("model=\"gpt-4\""), "should contain label value");
        assert!(prom.contains("42"), "should contain metric value");
        assert!(prom.contains("latency_ms"), "should contain second metric");
        assert!(prom.contains("123.5"), "should contain second metric value");
    }

    #[test]
    fn root_span_has_no_parent() {
        let ctx = SpanContext::new_root(true);
        assert!(ctx.parent_span_id.is_none());
    }

    #[test]
    fn span_attributes_and_events() {
        let ctx = SpanContext::new_root(true);
        let mut span = Span::new(ctx, "attr-test");
        span.set_attribute("key", AttributeValue::String("value".to_string()));
        span.set_attribute("count", AttributeValue::Int(7));
        span.add_event("cache-hit", HashMap::new());
        span.finish();

        assert_eq!(span.attributes.len(), 2);
        assert_eq!(span.events.len(), 1);
        assert_eq!(span.events[0].name, "cache-hit");
    }

    #[test]
    fn unique_trace_ids() {
        let ctx1 = SpanContext::new_root(true);
        let ctx2 = SpanContext::new_root(true);
        assert_ne!(ctx1.trace_id, ctx2.trace_id);
        assert_ne!(ctx1.span_id, ctx2.span_id);
    }
}
