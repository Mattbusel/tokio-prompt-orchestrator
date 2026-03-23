//! Adaptive worker pool with Kalman-filter latency prediction.
//!
//! Monitors pipeline queue depths and inferred latency to decide when to
//! spawn additional worker tasks or drain excess ones. Uses a one-dimensional
//! Kalman filter to smooth noisy queue-depth observations and predict the
//! short-term trend, avoiding oscillation from reactive on/off control.
//!
//! ## Design
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │  AdaptivePool                                             │
//! │                                                           │
//! │  Queue depth ──► KalmanFilter ──► predicted depth        │
//! │                                        │                 │
//! │  Latency EMA ──────────────────────► ScaleDecision       │
//! │                                        │                 │
//! │                            spawn task / mark idle        │
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Kalman Filter
//!
//! A scalar 1-D Kalman filter tracks the queue depth signal:
//!
//! - **State** `x`: estimated true queue depth
//! - **Process noise** `Q`: models how fast the queue can change (default 1.0)
//! - **Measurement noise** `R`: models observation noise (default 5.0)
//!
//! The filter converges to the true depth in ~5–10 observations and provides a
//! smooth signal that drives scaling without reacting to single-sample spikes.
//!
//! ## Scaling Policy
//!
//! | Condition | Action |
//! |-----------|--------|
//! | predicted_depth > `scale_up_threshold` AND latency > `latency_threshold_ms` | Recommend scale-up |
//! | predicted_depth < `scale_down_threshold` AND pool_size > `min_workers` | Recommend scale-down |
//! | otherwise | Stable |

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Scaling recommendation produced by [`AdaptivePool::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScaleDecision {
    /// No change required — pool size is appropriate.
    Stable,
    /// Add `n` workers to handle increasing load.
    ScaleUp {
        /// Number of workers to add.
        by: usize,
    },
    /// Remove `n` workers to reduce idle resource consumption.
    ScaleDown {
        /// Number of workers to remove.
        by: usize,
    },
}

/// Configuration for the adaptive pool controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptivePoolConfig {
    /// Minimum number of workers to maintain at all times.
    pub min_workers: usize,
    /// Maximum number of workers the pool may grow to.
    pub max_workers: usize,
    /// Queue depth (predicted) above which scale-up is triggered.
    pub scale_up_threshold: f64,
    /// Queue depth (predicted) below which scale-down is considered.
    pub scale_down_threshold: f64,
    /// P99 latency (ms) above which scale-up is triggered alongside depth check.
    pub latency_threshold_ms: f64,
    /// Minimum time between consecutive scale events (prevents thrashing).
    pub cooldown: Duration,
    /// Process noise for the Kalman filter (models queue volatility).
    pub kalman_process_noise: f64,
    /// Measurement noise for the Kalman filter (models observation noise).
    pub kalman_measurement_noise: f64,
}

impl Default for AdaptivePoolConfig {
    fn default() -> Self {
        Self {
            min_workers: 1,
            max_workers: 32,
            scale_up_threshold: 50.0,
            scale_down_threshold: 5.0,
            latency_threshold_ms: 500.0,
            cooldown: Duration::from_secs(10),
            kalman_process_noise: 1.0,
            kalman_measurement_noise: 5.0,
        }
    }
}

/// Scalar 1-D Kalman filter.
///
/// Tracks a noisy scalar signal (e.g. queue depth) and produces a smoothed
/// estimate. The filter runs in O(1) time and O(1) space.
///
/// ## Math
///
/// Predict:
/// ```text
/// x_pred = x
/// p_pred = p + Q
/// ```
///
/// Update:
/// ```text
/// K = p_pred / (p_pred + R)
/// x = x_pred + K * (measurement - x_pred)
/// p = (1 - K) * p_pred
/// ```
#[derive(Debug, Clone)]
pub struct KalmanFilter {
    /// Current state estimate.
    pub x: f64,
    /// Estimate covariance.
    pub p: f64,
    /// Process noise covariance.
    pub q: f64,
    /// Measurement noise covariance.
    pub r: f64,
    /// Whether the filter has been initialised with a first measurement.
    initialised: bool,
}

impl KalmanFilter {
    /// Create a new filter with the given noise parameters.
    pub fn new(process_noise: f64, measurement_noise: f64) -> Self {
        Self {
            x: 0.0,
            p: 1.0,
            q: process_noise,
            r: measurement_noise,
            initialised: false,
        }
    }

    /// Feed a new observation and return the filtered estimate.
    ///
    /// On the first call, the filter is initialised directly to the measurement.
    pub fn update(&mut self, measurement: f64) -> f64 {
        if !self.initialised {
            self.x = measurement;
            self.initialised = true;
            return self.x;
        }

        // Predict step
        let x_pred = self.x;
        let p_pred = self.p + self.q;

        // Update step
        let k = p_pred / (p_pred + self.r);
        self.x = x_pred + k * (measurement - x_pred);
        self.p = (1.0 - k) * p_pred;

        self.x
    }

    /// Return the current smoothed estimate without feeding a new observation.
    pub fn estimate(&self) -> f64 {
        self.x
    }
}

/// EMA tracker for latency observations.
#[derive(Debug, Clone)]
pub struct LatencyEma {
    value: f64,
    alpha: f64,
    sample_count: u64,
}

impl LatencyEma {
    /// Create a new EMA tracker with the given smoothing factor (0 < alpha ≤ 1).
    pub fn new(alpha: f64) -> Self {
        Self {
            value: 0.0,
            alpha: alpha.clamp(0.001, 1.0),
            sample_count: 0,
        }
    }

    /// Feed a latency observation and return the new EMA.
    pub fn update(&mut self, latency_ms: f64) -> f64 {
        if self.sample_count == 0 {
            self.value = latency_ms;
        } else {
            self.value = self.alpha * latency_ms + (1.0 - self.alpha) * self.value;
        }
        self.sample_count += 1;
        self.value
    }

    /// Current EMA value.
    pub fn current(&self) -> f64 {
        self.value
    }

    /// Number of samples fed.
    pub fn count(&self) -> u64 {
        self.sample_count
    }
}

/// Internal mutable state, behind a mutex to allow async update.
struct PoolState {
    kalman: KalmanFilter,
    latency_ema: LatencyEma,
    current_workers: usize,
    last_scale_at: Option<Instant>,
}

/// Pool statistics snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct PoolStats {
    /// Current number of workers.
    pub current_workers: usize,
    /// Kalman-filtered queue depth estimate.
    pub estimated_queue_depth: f64,
    /// Latency EMA in milliseconds.
    pub latency_ema_ms: f64,
    /// Total scale-up decisions since creation.
    pub total_scale_ups: u64,
    /// Total scale-down decisions since creation.
    pub total_scale_downs: u64,
}

/// Adaptive pool controller.
///
/// This is a **controller** — it recommends scaling decisions but does not
/// itself spawn or kill tasks. The caller is responsible for acting on
/// [`ScaleDecision`] values returned by [`AdaptivePool::evaluate`].
///
/// # Thread Safety
///
/// `AdaptivePool` is `Send + Sync` and may be shared across tasks.
pub struct AdaptivePool {
    config: AdaptivePoolConfig,
    state: Mutex<PoolState>,
    total_scale_ups: AtomicU64,
    total_scale_downs: AtomicU64,
    observations: AtomicUsize,
}

impl AdaptivePool {
    /// Create a new pool controller with the given config and initial worker count.
    pub fn new(config: AdaptivePoolConfig, initial_workers: usize) -> Arc<Self> {
        let initial_workers = initial_workers.max(config.min_workers);
        Arc::new(Self {
            state: Mutex::new(PoolState {
                kalman: KalmanFilter::new(
                    config.kalman_process_noise,
                    config.kalman_measurement_noise,
                ),
                latency_ema: LatencyEma::new(0.15),
                current_workers: initial_workers,
                last_scale_at: None,
            }),
            config,
            total_scale_ups: AtomicU64::new(0),
            total_scale_downs: AtomicU64::new(0),
            observations: AtomicUsize::new(0),
        })
    }

    /// Feed a new observation and return a scaling recommendation.
    ///
    /// Call this on each pipeline tick (e.g. every 500 ms) to get decisions.
    ///
    /// # Arguments
    /// * `queue_depth` — raw queue depth observation (number of queued items).
    /// * `latency_ms`  — recent P99 or average latency in milliseconds.
    pub async fn evaluate(&self, queue_depth: usize, latency_ms: f64) -> ScaleDecision {
        let mut state = self.state.lock().await;
        self.observations.fetch_add(1, Ordering::Relaxed);

        // Update filters
        let smoothed_depth = state.kalman.update(queue_depth as f64);
        let smoothed_latency = state.latency_ema.update(latency_ms);

        debug!(
            raw_depth = queue_depth,
            smoothed_depth,
            raw_latency_ms = latency_ms,
            smoothed_latency_ms = smoothed_latency,
            current_workers = state.current_workers,
            "adaptive pool observation"
        );

        // Enforce cooldown
        if let Some(last) = state.last_scale_at {
            if last.elapsed() < self.config.cooldown {
                return ScaleDecision::Stable;
            }
        }

        let decision = if smoothed_depth > self.config.scale_up_threshold
            && smoothed_latency > self.config.latency_threshold_ms
            && state.current_workers < self.config.max_workers
        {
            // Scale up — add workers proportional to overload
            let headroom = self.config.max_workers - state.current_workers;
            let by = ((smoothed_depth / self.config.scale_up_threshold) as usize)
                .max(1)
                .min(headroom)
                .min(4); // cap single-event scale-up at 4
            ScaleDecision::ScaleUp { by }
        } else if smoothed_depth < self.config.scale_down_threshold
            && smoothed_latency < self.config.latency_threshold_ms * 0.5
            && state.current_workers > self.config.min_workers
        {
            let excess = state.current_workers - self.config.min_workers;
            let by = (excess / 2).max(1);
            ScaleDecision::ScaleDown { by }
        } else {
            ScaleDecision::Stable
        };

        if decision != ScaleDecision::Stable {
            state.last_scale_at = Some(Instant::now());
            match decision {
                ScaleDecision::ScaleUp { by } => {
                    state.current_workers =
                        (state.current_workers + by).min(self.config.max_workers);
                    self.total_scale_ups.fetch_add(1, Ordering::Relaxed);
                    info!(
                        by,
                        new_total = state.current_workers,
                        depth = smoothed_depth,
                        latency_ms = smoothed_latency,
                        "adaptive pool: scale up"
                    );
                }
                ScaleDecision::ScaleDown { by } => {
                    state.current_workers =
                        (state.current_workers - by).max(self.config.min_workers);
                    self.total_scale_downs.fetch_add(1, Ordering::Relaxed);
                    info!(
                        by,
                        new_total = state.current_workers,
                        depth = smoothed_depth,
                        latency_ms = smoothed_latency,
                        "adaptive pool: scale down"
                    );
                }
                ScaleDecision::Stable => {}
            }
        }

        decision
    }

    /// Notify the controller that the worker count changed externally.
    pub async fn set_workers(&self, count: usize) {
        let mut state = self.state.lock().await;
        state.current_workers = count.clamp(self.config.min_workers, self.config.max_workers);
    }

    /// Return a statistics snapshot.
    pub async fn stats(&self) -> PoolStats {
        let state = self.state.lock().await;
        PoolStats {
            current_workers: state.current_workers,
            estimated_queue_depth: state.kalman.estimate(),
            latency_ema_ms: state.latency_ema.current(),
            total_scale_ups: self.total_scale_ups.load(Ordering::Relaxed),
            total_scale_downs: self.total_scale_downs.load(Ordering::Relaxed),
        }
    }

    /// Total number of observations fed.
    pub fn observation_count(&self) -> usize {
        self.observations.load(Ordering::Relaxed)
    }
}

/// Runs the adaptive pool evaluation loop as a Tokio background task.
///
/// Polls `queue_depth_fn` and `latency_fn` at `interval`, feeds observations
/// to the pool, and calls `on_decision` whenever a non-Stable decision is made.
///
/// Returns a [`tokio::task::JoinHandle`] for the background task.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::adaptive_pool::{AdaptivePool, AdaptivePoolConfig, ScaleDecision};
///
/// # async fn example() {
/// let pool = AdaptivePool::new(AdaptivePoolConfig::default(), 2);
/// let pool_clone = Arc::clone(&pool);
///
/// let handle = tokio_prompt_orchestrator::adaptive_pool::run_pool_controller(
///     Arc::clone(&pool),
///     Duration::from_millis(500),
///     || 10usize,   // queue_depth_fn
///     || 200.0f64,  // latency_ms_fn
///     |decision| Box::pin(async move {
///         match decision {
///             ScaleDecision::ScaleUp { by } => println!("Spawning {by} workers"),
///             ScaleDecision::ScaleDown { by } => println!("Draining {by} workers"),
///             ScaleDecision::Stable => {}
///         }
///     }),
/// );
/// # }
/// ```
pub fn run_pool_controller<QFn, LFn, OFn, OFut>(
    pool: Arc<AdaptivePool>,
    interval: Duration,
    queue_depth_fn: QFn,
    latency_fn: LFn,
    on_decision: OFn,
) -> tokio::task::JoinHandle<()>
where
    QFn: Fn() -> usize + Send + 'static,
    LFn: Fn() -> f64 + Send + 'static,
    OFn: Fn(ScaleDecision) -> OFut + Send + 'static,
    OFut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            let depth = queue_depth_fn();
            let latency = latency_fn();
            let decision = pool.evaluate(depth, latency).await;
            if decision != ScaleDecision::Stable {
                on_decision(decision).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kalman_converges() {
        let mut kf = KalmanFilter::new(1.0, 5.0);
        // Feed constant signal of 100
        for _ in 0..20 {
            kf.update(100.0);
        }
        // Should converge close to 100
        assert!(
            (kf.estimate() - 100.0).abs() < 5.0,
            "estimate={}, expected ~100",
            kf.estimate()
        );
    }

    #[test]
    fn test_kalman_tracks_step_change() {
        let mut kf = KalmanFilter::new(1.0, 5.0);
        for _ in 0..10 {
            kf.update(50.0);
        }
        for _ in 0..10 {
            kf.update(100.0);
        }
        // Should have moved toward 100
        assert!(kf.estimate() > 70.0, "estimate={}", kf.estimate());
    }

    #[test]
    fn test_latency_ema_initialises_to_first_sample() {
        let mut ema = LatencyEma::new(0.1);
        let v = ema.update(200.0);
        assert_eq!(v, 200.0);
    }

    #[test]
    fn test_latency_ema_blends_samples() {
        let mut ema = LatencyEma::new(0.5);
        ema.update(100.0);
        let v = ema.update(200.0);
        assert!((v - 150.0).abs() < 1.0, "v={v}");
    }

    #[tokio::test]
    async fn test_stable_when_within_thresholds() {
        let config = AdaptivePoolConfig {
            scale_up_threshold: 100.0,
            scale_down_threshold: 5.0,
            latency_threshold_ms: 500.0,
            min_workers: 1,
            max_workers: 16,
            cooldown: Duration::from_millis(0),
            ..Default::default()
        };
        let pool = AdaptivePool::new(config, 2);
        let decision = pool.evaluate(10, 100.0).await;
        assert_eq!(decision, ScaleDecision::Stable);
    }

    #[tokio::test]
    async fn test_scale_up_under_high_load() {
        let config = AdaptivePoolConfig {
            scale_up_threshold: 10.0,
            latency_threshold_ms: 100.0,
            min_workers: 1,
            max_workers: 16,
            cooldown: Duration::from_millis(0),
            ..Default::default()
        };
        let pool = AdaptivePool::new(config, 2);
        // Feed high load to warm up Kalman filter
        for _ in 0..5 {
            pool.evaluate(200, 1000.0).await;
        }
        // After warmup should see scale-up
        let stats = pool.stats().await;
        assert!(stats.current_workers > 2, "expected scale-up, got {} workers", stats.current_workers);
    }

    #[tokio::test]
    async fn test_cooldown_prevents_rapid_scaling() {
        let config = AdaptivePoolConfig {
            scale_up_threshold: 10.0,
            latency_threshold_ms: 50.0,
            cooldown: Duration::from_secs(60), // very long cooldown
            min_workers: 1,
            max_workers: 16,
            ..Default::default()
        };
        let pool = AdaptivePool::new(config, 2);
        let d1 = pool.evaluate(200, 1000.0).await;
        let d2 = pool.evaluate(200, 1000.0).await;
        // Second decision should be Stable due to cooldown
        if d1 != ScaleDecision::Stable {
            assert_eq!(d2, ScaleDecision::Stable, "cooldown should suppress second scale event");
        }
    }

    #[tokio::test]
    async fn test_scale_down_under_low_load() {
        let config = AdaptivePoolConfig {
            scale_up_threshold: 100.0,
            scale_down_threshold: 20.0,
            latency_threshold_ms: 500.0,
            min_workers: 1,
            max_workers: 16,
            cooldown: Duration::from_millis(0),
            ..Default::default()
        };
        let pool = AdaptivePool::new(config, 8);

        for _ in 0..10 {
            pool.evaluate(1, 10.0).await;
        }

        let stats = pool.stats().await;
        assert!(
            stats.current_workers < 8,
            "expected scale-down from 8, got {}",
            stats.current_workers
        );
    }

    #[tokio::test]
    async fn test_min_workers_respected() {
        let config = AdaptivePoolConfig {
            scale_down_threshold: 100.0, // always trigger scale-down
            latency_threshold_ms: 1000.0,
            min_workers: 3,
            max_workers: 16,
            cooldown: Duration::from_millis(0),
            ..Default::default()
        };
        let pool = AdaptivePool::new(config, 5);
        for _ in 0..20 {
            pool.evaluate(0, 0.0).await;
        }
        let stats = pool.stats().await;
        assert!(
            stats.current_workers >= 3,
            "must not go below min_workers=3, got {}",
            stats.current_workers
        );
    }
}
