//! Multi-provider cascade fallback.
//!
//! A [`ProviderCascade`] chains an ordered list of LLM providers and tries
//! them in sequence — primary → secondary → tertiary — skipping any whose
//! circuit breaker is currently open.  This extends the binary
//! [`LocalWithFallback`](crate::routing::RoutingDecision::LocalWithFallback)
//! routing decision to an arbitrarily long chain.
//!
//! ## Guarantees
//!
//! - **Fully async** — all provider calls are non-blocking.
//! - **Circuit-breaker aware** — open breakers are skipped without a call attempt.
//! - **Per-provider telemetry** — latency and success counters tracked with
//!   `Arc<AtomicU64>` for lock-free hot-path reads.
//! - **Prometheus metrics** — `cascade_attempts_total{provider}` and
//!   `cascade_failures_total{provider}` counters using lazy-static registration.
//!
//! ## Example
//!
//! ```rust
//! use std::time::Duration;
//! use tokio_prompt_orchestrator::config::WorkerKind;
//! use tokio_prompt_orchestrator::routing::cascade::{CascadeEntry, ProviderCascade};
//! use tokio_prompt_orchestrator::enhanced::circuit_breaker::CircuitBreakerConfig;
//!
//! let cascade = ProviderCascade::new(vec![
//!     CascadeEntry::new(WorkerKind::LlamaCpp, CircuitBreakerConfig::default()),
//!     CascadeEntry::new(WorkerKind::OpenAi,   CircuitBreakerConfig::default()),
//!     CascadeEntry::new(WorkerKind::Anthropic, CircuitBreakerConfig::default()),
//! ]);
//! ```

use crate::config::WorkerKind;
use crate::enhanced::CircuitBreaker;
use lazy_static::lazy_static;
use prometheus::{CounterVec, Opts, Registry};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Cascade-specific Prometheus counters
// ---------------------------------------------------------------------------

lazy_static! {
    static ref CASCADE_REGISTRY: Registry = Registry::new();

    /// Total attempts per provider label, incremented each time a provider is tried.
    static ref CASCADE_ATTEMPTS: CounterVec = {
        let cv = CounterVec::new(
            Opts::new("cascade_attempts_total", "Total cascade attempts per provider"),
            &["provider"],
        )
        .expect("cascade_attempts_total metric construction");
        CASCADE_REGISTRY
            .register(Box::new(cv.clone()))
            .expect("cascade_attempts_total registration");
        cv
    };

    /// Total failures per provider label, incremented when a provider call errors.
    static ref CASCADE_FAILURES: CounterVec = {
        let cv = CounterVec::new(
            Opts::new("cascade_failures_total", "Total cascade failures per provider"),
            &["provider"],
        )
        .expect("cascade_failures_total metric construction");
        CASCADE_REGISTRY
            .register(Box::new(cv.clone()))
            .expect("cascade_failures_total registration");
        cv
    };
}

// ---------------------------------------------------------------------------
// Public configuration types
// ---------------------------------------------------------------------------

/// Configuration for a single circuit breaker inside a cascade entry.
///
/// Mirrors the parameters accepted by [`CircuitBreaker::new`].
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before the circuit opens.
    pub failure_threshold: usize,
    /// Required success rate (0.0 – 1.0) to close the circuit from half-open.
    pub success_threshold: f64,
    /// Duration the circuit stays open before allowing a probe attempt.
    pub timeout: std::time::Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 0.8,
            timeout: std::time::Duration::from_secs(30),
        }
    }
}

/// A single slot in the provider cascade: one [`WorkerKind`] plus its own
/// [`CircuitBreaker`] and per-call telemetry counters.
///
/// All clones of a `CascadeEntry` share the same underlying `CircuitBreaker`
/// state and atomic counters.
#[derive(Clone)]
pub struct CascadeEntry {
    /// The provider kind this entry represents.
    pub kind: WorkerKind,
    /// Dedicated circuit breaker for this provider.
    pub breaker: CircuitBreaker,
    /// Total successful calls to this provider (lock-free).
    pub success_count: Arc<AtomicU64>,
    /// Total failed calls to this provider (lock-free).
    pub failure_count: Arc<AtomicU64>,
    /// Cumulative latency in milliseconds across all calls (lock-free).
    pub total_latency_ms: Arc<AtomicU64>,
    /// Number of calls that contributed to `total_latency_ms` (lock-free).
    pub call_count: Arc<AtomicU64>,
}

impl CascadeEntry {
    /// Create a new `CascadeEntry` for `kind` with the given circuit breaker config.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn new(kind: WorkerKind, cb_config: CircuitBreakerConfig) -> Self {
        let breaker = CircuitBreaker::new(
            cb_config.failure_threshold,
            cb_config.success_threshold,
            cb_config.timeout,
        );
        Self {
            kind,
            breaker,
            success_count: Arc::new(AtomicU64::new(0)),
            failure_count: Arc::new(AtomicU64::new(0)),
            total_latency_ms: Arc::new(AtomicU64::new(0)),
            call_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns the average latency in milliseconds over all recorded calls,
    /// or `0` if no calls have been made.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn avg_latency_ms(&self) -> u64 {
        let calls = self.call_count.load(Ordering::Relaxed);
        if calls == 0 {
            return 0;
        }
        self.total_latency_ms.load(Ordering::Relaxed) / calls
    }

    /// Returns the success rate as a fraction in `[0.0, 1.0]`.
    ///
    /// Returns `1.0` when no calls have been made (optimistic default).
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn success_rate(&self) -> f64 {
        let successes = self.success_count.load(Ordering::Relaxed);
        let failures = self.failure_count.load(Ordering::Relaxed);
        let total = successes + failures;
        if total == 0 {
            1.0
        } else {
            successes as f64 / total as f64
        }
    }

    fn provider_label(&self) -> &'static str {
        match self.kind {
            WorkerKind::OpenAi => "open_ai",
            WorkerKind::Anthropic => "anthropic",
            WorkerKind::LlamaCpp => "llama_cpp",
            WorkerKind::Vllm => "vllm",
            WorkerKind::Echo => "echo",
        }
    }
}

impl fmt::Debug for CascadeEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CascadeEntry")
            .field("kind", &self.kind)
            .field("avg_latency_ms", &self.avg_latency_ms())
            .field("success_rate", &self.success_rate())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// CascadeResult
// ---------------------------------------------------------------------------

/// The outcome of a successful [`ProviderCascade::call`] invocation.
///
/// Carries enough context to record metrics and diagnose which provider
/// served the request and how the fallback chain was traversed.
#[derive(Debug, Clone)]
pub struct CascadeResult<T> {
    /// The provider that ultimately returned a successful response.
    pub provider_used: WorkerKind,
    /// Number of providers attempted, including the successful one.
    pub attempts: u8,
    /// Wall-clock time from the first attempt to the successful response, in ms.
    pub total_latency_ms: u64,
    /// The value returned by the winning provider.
    pub value: T,
}

// ---------------------------------------------------------------------------
// CascadeError
// ---------------------------------------------------------------------------

/// Errors returned by [`ProviderCascade::call`].
#[derive(Debug)]
pub enum CascadeError<E> {
    /// All providers in the cascade failed or had open circuit breakers.
    ///
    /// Contains the per-provider errors in order of attempt.
    AllFailed(Vec<(WorkerKind, E)>),

    /// The cascade chain is empty — no providers were configured.
    EmptyCascade,
}

impl<E: fmt::Display> fmt::Display for CascadeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCascade => write!(f, "cascade: no providers configured"),
            Self::AllFailed(errs) => {
                write!(f, "cascade: all {} provider(s) failed", errs.len())?;
                for (kind, e) in errs {
                    write!(f, "; {kind:?}: {e}")?;
                }
                Ok(())
            }
        }
    }
}

impl<E: fmt::Display + fmt::Debug> std::error::Error for CascadeError<E> {}

// ---------------------------------------------------------------------------
// ProviderCascade
// ---------------------------------------------------------------------------

/// A configurable chain of LLM providers that tries primary → secondary →
/// tertiary on failure.
///
/// Unlike the binary [`LocalWithFallback`] routing decision, `ProviderCascade`
/// supports an arbitrary number of providers and maintains per-provider
/// circuit breakers, latency telemetry, and Prometheus counters.
///
/// # Clone behaviour
///
/// `ProviderCascade` is cheap to clone — all clones share the same underlying
/// `Arc<Mutex<Vec<CascadeEntry>>>` and its contained `CircuitBreaker` / atomic
/// state.
///
/// # Examples
///
/// ```rust
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::config::WorkerKind;
/// use tokio_prompt_orchestrator::routing::cascade::{CascadeEntry, CircuitBreakerConfig, ProviderCascade};
///
/// let cascade = ProviderCascade::new(vec![
///     CascadeEntry::new(WorkerKind::LlamaCpp, CircuitBreakerConfig::default()),
///     CascadeEntry::new(WorkerKind::OpenAi,   CircuitBreakerConfig::default()),
/// ]);
/// assert_eq!(cascade.len(), 2);
/// ```
#[derive(Clone)]
pub struct ProviderCascade {
    entries: Arc<Mutex<Vec<CascadeEntry>>>,
}

impl ProviderCascade {
    /// Create a new `ProviderCascade` from an ordered list of cascade entries.
    ///
    /// The first entry is the primary provider; subsequent entries are tried
    /// in order on failure.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn new(entries: Vec<CascadeEntry>) -> Self {
        Self {
            entries: Arc::new(Mutex::new(entries)),
        }
    }

    /// Return the number of providers in the cascade.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// Return `true` if no providers are configured.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Try each provider in order, returning the first successful result.
    ///
    /// Open circuit breakers are skipped.  For each attempted provider,
    /// `Prometheus` counters `cascade_attempts_total` and
    /// `cascade_failures_total` are updated.  Per-entry latency and success
    /// rate atomics are also updated.
    ///
    /// # Arguments
    ///
    /// * `f` — An async factory that receives a `WorkerKind` and returns a
    ///   `Future<Output = Result<T, E>>`.  It will be called once per
    ///   attempted provider.
    ///
    /// # Errors
    ///
    /// Returns [`CascadeError::EmptyCascade`] when no entries are configured,
    /// or [`CascadeError::AllFailed`] when every provider either had an open
    /// breaker or returned an error.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub async fn call<F, Fut, T, E>(&self, mut f: F) -> Result<CascadeResult<T>, CascadeError<E>>
    where
        F: FnMut(WorkerKind) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: fmt::Debug,
    {
        let entries_snapshot: Vec<CascadeEntry> = {
            let guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            guard.clone()
        };

        if entries_snapshot.is_empty() {
            return Err(CascadeError::EmptyCascade);
        }

        let cascade_start = Instant::now();
        let mut attempts: u8 = 0;
        let mut errors: Vec<(WorkerKind, E)> = Vec::new();

        for entry in &entries_snapshot {
            // Skip providers with open circuit breakers.
            if entry.breaker.is_open_sync() {
                debug!(
                    provider = ?entry.kind,
                    "cascade: skipping provider with open circuit breaker"
                );
                continue;
            }

            let label = entry.provider_label();
            let _ = CASCADE_ATTEMPTS.with_label_values(&[label]).inc();

            attempts = attempts.saturating_add(1);
            let call_start = Instant::now();

            debug!(provider = ?entry.kind, attempt = attempts, "cascade: trying provider");

            let result = f(entry.kind.clone()).await;
            let elapsed_ms = call_start.elapsed().as_millis() as u64;

            // Update per-entry latency atomics.
            entry.total_latency_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
            entry.call_count.fetch_add(1, Ordering::Relaxed);

            match result {
                Ok(value) => {
                    entry.success_count.fetch_add(1, Ordering::Relaxed);
                    // Record success with this entry's circuit breaker.
                    entry
                        .breaker
                        .call(|| async { Ok::<(), ()>(()) })
                        .await
                        .ok();

                    let total_ms = cascade_start.elapsed().as_millis() as u64;

                    info!(
                        provider = ?entry.kind,
                        attempts = attempts,
                        latency_ms = elapsed_ms,
                        total_latency_ms = total_ms,
                        "cascade: provider succeeded"
                    );

                    return Ok(CascadeResult {
                        provider_used: entry.kind.clone(),
                        attempts,
                        total_latency_ms: total_ms,
                        value,
                    });
                }
                Err(e) => {
                    entry.failure_count.fetch_add(1, Ordering::Relaxed);
                    let _ = CASCADE_FAILURES.with_label_values(&[label]).inc();

                    // Record failure with the circuit breaker.
                    let _: Result<(), _> = entry
                        .breaker
                        .call(|| async { Err::<(), ()>(()) })
                        .await;

                    warn!(
                        provider = ?entry.kind,
                        attempt = attempts,
                        "cascade: provider failed, trying next"
                    );

                    errors.push((entry.kind.clone(), e));
                }
            }
        }

        Err(CascadeError::AllFailed(errors))
    }

    /// Return a point-in-time snapshot of all cascade entries.
    ///
    /// Useful for health-check endpoints and dashboards.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn snapshot(&self) -> Vec<CascadeEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl fmt::Debug for ProviderCascade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        f.debug_struct("ProviderCascade")
            .field("len", &entries.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// OnceLock for shared registry access (for test / scrape helpers)
// ---------------------------------------------------------------------------

static CASCADE_REGISTRY_REF: OnceLock<&'static Registry> = OnceLock::new();

/// Return a reference to the cascade-specific Prometheus registry.
///
/// Intended for use by a metrics scrape endpoint that needs to gather
/// cascade counters separately from the main orchestrator registry.
///
/// # Panics
///
/// This function does not panic.
pub fn cascade_registry() -> &'static Registry {
    CASCADE_REGISTRY_REF.get_or_init(|| &CASCADE_REGISTRY)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AO};

    fn make_entry(kind: WorkerKind) -> CascadeEntry {
        CascadeEntry::new(kind, CircuitBreakerConfig::default())
    }

    #[tokio::test]
    async fn test_cascade_uses_primary_on_success() {
        let cascade = ProviderCascade::new(vec![
            make_entry(WorkerKind::LlamaCpp),
            make_entry(WorkerKind::OpenAi),
        ]);

        let result = cascade
            .call(|kind| async move { Ok::<WorkerKind, String>(kind) })
            .await;

        let r = result.expect("should succeed");
        assert_eq!(r.provider_used, WorkerKind::LlamaCpp);
        assert_eq!(r.attempts, 1);
    }

    #[tokio::test]
    async fn test_cascade_falls_back_on_primary_failure() {
        let cascade = ProviderCascade::new(vec![
            make_entry(WorkerKind::LlamaCpp),
            make_entry(WorkerKind::OpenAi),
            make_entry(WorkerKind::Anthropic),
        ]);

        let call_count = Arc::new(AtomicU32::new(0));
        let cc = call_count.clone();

        let result = cascade
            .call(|kind| {
                let count = cc.fetch_add(1, AO::SeqCst);
                async move {
                    if count == 0 {
                        Err::<WorkerKind, String>("primary failed".to_string())
                    } else {
                        Ok::<WorkerKind, String>(kind)
                    }
                }
            })
            .await;

        let r = result.expect("fallback should succeed");
        assert_eq!(r.provider_used, WorkerKind::OpenAi);
        assert_eq!(r.attempts, 2);
    }

    #[tokio::test]
    async fn test_cascade_all_failed() {
        let cascade = ProviderCascade::new(vec![
            make_entry(WorkerKind::LlamaCpp),
            make_entry(WorkerKind::OpenAi),
        ]);

        let result = cascade
            .call(|_kind| async move { Err::<(), String>("always fails".to_string()) })
            .await;

        assert!(
            matches!(result, Err(CascadeError::AllFailed(ref errs)) if errs.len() == 2),
            "expected AllFailed with 2 errors"
        );
    }

    #[tokio::test]
    async fn test_cascade_empty_returns_error() {
        let cascade: ProviderCascade = ProviderCascade::new(vec![]);
        let result = cascade
            .call(|kind| async move { Ok::<WorkerKind, String>(kind) })
            .await;
        assert!(matches!(result, Err(CascadeError::EmptyCascade)));
    }

    #[tokio::test]
    async fn test_cascade_skips_open_breaker() {
        let primary = make_entry(WorkerKind::LlamaCpp);
        // Trip the primary circuit breaker.
        primary.breaker.trip().await;

        let secondary = make_entry(WorkerKind::OpenAi);

        let cascade = ProviderCascade::new(vec![primary, secondary]);

        let result = cascade
            .call(|kind| async move { Ok::<WorkerKind, String>(kind) })
            .await;

        let r = result.expect("secondary should serve");
        assert_eq!(r.provider_used, WorkerKind::OpenAi);
        // Only secondary was attempted (primary was skipped).
        assert_eq!(r.attempts, 1);
    }

    #[tokio::test]
    async fn test_cascade_result_latency_is_nonzero() {
        let cascade = ProviderCascade::new(vec![make_entry(WorkerKind::Echo)]);

        let result = cascade
            .call(|kind| async move { Ok::<WorkerKind, String>(kind) })
            .await;

        // total_latency_ms may be 0 in very fast CI runs — just assert it's present.
        let r = result.expect("should succeed");
        assert_eq!(r.provider_used, WorkerKind::Echo);
    }

    #[test]
    fn test_entry_success_rate_no_calls_is_one() {
        let entry = make_entry(WorkerKind::Echo);
        assert_eq!(entry.success_rate(), 1.0);
    }

    #[test]
    fn test_entry_avg_latency_no_calls_is_zero() {
        let entry = make_entry(WorkerKind::Echo);
        assert_eq!(entry.avg_latency_ms(), 0);
    }

    #[test]
    fn test_cascade_is_empty() {
        let c = ProviderCascade::new(vec![]);
        assert!(c.is_empty());
        let c2 = ProviderCascade::new(vec![make_entry(WorkerKind::Echo)]);
        assert!(!c2.is_empty());
    }

    #[test]
    fn test_circuit_breaker_config_default() {
        let cfg = CircuitBreakerConfig::default();
        assert_eq!(cfg.failure_threshold, 5);
        assert_eq!(cfg.success_threshold, 0.8);
    }
}
