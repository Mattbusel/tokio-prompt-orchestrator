//! # tokio-prompt-orchestrator
//!
//! A production-grade orchestrator for multi-stage LLM pipelines over Tokio.
//!
//! ## Architecture
//!
//! Five-stage pipeline with bounded channels and backpressure:
//! ```text
//! PromptRequest → RAG(512) → Assemble(512) → Inference(1024) → Post(512) → Stream(256)
//! ```

#![doc = include_str!("../README.md")]

use std::collections::HashMap;
use thiserror::Error;

pub mod config;
pub mod coordination;
// pub mod momentum; // module removed — C++ SIMD momentum is in crates/
#[cfg(feature = "distributed")]
pub mod distributed;
pub mod enhanced;
pub mod metrics;
pub mod routing;
pub mod stages;
pub mod worker;

#[cfg(feature = "metrics-server")]
pub mod metrics_server;

#[cfg(feature = "web-api")]
pub mod web_api;

#[cfg(feature = "self-tune")]
pub mod self_tune;

#[cfg(feature = "self-modify")]
pub mod self_modify;

#[cfg(feature = "intelligence")]
pub mod intelligence;

#[cfg(feature = "evolution")]
pub mod evolution;

#[cfg(all(
    feature = "self-tune",
    feature = "self-modify",
    feature = "intelligence"
))]
pub mod self_improve;

#[cfg(all(feature = "self-tune", feature = "self-modify"))]
pub mod self_improve_loop;

#[cfg(feature = "tui")]
pub mod tui;

// Re-exports
pub use stages::{
    spawn_pipeline, spawn_pipeline_with_config, LogSink, OutputSink, PipelineHandles, SinkError,
};
pub use worker::{
    stream_worker, AnthropicWorker, EchoWorker, LlamaCppWorker, LoadBalancedWorker, ModelWorker,
    OpenAiWorker, VllmWorker,
};

/// Orchestrator-specific errors.
///
/// All variants are non-panicking. Callers should match on the variant to
/// decide whether to retry, shed, or propagate the error.
#[derive(Error, Debug)]
pub enum OrchestratorError {
    /// A pipeline channel was closed before the request could be delivered.
    ///
    /// This typically means a pipeline stage task has exited. The pipeline
    /// should be restarted. This error is not retryable within the same pipeline instance.
    #[error("channel closed unexpectedly")]
    ChannelClosed,

    /// An inference worker returned an error.
    ///
    /// The inner string contains the provider error message. May be retryable
    /// depending on the underlying cause (transient network vs. invalid request).
    #[error("inference failed: {0}")]
    Inference(String),

    /// Pipeline or worker configuration is invalid.
    ///
    /// Returned during startup validation. Not retryable without a config change.
    #[error("configuration error: {0}")]
    ConfigError(String),

    /// Provider returned HTTP 429 — callers should back off for `retry_after`.
    #[error("rate limited by provider (retry after {retry_after_secs}s)")]
    RateLimited {
        /// Seconds to wait before retrying, parsed from `Retry-After` header.
        retry_after_secs: u64,
    },

    /// Spending cap reached — no further inference allowed this session.
    #[error("budget exceeded: spent ${spent:.4} of ${limit:.4} limit")]
    BudgetExceeded {
        /// Amount spent so far in USD.
        spent: f64,
        /// Configured spending limit in USD.
        limit: f64,
    },

    /// A catch-all error variant for errors that do not fit the other categories.
    #[error("{0}")]
    Other(String),
}

/// Unique session identifier for request tracking and affinity
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Create a new `SessionId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Initial prompt request from client
#[derive(Debug, Clone)]
pub struct PromptRequest {
    /// Session this request belongs to.  Used for affinity sharding and
    /// deduplication key generation.
    pub session: SessionId,
    /// Unique ID for distributed trace correlation across all pipeline stages.
    pub request_id: String,
    /// The raw prompt text to send to the inference backend.
    pub input: String,
    /// Arbitrary key-value metadata forwarded unchanged through the pipeline.
    pub meta: HashMap<String, String>,
    /// Optional absolute deadline for this request.  When `Some`, the inference
    /// stage drops the request and increments `requests_expired_total` if the
    /// deadline has already passed at dequeue time.
    pub deadline: Option<std::time::Instant>,
}

impl PromptRequest {
    /// Builder-style helper that sets an absolute deadline `duration` from now.
    ///
    /// # Example
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::{PromptRequest, SessionId};
    ///
    /// let req = PromptRequest {
    ///     session: SessionId::new("s1"),
    ///     request_id: "r1".to_string(),
    ///     input: "hello".to_string(),
    ///     meta: HashMap::new(),
    ///     deadline: None,
    /// }
    /// .with_deadline(Duration::from_secs(5));
    ///
    /// assert!(req.deadline.is_some());
    /// ```
    #[must_use]
    pub fn with_deadline(mut self, duration: std::time::Duration) -> Self {
        self.deadline = Some(std::time::Instant::now() + duration);
        self
    }
}

/// Output from the RAG (retrieval-augmented generation) stage.
///
/// Carries the retrieved context string alongside the original request so the
/// assembly stage can compose the final prompt without re-reading the original.
#[derive(Debug, Clone)]
pub struct RagOutput {
    /// Session this output belongs to.
    pub session: SessionId,
    /// Retrieved context text (documents, embeddings, etc.) for the prompt.
    pub context: String,
    /// The original request, forwarded intact for use in the assembly stage.
    pub original: PromptRequest,
    /// Deadline propagated from the originating [`PromptRequest`], if any.
    pub deadline: Option<std::time::Instant>,
}

/// Output from the prompt assembly stage.
///
/// The assembled `prompt` string is the final, context-injected input that will
/// be sent to the inference worker.
#[derive(Debug, Clone)]
pub struct AssembleOutput {
    /// Session this output belongs to.
    pub session: SessionId,
    /// Unique request identifier for distributed trace correlation.
    pub request_id: String,
    /// The fully assembled prompt string ready for inference.
    pub prompt: String,
    /// Deadline propagated from the originating [`PromptRequest`], if any.
    pub deadline: Option<std::time::Instant>,
}

/// Output from the inference stage.
///
/// Contains the raw token list as returned by the model worker before
/// post-processing joins them into a coherent response string.
#[derive(Debug, Clone)]
pub struct InferenceOutput {
    /// Session this output belongs to.
    pub session: SessionId,
    /// Unique request identifier for distributed trace correlation.
    pub request_id: String,
    /// Raw token list from the model worker.
    pub tokens: Vec<String>,
}

/// Output from the post-processing stage.
///
/// Tokens have been joined, filtered, and formatted into the final
/// response string delivered to the stream stage.
#[derive(Debug, Clone)]
pub struct PostOutput {
    /// Session this output belongs to.
    pub session: SessionId,
    /// Unique request identifier for distributed trace correlation.
    pub request_id: String,
    /// Final response text after post-processing.
    pub text: String,
}

/// FNV-1a hash — deterministic across process restarts unlike `DefaultHasher`.
fn fnv1a_hash(s: &str) -> u64 {
    const PRIME: u64 = 1_099_511_628_211;
    const BASIS: u64 = 14_695_981_039_346_656_037;
    s.bytes().fold(BASIS, |acc, b| acc.wrapping_mul(PRIME) ^ b as u64)
}

/// Session affinity sharding helper.
///
/// Uses FNV-1a for stable hashing across process restarts so that the same
/// session always routes to the same shard after a restart.
pub fn shard_session(session: &SessionId, shards: usize) -> usize {
    if shards == 0 {
        return 0;
    }
    (fnv1a_hash(&session.0) as usize) % shards
}

/// A request that was dropped (shed) by the pipeline due to backpressure or
/// failure.  Stored in the [`DeadLetterQueue`] for inspection and replay.
#[derive(Debug, Clone)]
pub struct DroppedRequest {
    /// The original request ID for trace correlation.
    pub request_id: String,
    /// The session this request belonged to.
    pub session_id: String,
    /// Human-readable reason the request was dropped.
    pub reason: String,
    /// Wall-clock time at which the request was dropped.
    pub dropped_at: std::time::SystemTime,
}

/// In-memory dead-letter queue for shed pipeline requests.
///
/// Stores up to `capacity` most-recent dropped requests in a ring buffer.
/// When full, the oldest entry is evicted to make room for the newest.
///
/// Thread-safe via an internal `Mutex`.  Clone is cheap — all clones share
/// the same underlying ring.
#[derive(Clone)]
pub struct DeadLetterQueue {
    inner: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<DroppedRequest>>>,
    capacity: usize,
}

impl DeadLetterQueue {
    /// Create a new `DeadLetterQueue` with the given ring-buffer capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(capacity.min(1024)),
            )),
            capacity,
        }
    }

    /// Push a dropped request into the queue.  Evicts the oldest entry if full.
    pub fn push(&self, req: DroppedRequest) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| {
            tracing::warn!("DeadLetterQueue: recovering from poisoned mutex");
            crate::metrics::inc_dlq_lock_poisoned();
            p.into_inner()
        });
        if guard.len() >= self.capacity {
            guard.pop_front();
        }
        guard.push_back(req);
    }

    /// Drain all queued entries and return them, clearing the queue.
    pub fn drain(&self) -> Vec<DroppedRequest> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| {
            tracing::warn!("DeadLetterQueue: recovering from poisoned mutex");
            crate::metrics::inc_dlq_lock_poisoned();
            p.into_inner()
        });
        guard.drain(..).collect()
    }

    /// Return the number of entries currently in the queue.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| {
                tracing::warn!("DeadLetterQueue: recovering from poisoned mutex");
                crate::metrics::inc_dlq_lock_poisoned();
                p.into_inner()
            })
            .len()
    }

    /// Return `true` if the queue contains no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Type alias for the optional OpenTelemetry tracing layer used in main.rs.
///
/// Exported so binary crates can declare `Option<OtelLayer>` without spelling
/// out the full generic type.
pub type OtelLayer = tracing_opentelemetry::OpenTelemetryLayer<
    tracing_subscriber::Registry,
    opentelemetry_sdk::trace::Tracer,
>;

/// Initialise tracing with env-filter support. Call once at binary startup.
///
/// If `RUST_LOG_FORMAT=json` is set the subscriber emits newline-delimited
/// JSON suitable for log aggregation pipelines.  Otherwise the human-readable
/// `fmt` pretty format is used for local development.
///
/// An OpenTelemetry layer is added when `OTEL_EXPORTER_OTLP_ENDPOINT` or
/// `JAEGER_ENDPOINT` is present in the environment; otherwise the OTel layer
/// is omitted so the binary runs without a collector.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter, Layer, Registry};

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let use_json = std::env::var("RUST_LOG_FORMAT")
        .map(|v| v.to_lowercase() == "json")
        .unwrap_or(false);

    // Build the OTel layer if an endpoint is configured, boxing it so the
    // concrete type does not propagate into the subscriber stack.
    let otel_layer: Option<Box<dyn Layer<Registry> + Send + Sync>> =
        try_build_otel_layer().map(|l| l.boxed());

    if use_json {
        let subscriber = Registry::default()
            .with(otel_layer)
            .with(env_filter)
            .with(fmt::layer().json().with_target(false));
        let _ = tracing::subscriber::set_global_default(subscriber);
    } else {
        let subscriber = Registry::default()
            .with(otel_layer)
            .with(env_filter)
            .with(fmt::layer().with_target(false));
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}

/// Attempt to build an OpenTelemetry tracing layer, returning `None` on error.
///
/// Reads `JAEGER_ENDPOINT` (e.g. `http://localhost:4318`) or
/// `OTEL_EXPORTER_OTLP_ENDPOINT`.  When neither is set, or when the exporter
/// fails to build, this function returns `None` so startup is never blocked
/// by observability infrastructure.
///
/// **NOTE**: This function must be called **after** the Tokio runtime has been
/// started because the batch exporter uses `rt-tokio`.
///
/// # Panics
///
/// This function never panics.
pub fn try_build_otel_layer() -> Option<
    tracing_opentelemetry::OpenTelemetryLayer<
        tracing_subscriber::Registry,
        opentelemetry_sdk::trace::Tracer,
    >,
> {
    use opentelemetry::global;
    use opentelemetry_otlp::WithExportConfig;

    let endpoint = std::env::var("JAEGER_ENDPOINT")
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"))
        .ok()?;

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("OTel exporter build failed: {e}");
            return None;
        }
    };

    let provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(opentelemetry_sdk::Resource::new(vec![
            opentelemetry::KeyValue::new("service.name", "tokio-prompt-orchestrator"),
        ]))
        .build();

    use opentelemetry::trace::TracerProvider as _;
    let tracer = provider.tracer("tokio-prompt-orchestrator");
    global::set_tracer_provider(provider);
    Some(tracing_opentelemetry::layer().with_tracer(tracer))
}

/// Outcome of a [`send_with_shed`] call.
///
/// Distinguishes between successful delivery and a graceful shed so callers
/// can log/metric them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The item was successfully placed in the channel.
    Queued,
    /// The channel was full; the item was dropped to shed load.
    Shed,
}

/// Pipeline stage identifier for metrics and logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PipelineStage {
    /// Retrieval-augmented generation — fetches context before assembly.
    Rag,
    /// Prompt assembly — combines context and user input into a full prompt.
    Assemble,
    /// Inference — sends the assembled prompt to the model backend.
    Inference,
    /// Post-processing — formats and filters the raw model response.
    Post,
    /// Streaming output — delivers the processed response downstream.
    Stream,
}

impl PipelineStage {
    /// Return the canonical lowercase ASCII name used in metrics labels.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rag => "rag",
            Self::Assemble => "assemble",
            Self::Inference => "inference",
            Self::Post => "post",
            Self::Stream => "stream",
        }
    }
}

impl std::fmt::Display for PipelineStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Send with graceful shedding on backpressure.
///
/// # Returns
/// - `Ok(SendOutcome::Queued)` — item was accepted by the channel.
/// - `Ok(SendOutcome::Shed)` — channel was full; item was dropped gracefully.
/// - `Err(OrchestratorError::ChannelClosed)` — the receiver has been dropped.
///
/// # Panics
///
/// This function never panics.
pub async fn send_with_shed<T>(
    tx: &tokio::sync::mpsc::Sender<T>,
    item: T,
    stage: PipelineStage,
) -> Result<SendOutcome, OrchestratorError> {
    match tx.try_send(item) {
        Ok(_) => Ok(SendOutcome::Queued),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(stage = %stage, "queue full, shedding request");
            Ok(SendOutcome::Shed)
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err(OrchestratorError::ChannelClosed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_with_shed_returns_shed_outcome_when_channel_full() {
        // Channel of capacity 1 — fill it, then send again to trigger shed
        let (tx, _rx) = tokio::sync::mpsc::channel::<u32>(1);
        // Fill the channel
        let first = send_with_shed(&tx, 1u32, PipelineStage::Rag).await.unwrap();
        assert_eq!(first, SendOutcome::Queued, "first send should be Queued");
        // Channel is now full — next send must be Shed
        let second = send_with_shed(&tx, 2u32, PipelineStage::Rag).await.unwrap();
        assert_eq!(
            second,
            SendOutcome::Shed,
            "second send on full channel should be Shed"
        );
    }

    #[tokio::test]
    async fn test_send_with_shed_returns_queued_when_space_available() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<u32>(10);
        let outcome = send_with_shed(&tx, 42u32, PipelineStage::Rag).await.unwrap();
        assert_eq!(outcome, SendOutcome::Queued);
    }

    #[tokio::test]
    async fn test_send_with_shed_returns_error_when_channel_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel::<u32>(10);
        drop(rx);
        let result = send_with_shed(&tx, 1u32, PipelineStage::Rag).await;
        assert!(matches!(result, Err(OrchestratorError::ChannelClosed)));
    }

    #[test]
    fn test_shard_session_deterministic() {
        let s = SessionId::new("test-session-123");
        assert_eq!(shard_session(&s, 4), shard_session(&s, 4));
    }

    #[test]
    fn test_shard_session_distribution() {
        let sessions: Vec<_> = (0..100).map(|i| SessionId::new(format!("s-{i}"))).collect();
        let counts: Vec<_> = (0..4usize)
            .map(|sh| {
                sessions
                    .iter()
                    .filter(|s| shard_session(s, 4) == sh)
                    .count()
            })
            .collect();
        assert!(counts.iter().all(|&c| c > 0));
    }

    /// Verify that tracing events can be captured and that when RUST_LOG_FORMAT=json
    /// is set, the output is valid newline-delimited JSON with expected fields.
    #[test]
    fn test_json_log_output_is_valid_json() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

        // Shared buffer to capture log output.
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let buf_clone = buf.clone();

        // Build an isolated JSON subscriber for this test.
        let writer = tracing_subscriber::fmt::writer::BoxMakeWriter::new(move || {
            struct BufWriter(Arc<Mutex<Vec<u8>>>);
            impl std::io::Write for BufWriter {
                fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                    self.0
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend_from_slice(b);
                    Ok(b.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            BufWriter(buf_clone.clone())
        });

        let subscriber = tracing_subscriber::Registry::default()
            .with(EnvFilter::new("info"))
            .with(fmt::layer().json().with_writer(writer).with_target(false));

        // Use a local dispatcher so this test doesn't interfere with globals.
        let _guard = tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                stage = "rag",
                session_id = "s1",
                request_id = "r1",
                "test event"
            );
        });

        let captured = buf.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert!(!captured.is_empty(), "captured log must be non-empty");

        let text = std::str::from_utf8(&captured).expect("log output must be valid UTF-8");
        // Each line must be valid JSON.
        for line in text.lines().filter(|l| !l.is_empty()) {
            let v: serde_json::Value =
                serde_json::from_str(line).expect("each log line must be valid JSON");
            // Verify expected fields are present.
            assert!(v.get("fields").is_some(), "JSON log must have 'fields' key");
        }
    }

    /// Verify that trace IDs remain consistent across a pipeline processing a
    /// single request (OTel context propagation).  Without a live collector the
    /// test only checks that the tracing infrastructure works without panicking;
    /// the trace_id field is non-zero within a span.
    #[test]
    fn test_trace_id_is_non_zero_within_span() {
        use opentelemetry::trace::{SpanContext, TraceContextExt};
        use tracing_opentelemetry::OpenTelemetrySpanExt;
        use tracing_subscriber::layer::SubscriberExt;

        // Set up a minimal OTel provider with default config (no exporter).
        let provider = opentelemetry_sdk::trace::TracerProvider::builder().build();
        use opentelemetry::trace::TracerProvider as _;
        let tracer = provider.tracer("test");

        let subscriber = tracing_subscriber::Registry::default()
            .with(tracing_opentelemetry::layer().with_tracer(tracer));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("test.root");
            let _guard = span.enter();
            let ctx = tracing::Span::current().context();
            let span_ref = ctx.span();
            let span_ctx: &SpanContext = span_ref.span_context();
            // trace_id must be non-zero when within a valid span.
            assert!(
                span_ctx.is_valid(),
                "span context must be valid inside an instrumented span"
            );
            assert_ne!(
                span_ctx.trace_id(),
                opentelemetry::trace::TraceId::INVALID,
                "trace_id must be non-zero"
            );
        });
    }

    #[test]
    fn test_prompt_request_with_deadline_sets_future_instant() {
        use std::time::Duration;

        let req = PromptRequest {
            session: SessionId::new("s1"),
            request_id: "r1".to_string(),
            input: "hello".to_string(),
            meta: HashMap::new(),
            deadline: None,
        }
        .with_deadline(Duration::from_secs(10));

        let deadline = req.deadline.expect("deadline must be Some after with_deadline");
        assert!(
            deadline > std::time::Instant::now(),
            "deadline must be in the future"
        );
    }

    #[test]
    fn test_prompt_request_default_deadline_is_none() {
        let req = PromptRequest {
            session: SessionId::new("s2"),
            request_id: "r2".to_string(),
            input: "world".to_string(),
            meta: HashMap::new(),
            deadline: None,
        };
        assert!(req.deadline.is_none(), "default deadline must be None");
    }
}
