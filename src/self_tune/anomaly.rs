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
//! # Anomaly Detection (Task 1.4)
//!
//! ## Responsibility
//! Detect statistical anomalies in telemetry streams using multiple detection
//! algorithms: Z-score for sudden spikes, CUSUM for gradual drift, and a simple
//! isolation score for multivariate outliers.
//!
//! ## Guarantees
//! - Thread-safe: all operations safe under concurrent access
//! - Non-blocking: detection runs in O(window_size) time
//! - Bounded memory: rolling windows cap sample retention
//! - Critical anomalies trigger rollback recommendations
//!
//! ## NOT Responsible For
//! - Executing rollbacks (that's the controller's job)
//! - Cross-node anomaly correlation (see evolution/ module)

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by the anomaly detection subsystem.
#[derive(Debug, Error)]
pub enum AnomalyError {
    /// An internal mutex was poisoned by a panicking thread.
    #[error("internal lock poisoned")]
    LockPoisoned,

    /// Not enough data points have been collected to run detection.
    #[error("insufficient data for metric '{metric}': have {have}, need {need}")]
    InsufficientData {
        /// The metric that lacks data.
        metric: String,
        /// Number of samples currently available.
        have: usize,
        /// Minimum number of samples required.
        need: usize,
    },

    /// The requested metric has never been ingested.
    #[error("metric not found: '{0}'")]
    MetricNotFound(String),
}

// ─── Enums ───────────────────────────────────────────────────────────────────

/// Severity classification for a detected anomaly.
///
/// Ordered from least to most severe so that `Ord` comparisons are meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    /// Mild deviation, informational only.
    Info,
    /// Notable deviation, worth monitoring.
    Warning,
    /// Severe deviation, recommend rollback.
    Critical,
}

/// The algorithm that detected an anomaly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectionMethod {
    /// Standard-deviation-based spike detection.
    ZScore,
    /// Cumulative sum control chart for gradual drift.
    Cusum,
    /// Multivariate outlier detection via averaged z-scores.
    IsolationScore,
}

// ─── Anomaly record ──────────────────────────────────────────────────────────

/// A single detected anomaly event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anomaly {
    /// Name of the metric on which the anomaly was detected.
    pub metric: String,
    /// The observed value that triggered detection.
    pub value: f64,
    /// Severity classification.
    pub severity: Severity,
    /// Algorithm that produced this detection.
    pub method: DetectionMethod,
    /// Detection score (z-score value, cusum value, or isolation score).
    pub score: f64,
    /// The threshold that was exceeded.
    pub threshold: f64,
    /// Human-readable description of the anomaly.
    pub message: String,
    /// Unix timestamp (seconds) when the anomaly was detected.
    pub detected_at_secs: u64,
}

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the Z-score detection algorithm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZScoreConfig {
    /// Number of recent samples to retain in the rolling window.
    pub window_size: usize,
    /// Z-score above which a [`Severity::Warning`] is raised.
    pub warning_threshold: f64,
    /// Z-score above which a [`Severity::Critical`] is raised.
    pub critical_threshold: f64,
}

impl Default for ZScoreConfig {
    fn default() -> Self {
        Self {
            window_size: 60,
            warning_threshold: 2.0,
            critical_threshold: 3.0,
        }
    }
}

/// Configuration for the CUSUM (cumulative sum) detection algorithm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CusumConfig {
    /// Allowable drift before flagging.
    pub drift_tolerance: f64,
    /// Cumulative sum threshold for raising an alarm.
    pub threshold: f64,
    /// Whether to reset the cumulative sums after an alarm fires.
    pub reset_on_alarm: bool,
}

impl Default for CusumConfig {
    fn default() -> Self {
        Self {
            drift_tolerance: 0.5,
            threshold: 5.0,
            reset_on_alarm: true,
        }
    }
}

/// Top-level configuration for the [`AnomalyDetector`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyDetectorConfig {
    /// Z-score algorithm configuration.
    pub z_score: ZScoreConfig,
    /// CUSUM algorithm configuration.
    pub cusum: CusumConfig,
    /// Whether the multivariate isolation score algorithm is enabled.
    pub isolation_enabled: bool,
    /// Isolation score threshold above which an anomaly is raised.
    pub isolation_threshold: f64,
    /// Maximum number of samples retained per metric window.
    pub max_window_size: usize,
}

impl Default for AnomalyDetectorConfig {
    fn default() -> Self {
        Self {
            z_score: ZScoreConfig::default(),
            cusum: CusumConfig::default(),
            isolation_enabled: true,
            isolation_threshold: 2.5,
            max_window_size: 120,
        }
    }
}

// ─── Internal state ──────────────────────────────────────────────────────────

/// Per-metric CUSUM accumulator state.
#[derive(Debug, Clone)]
struct CusumState {
    /// Cumulative sum tracking upward shifts.
    high: f64,
    /// Cumulative sum tracking downward shifts.
    low: f64,
}

impl CusumState {
    fn new() -> Self {
        Self {
            high: 0.0,
            low: 0.0,
        }
    }
}

/// Inner mutable state protected by a mutex.
#[derive(Debug)]
struct DetectorInner {
    config: AnomalyDetectorConfig,
    windows: HashMap<String, VecDeque<f64>>,
    cusum_states: HashMap<String, CusumState>,
    anomaly_history: Vec<Anomaly>,
    max_history: usize,
}

// ─── AnomalyDetector ─────────────────────────────────────────────────────────

/// Thread-safe statistical anomaly detector for telemetry streams.
///
/// Uses Z-score, CUSUM, and multivariate isolation scoring to detect anomalies
/// across named metrics.  Cloning this struct produces a handle that shares the
/// same internal state (via `Arc<Mutex<_>>`).
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::self_tune::anomaly::{AnomalyDetector, AnomalyDetectorConfig};
///
/// let detector = AnomalyDetector::new(AnomalyDetectorConfig::default());
/// // Ingest normal data
/// for _ in 0..10 {
///     let _ = detector.ingest("cpu", 50.0);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct AnomalyDetector {
    inner: Arc<Mutex<DetectorInner>>,
}

impl AnomalyDetector {
    /// Create a new anomaly detector with the given configuration.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: AnomalyDetectorConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DetectorInner {
                config,
                windows: HashMap::new(),
                cusum_states: HashMap::new(),
                anomaly_history: Vec::new(),
                max_history: 10_000,
            })),
        }
    }

    /// Create a new anomaly detector with default configuration.
    ///
    /// # Panics
    /// This function never panics.
    pub fn with_defaults() -> Self {
        Self::new(AnomalyDetectorConfig::default())
    }

    /// Ingest a single metric value, run all enabled detectors, and return any
    /// anomalies found.
    ///
    /// The value is appended to the metric's rolling window (bounded by
    /// `max_window_size`).  Z-score, CUSUM, and isolation score detectors each
    /// evaluate the new value independently.
    ///
    /// # Arguments
    /// * `metric` — Name of the metric being observed
    /// * `value` — The observed numeric value
    ///
    /// # Returns
    /// A vector of detected anomalies (possibly empty).
    ///
    /// # Errors
    /// Returns [`AnomalyError::LockPoisoned`] if the internal mutex is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn ingest(&self, metric: &str, value: f64) -> Result<Vec<Anomaly>, AnomalyError> {
        let mut inner = self.inner.lock().map_err(|_| AnomalyError::LockPoisoned)?;
        let anomalies = Self::ingest_inner(&mut inner, metric, value);
        Ok(anomalies)
    }

    /// Ingest multiple metrics at once, returning all anomalies found across
    /// every metric.
    ///
    /// # Arguments
    /// * `metrics` — Map of metric names to observed values
    ///
    /// # Returns
    /// A vector of all detected anomalies across all metrics.
    ///
    /// # Errors
    /// Returns [`AnomalyError::LockPoisoned`] if the internal mutex is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn ingest_batch(
        &self,
        metrics: &HashMap<String, f64>,
    ) -> Result<Vec<Anomaly>, AnomalyError> {
        let mut inner = self.inner.lock().map_err(|_| AnomalyError::LockPoisoned)?;
        let mut all_anomalies = Vec::new();

        for (metric, &value) in metrics {
            let anomalies = Self::ingest_inner(&mut inner, metric, value);
            all_anomalies.extend(anomalies);
        }

        // Check isolation score across all metrics if enabled
        if inner.config.isolation_enabled {
            let iso_score = Self::compute_isolation_score(&inner.windows, metrics);
            let threshold = inner.config.isolation_threshold;
            if iso_score > threshold {
                let now = unix_now();
                let severity = if iso_score > threshold * 1.5 {
                    Severity::Critical
                } else {
                    Severity::Warning
                };
                let anomaly = Anomaly {
                    metric: "_multivariate_".to_string(),
                    value: iso_score,
                    severity,
                    method: DetectionMethod::IsolationScore,
                    score: iso_score,
                    threshold,
                    message: format!(
                        "Multivariate isolation score {iso_score:.4} exceeds threshold {threshold:.4}"
                    ),
                    detected_at_secs: now,
                };
                inner.anomaly_history.push(anomaly.clone());
                Self::cap_history(&mut inner);
                all_anomalies.push(anomaly);
            }
        }

        Ok(all_anomalies)
    }

    /// Compute the Z-score for a value given a window of samples.
    ///
    /// Returns `None` if the window has fewer than 2 samples or if the standard
    /// deviation is zero (all values identical).
    ///
    /// # Arguments
    /// * `window` — Rolling window of recent samples
    /// * `value` — The new observation to score
    /// * `config` — Z-score algorithm configuration
    ///
    /// # Returns
    /// `Some((z_score, severity))` if an anomaly is detected, `None` otherwise.
    ///
    /// # Panics
    /// This function never panics.
    pub fn check_z_score(
        window: &VecDeque<f64>,
        value: f64,
        config: &ZScoreConfig,
    ) -> Option<(f64, Severity)> {
        if window.len() < 2 {
            return None;
        }

        let (mean, stddev) = window_mean_stddev(window);

        if stddev < f64::EPSILON {
            return None;
        }

        let z = (value - mean).abs() / stddev;

        if z > config.critical_threshold {
            Some((z, Severity::Critical))
        } else if z > config.warning_threshold {
            Some((z, Severity::Warning))
        } else {
            None
        }
    }

    /// Run the CUSUM algorithm against the current state for a metric.
    ///
    /// Maintains cumulative high/low sums and triggers when either exceeds the
    /// configured threshold.
    ///
    /// # Arguments
    /// * `state` — Mutable CUSUM accumulator state
    /// * `value` — The new observation
    /// * `config` — CUSUM algorithm configuration
    ///
    /// # Returns
    /// `Some((cusum_value, severity))` if an anomaly is detected, `None` otherwise.
    ///
    /// # Panics
    /// This function never panics.
    fn check_cusum(
        state: &mut CusumState,
        value: f64,
        mean: f64,
        config: &CusumConfig,
    ) -> Option<(f64, Severity)> {
        let target = mean;

        state.high += (value - target) - config.drift_tolerance;
        state.low += (target - value) - config.drift_tolerance;
        state.high = state.high.max(0.0);
        state.low = state.low.max(0.0);

        let max_cusum = state.high.max(state.low);

        if max_cusum > config.threshold {
            let severity = if max_cusum > config.threshold * 2.0 {
                Severity::Critical
            } else {
                Severity::Warning
            };

            if config.reset_on_alarm {
                state.high = 0.0;
                state.low = 0.0;
            }

            Some((max_cusum, severity))
        } else {
            None
        }
    }

    /// Compute a simple multivariate isolation score.
    ///
    /// Calculates the average absolute Z-score across all provided metrics
    /// using their respective rolling windows.  Metrics without enough data
    /// are skipped.
    ///
    /// # Arguments
    /// * `windows` — All metric rolling windows
    /// * `metrics` — Current metric values to score
    ///
    /// # Returns
    /// The average Z-score across all scoreable metrics, or `0.0` if none are
    /// scoreable.
    ///
    /// # Panics
    /// This function never panics.
    pub fn compute_isolation_score(
        windows: &HashMap<String, VecDeque<f64>>,
        metrics: &HashMap<String, f64>,
    ) -> f64 {
        let mut total_z = 0.0;
        let mut count = 0usize;

        for (name, &value) in metrics {
            if let Some(window) = windows.get(name) {
                if window.len() >= 2 {
                    let (mean, stddev) = window_mean_stddev(window);
                    if stddev > f64::EPSILON {
                        let z = (value - mean).abs() / stddev;
                        total_z += z;
                        count += 1;
                    }
                }
            }
        }

        if count == 0 {
            0.0
        } else {
            total_z / count as f64
        }
    }

    /// Return a clone of the full anomaly history.
    ///
    /// # Panics
    /// This function never panics.
    pub fn anomaly_history(&self) -> Vec<Anomaly> {
        self.inner
            .lock()
            .map(|inner| inner.anomaly_history.clone())
            .unwrap_or_default()
    }

    /// Return the most recent `n` anomalies.
    ///
    /// If fewer than `n` anomalies exist, all are returned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn recent_anomalies(&self, n: usize) -> Vec<Anomaly> {
        self.inner
            .lock()
            .map(|inner| {
                let len = inner.anomaly_history.len();
                let start = len.saturating_sub(n);
                inner.anomaly_history[start..].to_vec()
            })
            .unwrap_or_default()
    }

    /// Return only anomalies with [`Severity::Critical`].
    ///
    /// # Panics
    /// This function never panics.
    pub fn critical_anomalies(&self) -> Vec<Anomaly> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .anomaly_history
                    .iter()
                    .filter(|a| a.severity == Severity::Critical)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return the (mean, stddev) for a metric's current rolling window.
    ///
    /// Returns `None` if the metric has not been ingested.
    ///
    /// # Panics
    /// This function never panics.
    pub fn window_stats(&self, metric: &str) -> Option<(f64, f64)> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.windows.get(metric).map(window_mean_stddev))
    }

    /// Clear all recorded anomaly history.
    ///
    /// # Errors
    /// Returns [`AnomalyError::LockPoisoned`] if the internal mutex is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn clear_history(&self) -> Result<(), AnomalyError> {
        let mut inner = self.inner.lock().map_err(|_| AnomalyError::LockPoisoned)?;
        inner.anomaly_history.clear();
        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Core ingestion logic operating on the already-locked inner state.
    fn ingest_inner(inner: &mut DetectorInner, metric: &str, value: f64) -> Vec<Anomaly> {
        let mut anomalies = Vec::new();
        let now = unix_now();

        // Update rolling window
        let max_window = inner
            .config
            .max_window_size
            .max(inner.config.z_score.window_size);
        let window = inner.windows.entry(metric.to_string()).or_default();

        // Z-score detection (before pushing the new value)
        let z_config = inner.config.z_score.clone();
        if let Some((z, severity)) = Self::check_z_score(window, value, &z_config) {
            let anomaly = Anomaly {
                metric: metric.to_string(),
                value,
                severity,
                method: DetectionMethod::ZScore,
                score: z,
                threshold: if severity == Severity::Critical {
                    z_config.critical_threshold
                } else {
                    z_config.warning_threshold
                },
                message: format!("Z-score {z:.4} on metric '{metric}' (value={value:.4})"),
                detected_at_secs: now,
            };
            inner.anomaly_history.push(anomaly.clone());
            anomalies.push(anomaly);
        }

        // CUSUM detection — skip until the window has at least 5 samples so the
        // mean estimate is meaningful and the cold-start spike is avoided.
        let cusum_config = inner.config.cusum.clone();
        let (mean, _) = window_mean_stddev(window);
        let cusum_state = inner
            .cusum_states
            .entry(metric.to_string())
            .or_insert_with(CusumState::new);
        let cusum_result = if window.len() >= 5 {
            Self::check_cusum(cusum_state, value, mean, &cusum_config)
        } else {
            None
        };
        if let Some((cusum_val, severity)) = cusum_result {
            let anomaly = Anomaly {
                metric: metric.to_string(),
                value,
                severity,
                method: DetectionMethod::Cusum,
                score: cusum_val,
                threshold: cusum_config.threshold,
                message: format!("CUSUM {cusum_val:.4} on metric '{metric}' (value={value:.4})"),
                detected_at_secs: now,
            };
            inner.anomaly_history.push(anomaly.clone());
            anomalies.push(anomaly);
        }

        // Push value into window (after detection so the value doesn't score itself)
        window.push_back(value);
        while window.len() > max_window {
            window.pop_front();
        }

        Self::cap_history(inner);

        anomalies
    }

    /// Trim anomaly history to `max_history` entries.
    fn cap_history(inner: &mut DetectorInner) {
        let max = inner.max_history;
        if inner.anomaly_history.len() > max {
            let excess = inner.anomaly_history.len() - max;
            inner.anomaly_history.drain(..excess);
        }
    }
}

// ─── Free helpers ────────────────────────────────────────────────────────────

/// Compute mean and population standard deviation of a window.
///
/// Returns `(0.0, 0.0)` for an empty window.
fn window_mean_stddev(window: &VecDeque<f64>) -> (f64, f64) {
    let n = window.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let mean = window.iter().sum::<f64>() / n as f64;
    let variance = window.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
    (mean, variance.sqrt())
}

/// Return the current Unix timestamp in seconds, or `0` if the system clock is
/// before the epoch (should never happen in practice).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ─────────────────────────────────────────────────────────

    #[test]
    fn test_new_creates_detector() {
        let det = AnomalyDetector::new(AnomalyDetectorConfig::default());
        let inner = det.inner.lock().unwrap();
        assert!(inner.windows.is_empty());
        assert!(inner.cusum_states.is_empty());
        assert!(inner.anomaly_history.is_empty());
    }

    #[test]
    fn test_with_defaults() {
        let det = AnomalyDetector::with_defaults();
        let inner = det.inner.lock().unwrap();
        assert_eq!(inner.config.z_score.window_size, 60);
        assert_eq!(inner.config.cusum.threshold, 5.0);
        assert!(inner.config.isolation_enabled);
    }

    // ── Ingestion / Z-score ──────────────────────────────────────────────────

    #[test]
    fn test_ingest_no_anomaly_normal_data() {
        // Feed constant values: no z-score, no CUSUM drift, no isolation.
        //
        // CUSUM needs a high threshold because on the first ingestion the window
        // is empty (mean=0), causing a large one-time apparent deviation.
        // Setting threshold=10_000 ensures initialisation is not flagged.
        let config = AnomalyDetectorConfig {
            cusum: CusumConfig {
                drift_tolerance: 0.1,
                threshold: 10_000.0,
                reset_on_alarm: true,
            },
            isolation_enabled: false,
            ..Default::default()
        };
        let det = AnomalyDetector::new(config);
        for _ in 0..20 {
            let result = det.ingest("cpu", 50.0);
            assert!(result.is_ok());
            assert!(result.unwrap().is_empty());
        }
    }

    #[test]
    fn test_ingest_z_score_warning() {
        let config = AnomalyDetectorConfig {
            z_score: ZScoreConfig {
                window_size: 60,
                warning_threshold: 2.0,
                critical_threshold: 3.0,
            },
            cusum: CusumConfig {
                drift_tolerance: 100.0, // high to suppress cusum
                threshold: 1000.0,
                reset_on_alarm: true,
            },
            isolation_enabled: false,
            ..Default::default()
        };
        let det = AnomalyDetector::new(config);

        // Build a stable window
        for _ in 0..30 {
            let _ = det.ingest("latency", 100.0);
        }
        // Add slight variance
        for _ in 0..10 {
            let _ = det.ingest("latency", 101.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("latency", 99.0);
        }

        // Spike that should trigger warning but not critical
        let result = det.ingest("latency", 106.0).unwrap();
        // Check if any anomaly was warning-level via z-score
        let has_zscore_warning = result
            .iter()
            .any(|a| a.method == DetectionMethod::ZScore && a.severity >= Severity::Warning);
        assert!(
            has_zscore_warning,
            "expected z-score warning, got: {result:?}"
        );
    }

    #[test]
    fn test_ingest_z_score_critical() {
        let config = AnomalyDetectorConfig {
            z_score: ZScoreConfig {
                window_size: 60,
                warning_threshold: 2.0,
                critical_threshold: 3.0,
            },
            cusum: CusumConfig {
                drift_tolerance: 1000.0,
                threshold: 10000.0,
                reset_on_alarm: true,
            },
            isolation_enabled: false,
            ..Default::default()
        };
        let det = AnomalyDetector::new(config);

        // Build window with some variance
        for _ in 0..30 {
            let _ = det.ingest("mem", 50.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("mem", 51.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("mem", 49.0);
        }

        // Extreme spike
        let result = det.ingest("mem", 200.0).unwrap();
        let has_critical = result
            .iter()
            .any(|a| a.method == DetectionMethod::ZScore && a.severity == Severity::Critical);
        assert!(has_critical, "expected z-score critical, got: {result:?}");
    }

    #[test]
    fn test_z_score_insufficient_data_no_anomaly() {
        let det = AnomalyDetector::with_defaults();
        // Only one sample — not enough for z-score
        let result = det.ingest("new_metric", 999.0).unwrap();
        let z_anomalies: Vec<_> = result
            .iter()
            .filter(|a| a.method == DetectionMethod::ZScore)
            .collect();
        assert!(
            z_anomalies.is_empty(),
            "z-score should not fire with <2 samples"
        );
    }

    #[test]
    fn test_z_score_zero_stddev_no_anomaly() {
        let det = AnomalyDetector::with_defaults();
        // All identical values → stddev = 0
        for _ in 0..10 {
            let _ = det.ingest("constant", 42.0);
        }
        let result = det.ingest("constant", 42.0).unwrap();
        let z_anomalies: Vec<_> = result
            .iter()
            .filter(|a| a.method == DetectionMethod::ZScore)
            .collect();
        assert!(
            z_anomalies.is_empty(),
            "z-score should not fire when stddev is zero"
        );
    }

    #[test]
    fn test_z_score_returns_correct_severity() {
        let config = ZScoreConfig {
            window_size: 60,
            warning_threshold: 2.0,
            critical_threshold: 3.0,
        };
        // Window with mean=0, stddev~=1
        let mut window: VecDeque<f64> = VecDeque::new();
        for i in 0..1000 {
            if i % 2 == 0 {
                window.push_back(1.0);
            } else {
                window.push_back(-1.0);
            }
        }

        // Value 2.5 stddevs away → Warning
        let result = AnomalyDetector::check_z_score(&window, 2.5, &config);
        assert!(result.is_some());
        let (_, severity) = result.unwrap();
        assert_eq!(severity, Severity::Warning);

        // Value 4.0 stddevs away → Critical
        let result = AnomalyDetector::check_z_score(&window, 4.0, &config);
        assert!(result.is_some());
        let (_, severity) = result.unwrap();
        assert_eq!(severity, Severity::Critical);

        // Value 1.0 stddev away → no anomaly
        let result = AnomalyDetector::check_z_score(&window, 1.0, &config);
        assert!(result.is_none());
    }

    // ── CUSUM ────────────────────────────────────────────────────────────────

    #[test]
    fn test_cusum_detects_drift() {
        let config = CusumConfig {
            drift_tolerance: 0.5,
            threshold: 5.0,
            reset_on_alarm: true,
        };
        let mut state = CusumState::new();
        let mean = 10.0;

        // Feed sustained upward drift
        let mut detected = false;
        for _ in 0..50 {
            if AnomalyDetector::check_cusum(&mut state, 15.0, mean, &config).is_some() {
                detected = true;
                break;
            }
        }
        assert!(detected, "CUSUM should detect sustained drift");
    }

    #[test]
    fn test_cusum_no_drift_normal_data() {
        let config = CusumConfig {
            drift_tolerance: 0.5,
            threshold: 5.0,
            reset_on_alarm: true,
        };
        let mut state = CusumState::new();
        let mean = 10.0;

        // Feed values close to the mean — should not alarm
        for _ in 0..100 {
            let result = AnomalyDetector::check_cusum(&mut state, 10.0, mean, &config);
            assert!(result.is_none(), "stable values should not trigger CUSUM");
        }
    }

    #[test]
    fn test_cusum_reset_on_alarm() {
        let config = CusumConfig {
            drift_tolerance: 0.5,
            threshold: 5.0,
            reset_on_alarm: true,
        };
        let mut state = CusumState::new();

        // Trigger alarm
        for _ in 0..50 {
            let _ = AnomalyDetector::check_cusum(&mut state, 20.0, 10.0, &config);
        }

        // After alarm with reset, state should be back to 0
        assert!(
            state.high < f64::EPSILON && state.low < f64::EPSILON,
            "CUSUM should reset after alarm, got high={} low={}",
            state.high,
            state.low
        );
    }

    #[test]
    fn test_cusum_no_reset_when_disabled() {
        let config = CusumConfig {
            drift_tolerance: 0.5,
            threshold: 5.0,
            reset_on_alarm: false,
        };
        let mut state = CusumState::new();

        // Trigger alarm
        let mut alarmed = false;
        for _ in 0..50 {
            if AnomalyDetector::check_cusum(&mut state, 20.0, 10.0, &config).is_some() {
                alarmed = true;
                break;
            }
        }
        assert!(alarmed);

        // State should NOT be reset
        // Feed another large deviation — cusum should still be elevated or accumulate
        let _ = AnomalyDetector::check_cusum(&mut state, 20.0, 10.0, &config);
        // high should be non-zero (accumulated, not reset)
        assert!(
            state.high > 0.0 || state.low > 0.0,
            "CUSUM state should not reset when reset_on_alarm is false"
        );
    }

    // ── Isolation score ──────────────────────────────────────────────────────

    #[test]
    fn test_isolation_score_computes() {
        let mut windows: HashMap<String, VecDeque<f64>> = HashMap::new();

        // Build windows with known distributions
        let mut w1 = VecDeque::new();
        for _ in 0..100 {
            w1.push_back(10.0);
        }
        w1.push_back(11.0); // slight variance
        windows.insert("metric_a".to_string(), w1);

        let mut w2 = VecDeque::new();
        for _ in 0..100 {
            w2.push_back(20.0);
        }
        w2.push_back(21.0);
        windows.insert("metric_b".to_string(), w2);

        // Values at the mean → low score
        let mut metrics = HashMap::new();
        metrics.insert("metric_a".to_string(), 10.0);
        metrics.insert("metric_b".to_string(), 20.0);

        let score = AnomalyDetector::compute_isolation_score(&windows, &metrics);
        assert!(score < 1.0, "mean values should have low isolation score");

        // Outlier values → higher score
        metrics.insert("metric_a".to_string(), 100.0);
        metrics.insert("metric_b".to_string(), 200.0);
        let score = AnomalyDetector::compute_isolation_score(&windows, &metrics);
        assert!(
            score > 1.0,
            "outlier values should have higher isolation score, got {score}"
        );
    }

    // ── Batch ingestion ──────────────────────────────────────────────────────

    #[test]
    fn test_ingest_batch_processes_all() {
        let det = AnomalyDetector::with_defaults();
        let mut metrics = HashMap::new();
        metrics.insert("cpu".to_string(), 50.0);
        metrics.insert("mem".to_string(), 70.0);
        metrics.insert("disk".to_string(), 30.0);

        let result = det.ingest_batch(&metrics);
        assert!(result.is_ok());

        // All three metrics should have windows
        let inner = det.inner.lock().unwrap();
        assert!(inner.windows.contains_key("cpu"));
        assert!(inner.windows.contains_key("mem"));
        assert!(inner.windows.contains_key("disk"));
    }

    // ── History ──────────────────────────────────────────────────────────────

    #[test]
    fn test_anomaly_history_records() {
        let config = AnomalyDetectorConfig {
            z_score: ZScoreConfig {
                window_size: 60,
                warning_threshold: 2.0,
                critical_threshold: 3.0,
            },
            cusum: CusumConfig {
                drift_tolerance: 1000.0,
                threshold: 10000.0,
                reset_on_alarm: true,
            },
            isolation_enabled: false,
            ..Default::default()
        };
        let det = AnomalyDetector::new(config);

        // Build window with variance
        for _ in 0..30 {
            let _ = det.ingest("x", 10.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("x", 11.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("x", 9.0);
        }

        // Trigger anomaly
        let _ = det.ingest("x", 500.0);

        let history = det.anomaly_history();
        assert!(
            !history.is_empty(),
            "anomaly history should contain detected anomalies"
        );
    }

    #[test]
    fn test_recent_anomalies_limits() {
        let config = AnomalyDetectorConfig {
            z_score: ZScoreConfig {
                window_size: 60,
                warning_threshold: 2.0,
                critical_threshold: 3.0,
            },
            cusum: CusumConfig {
                drift_tolerance: 1000.0,
                threshold: 10000.0,
                reset_on_alarm: true,
            },
            isolation_enabled: false,
            ..Default::default()
        };
        let det = AnomalyDetector::new(config);

        // Build stable window then inject spikes
        for _ in 0..30 {
            let _ = det.ingest("y", 10.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("y", 11.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("y", 9.0);
        }

        // Multiple spikes to generate multiple anomalies
        for _ in 0..5 {
            let _ = det.ingest("y", 500.0);
        }

        let recent = det.recent_anomalies(2);
        assert!(
            recent.len() <= 2,
            "recent_anomalies(2) should return at most 2"
        );
    }

    #[test]
    fn test_critical_anomalies_filters() {
        let config = AnomalyDetectorConfig {
            z_score: ZScoreConfig {
                window_size: 60,
                warning_threshold: 2.0,
                critical_threshold: 3.0,
            },
            cusum: CusumConfig {
                drift_tolerance: 1000.0,
                threshold: 10000.0,
                reset_on_alarm: true,
            },
            isolation_enabled: false,
            ..Default::default()
        };
        let det = AnomalyDetector::new(config);

        for _ in 0..30 {
            let _ = det.ingest("z", 10.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("z", 11.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("z", 9.0);
        }

        let _ = det.ingest("z", 1000.0);

        let critical = det.critical_anomalies();
        for a in &critical {
            assert_eq!(a.severity, Severity::Critical);
        }
    }

    // ── Window stats ─────────────────────────────────────────────────────────

    #[test]
    fn test_window_stats_returns_mean_stddev() {
        let det = AnomalyDetector::with_defaults();
        for v in &[10.0, 20.0, 30.0] {
            let _ = det.ingest("stat_metric", *v);
        }

        let stats = det.window_stats("stat_metric");
        assert!(stats.is_some());
        let (mean, stddev) = stats.unwrap();
        // mean of [10, 20, 30] = 20
        assert!((mean - 20.0).abs() < 1e-10);
        // stddev of [10, 20, 30] (population) = sqrt(200/3) ≈ 8.165
        assert!(stddev > 8.0 && stddev < 8.5, "stddev={stddev}");
    }

    #[test]
    fn test_window_stats_unknown_metric() {
        let det = AnomalyDetector::with_defaults();
        assert!(det.window_stats("nonexistent").is_none());
    }

    // ── Clear history ────────────────────────────────────────────────────────

    #[test]
    fn test_clear_history() {
        let det = AnomalyDetector::with_defaults();

        // Manually push an anomaly into history
        {
            let mut inner = det.inner.lock().unwrap();
            inner.anomaly_history.push(Anomaly {
                metric: "test".to_string(),
                value: 1.0,
                severity: Severity::Info,
                method: DetectionMethod::ZScore,
                score: 1.0,
                threshold: 2.0,
                message: "test".to_string(),
                detected_at_secs: 0,
            });
        }

        assert_eq!(det.anomaly_history().len(), 1);
        det.clear_history().unwrap();
        assert!(det.anomaly_history().is_empty());
    }

    // ── Window bounding ──────────────────────────────────────────────────────

    #[test]
    fn test_window_bounded_by_max() {
        let config = AnomalyDetectorConfig {
            max_window_size: 10,
            z_score: ZScoreConfig {
                window_size: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        let det = AnomalyDetector::new(config);

        for i in 0..100 {
            let _ = det.ingest("bounded", i as f64);
        }

        let inner = det.inner.lock().unwrap();
        let window = inner.windows.get("bounded").unwrap();
        assert!(
            window.len() <= 10,
            "window should be bounded, got len={}",
            window.len()
        );
    }

    // ── Severity ordering ────────────────────────────────────────────────────

    #[test]
    fn test_severity_ordering() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Critical);
        assert!(Severity::Info < Severity::Critical);
    }

    // ── Serialization ────────────────────────────────────────────────────────

    #[test]
    fn test_anomaly_serialization() {
        let anomaly = Anomaly {
            metric: "cpu".to_string(),
            value: 99.5,
            severity: Severity::Critical,
            method: DetectionMethod::ZScore,
            score: 4.2,
            threshold: 3.0,
            message: "high cpu".to_string(),
            detected_at_secs: 1700000000,
        };

        let json = serde_json::to_string(&anomaly).unwrap();
        let deserialized: Anomaly = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.metric, "cpu");
        assert_eq!(deserialized.severity, Severity::Critical);
        assert_eq!(deserialized.method, DetectionMethod::ZScore);
        assert!((deserialized.score - 4.2).abs() < f64::EPSILON);
    }

    #[test]
    fn test_detection_method_serialization() {
        let methods = [
            DetectionMethod::ZScore,
            DetectionMethod::Cusum,
            DetectionMethod::IsolationScore,
        ];
        for method in &methods {
            let json = serde_json::to_string(method).unwrap();
            let deserialized: DetectionMethod = serde_json::from_str(&json).unwrap();
            assert_eq!(*method, deserialized);
        }
    }

    // ── Config defaults ──────────────────────────────────────────────────────

    #[test]
    fn test_config_default_values() {
        let config = AnomalyDetectorConfig::default();
        assert_eq!(config.z_score.window_size, 60);
        assert!((config.z_score.warning_threshold - 2.0).abs() < f64::EPSILON);
        assert!((config.z_score.critical_threshold - 3.0).abs() < f64::EPSILON);
        assert!((config.cusum.drift_tolerance - 0.5).abs() < f64::EPSILON);
        assert!((config.cusum.threshold - 5.0).abs() < f64::EPSILON);
        assert!(config.cusum.reset_on_alarm);
        assert!(config.isolation_enabled);
        assert!((config.isolation_threshold - 2.5).abs() < f64::EPSILON);
        assert_eq!(config.max_window_size, 120);
    }

    // ── Clone shares state ───────────────────────────────────────────────────

    #[test]
    fn test_clone_shares_state() {
        let det1 = AnomalyDetector::with_defaults();
        let det2 = det1.clone();

        let _ = det1.ingest("shared_metric", 42.0);

        // det2 should see the same window
        let inner = det2.inner.lock().unwrap();
        assert!(inner.windows.contains_key("shared_metric"));
    }

    // ── Multiple metrics independent ─────────────────────────────────────────

    #[test]
    fn test_multiple_metrics_independent() {
        let det = AnomalyDetector::with_defaults();

        for _ in 0..10 {
            let _ = det.ingest("alpha", 100.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("beta", 200.0);
        }

        let inner = det.inner.lock().unwrap();
        let alpha_window = inner.windows.get("alpha").unwrap();
        let beta_window = inner.windows.get("beta").unwrap();

        // Windows should be independent
        let alpha_mean: f64 = alpha_window.iter().sum::<f64>() / alpha_window.len() as f64;
        let beta_mean: f64 = beta_window.iter().sum::<f64>() / beta_window.len() as f64;

        assert!((alpha_mean - 100.0).abs() < f64::EPSILON);
        assert!((beta_mean - 200.0).abs() < f64::EPSILON);
    }

    // ── Gradual drift vs spike ───────────────────────────────────────────────

    #[test]
    fn test_gradual_drift_detected_by_cusum_not_zscore() {
        let config = AnomalyDetectorConfig {
            z_score: ZScoreConfig {
                window_size: 60,
                warning_threshold: 2.0,
                critical_threshold: 3.0,
            },
            cusum: CusumConfig {
                drift_tolerance: 0.5,
                threshold: 5.0,
                reset_on_alarm: true,
            },
            isolation_enabled: false,
            ..Default::default()
        };
        let det = AnomalyDetector::new(config);

        // Build stable baseline
        for _ in 0..50 {
            let _ = det.ingest("drift", 10.0);
        }

        // Gradual drift: small increments that individually are within z-score
        // but accumulate in CUSUM
        let mut cusum_detected = false;
        let mut zscore_detected = false;
        for i in 0..50 {
            let value = 10.0 + (i as f64) * 0.2; // slow drift upward
            let result = det.ingest("drift", value).unwrap();
            for a in &result {
                if a.method == DetectionMethod::Cusum {
                    cusum_detected = true;
                }
                if a.method == DetectionMethod::ZScore {
                    zscore_detected = true;
                }
            }
        }

        // CUSUM should catch drift; z-score may or may not depending on window
        // The main assertion is that CUSUM detects it
        assert!(cusum_detected, "CUSUM should detect gradual drift");
        // Note: z-score may also eventually detect as drift becomes large enough
        // relative to the expanding window, so we don't assert !zscore_detected
        let _ = zscore_detected; // suppress unused warning
    }

    #[test]
    fn test_spike_detected_by_zscore() {
        let config = AnomalyDetectorConfig {
            z_score: ZScoreConfig {
                window_size: 60,
                warning_threshold: 2.0,
                critical_threshold: 3.0,
            },
            cusum: CusumConfig {
                drift_tolerance: 1000.0, // suppress cusum
                threshold: 10000.0,
                reset_on_alarm: true,
            },
            isolation_enabled: false,
            ..Default::default()
        };
        let det = AnomalyDetector::new(config);

        // Build window with slight variance
        for _ in 0..30 {
            let _ = det.ingest("spike", 10.0);
        }
        for _ in 0..10 {
            let _ = det.ingest("spike", 10.5);
        }
        for _ in 0..10 {
            let _ = det.ingest("spike", 9.5);
        }

        // Sudden spike
        let result = det.ingest("spike", 500.0).unwrap();
        let zscore_detected = result.iter().any(|a| a.method == DetectionMethod::ZScore);
        assert!(zscore_detected, "Z-score should detect sudden spike");
    }

    // ── History cap ──────────────────────────────────────────────────────────

    #[test]
    fn test_history_capped() {
        let det = AnomalyDetector::with_defaults();

        // Set a low cap for testing
        {
            let mut inner = det.inner.lock().unwrap();
            inner.max_history = 5;
        }

        // Insert many anomalies directly
        {
            let mut inner = det.inner.lock().unwrap();
            for i in 0..20 {
                inner.anomaly_history.push(Anomaly {
                    metric: format!("m{i}"),
                    value: i as f64,
                    severity: Severity::Info,
                    method: DetectionMethod::ZScore,
                    score: 1.0,
                    threshold: 2.0,
                    message: format!("test {i}"),
                    detected_at_secs: i as u64,
                });
            }
            AnomalyDetector::cap_history(&mut inner);
        }

        let history = det.anomaly_history();
        assert!(
            history.len() <= 5,
            "history should be capped at max_history, got {}",
            history.len()
        );
        // Most recent entries should be preserved
        assert_eq!(history.last().unwrap().detected_at_secs, 19);
    }
}
