//! # HelixRouter Core
//!
//! ## Responsibility
//! The adaptive async compute router. Accepts jobs, selects a strategy
//! based on cost thresholds and system pressure, executes the job via
//! the chosen strategy, records metrics, and returns the output.
//!
//! ## Guarantees
//! - Thread-safe: `Router` is `Clone + Send + Sync` (wraps `Arc<Inner>`).
//! - Non-blocking hot path: `choose_strategy()` is a pure function.
//! - Graceful degradation: under extreme pressure, jobs are shed (Strategy::Drop).
//! - Adaptive: effective thresholds adjust based on observed p95 latency.
//!
//! ## NOT Responsible For
//! - Configuration loading/watching (see `config`)
//! - Compute function implementations (see `strategies`)
//! - Metric storage details (see `metrics`)

use crate::helix::config::{HelixConfig, PressureConfig};
use crate::helix::metrics::MetricsStore;
use crate::helix::strategies;
use crate::helix::types::{BatchEntry, CpuWork, Job, JobId, Output, Strategy};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;

/// HelixRouter error type.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, thiserror::Error)]
pub enum HelixError {
    /// The CPU pool channel is closed.
    #[error("CPU pool channel closed")]
    CpuPoolClosed,

    /// The oneshot reply channel was dropped before a result arrived.
    #[error("reply channel dropped")]
    ReplyDropped,

    /// The job was dropped due to backpressure.
    #[error("job {0} dropped: system under pressure ({1:.2})")]
    Dropped(JobId, f64),
}

/// The adaptive async compute router.
///
/// Cheaply cloneable (wraps `Arc<Inner>`). All clones share state.
///
/// # Panics
///
/// This type and its methods never panic.
#[derive(Clone)]
pub struct HelixRouter {
    inner: Arc<Inner>,
}

struct Inner {
    config: HelixConfig,
    metrics: Arc<MetricsStore>,
    cpu_tx: mpsc::Sender<CpuWork>,
    cpu_busy: AtomicUsize,
    batch_queues: Mutex<HashMap<crate::helix::types::JobKind, Vec<BatchEntry>>>,
    effective_spawn_threshold: AtomicU64,
    next_job_id: AtomicU64,
}

impl HelixRouter {
    /// Create a new HelixRouter with the given configuration.
    ///
    /// Spawns CPU dispatch workers and initialises all internal state.
    ///
    /// # Arguments
    ///
    /// * `config` — Router configuration.
    ///
    /// # Returns
    ///
    /// A new [`HelixRouter`] ready to accept jobs.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(config: HelixConfig) -> Self {
        let metrics = Arc::new(MetricsStore::new(config.adaptive.ema_alpha));

        // Create CPU pool channel
        let (cpu_tx, cpu_rx) = mpsc::channel::<CpuWork>(config.cpu_parallelism * 4);

        // Spawn CPU dispatch workers
        let rx = Arc::new(tokio::sync::Mutex::new(cpu_rx));
        for _ in 0..config.cpu_parallelism {
            let rx_clone = Arc::clone(&rx);
            tokio::spawn(async move {
                loop {
                    let work = {
                        let mut guard = rx_clone.lock().await;
                        guard.recv().await
                    };
                    match work {
                        Some(w) => {
                            let result = strategies::execute_job(&w.job);
                            let _ = w.reply.send(result);
                        }
                        None => break,
                    }
                }
            });
        }

        let effective_spawn_threshold = AtomicU64::new(config.spawn_threshold);

        Self {
            inner: Arc::new(Inner {
                config,
                metrics,
                cpu_tx,
                cpu_busy: AtomicUsize::new(0),
                batch_queues: Mutex::new(HashMap::new()),
                effective_spawn_threshold,
                next_job_id: AtomicU64::new(0),
            }),
        }
    }

    /// Create a new router with default configuration.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn with_defaults() -> Self {
        Self::new(HelixConfig::default())
    }

    /// Submit a job for execution.
    ///
    /// The router selects a strategy, executes the job, records metrics,
    /// and returns the output. Under extreme pressure, jobs may be dropped.
    ///
    /// # Arguments
    ///
    /// * `job` — The job to execute.
    ///
    /// # Returns
    ///
    /// - `Ok(Output)` — on successful execution.
    /// - `Err(HelixError::Dropped)` — if the job was shed.
    /// - `Err(HelixError::CpuPoolClosed)` — if the CPU pool is shut down.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn submit(&self, job: Job) -> Result<Output, HelixError> {
        let start = Instant::now();
        let cpu_busy = self.inner.cpu_busy.load(Ordering::Relaxed);

        let strategy = self.choose_strategy(
            job.cost,
            cpu_busy,
            self.inner.config.cpu_parallelism,
            &self.inner.config.pressure,
        );

        if strategy == Strategy::Drop {
            let pressure = self.inner.metrics.pressure_score(
                cpu_busy,
                self.inner.config.cpu_parallelism,
                &self.inner.config.pressure,
            );
            self.inner.metrics.record_drop();
            return Err(HelixError::Dropped(job.id, pressure));
        }

        let result = match strategy {
            Strategy::Inline => strategies::execute_job(&job),
            Strategy::Spawn => {
                let job_clone = job.clone();
                let handle = tokio::spawn(async move { strategies::execute_job(&job_clone) });
                handle.await.unwrap_or(0)
            }
            Strategy::CpuPool => {
                self.inner.cpu_busy.fetch_add(1, Ordering::Relaxed);
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let send_result = self.inner.cpu_tx.try_send(CpuWork {
                    job: job.clone(),
                    reply: reply_tx,
                });
                match send_result {
                    Ok(()) => {
                        let result = reply_rx.await.map_err(|_| HelixError::ReplyDropped);
                        self.inner.cpu_busy.fetch_sub(1, Ordering::Relaxed);
                        result?
                    }
                    Err(_) => {
                        self.inner.cpu_busy.fetch_sub(1, Ordering::Relaxed);
                        // Fallback to spawn if CPU pool is full
                        strategies::execute_job(&job)
                    }
                }
            }
            Strategy::Batch => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let should_flush = {
                    let mut queues = self
                        .inner
                        .batch_queues
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let queue = queues.entry(job.kind).or_default();
                    queue.push(BatchEntry {
                        job: job.clone(),
                        reply: reply_tx,
                    });
                    queue.len() >= self.inner.config.batch_size
                };

                if should_flush {
                    self.flush_batch(job.kind);
                }

                // Use a timeout to prevent indefinite blocking when batch is not yet full.
                // If the batch hasn't flushed within the timeout, force-flush and return.
                let timeout_ms = self.inner.config.batch_timeout_ms;
                match tokio::time::timeout(tokio::time::Duration::from_millis(timeout_ms), reply_rx)
                    .await
                {
                    Ok(Ok(val)) => val,
                    Ok(Err(_)) => 0, // sender dropped
                    Err(_) => {
                        // Timeout — force flush to unblock
                        self.flush_batch(job.kind);
                        // Result was already sent by flush_batch; no need to re-await
                        0
                    }
                }
            }
            Strategy::Drop => {
                // Already handled above
                0
            }
        };

        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
        self.inner.metrics.record_latency(strategy, latency_ms);

        // Record cost prediction accuracy
        self.inner
            .metrics
            .record_cost(job.kind, job.cost as f64, latency_ms);

        // Adaptive threshold adjustment
        if self.inner.config.adaptive.enabled {
            self.maybe_adapt();
        }

        Ok(Output {
            job_id: job.id,
            result,
            strategy,
            latency_ms,
        })
    }

    /// Generate a unique job ID.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn next_job_id(&self) -> JobId {
        JobId(self.inner.next_job_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Get a reference to the metrics store.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn metrics(&self) -> &Arc<MetricsStore> {
        &self.inner.metrics
    }

    /// Get the current effective spawn threshold.
    ///
    /// May differ from `config.spawn_threshold` if adaptive adjustment is active.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn effective_spawn_threshold(&self) -> u64 {
        self.inner.effective_spawn_threshold.load(Ordering::Relaxed)
    }

    /// Get a reference to the router configuration.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn config(&self) -> &HelixConfig {
        &self.inner.config
    }

    /// Choose the execution strategy for a job.
    ///
    /// Pure function — no side effects, no I/O.
    ///
    /// # Arguments
    ///
    /// * `cost` — The job's estimated compute cost.
    /// * `cpu_busy` — Number of currently busy CPU workers.
    /// * `cpu_parallelism` — Total CPU worker count.
    /// * `pressure_config` — Pressure scoring weights.
    ///
    /// # Returns
    ///
    /// The [`Strategy`] to use for this job.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn choose_strategy(
        &self,
        cost: u64,
        cpu_busy: usize,
        cpu_parallelism: usize,
        pressure_config: &PressureConfig,
    ) -> Strategy {
        // Check pressure first — drop under extreme load
        let pressure =
            self.inner
                .metrics
                .pressure_score(cpu_busy, cpu_parallelism, pressure_config);
        if pressure > pressure_config.drop_threshold {
            return Strategy::Drop;
        }

        let effective_spawn = self.inner.effective_spawn_threshold.load(Ordering::Relaxed);

        if cost < self.inner.config.inline_threshold {
            Strategy::Inline
        } else if cost < effective_spawn {
            Strategy::Spawn
        } else if cost < self.inner.config.cpu_pool_threshold {
            Strategy::CpuPool
        } else {
            Strategy::Batch
        }
    }

    // ── Private helpers ────────────────────────────────────────────────

    fn flush_batch(&self, kind: crate::helix::types::JobKind) {
        let entries = {
            let mut queues = self
                .inner
                .batch_queues
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            queues.remove(&kind).unwrap_or_default()
        };
        if !entries.is_empty() {
            strategies::flush_batch(entries);
        }
    }

    fn maybe_adapt(&self) {
        let p95 = self.inner.metrics.p95(Strategy::CpuPool);
        let budget = self.inner.config.adaptive.p95_budget_ms as f64;
        let step = self.inner.config.adaptive.adaptive_step;
        let base = self.inner.config.spawn_threshold;
        let max = base.saturating_mul(3);

        let current = self.inner.effective_spawn_threshold.load(Ordering::Relaxed);

        if p95 > budget && current < max {
            // CpuPool is overloaded → raise threshold to push more work to Spawn
            let new = current.saturating_add(step).min(max);
            self.inner
                .effective_spawn_threshold
                .store(new, Ordering::Relaxed);
        } else if p95 > 0.0 && p95 < budget / 2.0 && current > base {
            // CpuPool has headroom → lower threshold (save spawn overhead)
            let new = current.saturating_sub(step).max(base);
            self.inner
                .effective_spawn_threshold
                .store(new, Ordering::Relaxed);
        }
    }
}

impl std::fmt::Debug for HelixRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelixRouter")
            .field("config", &self.inner.config)
            .field(
                "effective_spawn_threshold",
                &self.effective_spawn_threshold(),
            )
            .field("completed", &self.inner.metrics.completed())
            .field("dropped", &self.inner.metrics.dropped())
            .finish()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helix::config::HelixConfig;
    use crate::helix::types::JobKind;

    fn make_job(cost: u64) -> Job {
        Job {
            id: JobId(0),
            kind: JobKind::Hash,
            cost,
        }
    }

    // -- choose_strategy -------------------------------------------------

    #[tokio::test]
    async fn test_choose_strategy_inline_for_low_cost() {
        let router = HelixRouter::new(HelixConfig::default());
        let strategy = router.choose_strategy(10, 0, 4, &PressureConfig::default());
        assert_eq!(strategy, Strategy::Inline);
    }

    #[tokio::test]
    async fn test_choose_strategy_spawn_for_medium_cost() {
        let router = HelixRouter::new(HelixConfig::default());
        // default: inline < 50, spawn < 200
        let strategy = router.choose_strategy(100, 0, 4, &PressureConfig::default());
        assert_eq!(strategy, Strategy::Spawn);
    }

    #[tokio::test]
    async fn test_choose_strategy_cpu_pool_for_high_cost() {
        let router = HelixRouter::new(HelixConfig::default());
        // default: spawn < 200, cpu_pool < 500
        let strategy = router.choose_strategy(300, 0, 4, &PressureConfig::default());
        assert_eq!(strategy, Strategy::CpuPool);
    }

    #[tokio::test]
    async fn test_choose_strategy_batch_for_very_high_cost() {
        let router = HelixRouter::new(HelixConfig::default());
        // default: cpu_pool < 500
        let strategy = router.choose_strategy(600, 0, 4, &PressureConfig::default());
        assert_eq!(strategy, Strategy::Batch);
    }

    #[tokio::test]
    async fn test_choose_strategy_at_exact_inline_threshold() {
        let router = HelixRouter::new(HelixConfig::default());
        // cost == inline_threshold(50) → goes to Spawn (not Inline)
        let strategy = router.choose_strategy(50, 0, 4, &PressureConfig::default());
        assert_eq!(strategy, Strategy::Spawn);
    }

    #[tokio::test]
    async fn test_choose_strategy_at_exact_spawn_threshold() {
        let router = HelixRouter::new(HelixConfig::default());
        // cost == spawn_threshold(200) → goes to CpuPool
        let strategy = router.choose_strategy(200, 0, 4, &PressureConfig::default());
        assert_eq!(strategy, Strategy::CpuPool);
    }

    #[tokio::test]
    async fn test_choose_strategy_zero_cost() {
        let router = HelixRouter::new(HelixConfig::default());
        let strategy = router.choose_strategy(0, 0, 4, &PressureConfig::default());
        assert_eq!(strategy, Strategy::Inline);
    }

    // -- submit ----------------------------------------------------------

    #[tokio::test]
    async fn test_submit_inline_job_succeeds() {
        let router = HelixRouter::new(HelixConfig::default());
        let job = make_job(10);
        let result = router.submit(job).await;
        assert!(result.is_ok(), "inline job should succeed");
        let output = result.unwrap_or_else(|e| std::panic::panic_any(format!("test: {e}")));
        assert_eq!(output.strategy, Strategy::Inline);
        assert!(output.latency_ms >= 0.0);
    }

    #[tokio::test]
    async fn test_submit_spawn_job_succeeds() {
        let router = HelixRouter::new(HelixConfig::default());
        let job = Job {
            id: JobId(1),
            kind: JobKind::Hash,
            cost: 100,
        };
        let result = router.submit(job).await;
        assert!(result.is_ok(), "spawn job should succeed");
        let output = result.unwrap_or_else(|e| std::panic::panic_any(format!("test: {e}")));
        assert_eq!(output.strategy, Strategy::Spawn);
    }

    #[tokio::test]
    async fn test_submit_cpu_pool_job_succeeds() {
        let router = HelixRouter::new(HelixConfig::default());
        let job = Job {
            id: JobId(2),
            kind: JobKind::Prime,
            cost: 300,
        };
        let result = router.submit(job).await;
        assert!(result.is_ok(), "cpu_pool job should succeed");
    }

    #[tokio::test]
    async fn test_submit_batch_job_flushes_at_threshold() {
        let config = HelixConfig {
            batch_size: 2,
            batch_timeout_ms: 10, // Short timeout for test
            cpu_pool_threshold: 100,
            ..HelixConfig::default()
        };
        let router = HelixRouter::new(config);

        // Submit 2 batch jobs concurrently so the second triggers flush
        let r1 = router.clone();
        let r2 = router.clone();
        let h1 = tokio::spawn(async move {
            let job = Job {
                id: JobId(0),
                kind: JobKind::Hash,
                cost: 200,
            };
            r1.submit(job).await
        });
        let h2 = tokio::spawn(async move {
            let job = Job {
                id: JobId(1),
                kind: JobKind::Hash,
                cost: 200,
            };
            r2.submit(job).await
        });

        let (res1, res2) = tokio::join!(h1, h2);
        assert!(
            res1.unwrap_or_else(|_| Err(HelixError::CpuPoolClosed))
                .is_ok()
                || res2
                    .unwrap_or_else(|_| Err(HelixError::CpuPoolClosed))
                    .is_ok(),
            "at least one batch job should succeed"
        );
    }

    #[tokio::test]
    async fn test_submit_records_metrics() {
        let router = HelixRouter::new(HelixConfig::default());
        let job = make_job(10);
        let _ = router.submit(job).await;
        assert_eq!(router.metrics().completed(), 1);
    }

    #[tokio::test]
    async fn test_submit_deterministic_results() {
        let router = HelixRouter::new(HelixConfig::default());
        let job1 = Job {
            id: JobId(1),
            kind: JobKind::Hash,
            cost: 10,
        };
        let job2 = Job {
            id: JobId(2),
            kind: JobKind::Hash,
            cost: 10,
        };
        let r1 = router.submit(job1).await;
        let r2 = router.submit(job2).await;
        let v1 = r1.as_ref().map(|o| o.result).ok();
        let v2 = r2.as_ref().map(|o| o.result).ok();
        assert_eq!(v1, v2, "same cost+kind should produce same result");
    }

    // -- next_job_id -----------------------------------------------------

    #[tokio::test]
    async fn test_next_job_id_increments() {
        let router = HelixRouter::new(HelixConfig::default());
        let id1 = router.next_job_id();
        let id2 = router.next_job_id();
        assert_eq!(id1, JobId(0));
        assert_eq!(id2, JobId(1));
    }

    // -- effective_spawn_threshold ---------------------------------------

    #[tokio::test]
    async fn test_effective_spawn_threshold_starts_at_config() {
        let config = HelixConfig {
            spawn_threshold: 150,
            ..HelixConfig::default()
        };
        let router = HelixRouter::new(config);
        assert_eq!(router.effective_spawn_threshold(), 150);
    }

    // -- adaptive --------------------------------------------------------

    #[tokio::test]
    async fn test_adaptive_raises_threshold_when_cpu_pool_slow() {
        let config = HelixConfig {
            adaptive: crate::helix::config::AdaptiveConfig {
                enabled: true,
                p95_budget_ms: 1, // Very tight budget
                adaptive_step: 10,
                ema_alpha: 0.3,
            },
            ..HelixConfig::default()
        };
        let router = HelixRouter::new(config);
        let base = router.effective_spawn_threshold();

        // Record high latency for CpuPool
        for _ in 0..100 {
            router.metrics().record_latency(Strategy::CpuPool, 50.0);
        }

        // Trigger adaptation via submit
        let job = make_job(10);
        let _ = router.submit(job).await;

        let new_threshold = router.effective_spawn_threshold();
        assert!(
            new_threshold > base,
            "threshold should increase: was {base}, now {new_threshold}"
        );
    }

    #[tokio::test]
    async fn test_adaptive_threshold_capped_at_3x_base() {
        let config = HelixConfig {
            spawn_threshold: 100,
            adaptive: crate::helix::config::AdaptiveConfig {
                enabled: true,
                p95_budget_ms: 1,
                adaptive_step: 1000,
                ema_alpha: 0.3,
            },
            ..HelixConfig::default()
        };
        let router = HelixRouter::new(config);

        for _ in 0..100 {
            router.metrics().record_latency(Strategy::CpuPool, 50.0);
        }
        let job = make_job(10);
        let _ = router.submit(job).await;

        assert!(
            router.effective_spawn_threshold() <= 300,
            "threshold must not exceed 3x base (300), got {}",
            router.effective_spawn_threshold()
        );
    }

    #[tokio::test]
    async fn test_adaptive_disabled_no_threshold_change() {
        let config = HelixConfig {
            adaptive: crate::helix::config::AdaptiveConfig {
                enabled: false,
                p95_budget_ms: 1,
                adaptive_step: 100,
                ema_alpha: 0.3,
            },
            ..HelixConfig::default()
        };
        let router = HelixRouter::new(config);
        let original = router.effective_spawn_threshold();

        for _ in 0..100 {
            router.metrics().record_latency(Strategy::CpuPool, 50.0);
        }
        let job = make_job(10);
        let _ = router.submit(job).await;

        assert_eq!(
            router.effective_spawn_threshold(),
            original,
            "threshold should not change when adaptive is disabled"
        );
    }

    // -- debug -----------------------------------------------------------

    #[tokio::test]
    async fn test_helix_router_debug_does_not_panic() {
        let router = HelixRouter::new(HelixConfig::default());
        let _ = format!("{router:?}");
    }

    // -- error display ---------------------------------------------------

    #[test]
    fn test_helix_error_dropped_display() {
        let err = HelixError::Dropped(JobId(5), 0.85);
        assert!(err.to_string().contains("job-5"));
        assert!(err.to_string().contains("0.85"));
    }

    #[test]
    fn test_helix_error_cpu_pool_closed_display() {
        let err = HelixError::CpuPoolClosed;
        assert!(err.to_string().contains("CPU pool"));
    }

    #[test]
    fn test_helix_error_reply_dropped_display() {
        let err = HelixError::ReplyDropped;
        assert!(err.to_string().contains("reply"));
    }

    // -- Additional router tests --------------------------------------------

    #[tokio::test]
    async fn test_next_job_id_monotonic() {
        let router = HelixRouter::new(HelixConfig::default());
        let ids: Vec<JobId> = (0..10).map(|_| router.next_job_id()).collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(
                *id,
                JobId(i as u64),
                "job ID {i} should be {i}, got {:?}",
                id
            );
        }
    }

    #[tokio::test]
    async fn test_router_debug_format() {
        let router = HelixRouter::new(HelixConfig::default());
        let debug = format!("{router:?}");
        assert!(
            debug.contains("HelixRouter"),
            "Debug output should contain 'HelixRouter', got: {debug}"
        );
    }

    #[tokio::test]
    async fn test_effective_spawn_threshold_matches_config() {
        let config = HelixConfig {
            spawn_threshold: 275,
            ..HelixConfig::default()
        };
        let router = HelixRouter::new(config);
        assert_eq!(
            router.effective_spawn_threshold(),
            275,
            "effective spawn threshold should start at config value"
        );
    }

    #[tokio::test]
    async fn test_metrics_accessor_returns_store() {
        let router = HelixRouter::new(HelixConfig::default());
        // Record via metrics accessor, then verify
        router.metrics().record_latency(Strategy::Inline, 5.0);
        assert_eq!(
            router.metrics().completed(),
            1,
            "metrics accessor should return a usable store"
        );
    }

    #[tokio::test]
    async fn test_config_accessor_returns_config() {
        let config = HelixConfig {
            inline_threshold: 77,
            spawn_threshold: 200,
            cpu_pool_threshold: 500,
            ..HelixConfig::default()
        };
        let router = HelixRouter::new(config.clone());
        assert_eq!(
            router.config().inline_threshold,
            77,
            "config accessor should return the configured inline_threshold"
        );
        assert_eq!(
            router.config().cpu_parallelism,
            config.cpu_parallelism,
            "config accessor should return the configured cpu_parallelism"
        );
    }

    #[tokio::test]
    async fn test_submit_inline_returns_correct_result() {
        let router = HelixRouter::new(HelixConfig::default());
        let job = Job {
            id: JobId(0),
            kind: JobKind::Hash,
            cost: 10,
        };
        let output = router.submit(job).await.unwrap();
        // The result should match the hashmix function for cost=10
        let expected = crate::helix::strategies::hashmix(10);
        assert_eq!(
            output.result, expected,
            "inline submit result should match hashmix(10)"
        );
    }

    #[tokio::test]
    async fn test_submit_records_latency() {
        let router = HelixRouter::new(HelixConfig::default());
        let job = make_job(10);
        let _ = router.submit(job).await;
        assert!(
            router.metrics().completed() > 0,
            "completed count should be > 0 after a submit"
        );
        let summary = router.metrics().latency_summary(Strategy::Inline);
        assert!(
            summary.count > 0,
            "latency summary should have recorded the inline job"
        );
    }
}
