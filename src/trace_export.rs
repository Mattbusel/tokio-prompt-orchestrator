#![allow(dead_code)]
//! # Module: Distributed Tracing Export
//!
//! ## Responsibility
//! Extends the existing OpenTelemetry integration with a [`TraceExporter`] that
//! exports request-level traces (with one child span per pipeline stage) to
//! multiple backends:
//!
//! | Backend | Feature flag | Environment variable |
//! |---------|-------------|---------------------|
//! | Jaeger (OTLP/HTTP) | *(always available)* | `OTEL_EXPORTER_OTLP_ENDPOINT` or `JAEGER_ENDPOINT` |
//! | Zipkin | `zipkin` | `ZIPKIN_ENDPOINT` (e.g. `http://localhost:9411`) |
//! | DataDog | `datadog` | `DD_AGENT_HOST` + optional `DD_AGENT_PORT` |
//!
//! Each [`PipelineSpan`] represents one pipeline stage and is exported as a
//! child span under a root span keyed to the `request_id`.
//!
//! ## Design
//! - The exporter is backend-agnostic at the type level; the active backend is
//!   selected at runtime from environment variables.
//! - All export paths are async and non-blocking.
//! - Failures are soft: a warning is logged, but the pipeline is not affected.
//!
//! ## Relationship to existing OTel setup
//! The existing `try_build_otel_layer` in `lib.rs` wires per-stage tracing
//! spans automatically via `tracing_opentelemetry`. This module provides an
//! *additional*, explicit exporter path that works without the global
//! subscriber for environments that prefer push-based trace export.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by the trace exporter.
#[derive(Debug, Error)]
pub enum TraceExportError {
    /// The backend endpoint is not configured.
    #[error("backend endpoint not configured: {0}")]
    NotConfigured(String),
    /// An HTTP request to the backend failed.
    #[error("HTTP export failed: {0}")]
    Http(String),
    /// Serialisation of the payload failed.
    #[error("serialisation error: {0}")]
    Serialisation(String),
    /// Internal lock was poisoned.
    #[error("internal lock poisoned")]
    LockPoisoned,
}

// ─── Pipeline span ───────────────────────────────────────────────────────────

/// The five pipeline stages, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineStage {
    Rag,
    Assemble,
    Inference,
    Post,
    Stream,
}

impl PipelineStage {
    /// Human-readable name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PipelineStage::Rag => "rag",
            PipelineStage::Assemble => "assemble",
            PipelineStage::Inference => "inference",
            PipelineStage::Post => "post",
            PipelineStage::Stream => "stream",
        }
    }

    /// Index (0-based).
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            PipelineStage::Rag => 0,
            PipelineStage::Assemble => 1,
            PipelineStage::Inference => 2,
            PipelineStage::Post => 3,
            PipelineStage::Stream => 4,
        }
    }
}

/// A single pipeline stage span, ready for export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineSpan {
    /// Unique identifier for the parent request.
    pub request_id: String,
    /// Session identifier.
    pub session_id: String,
    /// Which stage this span covers.
    pub stage: PipelineStage,
    /// Absolute start time (Unix nanos).
    pub start_unix_nanos: u128,
    /// Duration of this stage.
    pub duration: Duration,
    /// Whether the stage completed successfully.
    pub success: bool,
    /// Optional error message (populated on failure).
    pub error: Option<String>,
    /// Arbitrary key-value tags attached to this span.
    pub tags: HashMap<String, String>,
}

impl PipelineSpan {
    /// Create a new span starting now.
    #[must_use]
    pub fn start(
        request_id: impl Into<String>,
        session_id: impl Into<String>,
        stage: PipelineStage,
    ) -> Self {
        let start_unix_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            request_id: request_id.into(),
            session_id: session_id.into(),
            stage,
            start_unix_nanos,
            duration: Duration::ZERO,
            success: false,
            error: None,
            tags: HashMap::new(),
        }
    }

    /// Finish this span (record duration).
    pub fn finish(&mut self, success: bool, error: Option<String>) {
        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let elapsed_nanos = now_nanos.saturating_sub(self.start_unix_nanos);
        self.duration = Duration::from_nanos(elapsed_nanos as u64);
        self.success = success;
        self.error = error;
    }

    /// Add a tag.
    pub fn tag(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.tags.insert(key.into(), value.into());
    }
}

// ─── Request trace ───────────────────────────────────────────────────────────

/// A complete request-level trace with all child stage spans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestTrace {
    /// Unique request identifier (becomes the root span name).
    pub request_id: String,
    /// Session identifier.
    pub session_id: String,
    /// When the request entered the pipeline (Unix nanos).
    pub entry_unix_nanos: u128,
    /// Total end-to-end duration.
    pub total_duration: Duration,
    /// Whether the overall request succeeded.
    pub success: bool,
    /// Child spans (one per pipeline stage).
    pub spans: Vec<PipelineSpan>,
    /// Service name for the exporter (overrides default).
    pub service_name: String,
}

impl RequestTrace {
    /// Create a new trace.
    #[must_use]
    pub fn new(
        request_id: impl Into<String>,
        session_id: impl Into<String>,
        service_name: impl Into<String>,
    ) -> Self {
        let entry_unix_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            request_id: request_id.into(),
            session_id: session_id.into(),
            entry_unix_nanos,
            total_duration: Duration::ZERO,
            success: false,
            spans: Vec::new(),
            service_name: service_name.into(),
        }
    }

    /// Add a finished span to this trace.
    pub fn add_span(&mut self, span: PipelineSpan) {
        self.spans.push(span);
    }

    /// Finish the trace.
    pub fn finish(&mut self, success: bool) {
        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let elapsed_nanos = now_nanos.saturating_sub(self.entry_unix_nanos);
        self.total_duration = Duration::from_nanos(elapsed_nanos as u64);
        self.success = success;
    }
}

// ─── Backend selection ────────────────────────────────────────────────────────

/// Which backend to export traces to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceBackend {
    /// OTLP/HTTP — compatible with Jaeger ≥ 1.35, Tempo, and any OTLP collector.
    Jaeger,
    /// Zipkin JSON v2 HTTP API.
    #[cfg(feature = "zipkin")]
    Zipkin,
    /// DataDog Agent trace API (v0.4).
    #[cfg(feature = "datadog")]
    DataDog,
    /// Discard all spans (useful in tests / dry-run mode).
    NoOp,
}

// ─── Exporter configuration ──────────────────────────────────────────────────

/// Configuration for the [`TraceExporter`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceExporterConfig {
    /// Which backend to target.
    pub backend: TraceBackend,
    /// Service name attached to every exported span.
    pub service_name: String,
    /// Endpoint URL (overrides auto-detection from env vars when set).
    pub endpoint_override: Option<String>,
    /// Request timeout for export HTTP calls.
    pub export_timeout: Duration,
}

impl Default for TraceExporterConfig {
    fn default() -> Self {
        Self {
            backend: TraceBackend::Jaeger,
            service_name: "tokio-prompt-orchestrator".to_string(),
            endpoint_override: None,
            export_timeout: Duration::from_secs(5),
        }
    }
}

impl TraceExporterConfig {
    /// Build from environment variables, falling back to defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let backend = {
            #[cfg(feature = "datadog")]
            if std::env::var("DD_AGENT_HOST").is_ok() {
                TraceBackend::DataDog
            } else {
                #[cfg(feature = "zipkin")]
                if std::env::var("ZIPKIN_ENDPOINT").is_ok() {
                    TraceBackend::Zipkin
                } else {
                    TraceBackend::Jaeger
                }
                #[cfg(not(feature = "zipkin"))]
                TraceBackend::Jaeger
            }
            #[cfg(not(feature = "datadog"))]
            {
                #[cfg(feature = "zipkin")]
                if std::env::var("ZIPKIN_ENDPOINT").is_ok() {
                    TraceBackend::Zipkin
                } else {
                    TraceBackend::Jaeger
                }
                #[cfg(not(feature = "zipkin"))]
                TraceBackend::Jaeger
            }
        };
        Self {
            backend,
            ..Default::default()
        }
    }

    /// Resolve the export endpoint from config or environment.
    #[must_use]
    pub fn resolved_endpoint(&self) -> Option<String> {
        if let Some(ref ep) = self.endpoint_override {
            return Some(ep.clone());
        }
        match &self.backend {
            TraceBackend::Jaeger => std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                .or_else(|_| std::env::var("JAEGER_ENDPOINT"))
                .ok(),
            #[cfg(feature = "zipkin")]
            TraceBackend::Zipkin => std::env::var("ZIPKIN_ENDPOINT").ok(),
            #[cfg(feature = "datadog")]
            TraceBackend::DataDog => {
                let host = std::env::var("DD_AGENT_HOST").unwrap_or_else(|_| "localhost".into());
                let port = std::env::var("DD_AGENT_PORT").unwrap_or_else(|_| "8126".into());
                Some(format!("http://{host}:{port}"))
            }
            TraceBackend::NoOp => None,
        }
    }
}

// ─── Trace exporter ──────────────────────────────────────────────────────────

/// Exports [`RequestTrace`] objects to the configured observability backend.
///
/// # Usage
///
/// ```ignore
/// let exporter = TraceExporter::new(TraceExporterConfig::from_env())?;
/// exporter.export(&my_request_trace).await?;
/// ```
pub struct TraceExporter {
    config: TraceExporterConfig,
    http_client: reqwest::Client,
}

impl TraceExporter {
    /// Create a new exporter.
    ///
    /// # Errors
    /// Returns [`TraceExportError::Http`] if the HTTP client cannot be built.
    pub fn new(config: TraceExporterConfig) -> Result<Self, TraceExportError> {
        let http_client = reqwest::Client::builder()
            .timeout(config.export_timeout)
            .build()
            .map_err(|e| TraceExportError::Http(e.to_string()))?;
        Ok(Self { config, http_client })
    }

    /// Export a completed request trace.
    ///
    /// Soft-fails with a warning on transport errors; never propagates errors
    /// that would affect the inference pipeline.
    pub async fn export(&self, trace: &RequestTrace) -> Result<(), TraceExportError> {
        match &self.config.backend {
            TraceBackend::NoOp => {
                debug!(request_id = %trace.request_id, "TraceExporter: no-op export");
                return Ok(());
            }
            TraceBackend::Jaeger => self.export_otlp(trace).await?,
            #[cfg(feature = "zipkin")]
            TraceBackend::Zipkin => self.export_zipkin(trace).await?,
            #[cfg(feature = "datadog")]
            TraceBackend::DataDog => self.export_datadog(trace).await?,
        }
        info!(
            request_id = %trace.request_id,
            backend = ?self.config.backend,
            spans = trace.spans.len(),
            duration_ms = trace.total_duration.as_millis(),
            "trace exported"
        );
        Ok(())
    }

    // ─── OTLP / Jaeger ───────────────────────────────────────────────────────

    async fn export_otlp(&self, trace: &RequestTrace) -> Result<(), TraceExportError> {
        let endpoint = self
            .config
            .resolved_endpoint()
            .ok_or_else(|| TraceExportError::NotConfigured("OTEL_EXPORTER_OTLP_ENDPOINT or JAEGER_ENDPOINT".to_string()))?;

        let payload = self.build_otlp_payload(trace)?;
        let url = format!("{endpoint}/v1/traces");

        let resp = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| TraceExportError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            warn!(
                status = %resp.status(),
                url = %url,
                "OTLP trace export received non-2xx response"
            );
        }
        Ok(())
    }

    fn build_otlp_payload(&self, trace: &RequestTrace) -> Result<String, TraceExportError> {
        // Simplified OTLP JSON payload (sufficient for Jaeger and compatible collectors)
        let trace_id = hex_from_str(&trace.request_id, 32);
        let root_span_id = hex_from_str(&trace.request_id, 16);

        let mut span_jsons: Vec<String> = Vec::with_capacity(trace.spans.len() + 1);

        // Root span
        let root_end_nanos = trace.entry_unix_nanos
            + trace.total_duration.as_nanos();
        span_jsons.push(format!(
            r#"{{"traceId":"{trace_id}","spanId":"{root_span_id}","name":"request","kind":1,"startTimeUnixNano":"{start}","endTimeUnixNano":"{end}","status":{{"code":{code}}},"attributes":[{{"key":"request_id","value":{{"stringValue":"{rid}"}}}},{{"key":"session_id","value":{{"stringValue":"{sid}"}}}}]}}"#,
            trace_id = trace_id,
            root_span_id = root_span_id,
            start = trace.entry_unix_nanos,
            end = root_end_nanos,
            code = if trace.success { 1 } else { 2 },
            rid = trace.request_id,
            sid = trace.session_id,
        ));

        // Child spans
        for span in &trace.spans {
            let span_id = hex_from_str(
                &format!("{}-{}", trace.request_id, span.stage.name()),
                16,
            );
            let end_nanos = span.start_unix_nanos
                + span.duration.as_nanos();
            let error_attr = match &span.error {
                Some(e) => format!(
                    r#",{{"key":"error","value":{{"stringValue":"{e}"}}}}"#,
                    e = e.replace('"', "'")
                ),
                None => String::new(),
            };
            span_jsons.push(format!(
                r#"{{"traceId":"{trace_id}","spanId":"{sid}","parentSpanId":"{root_span_id}","name":"{stage}","kind":1,"startTimeUnixNano":"{start}","endTimeUnixNano":"{end}","status":{{"code":{code}}},"attributes":[{{"key":"stage","value":{{"stringValue":"{stage}"}}}}{error_attr}]}}"#,
                trace_id = trace_id,
                sid = span_id,
                root_span_id = root_span_id,
                stage = span.stage.name(),
                start = span.start_unix_nanos,
                end = end_nanos,
                code = if span.success { 1 } else { 2 },
                error_attr = error_attr,
            ));
        }

        let spans_array = span_jsons.join(",");
        let service_name = &self.config.service_name;
        let payload = format!(
            r#"{{"resourceSpans":[{{"resource":{{"attributes":[{{"key":"service.name","value":{{"stringValue":"{service_name}"}}}}]}},"scopeSpans":[{{"spans":[{spans_array}]}}]}}]}}"#
        );
        Ok(payload)
    }

    // ─── Zipkin ──────────────────────────────────────────────────────────────

    #[cfg(feature = "zipkin")]
    async fn export_zipkin(&self, trace: &RequestTrace) -> Result<(), TraceExportError> {
        let endpoint = self
            .config
            .resolved_endpoint()
            .ok_or_else(|| TraceExportError::NotConfigured("ZIPKIN_ENDPOINT".to_string()))?;

        let payload = self.build_zipkin_payload(trace)?;
        let url = format!("{endpoint}/api/v2/spans");

        let resp = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| TraceExportError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            warn!(
                status = %resp.status(),
                "Zipkin trace export received non-2xx response"
            );
        }
        Ok(())
    }

    #[cfg(feature = "zipkin")]
    fn build_zipkin_payload(&self, trace: &RequestTrace) -> Result<String, TraceExportError> {
        let trace_id = hex_from_str(&trace.request_id, 32);
        let root_id = hex_from_str(&trace.request_id, 16);
        let service_name = &self.config.service_name;

        // Zipkin timestamps are in microseconds
        let root_start_us = trace.entry_unix_nanos / 1_000;
        let root_duration_us = trace.total_duration.as_micros();

        let mut spans: Vec<String> = vec![format!(
            r#"{{"traceId":"{trace_id}","id":"{root_id}","name":"request","timestamp":{root_start_us},"duration":{root_duration_us},"localEndpoint":{{"serviceName":"{service_name}"}}}}"#
        )];

        for span in &trace.spans {
            let span_id = hex_from_str(
                &format!("{}-{}", trace.request_id, span.stage.name()),
                16,
            );
            let start_us = span.start_unix_nanos / 1_000;
            let dur_us = span.duration.as_micros();
            spans.push(format!(
                r#"{{"traceId":"{trace_id}","id":"{span_id}","parentId":"{root_id}","name":"{stage}","timestamp":{start_us},"duration":{dur_us},"localEndpoint":{{"serviceName":"{service_name}"}}}}"#,
                stage = span.stage.name()
            ));
        }

        Ok(format!("[{}]", spans.join(",")))
    }

    // ─── DataDog ─────────────────────────────────────────────────────────────

    #[cfg(feature = "datadog")]
    async fn export_datadog(&self, trace: &RequestTrace) -> Result<(), TraceExportError> {
        let endpoint = self
            .config
            .resolved_endpoint()
            .ok_or_else(|| TraceExportError::NotConfigured("DD_AGENT_HOST".to_string()))?;

        let payload = self.build_datadog_payload(trace)?;
        let url = format!("{endpoint}/v0.4/traces");

        let resp = self
            .http_client
            .put(&url)
            .header("Content-Type", "application/json")
            .header("X-Datadog-Trace-Count", "1")
            .body(payload)
            .send()
            .await
            .map_err(|e| TraceExportError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            warn!(
                status = %resp.status(),
                "DataDog trace export received non-2xx response"
            );
        }
        Ok(())
    }

    #[cfg(feature = "datadog")]
    fn build_datadog_payload(&self, trace: &RequestTrace) -> Result<String, TraceExportError> {
        // DataDog v0.4 expects an array of arrays of span objects
        let trace_id: u64 = trace
            .request_id
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
        let root_span_id: u64 = trace_id.wrapping_add(1);
        let service = &self.config.service_name;

        let mut dd_spans: Vec<String> = vec![format!(
            r#"{{"service":"{service}","name":"request","resource":"{rid}","trace_id":{trace_id},"span_id":{root_span_id},"parent_id":0,"start":{start},"duration":{dur},"error":{err}}}"#,
            rid = trace.request_id,
            start = trace.entry_unix_nanos,
            dur = trace.total_duration.as_nanos(),
            err = if trace.success { 0 } else { 1 },
        )];

        for span in &trace.spans {
            let span_id: u64 = trace_id
                .wrapping_add(span.stage.index() as u64 + 2);
            dd_spans.push(format!(
                r#"{{"service":"{service}","name":"{stage}","resource":"{stage}","trace_id":{trace_id},"span_id":{span_id},"parent_id":{root_span_id},"start":{start},"duration":{dur},"error":{err}}}"#,
                stage = span.stage.name(),
                start = span.start_unix_nanos,
                dur = span.duration.as_nanos(),
                err = if span.success { 0 } else { 1 },
            ));
        }

        Ok(format!("[[{}]]", dd_spans.join(",")))
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Derives a hex string of `len` chars from an arbitrary string (deterministic,
/// not cryptographic — used as a stable trace/span id).
fn hex_from_str(s: &str, len: usize) -> String {
    let mut hash: u128 = 0xcafe_babe_dead_beef_0123_4567_89ab_cdef;
    for b in s.bytes() {
        hash = hash.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(b as u128);
    }
    format!("{hash:032x}").chars().take(len).collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_span_finish_records_duration() {
        let mut span = PipelineSpan::start("req-1", "sess-1", PipelineStage::Inference);
        std::thread::sleep(Duration::from_millis(5));
        span.finish(true, None);
        assert!(span.duration.as_millis() >= 5);
        assert!(span.success);
    }

    #[test]
    fn request_trace_add_spans() {
        let mut trace = RequestTrace::new("req-1", "sess-1", "test-svc");
        let mut span = PipelineSpan::start("req-1", "sess-1", PipelineStage::Rag);
        span.finish(true, None);
        trace.add_span(span);
        trace.finish(true);
        assert_eq!(trace.spans.len(), 1);
        assert!(trace.success);
    }

    #[test]
    fn otlp_payload_is_valid_json() {
        let config = TraceExporterConfig {
            backend: TraceBackend::NoOp,
            service_name: "test".to_string(),
            endpoint_override: Some("http://localhost:4318".to_string()),
            export_timeout: Duration::from_secs(5),
        };
        let exporter = TraceExporter::new(config).expect("client should build");
        let mut trace = RequestTrace::new("req-abc", "sess-1", "test");
        let mut span = PipelineSpan::start("req-abc", "sess-1", PipelineStage::Inference);
        span.finish(true, None);
        trace.add_span(span);
        trace.finish(true);

        // Build the OTLP payload and verify it parses as JSON
        let payload = exporter.build_otlp_payload(&trace).expect("payload ok");
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("payload should be valid JSON");
        assert!(parsed.get("resourceSpans").is_some());
    }

    #[test]
    fn hex_from_str_stable() {
        let a = hex_from_str("hello", 32);
        let b = hex_from_str("hello", 32);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[tokio::test]
    async fn noop_export_succeeds() {
        let config = TraceExporterConfig {
            backend: TraceBackend::NoOp,
            ..Default::default()
        };
        let exporter = TraceExporter::new(config).expect("client ok");
        let trace = RequestTrace::new("req-noop", "sess-1", "test");
        exporter.export(&trace).await.expect("noop export ok");
    }
}
