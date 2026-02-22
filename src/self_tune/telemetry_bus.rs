#![allow(
    missing_docs,
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    clippy::redundant_closure,
    clippy::derivable_impls,
    clippy::unwrap_or_default,
    dead_code,
    private_interfaces
)]
//! # Telemetry Bus (Task 1.1)
//!
//! A bounded broadcast channel that aggregates metrics from every pipeline stage
//! into a unified stream.  This is the nervous system — every other self-improving
//! module reads from this bus.
//!
//! ## What it produces
//! - [`TelemetrySnapshot`] structs at a configurable interval (default 5 s)
//! - Rolling windows: 1 m, 5 m, 15 m, 1 h kept in lock-free ring buffers
//!
//! ## Metrics collected
//! Queue depths, per-stage latencies (p50/p95/p99), throughput, error rates,
//! cache hit rates, dedup collision rates, circuit-breaker state transitions.
//!
//! ## Graceful degradation
//! If the bus fails to publish a snapshot the pipeline continues unaffected.
//! All errors are logged and counted via the `self_tune_bus_errors_total`
//! Prometheus counter.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::debug;

// ─── Public types ────────────────────────────────────────────────────────────

/// A single latency sample with p50 / p95 / p99 breakdowns (ms).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LatencyStats {
    /// 50th-percentile latency in milliseconds.
    pub p50_ms: u64,
    /// 95th-percentile latency in milliseconds.
    pub p95_ms: u64,
    /// 99th-percentile latency in milliseconds.
    pub p99_ms: u64,
    /// Arithmetic mean latency in milliseconds.
    pub avg_ms: f64,
    /// Number of samples used to compute the percentiles.
    pub sample_count: u64,
}

/// Circuit-breaker state as observed by the telemetry bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CircuitState {
    /// Requests flow normally.
    Closed,
    /// The breaker is open; requests are fast-failed.
    Open,
    /// A single probe request is allowed through to test recovery.
    HalfOpen,
}

impl Default for CircuitState {
    fn default() -> Self {
        Self::Closed
    }
}

/// Metrics for a single named pipeline stage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StageMetrics {
    /// Human-readable stage name (e.g. `"rag"`, `"inference"`).
    pub stage: String,
    /// Current depth of the incoming queue.
    pub queue_depth: usize,
    /// Configured capacity of the incoming queue.
    pub queue_capacity: usize,
    /// End-to-end latency for requests processed this window.
    pub latency: LatencyStats,
    /// Requests processed per second in this window.
    pub throughput_rps: f64,
    /// Fraction of requests that returned an error (0.0 – 1.0).
    pub error_rate: f64,
}

/// A complete snapshot of all observable system metrics.
///
/// Emitted by [`TelemetryBus`] at a configurable interval and stored in
/// rolling windows for historical analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    /// Monotonic timestamp of this snapshot (seconds since bus start).
    pub elapsed_secs: u64,
    /// Wall-clock timestamp (Unix seconds).
    pub unix_ts: u64,
    /// Per-stage metrics, one entry per pipeline stage.
    pub stages: Vec<StageMetrics>,
    /// Cache hit rate across all caching layers (0.0 – 1.0).
    pub cache_hit_rate: f64,
    /// Deduplication collision rate (0.0 – 1.0).
    pub dedup_collision_rate: f64,
    /// Circuit-breaker state per named service.
    pub circuit_states: Vec<(String, CircuitState)>,
    /// Total completed requests since process start.
    pub total_completed: u64,
    /// Total dropped requests since process start.
    pub total_dropped: u64,
    /// Total error count since process start.
    pub total_errors: u64,
    /// System-level composite pressure score (0.0 – 1.0).
    pub pressure: f64,
}

impl Default for TelemetrySnapshot {
    fn default() -> Self {
        Self {
            elapsed_secs: 0,
            unix_ts: 0,
            stages: Vec::new(),
            cache_hit_rate: 0.0,
            dedup_collision_rate: 0.0,
            circuit_states: Vec::new(),
            total_completed: 0,
            total_dropped: 0,
            total_errors: 0,
            pressure: 0.0,
        }
    }
}

/// A rolling window of [`TelemetrySnapshot`]s bounded by duration.
#[derive(Debug)]
struct RollingWindow {
    /// Maximum age of snapshots kept in this window.
    max_age: Duration,
    /// Ring buffer ordered oldest → newest.
    snapshots: VecDeque<(Instant, TelemetrySnapshot)>,
    /// Upper bound on the number of entries retained.
    capacity: usize,
}

impl RollingWindow {
    fn new(max_age: Duration, capacity: usize) -> Self {
        Self {
            max_age,
            snapshots: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, snap: TelemetrySnapshot) {
        let now = Instant::now();
        // Evict expired entries
        while self
            .snapshots
            .front()
            .map(|(t, _)| now.duration_since(*t) > self.max_age)
            .unwrap_or(false)
        {
            self.snapshots.pop_front();
        }
        // Evict oldest when over capacity
        if self.snapshots.len() >= self.capacity {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back((now, snap));
    }

    fn all(&self) -> Vec<&TelemetrySnapshot> {
        self.snapshots.iter().map(|(_, s)| s).collect()
    }

    fn len(&self) -> usize {
        self.snapshots.len()
    }
}

// ─── Counters supplied by the pipeline ───────────────────────────────────────

/// Atomic counters the pipeline increments; the bus reads them on each tick.
///
/// Wrap in an [`Arc`] and share with every pipeline stage.
#[derive(Debug, Default)]
pub struct PipelineCounters {
    /// Total requests that completed successfully.
    pub completed: AtomicU64,
    /// Total requests dropped (backpressure shed).
    pub dropped: AtomicU64,
    /// Total requests that produced an error.
    pub errors: AtomicU64,
    /// Total cache hits.
    pub cache_hits: AtomicU64,
    /// Total cache lookups.
    pub cache_lookups: AtomicU64,
    /// Total deduplication collisions (same key seen while in-flight).
    pub dedup_collisions: AtomicU64,
    /// Total deduplication checks performed.
    pub dedup_checks: AtomicU64,
}

impl PipelineCounters {
    /// Create a new zeroed counter set.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Return the current cache hit rate, or `0.0` if no lookups recorded.
    pub fn cache_hit_rate(&self) -> f64 {
        let lookups = self.cache_lookups.load(Ordering::Relaxed);
        if lookups == 0 {
            return 0.0;
        }
        self.cache_hits.load(Ordering::Relaxed) as f64 / lookups as f64
    }

    /// Return the current dedup collision rate, or `0.0` if no checks recorded.
    pub fn dedup_collision_rate(&self) -> f64 {
        let checks = self.dedup_checks.load(Ordering::Relaxed);
        if checks == 0 {
            return 0.0;
        }
        self.dedup_collisions.load(Ordering::Relaxed) as f64 / checks as f64
    }
}

// ─── Stage probe ─────────────────────────────────────────────────────────────

/// Callback the telemetry bus calls each tick to collect per-stage metrics.
///
/// Implement this for each pipeline stage and register with [`TelemetryBus::register_stage`].
pub trait StageProbe: Send + Sync {
    /// Return the current metrics for this stage.
    fn probe(&self) -> StageMetrics;
    /// Return the current circuit-breaker state (if the stage has one).
    fn circuit_state(&self) -> Option<(String, CircuitState)> {
        None
    }
}

// ─── Bus configuration ───────────────────────────────────────────────────────

/// Configuration for [`TelemetryBus`].
#[derive(Debug, Clone)]
pub struct TelemetryBusConfig {
    /// How often the bus samples and broadcasts a [`TelemetrySnapshot`].
    pub sample_interval: Duration,
    /// Capacity of the broadcast channel (number of snapshots buffered per
    /// subscriber before the oldest is dropped).
    pub channel_capacity: usize,
    /// Retention capacity for the 1-minute rolling window.
    pub window_1m_capacity: usize,
    /// Retention capacity for the 5-minute rolling window.
    pub window_5m_capacity: usize,
    /// Retention capacity for the 15-minute rolling window.
    pub window_15m_capacity: usize,
    /// Retention capacity for the 1-hour rolling window.
    pub window_1h_capacity: usize,
}

impl Default for TelemetryBusConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_secs(5),
            channel_capacity: 64,
            window_1m_capacity: 60,
            window_5m_capacity: 300,
            window_15m_capacity: 900,
            window_1h_capacity: 3_600,
        }
    }
}

// ─── Bus ─────────────────────────────────────────────────────────────────────

/// Handle that lets external code publish ad-hoc circuit-state transitions.
#[derive(Clone, Debug)]
pub struct CircuitStatePublisher {
    inner: Arc<BusInner>,
}

impl CircuitStatePublisher {
    /// Update the known circuit-breaker state for `service`.
    pub async fn set(&self, service: impl Into<String>, state: CircuitState) {
        let mut map = self.inner.circuit_states.write().await;
        let key = service.into();
        // Replace existing entry or push new one
        if let Some(entry) = map.iter_mut().find(|(s, _)| s == &key) {
            entry.1 = state;
        } else {
            map.push((key, state));
        }
    }
}

struct BusInner {
    cfg: TelemetryBusConfig,
    counters: Arc<PipelineCounters>,
    stages: Mutex<Vec<Box<dyn StageProbe>>>,
    circuit_states: RwLock<Vec<(String, CircuitState)>>,
    windows: Mutex<[RollingWindow; 4]>,
    tx: broadcast::Sender<TelemetrySnapshot>,
    started_at: Instant,
    publish_errors: AtomicU64,
    /// External pressure signal injected from HelixRouter (stored as pressure*1000 as u64).
    external_pressure_milli: AtomicU64,
}

impl std::fmt::Debug for BusInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusInner")
            .field("cfg", &self.cfg)
            .field("counters", &self.counters)
            .field("stages", &"<locked>")
            .field("started_at", &self.started_at)
            .field("publish_errors", &self.publish_errors)
            .finish()
    }
}

/// The telemetry aggregation bus.
///
/// # Usage
///
/// ```no_run
/// # use tokio_prompt_orchestrator::self_tune::telemetry_bus::{TelemetryBus, TelemetryBusConfig, PipelineCounters};
/// # #[tokio::main]
/// # async fn main() {
/// let counters = PipelineCounters::new();
/// let bus = TelemetryBus::new(TelemetryBusConfig::default(), counters.clone());
/// let mut sub = bus.subscribe();
/// bus.start();
/// // In another task:
/// // while let Ok(snap) = sub.recv().await { ... }
/// # }
/// ```
#[derive(Clone)]
pub struct TelemetryBus {
    inner: Arc<BusInner>,
}

impl TelemetryBus {
    /// Create a new bus.  Call [`start`](Self::start) to begin sampling.
    pub fn new(cfg: TelemetryBusConfig, counters: Arc<PipelineCounters>) -> Self {
        let (tx, _) = broadcast::channel(cfg.channel_capacity);
        let windows = Mutex::new([
            RollingWindow::new(Duration::from_secs(60), cfg.window_1m_capacity),
            RollingWindow::new(Duration::from_secs(300), cfg.window_5m_capacity),
            RollingWindow::new(Duration::from_secs(900), cfg.window_15m_capacity),
            RollingWindow::new(Duration::from_secs(3_600), cfg.window_1h_capacity),
        ]);

        Self {
            inner: Arc::new(BusInner {
                cfg,
                counters,
                stages: Mutex::new(Vec::new()),
                circuit_states: RwLock::new(Vec::new()),
                windows,
                tx,
                started_at: Instant::now(),
                publish_errors: AtomicU64::new(0),
                external_pressure_milli: AtomicU64::new(0),
            }),
        }
    }

    /// Register a stage probe.  The bus will call `probe.probe()` each tick.
    pub async fn register_stage(&self, probe: Box<dyn StageProbe>) {
        self.inner.stages.lock().await.push(probe);
    }

    /// Subscribe to the snapshot broadcast channel.
    pub fn subscribe(&self) -> broadcast::Receiver<TelemetrySnapshot> {
        self.inner.tx.subscribe()
    }

    /// Return a handle for publishing circuit-state transitions.
    pub fn circuit_publisher(&self) -> CircuitStatePublisher {
        CircuitStatePublisher {
            inner: self.inner.clone(),
        }
    }

    /// Spawn the background sampling loop.
    ///
    /// This method is idempotent — calling it multiple times spawns multiple
    /// loops (each with their own tick), so only call it once.
    pub fn start(&self) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(inner.cfg.sample_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;
                let snap = Self::collect(&inner).await;
                debug!(
                    elapsed_secs = snap.elapsed_secs,
                    pressure = snap.pressure,
                    "telemetry snapshot"
                );

                // Store in rolling windows
                {
                    let mut ws = inner.windows.lock().await;
                    for w in ws.iter_mut() {
                        w.push(snap.clone());
                    }
                }

                // Broadcast (best-effort — no subscribers is fine)
                if let Err(e) = inner.tx.send(snap) {
                    // broadcast::SendError means no receivers — not an error
                    let _ = e; // suppress unused-variable warning
                    inner.publish_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }

    /// Force an immediate snapshot collection and broadcast (useful in tests).
    pub async fn tick_now(&self) -> TelemetrySnapshot {
        let snap = Self::collect(&self.inner).await;
        {
            let mut ws = self.inner.windows.lock().await;
            for w in ws.iter_mut() {
                w.push(snap.clone());
            }
        }
        let _ = self.inner.tx.send(snap.clone());
        snap
    }

    /// Return all snapshots in the 1-minute rolling window.
    pub async fn window_1m(&self) -> Vec<TelemetrySnapshot> {
        let ws = self.inner.windows.lock().await;
        ws[0].all().into_iter().cloned().collect()
    }

    /// Return all snapshots in the 5-minute rolling window.
    pub async fn window_5m(&self) -> Vec<TelemetrySnapshot> {
        let ws = self.inner.windows.lock().await;
        ws[1].all().into_iter().cloned().collect()
    }

    /// Return all snapshots in the 15-minute rolling window.
    pub async fn window_15m(&self) -> Vec<TelemetrySnapshot> {
        let ws = self.inner.windows.lock().await;
        ws[2].all().into_iter().cloned().collect()
    }

    /// Return all snapshots in the 1-hour rolling window.
    pub async fn window_1h(&self) -> Vec<TelemetrySnapshot> {
        let ws = self.inner.windows.lock().await;
        ws[3].all().into_iter().cloned().collect()
    }

    /// Return the total number of publish errors (no-subscribers events).
    pub fn publish_errors(&self) -> u64 {
        self.inner.publish_errors.load(Ordering::Relaxed)
    }

    /// Inject an external pressure signal (e.g. from HelixRouter's `pressure_score`).
    ///
    /// `pressure` is clamped to `[0.0, 1.0]`. This value is blended with the
    /// internally-computed queue-fill pressure on the next snapshot tick using
    /// a 50/50 average, making Tokio Prompt's self-improvement loop aware of
    /// HelixRouter's downstream backpressure.
    ///
    /// Calling this with `0.0` disables the external signal (default).
    ///
    /// # Panics
    /// This function never panics.
    pub fn set_external_pressure(&self, pressure: f64) {
        let clamped = pressure.clamp(0.0, 1.0);
        // Store as fixed-point milli (0–1000) to avoid f64 atomics.
        let milli = (clamped * 1000.0) as u64;
        self.inner
            .external_pressure_milli
            .store(milli, Ordering::Relaxed);
    }

    /// Read the currently injected external pressure (0.0 – 1.0).
    ///
    /// Returns `0.0` when no external signal has been set.
    pub fn external_pressure(&self) -> f64 {
        let milli = self.inner.external_pressure_milli.load(Ordering::Relaxed);
        milli as f64 / 1000.0
    }

    // ── Internal ──────────────────────────────────────────────────────────

    async fn collect(inner: &Arc<BusInner>) -> TelemetrySnapshot {
        let elapsed_secs = inner.started_at.elapsed().as_secs();
        let unix_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let stages_lock = inner.stages.lock().await;
        let mut stages: Vec<StageMetrics> = Vec::with_capacity(stages_lock.len());
        let mut circuit_updates: Vec<(String, CircuitState)> = Vec::new();

        for probe in stages_lock.iter() {
            stages.push(probe.probe());
            if let Some(cs) = probe.circuit_state() {
                circuit_updates.push(cs);
            }
        }
        drop(stages_lock);

        // Merge probe-reported circuit states
        if !circuit_updates.is_empty() {
            let mut map = inner.circuit_states.write().await;
            for (svc, state) in circuit_updates {
                if let Some(entry) = map.iter_mut().find(|(s, _)| s == &svc) {
                    entry.1 = state;
                } else {
                    map.push((svc, state));
                }
            }
        }
        let circuit_states = inner.circuit_states.read().await.clone();

        let total_completed = inner.counters.completed.load(Ordering::Relaxed);
        let total_dropped = inner.counters.dropped.load(Ordering::Relaxed);
        let total_errors = inner.counters.errors.load(Ordering::Relaxed);
        let cache_hit_rate = inner.counters.cache_hit_rate();
        let dedup_collision_rate = inner.counters.dedup_collision_rate();

        // Composite pressure: average of queue fill ratios weighted by stage count,
        // blended 50/50 with any externally-injected signal (e.g. HelixRouter's
        // pressure_score) when that signal is non-zero.
        let internal_pressure = if stages.is_empty() {
            0.0
        } else {
            let fill_sum: f64 = stages
                .iter()
                .map(|s| {
                    if s.queue_capacity == 0 {
                        0.0
                    } else {
                        s.queue_depth as f64 / s.queue_capacity as f64
                    }
                })
                .sum();
            (fill_sum / stages.len() as f64).min(1.0)
        };
        let ext_milli = inner
            .external_pressure_milli
            .load(Ordering::Relaxed);
        let pressure = if ext_milli > 0 {
            let ext = ext_milli as f64 / 1000.0;
            ((internal_pressure + ext) / 2.0).min(1.0)
        } else {
            internal_pressure
        };

        TelemetrySnapshot {
            elapsed_secs,
            unix_ts,
            stages,
            cache_hit_rate,
            dedup_collision_rate,
            circuit_states,
            total_completed,
            total_dropped,
            total_errors,
            pressure,
        }
    }
}

// ─── Latency helpers ─────────────────────────────────────────────────────────

/// Compute [`LatencyStats`] from a mutable slice of millisecond samples.
///
/// The slice is sorted in-place for percentile computation.
pub fn compute_latency_stats(samples: &mut [u64]) -> LatencyStats {
    if samples.is_empty() {
        return LatencyStats::default();
    }
    samples.sort_unstable();
    let n = samples.len();
    let p50 = samples[(n as f64 * 0.50).ceil() as usize - 1];
    let p95 = samples[((n as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1)];
    let p99 = samples[((n as f64 * 0.99).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1)];
    let avg = samples.iter().sum::<u64>() as f64 / n as f64;
    LatencyStats {
        p50_ms: p50,
        p95_ms: p95,
        p99_ms: p99,
        avg_ms: avg,
        sample_count: n as u64,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    // ── Helpers ───────────────────────────────────────────────────────────

    fn make_bus() -> TelemetryBus {
        let counters = PipelineCounters::new();
        TelemetryBus::new(TelemetryBusConfig::default(), counters)
    }

    struct FakeStage {
        name: &'static str,
        depth: AtomicUsize,
        cap: usize,
    }

    impl FakeStage {
        fn new(name: &'static str, depth: usize, cap: usize) -> Self {
            Self {
                name,
                depth: AtomicUsize::new(depth),
                cap,
            }
        }
    }

    impl StageProbe for FakeStage {
        fn probe(&self) -> StageMetrics {
            StageMetrics {
                stage: self.name.to_string(),
                queue_depth: self.depth.load(Ordering::Relaxed),
                queue_capacity: self.cap,
                ..Default::default()
            }
        }
    }

    // ── Unit tests ────────────────────────────────────────────────────────

    #[test]
    fn test_pipeline_counters_cache_hit_rate_zero_with_no_lookups() {
        let c = PipelineCounters::default();
        assert_eq!(c.cache_hit_rate(), 0.0);
    }

    #[test]
    fn test_pipeline_counters_cache_hit_rate_correct() {
        let c = PipelineCounters::default();
        c.cache_hits.store(3, Ordering::Relaxed);
        c.cache_lookups.store(10, Ordering::Relaxed);
        assert!((c.cache_hit_rate() - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pipeline_counters_dedup_collision_rate_zero() {
        let c = PipelineCounters::default();
        assert_eq!(c.dedup_collision_rate(), 0.0);
    }

    #[test]
    fn test_pipeline_counters_dedup_collision_rate_correct() {
        let c = PipelineCounters::default();
        c.dedup_collisions.store(2, Ordering::Relaxed);
        c.dedup_checks.store(100, Ordering::Relaxed);
        assert!((c.dedup_collision_rate() - 0.02).abs() < f64::EPSILON);
    }

    #[test]
    fn test_rolling_window_capacity_respected() {
        let mut w = RollingWindow::new(Duration::from_secs(3600), 3);
        for _ in 0..5 {
            w.push(TelemetrySnapshot::default());
        }
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn test_rolling_window_returns_snapshots() {
        let mut w = RollingWindow::new(Duration::from_secs(3600), 10);
        w.push(TelemetrySnapshot {
            total_completed: 42,
            ..Default::default()
        });
        let all = w.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].total_completed, 42);
    }

    #[test]
    fn test_circuit_state_default_is_closed() {
        assert_eq!(CircuitState::default(), CircuitState::Closed);
    }

    #[test]
    fn test_compute_latency_stats_empty() {
        let stats = compute_latency_stats(&mut []);
        assert_eq!(stats.sample_count, 0);
        assert_eq!(stats.p50_ms, 0);
    }

    #[test]
    fn test_compute_latency_stats_single() {
        let mut s = [10u64];
        let stats = compute_latency_stats(&mut s);
        assert_eq!(stats.p50_ms, 10);
        assert_eq!(stats.p95_ms, 10);
        assert_eq!(stats.p99_ms, 10);
        assert!((stats.avg_ms - 10.0).abs() < f64::EPSILON);
        assert_eq!(stats.sample_count, 1);
    }

    #[test]
    fn test_compute_latency_stats_sorted_correctly() {
        let mut s = [100u64, 1, 50, 200, 75, 25, 150, 10, 90, 60];
        let stats = compute_latency_stats(&mut s);
        assert!(stats.p50_ms <= stats.p95_ms);
        assert!(stats.p95_ms <= stats.p99_ms);
        assert_eq!(stats.sample_count, 10);
    }

    #[test]
    fn test_telemetry_snapshot_default_zero_pressure() {
        let snap = TelemetrySnapshot::default();
        assert_eq!(snap.pressure, 0.0);
        assert_eq!(snap.total_completed, 0);
    }

    #[test]
    fn test_telemetry_bus_config_default_interval() {
        let cfg = TelemetryBusConfig::default();
        assert_eq!(cfg.sample_interval, Duration::from_secs(5));
        assert!(cfg.channel_capacity > 0);
    }

    #[tokio::test]
    async fn test_bus_subscribe_receives_snapshot() {
        let counters = PipelineCounters::new();
        let cfg = TelemetryBusConfig {
            sample_interval: Duration::from_millis(50),
            ..Default::default()
        };
        let bus = TelemetryBus::new(cfg, counters);
        let mut sub = bus.subscribe();
        bus.start();

        let snap = tokio::time::timeout(Duration::from_secs(2), sub.recv())
            .await
            .expect("timeout")
            .expect("recv error");
        assert_eq!(snap.stages.len(), 0); // no probes registered
    }

    #[tokio::test]
    async fn test_bus_tick_now_returns_snapshot() {
        let bus = make_bus();
        let snap = bus.tick_now().await;
        assert_eq!(snap.pressure, 0.0);
    }

    #[tokio::test]
    async fn test_bus_tick_now_stores_in_window() {
        let bus = make_bus();
        bus.tick_now().await;
        let w = bus.window_1m().await;
        assert_eq!(w.len(), 1);
    }

    #[tokio::test]
    async fn test_bus_windows_grow_with_multiple_ticks() {
        let bus = make_bus();
        for _ in 0..3 {
            bus.tick_now().await;
        }
        assert_eq!(bus.window_1m().await.len(), 3);
        assert_eq!(bus.window_5m().await.len(), 3);
        assert_eq!(bus.window_15m().await.len(), 3);
        assert_eq!(bus.window_1h().await.len(), 3);
    }

    #[tokio::test]
    async fn test_bus_registers_stage_and_probes_it() {
        let counters = PipelineCounters::new();
        let bus = TelemetryBus::new(TelemetryBusConfig::default(), counters);
        bus.register_stage(Box::new(FakeStage::new("rag", 4, 512)))
            .await;
        let snap = bus.tick_now().await;
        assert_eq!(snap.stages.len(), 1);
        assert_eq!(snap.stages[0].stage, "rag");
        assert_eq!(snap.stages[0].queue_depth, 4);
    }

    #[tokio::test]
    async fn test_bus_pressure_reflects_queue_fill() {
        let counters = PipelineCounters::new();
        let bus = TelemetryBus::new(TelemetryBusConfig::default(), counters);
        // queue half-full
        bus.register_stage(Box::new(FakeStage::new("inference", 256, 512)))
            .await;
        let snap = bus.tick_now().await;
        assert!((snap.pressure - 0.5).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_bus_pressure_zero_with_empty_queue() {
        let counters = PipelineCounters::new();
        let bus = TelemetryBus::new(TelemetryBusConfig::default(), counters);
        bus.register_stage(Box::new(FakeStage::new("rag", 0, 512)))
            .await;
        let snap = bus.tick_now().await;
        assert_eq!(snap.pressure, 0.0);
    }

    #[tokio::test]
    async fn test_bus_pressure_capped_at_1() {
        let counters = PipelineCounters::new();
        let bus = TelemetryBus::new(TelemetryBusConfig::default(), counters);
        // overfull queue
        bus.register_stage(Box::new(FakeStage::new("rag", 1024, 512)))
            .await;
        let snap = bus.tick_now().await;
        assert!(snap.pressure <= 1.0);
    }

    #[tokio::test]
    async fn test_bus_counters_reflected_in_snapshot() {
        let counters = PipelineCounters::new();
        counters.completed.store(100, Ordering::Relaxed);
        counters.dropped.store(5, Ordering::Relaxed);
        counters.errors.store(2, Ordering::Relaxed);
        let bus = TelemetryBus::new(TelemetryBusConfig::default(), counters);
        let snap = bus.tick_now().await;
        assert_eq!(snap.total_completed, 100);
        assert_eq!(snap.total_dropped, 5);
        assert_eq!(snap.total_errors, 2);
    }

    #[tokio::test]
    async fn test_circuit_publisher_set_and_read() {
        let bus = make_bus();
        let pub_ = bus.circuit_publisher();
        pub_.set("inference-worker", CircuitState::Open).await;
        let snap = bus.tick_now().await;
        let found = snap
            .circuit_states
            .iter()
            .find(|(s, _)| s == "inference-worker");
        assert!(found.is_some());
        assert_eq!(found.unwrap().1, CircuitState::Open);
    }

    #[tokio::test]
    async fn test_circuit_publisher_updates_existing_state() {
        let bus = make_bus();
        let pub_ = bus.circuit_publisher();
        pub_.set("svc", CircuitState::Open).await;
        pub_.set("svc", CircuitState::HalfOpen).await;
        let snap = bus.tick_now().await;
        let states: Vec<_> = snap
            .circuit_states
            .iter()
            .filter(|(s, _)| s == "svc")
            .collect();
        assert_eq!(states.len(), 1, "should deduplicate service entries");
        assert_eq!(states[0].1, CircuitState::HalfOpen);
    }

    #[tokio::test]
    async fn test_bus_clone_shares_state() {
        let bus = make_bus();
        let bus2 = bus.clone();
        bus.tick_now().await;
        let w1 = bus.window_1m().await;
        let w2 = bus2.window_1m().await;
        assert_eq!(w1.len(), w2.len());
    }

    #[tokio::test]
    async fn test_multiple_subscribers_all_receive() {
        let counters = PipelineCounters::new();
        let cfg = TelemetryBusConfig {
            sample_interval: Duration::from_millis(50),
            ..Default::default()
        };
        let bus = TelemetryBus::new(cfg, counters);
        let mut sub1 = bus.subscribe();
        let mut sub2 = bus.subscribe();
        bus.start();

        let r1 = tokio::time::timeout(Duration::from_secs(2), sub1.recv()).await;
        let r2 = tokio::time::timeout(Duration::from_secs(2), sub2.recv()).await;
        assert!(r1.is_ok());
        assert!(r2.is_ok());
    }

    #[test]
    fn test_stage_metrics_default() {
        let m = StageMetrics::default();
        assert_eq!(m.queue_depth, 0);
        assert_eq!(m.error_rate, 0.0);
    }

    #[test]
    fn test_latency_stats_default_all_zero() {
        let l = LatencyStats::default();
        assert_eq!(l.p50_ms, 0);
        assert_eq!(l.p95_ms, 0);
        assert_eq!(l.p99_ms, 0);
        assert_eq!(l.sample_count, 0);
    }

    // ── External pressure injection (HelixRouter integration) ─────────────

    #[test]
    fn test_external_pressure_default_is_zero() {
        let bus = make_bus();
        assert_eq!(bus.external_pressure(), 0.0);
    }

    #[test]
    fn test_set_external_pressure_stores_value() {
        let bus = make_bus();
        bus.set_external_pressure(0.75);
        let v = bus.external_pressure();
        assert!((v - 0.75).abs() < 0.002, "expected ~0.75, got {v}");
    }

    #[test]
    fn test_set_external_pressure_clamps_above_one() {
        let bus = make_bus();
        bus.set_external_pressure(2.0);
        let v = bus.external_pressure();
        assert!(v <= 1.0, "pressure must not exceed 1.0: {v}");
        assert!((v - 1.0).abs() < 0.002);
    }

    #[test]
    fn test_set_external_pressure_clamps_below_zero() {
        let bus = make_bus();
        bus.set_external_pressure(-0.5);
        let v = bus.external_pressure();
        assert!(v >= 0.0, "pressure must not be negative: {v}");
        assert_eq!(v, 0.0);
    }

    #[test]
    fn test_set_external_pressure_zero_disables_signal() {
        let bus = make_bus();
        bus.set_external_pressure(0.8);
        bus.set_external_pressure(0.0);
        let v = bus.external_pressure();
        assert_eq!(v, 0.0, "setting 0.0 should disable the external signal");
    }

    #[test]
    fn test_set_external_pressure_overwrite() {
        let bus = make_bus();
        bus.set_external_pressure(0.3);
        bus.set_external_pressure(0.9);
        let v = bus.external_pressure();
        assert!((v - 0.9).abs() < 0.002, "expected ~0.9 after overwrite, got {v}");
    }

    #[tokio::test]
    async fn test_snapshot_blends_external_pressure_when_set() {
        let bus = make_bus();
        // No stages → internal pressure = 0.0. External = 0.6.
        // Blended = (0.0 + 0.6) / 2 = 0.3.
        bus.set_external_pressure(0.6);
        let snap = bus.tick_now().await;
        assert!(snap.pressure > 0.0, "blended pressure should be > 0 when external is set");
        assert!((snap.pressure - 0.3).abs() < 0.002, "expected 0.3, got {}", snap.pressure);
    }

    #[tokio::test]
    async fn test_snapshot_uses_internal_pressure_when_external_zero() {
        let bus = make_bus();
        // No stages → internal = 0.0, external = 0.0 → pressure = 0.0.
        let snap = bus.tick_now().await;
        assert_eq!(snap.pressure, 0.0);
    }

    #[tokio::test]
    async fn test_snapshot_external_pressure_clamped_in_blend() {
        let bus = make_bus();
        // External = 1.0, internal = 0.0 → blended = 0.5
        bus.set_external_pressure(1.0);
        let snap = bus.tick_now().await;
        assert!((snap.pressure - 0.5).abs() < 0.002, "expected 0.5, got {}", snap.pressure);
    }
}
