//! Provider health dashboard snapshot.
//!
//! [`ProviderHealthSnapshot`] aggregates per-provider health metrics into a
//! single value that can be serialised to JSON for a `/health/providers` REST
//! endpoint or logged on demand.
//!
//! ## What It Aggregates
//!
//! For each registered provider the snapshot captures:
//!
//! | Field | Source |
//! |-------|--------|
//! | `last_success_at` | Unix timestamp (seconds) of the most recent success |
//! | `last_failure_at` | Unix timestamp (seconds) of the most recent failure |
//! | `consecutive_failures` | Failures since the last success |
//! | `success_rate_1h` | Fraction of calls that succeeded in the last hour |
//! | `p50_latency_ms` | Median latency over all recorded calls |
//! | `p95_latency_ms` | 95th-percentile latency over all recorded calls |
//!
//! ## Usage
//!
//! ```rust
//! use tokio_prompt_orchestrator::routing::health::{ProviderHealthBuilder, ProviderHealthSnapshot};
//! use tokio_prompt_orchestrator::config::WorkerKind;
//!
//! let mut builder = ProviderHealthBuilder::new();
//! builder.record_success(WorkerKind::Anthropic, 120);
//! builder.record_failure(WorkerKind::OpenAi);
//! let snapshot = builder.snapshot();
//! let json = snapshot.to_json();
//! assert!(json.is_object());
//! ```

use crate::config::WorkerKind;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

// ---------------------------------------------------------------------------
// Internal per-provider state
// ---------------------------------------------------------------------------

/// Rolling latency ring buffer for percentile estimation.
///
/// Keeps the most recent `capacity` latency values (in milliseconds).
/// Percentile estimation uses a sorted snapshot.
struct LatencyRing {
    buf: Vec<u64>,
    capacity: usize,
    pos: usize,
    full: bool,
}

impl LatencyRing {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0; capacity],
            capacity,
            pos: 0,
            full: false,
        }
    }

    fn push(&mut self, ms: u64) {
        self.buf[self.pos] = ms;
        self.pos = (self.pos + 1) % self.capacity;
        if self.pos == 0 {
            self.full = true;
        }
    }

    /// Percentile p in 0–100.
    fn percentile(&self, p: u8) -> u64 {
        let len = if self.full { self.capacity } else { self.pos };
        if len == 0 {
            return 0;
        }
        let mut sorted: Vec<u64> = self.buf[..len].to_vec();
        sorted.sort_unstable();
        let idx = ((p as usize) * (len - 1)) / 100;
        sorted[idx]
    }
}

/// A rolling 1-hour success/failure window.
///
/// Timestamps are in seconds since UNIX epoch.  Entries older than 3600 s are
/// pruned on each update so the window stays bounded.
struct HourWindow {
    /// `true` = success, `false` = failure, along with timestamp (seconds).
    entries: Vec<(u64, bool)>,
}

impl HourWindow {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn push(&mut self, success: bool) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.prune(now);
        self.entries.push((now, success));
    }

    fn prune(&mut self, now: u64) {
        self.entries.retain(|(ts, _)| now.saturating_sub(*ts) < 3600);
    }

    fn success_rate(&self) -> f64 {
        if self.entries.is_empty() {
            return 1.0;
        }
        let successes = self.entries.iter().filter(|(_, ok)| *ok).count();
        successes as f64 / self.entries.len() as f64
    }
}

/// Mutable per-provider health state.
struct ProviderState {
    last_success_at: Option<u64>,
    last_failure_at: Option<u64>,
    consecutive_failures: u64,
    hour_window: HourWindow,
    latency_ring: LatencyRing,
}

impl ProviderState {
    fn new() -> Self {
        Self {
            last_success_at: None,
            last_failure_at: None,
            consecutive_failures: 0,
            hour_window: HourWindow::new(),
            latency_ring: LatencyRing::new(512),
        }
    }
}

// ---------------------------------------------------------------------------
// Public per-provider metric struct
// ---------------------------------------------------------------------------

/// Health metrics for a single provider at a point in time.
///
/// Obtain via [`ProviderHealthSnapshot::providers`] or
/// [`ProviderHealthSnapshot::to_json`].
#[derive(Debug, Clone)]
pub struct ProviderMetrics {
    /// UNIX timestamp (seconds) of the last successful call, or `None`.
    pub last_success_at: Option<u64>,
    /// UNIX timestamp (seconds) of the last failed call, or `None`.
    pub last_failure_at: Option<u64>,
    /// Number of consecutive failures since the last success.
    pub consecutive_failures: u64,
    /// Fraction of calls in the last hour that succeeded (0.0–1.0).
    pub success_rate_1h: f64,
    /// Median call latency in milliseconds over all recorded calls.
    pub p50_latency_ms: u64,
    /// 95th-percentile call latency in milliseconds over all recorded calls.
    pub p95_latency_ms: u64,
}

// ---------------------------------------------------------------------------
// ProviderHealthSnapshot
// ---------------------------------------------------------------------------

/// An immutable snapshot of health metrics for all tracked providers.
///
/// Obtain via [`ProviderHealthBuilder::snapshot`].
///
/// # Serialisation
///
/// Use [`to_json`](Self::to_json) to convert to a `serde_json::Value` suitable
/// for embedding in a REST API response.
#[derive(Debug, Clone)]
pub struct ProviderHealthSnapshot {
    /// Per-provider metrics, keyed by the `WorkerKind` debug string.
    pub providers: HashMap<String, ProviderMetrics>,
    /// UNIX timestamp (seconds) at which this snapshot was taken.
    pub snapshot_at: u64,
}

impl ProviderHealthSnapshot {
    /// Serialise the snapshot to a [`serde_json::Value`].
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn to_json(&self) -> Value {
        let mut providers = serde_json::Map::new();

        for (name, m) in &self.providers {
            providers.insert(
                name.clone(),
                json!({
                    "last_success_at":       m.last_success_at,
                    "last_failure_at":       m.last_failure_at,
                    "consecutive_failures":  m.consecutive_failures,
                    "success_rate_1h":       m.success_rate_1h,
                    "p50_latency_ms":        m.p50_latency_ms,
                    "p95_latency_ms":        m.p95_latency_ms,
                }),
            );
        }

        json!({
            "snapshot_at": self.snapshot_at,
            "providers":   Value::Object(providers),
        })
    }
}

// ---------------------------------------------------------------------------
// ProviderHealthBuilder
// ---------------------------------------------------------------------------

/// Mutable builder that accumulates per-provider health observations and can
/// produce a [`ProviderHealthSnapshot`] on demand.
///
/// `ProviderHealthBuilder` is `Clone + Send + Sync`.  All clones share the
/// same underlying `Arc<Mutex<…>>` state.
///
/// # Examples
///
/// ```rust
/// use tokio_prompt_orchestrator::config::WorkerKind;
/// use tokio_prompt_orchestrator::routing::health::ProviderHealthBuilder;
///
/// let builder = ProviderHealthBuilder::new();
/// builder.record_success(WorkerKind::Anthropic, 95);
/// builder.record_failure(WorkerKind::OpenAi);
/// let snap = builder.snapshot();
/// let anthropic = snap.providers.get("Anthropic").expect("anthropic present");
/// assert_eq!(anthropic.consecutive_failures, 0);
/// let openai = snap.providers.get("OpenAi").expect("openai present");
/// assert_eq!(openai.consecutive_failures, 1);
/// ```
#[derive(Clone)]
pub struct ProviderHealthBuilder {
    state: Arc<Mutex<HashMap<String, ProviderState>>>,
}

impl ProviderHealthBuilder {
    /// Create a new empty `ProviderHealthBuilder`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record a successful call to `provider` with latency `latency_ms`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn record_success(&self, provider: WorkerKind, latency_ms: u64) {
        let key = format!("{provider:?}");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut map = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(key.clone()).or_insert_with(ProviderState::new);

        entry.last_success_at = Some(now);
        entry.consecutive_failures = 0;
        entry.hour_window.push(true);
        entry.latency_ring.push(latency_ms);

        debug!(provider = %key, latency_ms = latency_ms, "health: recorded success");
    }

    /// Record a failed call to `provider`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn record_failure(&self, provider: WorkerKind) {
        let key = format!("{provider:?}");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut map = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(key.clone()).or_insert_with(ProviderState::new);

        entry.last_failure_at = Some(now);
        entry.consecutive_failures += 1;
        entry.hour_window.push(false);

        debug!(provider = %key, "health: recorded failure");
    }

    /// Take an immutable snapshot of the current health state.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn snapshot(&self) -> ProviderHealthSnapshot {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let map = self.state.lock().unwrap_or_else(|p| p.into_inner());

        let providers = map
            .iter()
            .map(|(k, s)| {
                let metrics = ProviderMetrics {
                    last_success_at: s.last_success_at,
                    last_failure_at: s.last_failure_at,
                    consecutive_failures: s.consecutive_failures,
                    success_rate_1h: s.hour_window.success_rate(),
                    p50_latency_ms: s.latency_ring.percentile(50),
                    p95_latency_ms: s.latency_ring.percentile(95),
                };
                (k.clone(), metrics)
            })
            .collect();

        ProviderHealthSnapshot {
            providers,
            snapshot_at: now,
        }
    }
}

impl Default for ProviderHealthBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_success_clears_consecutive_failures() {
        let b = ProviderHealthBuilder::new();
        b.record_failure(WorkerKind::Anthropic);
        b.record_failure(WorkerKind::Anthropic);
        b.record_success(WorkerKind::Anthropic, 50);

        let snap = b.snapshot();
        let m = snap.providers.get("Anthropic").expect("present");
        assert_eq!(m.consecutive_failures, 0);
        assert!(m.last_success_at.is_some());
    }

    #[test]
    fn test_consecutive_failures_increment() {
        let b = ProviderHealthBuilder::new();
        b.record_failure(WorkerKind::OpenAi);
        b.record_failure(WorkerKind::OpenAi);
        b.record_failure(WorkerKind::OpenAi);

        let snap = b.snapshot();
        let m = snap.providers.get("OpenAi").expect("present");
        assert_eq!(m.consecutive_failures, 3);
        assert!(m.last_failure_at.is_some());
    }

    #[test]
    fn test_to_json_contains_all_fields() {
        let b = ProviderHealthBuilder::new();
        b.record_success(WorkerKind::LlamaCpp, 100);

        let snap = b.snapshot();
        let json = snap.to_json();

        assert!(json.get("snapshot_at").is_some());
        let providers = json.get("providers").expect("providers key");
        assert!(providers.get("LlamaCpp").is_some());

        let p = &providers["LlamaCpp"];
        assert!(p.get("p50_latency_ms").is_some());
        assert!(p.get("p95_latency_ms").is_some());
        assert!(p.get("success_rate_1h").is_some());
        assert!(p.get("consecutive_failures").is_some());
    }

    #[test]
    fn test_success_rate_mixed() {
        let b = ProviderHealthBuilder::new();
        for _ in 0..3 {
            b.record_success(WorkerKind::Vllm, 50);
        }
        for _ in 0..1 {
            b.record_failure(WorkerKind::Vllm);
        }

        let snap = b.snapshot();
        let m = snap.providers.get("Vllm").expect("present");
        // 3 successes, 1 failure → 75 %
        assert!((m.success_rate_1h - 0.75).abs() < 1e-9);
    }

    #[test]
    fn test_empty_builder_snapshot() {
        let b = ProviderHealthBuilder::new();
        let snap = b.snapshot();
        assert!(snap.providers.is_empty());
        let json = snap.to_json();
        assert!(json["providers"].as_object().unwrap().is_empty());
    }

    #[test]
    fn test_latency_percentiles() {
        let b = ProviderHealthBuilder::new();
        // Insert 10 known values.
        for i in 1u64..=10 {
            b.record_success(WorkerKind::Echo, i * 10); // 10, 20, ..., 100
        }

        let snap = b.snapshot();
        let m = snap.providers.get("Echo").expect("present");
        // p50 should be around 50 ms, p95 around 100 ms.
        assert!(m.p50_latency_ms >= 40 && m.p50_latency_ms <= 60);
        assert!(m.p95_latency_ms >= 90);
    }

    #[test]
    fn test_clone_shares_state() {
        let b = ProviderHealthBuilder::new();
        let b2 = b.clone();
        b.record_success(WorkerKind::Echo, 20);

        let snap = b2.snapshot();
        assert!(snap.providers.contains_key("Echo"));
    }

    #[test]
    fn test_default_trait() {
        let b = ProviderHealthBuilder::default();
        assert!(b.snapshot().providers.is_empty());
    }
}
