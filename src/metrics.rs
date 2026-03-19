//! Prometheus metrics for the orchestrator pipeline.
//!
//! ## Usage
//!
//! Call [`init_metrics`] once at process startup **before** spawning any pipeline
//! stages. The helper functions (`record_stage_latency`, `inc_request`, …) are
//! no-ops if `init_metrics` was never called, so the pipeline is always safe to
//! run  -  observability simply degrades gracefully.
//!
//! ## Metrics Exposed
//!
//! | Name | Type | Labels |
//! |------|------|--------|
//! | `orchestrator_requests_total` | Counter | `stage` |
//! | `orchestrator_requests_shed_total` | Counter | `stage` |
//! | `orchestrator_requests_dropped_total` | Counter | `stage` |
//! | `orchestrator_errors_total` | Counter | `stage`, `err_type` |
//! | `orchestrator_stage_duration_seconds` | Histogram | `stage` |
//! | `orchestrator_queue_depth` | Gauge | `stage` |
//! | `inference_time_to_first_token_seconds` | Histogram | `worker`, `model` |
//! | `orchestrator_requests_expired_total` | Counter | (none) |

use crate::OrchestratorError;
use prometheus::{
    core::Collector, Counter, CounterVec, Encoder, GaugeVec, HistogramOpts, HistogramVec,
    IntGaugeVec, Opts, Registry, TextEncoder,
};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

//  Internal metrics bundle

/// All Prometheus metrics for the orchestrator, bundled together so they can
/// be stored in a single [`OnceLock`] and initialised atomically.
pub struct Metrics {
    /// Prometheus registry that owns all metric descriptors.
    pub registry: Registry,
    /// Total requests processed per stage.
    pub requests_total: CounterVec,
    /// Requests shed (queue full) per stage.
    pub requests_shed: CounterVec,
    /// Requests dropped due to full queue, labelled by stage.
    pub requests_dropped: CounterVec,
    /// Errors by stage and error type.
    pub errors_total: CounterVec,
    /// Stage processing latency histogram.
    pub stage_duration: HistogramVec,
    /// Current queue depth per stage.
    pub queue_depth: IntGaugeVec,
    /// Cumulative USD cost of all inference calls.
    pub inference_cost_usd: prometheus::Counter,
    /// Time from request start to first streaming token, labelled by worker and model.
    pub ttft_seconds: HistogramVec,
    /// Total dedup hits (in-progress or cached responses returned without new inference).
    pub dedup_hits_total: Counter,
    /// Total dedup waiters unblocked (requests that waited and received a broadcast result).
    pub dedup_waiters_unblocked_total: Counter,
    /// Total inference timeouts.
    pub inference_timeouts_total: Counter,
    /// Requests dropped because their deadline had already passed at dequeue time.
    pub requests_expired_total: Counter,
    /// Incremented when a `DeadLetterQueue` mutex is recovered from a poisoned state.
    pub dlq_lock_poisoned_total: Counter,
    /// Incremented when a RAG-stage request is dropped because its deadline expired.
    pub rag_requests_expired_total: Counter,
    /// Circuit breaker state transitions, labelled by target state.
    pub circuit_breaker_state_transitions_total: CounterVec,
    /// Requests rejected by the circuit breaker (open state).
    pub circuit_breaker_requests_rejected_total: Counter,
    /// Config hot-reload attempts that failed validation or parse.
    pub config_reload_errors_total: Counter,
    /// Errors returned by a specific named worker, labelled by worker name.
    pub worker_errors_total: CounterVec,
    /// Cache hits on the response cache (in-memory or Redis).
    pub cache_hits_total: Counter,
    /// Cache misses on the response cache (in-memory or Redis).
    pub cache_misses_total: Counter,
    /// Current number of tokens remaining in the rate-limiter bucket per session.
    pub rate_limiter_tokens_remaining: GaugeVec,
    /// Duration of config hot-reload operations (parse + validate), in seconds.
    pub config_reload_duration_seconds: HistogramVec,
    /// Session affinity routing hits (request routed to its preferred shard).
    pub session_affinity_hits_total: Counter,
    /// Session affinity routing misses (preferred shard unavailable, rerouted).
    pub session_affinity_misses_total: Counter,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

//  Initialisation

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

    let requests_dropped = CounterVec::new(
        Opts::new(
            "requests_dropped_total",
            "Requests dropped due to full queue per stage",
        ),
        &["stage"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(requests_dropped.clone()))
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

    let inference_cost_usd = Counter::with_opts(Opts::new(
        "orchestrator_inference_cost_usd_total",
        "Cumulative USD cost of all inference calls",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(inference_cost_usd.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let ttft_seconds = HistogramVec::new(
        HistogramOpts::new(
            "inference_time_to_first_token_seconds",
            "Time from request start to first streaming token received",
        )
        .buckets(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0]),
        &["worker", "model"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(ttft_seconds.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let dedup_hits_total = Counter::with_opts(Opts::new(
        "orchestrator_dedup_hits_total",
        "Requests served from dedup cache (in-progress or completed) without new inference",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(dedup_hits_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let dedup_waiters_unblocked_total = Counter::with_opts(Opts::new(
        "orchestrator_dedup_waiters_unblocked_total",
        "Requests that waited on an in-progress dedup entry and received its broadcast result",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(dedup_waiters_unblocked_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let inference_timeouts_total = Counter::with_opts(Opts::new(
        "orchestrator_inference_timeouts_total",
        "Inference calls that exceeded the configured timeout and were cancelled",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(inference_timeouts_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let requests_expired_total = Counter::with_opts(Opts::new(
        "orchestrator_requests_expired_total",
        "Requests dropped at the inference stage because their deadline had already passed",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(requests_expired_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let dlq_lock_poisoned_total = Counter::with_opts(Opts::new(
        "orchestrator_dlq_lock_poisoned_total",
        "Number of times a DeadLetterQueue mutex was recovered from a poisoned state",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(dlq_lock_poisoned_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let rag_requests_expired_total = Counter::with_opts(Opts::new(
        "orchestrator_rag_requests_expired_total",
        "Requests dropped at the RAG stage because their deadline had already passed",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(rag_requests_expired_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let circuit_breaker_state_transitions_total = CounterVec::new(
        Opts::new(
            "orchestrator_circuit_breaker_state_transitions_total",
            "Circuit breaker state transitions, labelled by target state",
        ),
        &["state"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(circuit_breaker_state_transitions_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let circuit_breaker_requests_rejected_total = Counter::with_opts(Opts::new(
        "orchestrator_circuit_breaker_requests_rejected_total",
        "Requests rejected by the circuit breaker because the circuit is open",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(circuit_breaker_requests_rejected_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let config_reload_errors_total = Counter::with_opts(Opts::new(
        "orchestrator_config_reload_errors_total",
        "Config hot-reload attempts that failed validation or parse",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(config_reload_errors_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let worker_errors_total = CounterVec::new(
        Opts::new(
            "orchestrator_worker_errors_total",
            "Errors returned by a named worker, labelled by worker name",
        ),
        &["worker"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(worker_errors_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let cache_hits_total = Counter::with_opts(Opts::new(
        "orchestrator_cache_hits_total",
        "Response cache hits (in-memory or Redis)",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(cache_hits_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let cache_misses_total = Counter::with_opts(Opts::new(
        "orchestrator_cache_misses_total",
        "Response cache misses (in-memory or Redis)",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(cache_misses_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let rate_limiter_tokens_remaining = GaugeVec::new(
        Opts::new(
            "orchestrator_rate_limiter_tokens_remaining",
            "Current token-bucket tokens remaining for a session",
        ),
        &["session"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(rate_limiter_tokens_remaining.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let config_reload_duration_seconds = HistogramVec::new(
        HistogramOpts::new(
            "orchestrator_config_reload_duration_seconds",
            "Duration of config hot-reload operations (parse + validate)",
        )
        .buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]),
        &["result"],
    )
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(config_reload_duration_seconds.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let session_affinity_hits_total = Counter::with_opts(Opts::new(
        "orchestrator_session_affinity_hits_total",
        "Requests routed to their preferred affinity shard",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(session_affinity_hits_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    let session_affinity_misses_total = Counter::with_opts(Opts::new(
        "orchestrator_session_affinity_misses_total",
        "Requests rerouted because preferred affinity shard was unavailable",
    ))
    .map_err(|e| OrchestratorError::Other(format!("metrics init failed: {e}")))?;
    registry
        .register(Box::new(session_affinity_misses_total.clone()))
        .map_err(|e| OrchestratorError::Other(format!("metrics registration failed: {e}")))?;

    // If another thread raced us, the first one wins  -  both initializations
    // produce identical metric descriptors, so neither outcome is incorrect.
    let _ = METRICS.set(Metrics {
        registry,
        requests_total,
        requests_shed,
        requests_dropped,
        errors_total,
        stage_duration,
        queue_depth,
        inference_cost_usd,
        ttft_seconds,
        dedup_hits_total,
        dedup_waiters_unblocked_total,
        inference_timeouts_total,
        requests_expired_total,
        dlq_lock_poisoned_total,
        rag_requests_expired_total,
        circuit_breaker_state_transitions_total,
        circuit_breaker_requests_rejected_total,
        config_reload_errors_total,
        worker_errors_total,
        cache_hits_total,
        cache_misses_total,
        rate_limiter_tokens_remaining,
        config_reload_duration_seconds,
        session_affinity_hits_total,
        session_affinity_misses_total,
    });

    Ok(())
}

/// Return a reference to the initialised [`Metrics`], or `None` if
/// [`init_metrics`] has not been called yet.
fn metrics() -> Option<&'static Metrics> {
    METRICS.get()
}

//  Public helper functions

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

/// Increment the dropped-request counter for a pipeline stage.
///
/// This counter tracks requests that were discarded because the downstream
/// channel was full (backpressure shed).  It is incremented in addition to
/// `inc_shed` so callers can query drops independently.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_dropped(stage: &str) {
    if let Some(m) = metrics() {
        if let Ok(c) = m.requests_dropped.get_metric_with_label_values(&[stage]) {
            c.inc();
        }
    }
}

/// Record the time-to-first-token (TTFT) for a streaming inference call.
///
/// `worker` is a short identifier such as `"openai"` or `"anthropic"`.
/// `model` is the model name string reported by the worker.
/// `d` is the elapsed duration from request start until the first token arrived.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn record_ttft(worker: &str, model: &str, d: Duration) {
    if let Some(m) = metrics() {
        if let Ok(h) = m
            .ttft_seconds
            .get_metric_with_label_values(&[worker, model])
        {
            h.observe(d.as_secs_f64());
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

/// Record USD cost for an inference call.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn record_inference_cost(usd: f64) {
    if let Some(m) = metrics() {
        m.inference_cost_usd.inc_by(usd);
    }
}

/// Return the total USD spent on inference calls this session.
///
/// Returns `0.0` if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn total_inference_cost_usd() -> f64 {
    metrics().map_or(0.0, |m| m.inference_cost_usd.get())
}

/// Increment the dedup hit counter.
///
/// Call when a duplicate request is detected (InProgress or Cached result returned).
///
/// No-op if metrics have not been initialised.
pub fn inc_dedup_hit() {
    if let Some(m) = metrics() {
        m.dedup_hits_total.inc();
    }
}

/// Increment the dedup waiters-unblocked counter.
///
/// Call when `wait_for_result` successfully receives a broadcast result.
///
/// No-op if metrics have not been initialised.
pub fn inc_dedup_waiter_unblocked() {
    if let Some(m) = metrics() {
        m.dedup_waiters_unblocked_total.inc();
    }
}

/// Increment the inference timeout counter.
///
/// Call when an inference call is cancelled because it exceeded the configured timeout.
///
/// No-op if metrics have not been initialised.
pub fn inc_inference_timeout() {
    if let Some(m) = metrics() {
        m.inference_timeouts_total.inc();
    }
}

/// Increment the deadline-expired counter.
///
/// Called by the inference stage when a request is dropped because
/// `Instant::now()` is past its `deadline`.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_expired() {
    if let Some(m) = metrics() {
        m.requests_expired_total.inc();
    }
}

/// Increment the DLQ lock-poisoned counter.
///
/// Call when a `DeadLetterQueue` mutex is recovered from a poisoned state.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_dlq_lock_poisoned() {
    if let Some(m) = metrics() {
        m.dlq_lock_poisoned_total.inc();
    }
}

/// Increment the RAG-stage deadline-expired counter.
///
/// Call when a request is dropped at the RAG stage because its deadline
/// has already passed.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_rag_expired() {
    if let Some(m) = metrics() {
        m.rag_requests_expired_total.inc();
    }
}

/// Increment the circuit-breaker state-transition counter for `state`.
///
/// `state` should be `"open"`, `"half_open"`, or `"closed"`.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_cb_transition(state: &str) {
    if let Some(m) = metrics() {
        if let Ok(c) = m
            .circuit_breaker_state_transitions_total
            .get_metric_with_label_values(&[state])
        {
            c.inc();
        }
    }
}

/// Increment the circuit-breaker rejected-requests counter.
///
/// Call when a request is rejected because the circuit breaker is open.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_cb_rejected() {
    if let Some(m) = metrics() {
        m.circuit_breaker_requests_rejected_total.inc();
    }
}

/// Increment the config reload error counter.
///
/// Call when a config hot-reload attempt fails validation or parse.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_config_reload_error() {
    if let Some(m) = metrics() {
        m.config_reload_errors_total.inc();
    }
}

/// Increment the per-worker error counter.
///
/// Call when a named worker (e.g. `"openai"`, `"anthropic"`) returns an error.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_worker_error(worker: &str) {
    if let Some(m) = metrics() {
        if let Ok(c) = m.worker_errors_total.get_metric_with_label_values(&[worker]) {
            c.inc();
        }
    }
}

/// Increment the cache hit counter.
///
/// Call when the response cache returns a cached value.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_cache_hit() {
    if let Some(m) = metrics() {
        m.cache_hits_total.inc();
    }
}

/// Increment the cache miss counter.
///
/// Call when the response cache returns `None` (no entry or expired).
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_cache_miss() {
    if let Some(m) = metrics() {
        m.cache_misses_total.inc();
    }
}

/// Set the rate-limiter tokens-remaining gauge for a session.
///
/// Call after each rate-limit check to update the gauge with the current
/// number of tokens remaining in that session's bucket.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn set_rate_limiter_tokens(session: &str, tokens: f64) {
    if let Some(m) = metrics() {
        if let Ok(g) = m
            .rate_limiter_tokens_remaining
            .get_metric_with_label_values(&[session])
        {
            g.set(tokens);
        }
    }
}

/// Record the duration of a config hot-reload operation.
///
/// `result` should be `"ok"` on success or `"error"` on failure.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn record_config_reload_duration(result: &str, d: Duration) {
    if let Some(m) = metrics() {
        if let Ok(h) = m
            .config_reload_duration_seconds
            .get_metric_with_label_values(&[result])
        {
            h.observe(d.as_secs_f64());
        }
    }
}

/// Increment the session affinity hit counter.
///
/// Call when a request is successfully routed to its preferred affinity shard.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_session_affinity_hit() {
    if let Some(m) = metrics() {
        m.session_affinity_hits_total.inc();
    }
}

/// Increment the session affinity miss counter.
///
/// Call when a request's preferred affinity shard is unavailable and it must
/// be rerouted to a different shard.
///
/// No-op if metrics have not been initialised.
///
/// # Panics
///
/// This function never panics.
pub fn inc_session_affinity_miss() {
    if let Some(m) = metrics() {
        m.session_affinity_misses_total.inc();
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

        let requests_dropped = CounterVec::new(
            Opts::new("t_requests_dropped_total", "test counter"),
            &["stage"],
        )
        .expect("CounterVec construction must succeed in tests");
        registry
            .register(Box::new(requests_dropped.clone()))
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

        let inference_cost_usd =
            Counter::with_opts(Opts::new("orchestrator_inference_cost_usd_total", "test"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(inference_cost_usd.clone()))
            .expect("register must succeed in tests");

        let ttft_seconds = HistogramVec::new(
            HistogramOpts::new("t_ttft_seconds", "test ttft histogram"),
            &["worker", "model"],
        )
        .expect("HistogramVec construction must succeed in tests");
        registry
            .register(Box::new(ttft_seconds.clone()))
            .expect("register must succeed in tests");

        let dedup_hits_total = Counter::with_opts(Opts::new("t_dedup_hits_total", "test counter"))
            .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(dedup_hits_total.clone()))
            .expect("register must succeed in tests");

        let dedup_waiters_unblocked_total =
            Counter::with_opts(Opts::new("t_dedup_waiters_unblocked_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(dedup_waiters_unblocked_total.clone()))
            .expect("register must succeed in tests");

        let inference_timeouts_total =
            Counter::with_opts(Opts::new("t_inference_timeouts_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(inference_timeouts_total.clone()))
            .expect("register must succeed in tests");

        let requests_expired_total =
            Counter::with_opts(Opts::new("t_requests_expired_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(requests_expired_total.clone()))
            .expect("register must succeed in tests");

        let dlq_lock_poisoned_total =
            Counter::with_opts(Opts::new("t_dlq_lock_poisoned_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(dlq_lock_poisoned_total.clone()))
            .expect("register must succeed in tests");

        let rag_requests_expired_total =
            Counter::with_opts(Opts::new("t_rag_requests_expired_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(rag_requests_expired_total.clone()))
            .expect("register must succeed in tests");

        let circuit_breaker_state_transitions_total = CounterVec::new(
            Opts::new("t_circuit_breaker_state_transitions_total", "test counter"),
            &["state"],
        )
        .expect("CounterVec construction must succeed in tests");
        registry
            .register(Box::new(circuit_breaker_state_transitions_total.clone()))
            .expect("register must succeed in tests");

        let circuit_breaker_requests_rejected_total = Counter::with_opts(Opts::new(
            "t_circuit_breaker_requests_rejected_total",
            "test counter",
        ))
        .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(circuit_breaker_requests_rejected_total.clone()))
            .expect("register must succeed in tests");

        let config_reload_errors_total =
            Counter::with_opts(Opts::new("t_config_reload_errors_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(config_reload_errors_total.clone()))
            .expect("register must succeed in tests");

        let worker_errors_total = CounterVec::new(
            Opts::new("t_worker_errors_total", "test counter"),
            &["worker"],
        )
        .expect("CounterVec construction must succeed in tests");
        registry
            .register(Box::new(worker_errors_total.clone()))
            .expect("register must succeed in tests");

        let cache_hits_total =
            Counter::with_opts(Opts::new("t_cache_hits_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(cache_hits_total.clone()))
            .expect("register must succeed in tests");

        let cache_misses_total =
            Counter::with_opts(Opts::new("t_cache_misses_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(cache_misses_total.clone()))
            .expect("register must succeed in tests");

        let rate_limiter_tokens_remaining = GaugeVec::new(
            Opts::new("t_rate_limiter_tokens_remaining", "test gauge"),
            &["session"],
        )
        .expect("GaugeVec construction must succeed in tests");
        registry
            .register(Box::new(rate_limiter_tokens_remaining.clone()))
            .expect("register must succeed in tests");

        let config_reload_duration_seconds = HistogramVec::new(
            HistogramOpts::new("t_config_reload_duration_seconds", "test histogram"),
            &["result"],
        )
        .expect("HistogramVec construction must succeed in tests");
        registry
            .register(Box::new(config_reload_duration_seconds.clone()))
            .expect("register must succeed in tests");

        let session_affinity_hits_total =
            Counter::with_opts(Opts::new("t_session_affinity_hits_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(session_affinity_hits_total.clone()))
            .expect("register must succeed in tests");

        let session_affinity_misses_total =
            Counter::with_opts(Opts::new("t_session_affinity_misses_total", "test counter"))
                .expect("Counter construction must succeed in tests");
        registry
            .register(Box::new(session_affinity_misses_total.clone()))
            .expect("register must succeed in tests");

        Metrics {
            registry,
            requests_total,
            requests_shed,
            requests_dropped,
            errors_total,
            stage_duration,
            queue_depth,
            inference_cost_usd,
            ttft_seconds,
            dedup_hits_total,
            dedup_waiters_unblocked_total,
            inference_timeouts_total,
            requests_expired_total,
            dlq_lock_poisoned_total,
            rag_requests_expired_total,
            circuit_breaker_state_transitions_total,
            circuit_breaker_requests_rejected_total,
            config_reload_errors_total,
            worker_errors_total,
            cache_hits_total,
            cache_misses_total,
            rate_limiter_tokens_remaining,
            config_reload_duration_seconds,
            session_affinity_hits_total,
            session_affinity_misses_total,
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

    #[test]
    fn test_inc_dropped_increments_dropped_counter() {
        let m = make_test_metrics();
        m.requests_dropped
            .get_metric_with_label_values(&["rag"])
            .expect("label ok")
            .inc();
        m.requests_dropped
            .get_metric_with_label_values(&["rag"])
            .expect("label ok")
            .inc();

        let families = m.registry.gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "t_requests_dropped_total")
            .expect("dropped counter family must be present");
        let value = family.get_metric()[0].get_counter().get_value();
        assert!(
            (value - 2.0).abs() < f64::EPSILON,
            "counter must be 2.0, got {value}"
        );
    }

    #[test]
    fn test_record_ttft_records_observation_with_worker_model_labels() {
        let m = make_test_metrics();
        m.ttft_seconds
            .get_metric_with_label_values(&["openai", "gpt-4o"])
            .expect("label values must be valid")
            .observe(0.25);

        let families = m.registry.gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "t_ttft_seconds")
            .expect("ttft histogram family must be present");
        let count = family.get_metric()[0].get_histogram().get_sample_count();
        assert_eq!(count, 1, "one TTFT observation should have been recorded");
    }

    #[test]
    fn test_inc_expired_increments_expired_counter() {
        let m = make_test_metrics();
        m.requests_expired_total.inc();
        m.requests_expired_total.inc();

        let families = m.registry.gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "t_requests_expired_total")
            .expect("expired counter family must be present");
        let value = family.get_metric()[0].get_counter().get_value();
        assert!(
            (value - 2.0).abs() < f64::EPSILON,
            "counter must be 2.0, got {value}"
        );
    }
}
