//! # tokio-prompt-orchestrator
//!
//! A production-grade orchestrator for multi-stage LLM inference pipelines over Tokio.
//!
//! Provides a five-stage directed pipeline with bounded MPSC channels, backpressure,
//! request deduplication, circuit breakers, retry logic, rate limiting, Prometheus
//! metrics, OpenTelemetry tracing, and an optional autonomous self-improving control
//! loop.
//!
//! ## Architecture
//!
//! Five-stage pipeline with bounded channels and backpressure:
//!
//! ```text
//! PromptRequest -> RAG(512) -> Assemble(512) -> Inference(1024) -> Post(512) -> Stream(256)
//! ```
//!
//! Each stage runs as an independent [`tokio::task`]. When a downstream channel
//! fills, [`send_with_shed`] drops the incoming item and records it in the
//! [`DeadLetterQueue`] rather than blocking the upstream stage.
//!
//! ## Quick Start
//!
//! ```no_run
//! use std::collections::HashMap;
//! use std::sync::Arc;
//! use tokio_prompt_orchestrator::{spawn_pipeline, EchoWorker, PromptRequest, SessionId, ModelWorker};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
//!     let handles = spawn_pipeline(worker);
//!
//!     handles.input_tx.send(PromptRequest {
//!         session: SessionId::new("demo"),
//!         request_id: "req-1".to_string(),
//!         input: "Hello, pipeline!".to_string(),
//!         meta: HashMap::new(),
//!         deadline: None,
//!     }).await?;
//!
//!     let mut guard = handles.output_rx.lock().await;
//!     if let Some(rx) = guard.as_mut() {
//!         if let Some(output) = rx.recv().await {
//!             println!("{}", output.text);
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Module Organisation
//!
//! | Module | Description |
//! |--------|-------------|
//! | [`ab_test`] | Prompt A/B testing — consistent hashing assignment, Welch's t-test winner determination, Cohen's d effect size |
//! | [`stages`] | Five pipeline stage implementations and channel wiring |
//! | [`worker`] | [`ModelWorker`] trait and five production implementations |
//! | [`enhanced`] | Resilience primitives: circuit breaker, dedup, semantic dedup (SimHash), retry, cache, rate limiter, smart batching, tournament mode |
//! | [`metrics`] | Prometheus metrics initialisation and helper functions |
//! | [`config`] | TOML-deserialisable [`PipelineConfig`] with hot-reload support |
//! | [`routing`] | [`ModelRouter`] for complexity-scored routing; [`ArbitrageEngine`] for SLA-aware cheapest-provider selection; [`PoolSizer`] for adaptive worker scaling |
//! | [`security`] | [`PromptGuard`] — prompt injection and jailbreak detection middleware (zero external I/O) |
//! | [`coordination`] | Agent fleet management and task claiming |
//! | [`session`] | Multi-turn conversation context manager — auto-injects history per session |
//! | [`cascade`] | Multi-turn cascading inference engine — tool call loops with pluggable executors |
//! | [`multi_pipeline`] | Named pipeline fleet with heuristic prompt classification and per-class routing |
//! | [`adaptive_pool`] | Kalman-filter adaptive worker pool controller — predicts queue depth and recommends scale events |
//! | `self_tune` | PID controllers and telemetry bus (feature: `self-tune`) |
//! | `self_modify` | Task generation and validation gate (feature: `self-modify`) |
//! | `intelligence` | Learned router and autoscaler (feature: `intelligence`) |
//! | `evolution` | A/B experiments and snapshot rollback (feature: `evolution`) |
//! | `self_improve` | Wired self-improving loop (features: `self-tune`+`self-modify`+`intelligence`) |
//! | `web_api` | REST/SSE/WebSocket server (feature: `web-api`) |
//! | `distributed` | Redis dedup and NATS coordination (feature: `distributed`) |
//! | `tui` | Ratatui terminal dashboard (feature: `tui`) |
//!
//! [`PipelineConfig`]: config::PipelineConfig
//! [`ModelRouter`]: routing::router::ModelRouter
//! [`ArbitrageEngine`]: routing::ArbitrageEngine
//! [`PoolSizer`]: routing::PoolSizer
//! [`PromptGuard`]: security::PromptGuard

#![doc = include_str!("../README.md")]

use std::collections::HashMap;
use thiserror::Error;

pub mod ab_test;
pub mod adaptive_pool;
pub mod adaptive_timeout;
pub mod cost_estimator;
pub mod admission_control;
pub mod circuit_breaker;
pub mod cache;
pub mod cascade;
pub mod compression;
pub mod config;
pub mod conversation;
pub mod conversation_state;
pub mod eval_harness;
pub mod coordination;
pub mod failover;
pub mod provider_health;
pub mod rate_limiter;
pub mod semantic_cache;
pub mod smart_router;
#[cfg(feature = "distributed")]
pub mod distributed;
pub mod enhanced;
pub mod multi_pipeline;
pub mod templates;
pub mod load_balancer;
pub mod template;
pub mod metrics;
pub mod plugin;
pub mod request_dedup;
pub mod routing;
pub mod scheduler;
pub mod security;
pub mod session;
pub mod session_mgr;
pub mod job_scheduler;
pub mod context_mgr;
pub mod stream_agg;
pub mod stages;
pub mod token_budget;
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

pub mod audit;
pub mod feedback_loop;
pub mod conversation_graph;
pub mod pipeline;
pub mod cost_optimizer;
pub mod prompt_optimizer;
pub mod trace_export;
pub mod trace_ui;
pub mod webhooks;
pub mod priority_queue;
pub mod hot_config;
pub mod observability;
pub mod retry_policy;
pub mod worker_pool;
pub mod model_selector;
pub mod intent_classifier;
pub mod response_validator;
pub mod prompt_versioning;
pub mod multi_modal;
pub mod persona_manager;
pub mod chain_of_thought;
pub mod streaming_processor;
pub mod model_fallback;
pub mod provider_manager;
pub mod context_compression;
pub mod prompt_router;
pub mod tool_call_parser;
pub mod session_manager;
pub mod output_cache;
pub mod pipeline_builder;
pub mod prompt_safety;
pub mod experiment_runner;
pub mod prompt_template;
pub mod response_classifier;
pub mod conversation_analyzer;
pub mod retry_budget;
pub mod model_registry;
pub mod prompt_validator;
pub mod cache_warmer;
pub mod token_counter;

// Re-exports
pub use cache::{CacheConfig, CacheEntry, CacheStats, PromptCache};
pub use rate_limiter::{BucketConfig, ModelRateLimiterStats, RateLimitError, RateLimiter, RateLimiterStats};
pub use ab_test::{AbTestConfig, AbTestResult, AbTestRunner, SuccessMetric, Variant};
pub use conversation::{
    ConversationConfig, ConversationManager, PromptFormat, Role, Turn,
};
pub use stages::{
    spawn_pipeline, spawn_pipeline_with_config, LogSink, OutputSink, PipelineHandles, SinkError,
};
pub use templates::{
    AbExperiment, ExperimentReport, ExperimentVariant, PromptTemplate, TemplateError,
    TemplateRegistry,
};
pub use worker::{
    stream_worker, AnthropicWorker, EchoWorker, LlamaCppWorker, LoadBalancedWorker, ModelWorker,
    OpenAiWorker, VllmWorker,
};
pub use failover::FailoverChain;
pub use provider_health::{ProviderHealth, ProviderHealthMonitor};
pub use smart_router::{ModelPricing, RoutingDecision, RoutingRequirements, SmartRouter};
pub use load_balancer::{
    BalancerConfig, EndpointStats, LoadBalancer, LoadBalancerStats, ModelEndpoint,
};
pub use template::{TemplateContext, TemplateLibrary, TemplateValue};
pub use pipeline::{
    AppendStage, LanguageDetectStage, Pipeline, PipelineBuilder, PipelineError, PipelineResult,
    PipelineStats, PrependStage, RegexReplaceStage, TrimStage, TruncateStage,
};
pub use pipeline::PipelineStage as PromptPipelineStage;
pub use audit::{AuditEntry, AuditFilter, AuditLog, AuditQueryResponse, AuditStats, AuditStatsResponse};

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

    /// Provider rejected the request due to an authentication failure.
    ///
    /// Returned when the provider returns HTTP 401 or 403.
    /// Not retryable without a credential rotation.
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// The inference call exceeded the configured timeout and was cancelled.
    ///
    /// The `timeout_secs` field reflects the timeout that was breached.
    /// Retryable with a longer timeout or by shedding the request.
    #[error("inference timed out after {timeout_secs}s")]
    InferenceTimeout {
        /// Configured timeout that was exceeded.
        timeout_secs: u64,
    },

    /// A catch-all error variant for errors that do not fit the other categories.
    #[error("{0}")]
    Other(String),
}

impl OrchestratorError {
    /// Return a stable lowercase string identifying the error variant,
    /// suitable for use as a metric label or structured log field.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn error_kind(&self) -> &'static str {
        match self {
            Self::ChannelClosed         => "channel_closed",
            Self::Inference(_)          => "inference",
            Self::ConfigError(_)        => "config_error",
            Self::RateLimited { .. }    => "rate_limited",
            Self::BudgetExceeded { .. } => "budget_exceeded",
            Self::AuthFailed(_)         => "auth_failed",
            Self::InferenceTimeout { .. } => "inference_timeout",
            Self::Other(_)              => "other",
        }
    }

    /// Return `true` if retrying the request may succeed.
    ///
    /// Transient errors (`RateLimited`, `InferenceTimeout`, `Inference`) are
    /// retryable. Permanent errors (`ConfigError`, `AuthFailed`,
    /// `BudgetExceeded`, `ChannelClosed`) are not.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_prompt_orchestrator::OrchestratorError;
    ///
    /// assert!(OrchestratorError::RateLimited { retry_after_secs: 5 }.is_retryable());
    /// assert!(!OrchestratorError::AuthFailed("bad key".into()).is_retryable());
    /// ```
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::InferenceTimeout { .. } | Self::Inference(_)
        )
    }
}

/// Unique session identifier for request tracking and affinity
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Create a new `SessionId` from any string-like value.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the inner string slice.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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
    /// This is an infallible convenience wrapper around [`try_with_deadline`].
    /// It accepts any positive `Duration`; for validated input (e.g. user-supplied
    /// values) prefer [`try_with_deadline`] which rejects zero or excessively large
    /// timeouts at the call site.
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
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn with_deadline(mut self, duration: std::time::Duration) -> Self {
        self.deadline = Some(std::time::Instant::now() + duration);
        self
    }

    /// Validated builder that sets an absolute deadline `timeout_seconds` from now.
    ///
    /// Accepts a timeout expressed as a whole number of seconds and validates it
    /// before storing the deadline.
    ///
    /// # Errors
    ///
    /// Returns `Err(OrchestratorError::ConfigError(_))` when:
    /// - `timeout_seconds` is `0` — a zero-second deadline expires immediately.
    /// - `timeout_seconds` exceeds `3600` — unreasonably large timeouts are
    ///   rejected to prevent accidental resource leaks.
    ///
    /// # Example
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use tokio_prompt_orchestrator::{PromptRequest, SessionId};
    ///
    /// let req = PromptRequest {
    ///     session: SessionId::new("s1"),
    ///     request_id: "r1".to_string(),
    ///     input: "hello".to_string(),
    ///     meta: HashMap::new(),
    ///     deadline: None,
    /// }
    /// .try_with_deadline(30)
    /// .expect("30s is a valid timeout");
    ///
    /// assert!(req.deadline.is_some());
    /// ```
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn try_with_deadline(
        mut self,
        timeout_seconds: u64,
    ) -> Result<Self, OrchestratorError> {
        if timeout_seconds == 0 {
            return Err(OrchestratorError::ConfigError(
                "timeout_seconds must be > 0".into(),
            ));
        }
        if timeout_seconds > 3600 {
            return Err(OrchestratorError::ConfigError(
                "timeout_seconds must be \u{2264} 3600".into(),
            ));
        }
        self.deadline = Some(
            std::time::Instant::now()
                + std::time::Duration::from_secs(timeout_seconds),
        );
        Ok(self)
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
    s.bytes()
        .fold(BASIS, |acc, b| acc.wrapping_mul(PRIME) ^ b as u64)
}

/// Session affinity sharding helper.
///
/// Uses FNV-1a for stable hashing across process restarts so that the same
/// session always routes to the same shard after a restart.
///
/// # Panics
///
/// This function does not panic.
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
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(capacity.min(1024)),
            )),
            capacity,
        }
    }

    /// Push a dropped request into the queue.  Evicts the oldest entry if full.
    ///
    /// # Panics
    ///
    /// This function does not panic. If the internal mutex is poisoned it is
    /// recovered automatically and a warning is logged.
    pub fn push(&self, req: DroppedRequest) {
        // NOTE: std::sync::Mutex is safe here because the critical section is
        // extremely short (a deque push plus an optional pop_front) and there
        // is no `.await` inside the guard.  Using a sync lock avoids the
        // overhead of a tokio::sync::Mutex while keeping the operation
        // compatible with both sync and async callers.
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
    ///
    /// # Panics
    ///
    /// This function does not panic. If the internal mutex is poisoned it is
    /// recovered automatically and a warning is logged.
    pub fn drain(&self) -> Vec<DroppedRequest> {
        // NOTE: std::sync::Mutex is safe here because the critical section is
        // a single drain-and-collect with no `.await` inside the guard.  The
        // lock is always released before any async work can be scheduled.
        let mut guard = self.inner.lock().unwrap_or_else(|p| {
            tracing::warn!("DeadLetterQueue: recovering from poisoned mutex");
            crate::metrics::inc_dlq_lock_poisoned();
            p.into_inner()
        });
        guard.drain(..).collect()
    }

    /// Return the number of entries currently in the queue.
    ///
    /// # Panics
    ///
    /// This function does not panic. If the internal mutex is poisoned it is
    /// recovered automatically and a warning is logged.
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
    ///
    /// # Panics
    ///
    /// This function does not panic. If the internal mutex is poisoned it is
    /// recovered automatically and a warning is logged.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return the maximum number of entries this queue can hold before evicting.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Return a snapshot of all queued entries without removing them.
    ///
    /// Unlike [`drain`](Self::drain), this does not clear the queue. The
    /// snapshot is a clone taken under the lock; the queue continues to
    /// operate normally while the returned `Vec` is used.
    ///
    /// # Panics
    ///
    /// This function does not panic. If the internal mutex is poisoned it is
    /// recovered automatically and a warning is logged.
    pub fn peek(&self) -> Vec<DroppedRequest> {
        self.inner
            .lock()
            .unwrap_or_else(|p| {
                tracing::warn!("DeadLetterQueue: recovering from poisoned mutex");
                crate::metrics::inc_dlq_lock_poisoned();
                p.into_inner()
            })
            .iter()
            .cloned()
            .collect()
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
/// # Panics
///
/// This function does not panic.
///
/// ## Log format
///
/// If `RUST_LOG_FORMAT=json` is set the subscriber emits newline-delimited
/// JSON suitable for log aggregation pipelines.  Otherwise the human-readable
/// `fmt` pretty format is used for local development.
///
/// ## OpenTelemetry OTLP export
///
/// If the environment variable `OTEL_EXPORTER_OTLP_ENDPOINT` (or the legacy
/// `JAEGER_ENDPOINT`) is set to a valid OTLP collector URL (e.g.
/// `http://localhost:4318`), spans are exported via OTLP HTTP to that endpoint
/// using a batch exporter on the Tokio runtime.
///
/// If neither variable is set (the common case in local development), the OTel
/// layer is **silently omitted** — no error is printed and the binary starts
/// normally.  All other tracing output (stdout / log files) is unaffected.
///
/// A startup `info!` log is emitted either way so operators can confirm the
/// observability configuration at a glance:
///
/// - `"OpenTelemetry OTLP export enabled, sending to <endpoint>"`
/// - `"OpenTelemetry OTLP disabled (set OTEL_EXPORTER_OTLP_ENDPOINT to enable)"`
///
/// ## Calling requirement
///
/// This function **must** be called after the Tokio runtime has started because
/// the OTLP batch exporter uses `rt-tokio` internally.
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

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .or_else(|_| std::env::var("JAEGER_ENDPOINT"))
        .ok();

    let endpoint = match endpoint {
        Some(ep) => {
            tracing::info!(
                endpoint = ep.as_str(),
                "OpenTelemetry OTLP export enabled, sending to {ep}"
            );
            ep
        }
        None => {
            tracing::info!(
                "OpenTelemetry OTLP disabled (set OTEL_EXPORTER_OTLP_ENDPOINT to enable)"
            );
            return None;
        }
    };

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
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rag => "rag",
            Self::Assemble => "assemble",
            Self::Inference => "inference",
            Self::Post => "post",
            Self::Stream => "stream",
        }
    }

    /// Return a slice of all pipeline stage variants in pipeline order.
    ///
    /// Useful for iterating over all stages when initialising per-stage metrics
    /// or building dashboards.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_prompt_orchestrator::PipelineStage;
    ///
    /// let labels: Vec<&str> = PipelineStage::all().iter().map(|s| s.as_str()).collect();
    /// assert_eq!(labels, ["rag", "assemble", "inference", "post", "stream"]);
    /// ```
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn all() -> &'static [PipelineStage] {
        &[
            PipelineStage::Rag,
            PipelineStage::Assemble,
            PipelineStage::Inference,
            PipelineStage::Post,
            PipelineStage::Stream,
        ]
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
        let outcome = send_with_shed(&tx, 42u32, PipelineStage::Rag)
            .await
            .unwrap();
        assert_eq!(outcome, SendOutcome::Queued, "send into channel with space must return Queued");
    }

    #[tokio::test]
    async fn test_send_with_shed_returns_error_when_channel_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel::<u32>(10);
        drop(rx);
        let result = send_with_shed(&tx, 1u32, PipelineStage::Rag).await;
        assert!(matches!(result, Err(OrchestratorError::ChannelClosed)), "send after receiver drop must return ChannelClosed error");
    }

    #[test]
    fn test_shard_session_deterministic() {
        let s = SessionId::new("test-session-123");
        assert_eq!(shard_session(&s, 4), shard_session(&s, 4), "shard_session must be deterministic for the same session and shard count");
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
        assert!(counts.iter().all(|&c| c > 0), "each of the 4 shards must receive at least one session from the 100-session sample");
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

        let deadline = req
            .deadline
            .expect("deadline must be Some after with_deadline");
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
