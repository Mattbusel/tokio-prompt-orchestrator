//! # HelixRouter Configuration
//!
//! ## Responsibility
//! Define all tuneable parameters for the HelixRouter: strategy thresholds,
//! backpressure settings, batch limits, adaptive behaviour, and pressure
//! scoring weights.  Supports hot-reload via `tokio::sync::watch`.
//!
//! ## Guarantees
//! - All fields have sensible defaults via `Default`.
//! - Serde (de)serialisable for TOML/JSON config files.
//! - Validation function catches all invalid states before use.
//! - Hot-reload preserves base config on each reload.
//!
//! ## NOT Responsible For
//! - Applying the config (see `router`)
//! - File watching (uses existing `config::watcher` infra)

use serde::{Deserialize, Serialize};

// ── Default value functions ────────────────────────────────────────────

/// Default cost threshold below which jobs execute inline.
fn default_inline_threshold() -> u64 {
    50
}

/// Default cost threshold above which jobs get spawned as tasks.
fn default_spawn_threshold() -> u64 {
    200
}

/// Default cost threshold above which jobs go to the CPU pool.
fn default_cpu_pool_threshold() -> u64 {
    500
}

/// Default number of CPU-pool worker threads.
fn default_cpu_parallelism() -> usize {
    4
}

/// Default number of busy CPU workers that triggers backpressure.
fn default_backpressure_busy_threshold() -> usize {
    3
}

/// Default batch size before flushing.
fn default_batch_size() -> usize {
    10
}

/// Default batch flush timeout in milliseconds.
fn default_batch_timeout_ms() -> u64 {
    100
}

/// Default EMA smoothing factor.
fn default_ema_alpha() -> f64 {
    0.3
}

/// Default adaptive threshold enabled state.
fn default_adaptive_enabled() -> bool {
    true
}

/// Default p95 latency budget in milliseconds.
fn default_p95_budget_ms() -> u64 {
    50
}

/// Default adaptive step size.
fn default_adaptive_step() -> u64 {
    25
}

/// Default pressure weight for queue depth.
fn default_weight_queue_depth() -> f64 {
    0.4
}

/// Default pressure weight for latency trend.
fn default_weight_latency_trend() -> f64 {
    0.35
}

/// Default pressure weight for drop rate.
fn default_weight_drop_rate() -> f64 {
    0.25
}

/// Default composite pressure threshold for dropping.
fn default_drop_threshold() -> f64 {
    0.8
}

// ── RouterConfig ───────────────────────────────────────────────────────

/// Configuration for the HelixRouter.
///
/// Controls strategy selection thresholds, CPU pool sizing, batch parameters,
/// adaptive behaviour, and pressure scoring weights.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelixConfig {
    /// Cost threshold below which jobs execute inline.
    #[serde(default = "default_inline_threshold")]
    pub inline_threshold: u64,

    /// Cost threshold above which jobs get spawned as separate tasks.
    #[serde(default = "default_spawn_threshold")]
    pub spawn_threshold: u64,

    /// Cost threshold above which jobs go to the CPU thread pool.
    #[serde(default = "default_cpu_pool_threshold")]
    pub cpu_pool_threshold: u64,

    /// Number of concurrent CPU-pool workers.
    #[serde(default = "default_cpu_parallelism")]
    pub cpu_parallelism: usize,

    /// Number of busy CPU workers before backpressure engages.
    #[serde(default = "default_backpressure_busy_threshold")]
    pub backpressure_busy_threshold: usize,

    /// Number of jobs to accumulate before flushing a batch.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Maximum time (ms) to wait before flushing an incomplete batch.
    #[serde(default = "default_batch_timeout_ms")]
    pub batch_timeout_ms: u64,

    /// Adaptive routing settings.
    #[serde(default)]
    pub adaptive: AdaptiveConfig,

    /// Pressure scoring weights.
    #[serde(default)]
    pub pressure: PressureConfig,
}

impl Default for HelixConfig {
    fn default() -> Self {
        Self {
            inline_threshold: default_inline_threshold(),
            spawn_threshold: default_spawn_threshold(),
            cpu_pool_threshold: default_cpu_pool_threshold(),
            cpu_parallelism: default_cpu_parallelism(),
            backpressure_busy_threshold: default_backpressure_busy_threshold(),
            batch_size: default_batch_size(),
            batch_timeout_ms: default_batch_timeout_ms(),
            adaptive: AdaptiveConfig::default(),
            pressure: PressureConfig::default(),
        }
    }
}

/// Adaptive threshold adjustment settings.
///
/// When enabled, the router adjusts `spawn_threshold` based on observed
/// p95 latency: if `CpuPool` p95 exceeds `p95_budget_ms`, the spawn
/// threshold is raised (pushing more work to spawn/inline).
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdaptiveConfig {
    /// Whether adaptive threshold adjustment is enabled.
    #[serde(default = "default_adaptive_enabled")]
    pub enabled: bool,

    /// P95 latency budget in milliseconds. If CpuPool p95 exceeds this,
    /// the effective spawn threshold is raised.
    #[serde(default = "default_p95_budget_ms")]
    pub p95_budget_ms: u64,

    /// How much to raise/lower the spawn threshold per adaptation step.
    #[serde(default = "default_adaptive_step")]
    pub adaptive_step: u64,

    /// EMA smoothing factor for latency tracking (0.0–1.0).
    #[serde(default = "default_ema_alpha")]
    pub ema_alpha: f64,
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            enabled: default_adaptive_enabled(),
            p95_budget_ms: default_p95_budget_ms(),
            adaptive_step: default_adaptive_step(),
            ema_alpha: default_ema_alpha(),
        }
    }
}

/// Composite pressure scoring weights.
///
/// The pressure score is computed as:
/// `pressure = w1 * queue_depth_norm + w2 * latency_trend + w3 * drop_rate`
///
/// Weights should sum to approximately 1.0 for intuitive thresholding.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PressureConfig {
    /// Weight for normalized queue depth signal.
    #[serde(default = "default_weight_queue_depth")]
    pub weight_queue_depth: f64,

    /// Weight for latency trend signal.
    #[serde(default = "default_weight_latency_trend")]
    pub weight_latency_trend: f64,

    /// Weight for recent drop rate signal.
    #[serde(default = "default_weight_drop_rate")]
    pub weight_drop_rate: f64,

    /// Composite pressure threshold above which jobs are dropped.
    #[serde(default = "default_drop_threshold")]
    pub drop_threshold: f64,
}

impl Default for PressureConfig {
    fn default() -> Self {
        Self {
            weight_queue_depth: default_weight_queue_depth(),
            weight_latency_trend: default_weight_latency_trend(),
            weight_drop_rate: default_weight_drop_rate(),
            drop_threshold: default_drop_threshold(),
        }
    }
}

// ── Validation ─────────────────────────────────────────────────────────

/// Validate a [`HelixConfig`], returning a list of human-readable errors.
///
/// # Arguments
///
/// * `config` — The config to validate.
///
/// # Returns
///
/// An empty `Vec` on success, or one error string per violated constraint.
///
/// # Panics
///
/// This function never panics.
pub fn validate(config: &HelixConfig) -> Vec<String> {
    let mut errors = Vec::new();

    if config.inline_threshold >= config.spawn_threshold {
        errors.push(format!(
            "inline_threshold ({}) must be < spawn_threshold ({})",
            config.inline_threshold, config.spawn_threshold
        ));
    }

    if config.spawn_threshold >= config.cpu_pool_threshold {
        errors.push(format!(
            "spawn_threshold ({}) must be < cpu_pool_threshold ({})",
            config.spawn_threshold, config.cpu_pool_threshold
        ));
    }

    if config.cpu_parallelism == 0 {
        errors.push("cpu_parallelism must be > 0".to_string());
    }

    if config.backpressure_busy_threshold == 0 {
        errors.push("backpressure_busy_threshold must be > 0".to_string());
    }

    if config.backpressure_busy_threshold > config.cpu_parallelism {
        errors.push(format!(
            "backpressure_busy_threshold ({}) must be <= cpu_parallelism ({})",
            config.backpressure_busy_threshold, config.cpu_parallelism
        ));
    }

    if config.batch_size == 0 {
        errors.push("batch_size must be > 0".to_string());
    }

    if config.batch_timeout_ms == 0 {
        errors.push("batch_timeout_ms must be > 0".to_string());
    }

    if config.adaptive.ema_alpha <= 0.0 || config.adaptive.ema_alpha > 1.0 {
        errors.push(format!(
            "adaptive.ema_alpha must be in (0.0, 1.0], got {}",
            config.adaptive.ema_alpha
        ));
    }

    if config.adaptive.adaptive_step == 0 {
        errors.push("adaptive.adaptive_step must be > 0".to_string());
    }

    let weight_sum = config.pressure.weight_queue_depth
        + config.pressure.weight_latency_trend
        + config.pressure.weight_drop_rate;
    if weight_sum <= 0.0 {
        errors.push("pressure weights must sum to > 0".to_string());
    }

    if config.pressure.drop_threshold <= 0.0 || config.pressure.drop_threshold > 1.0 {
        errors.push(format!(
            "pressure.drop_threshold must be in (0.0, 1.0], got {}",
            config.pressure.drop_threshold
        ));
    }

    errors
}

// ── ConfigReloader ─────────────────────────────────────────────────────

/// Hot-reload handle for HelixRouter configuration.
///
/// Wraps a `tokio::sync::watch` channel so the router can subscribe to
/// config changes without polling.
///
/// # Panics
///
/// This type never panics.
#[derive(Clone)]
pub struct ConfigReloader {
    tx: tokio::sync::watch::Sender<HelixConfig>,
    rx: tokio::sync::watch::Receiver<HelixConfig>,
}

impl ConfigReloader {
    /// Create a new config reloader with an initial config.
    ///
    /// # Arguments
    ///
    /// * `initial` — The starting configuration.
    ///
    /// # Returns
    ///
    /// A new [`ConfigReloader`].
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(initial: HelixConfig) -> Self {
        let (tx, rx) = tokio::sync::watch::channel(initial);
        Self { tx, rx }
    }

    /// Push a new configuration. Receivers are notified immediately.
    ///
    /// # Arguments
    ///
    /// * `config` — The new configuration to broadcast.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the config was accepted, or a list of validation errors.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn update(&self, config: HelixConfig) -> Result<(), Vec<String>> {
        let errors = validate(&config);
        if !errors.is_empty() {
            return Err(errors);
        }
        // send() only fails if all receivers are dropped, which is fine.
        let _ = self.tx.send(config);
        Ok(())
    }

    /// Get the current configuration.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn current(&self) -> HelixConfig {
        self.rx.borrow().clone()
    }

    /// Get a receiver for subscribing to config changes.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<HelixConfig> {
        self.rx.clone()
    }
}

impl std::fmt::Debug for ConfigReloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigReloader")
            .field("current", &*self.rx.borrow())
            .finish()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- defaults --------------------------------------------------------

    #[test]
    fn test_default_config_passes_validation() {
        let errors = validate(&HelixConfig::default());
        assert!(errors.is_empty(), "default config errors: {errors:?}");
    }

    #[test]
    fn test_default_inline_threshold() {
        assert_eq!(HelixConfig::default().inline_threshold, 50);
    }

    #[test]
    fn test_default_spawn_threshold() {
        assert_eq!(HelixConfig::default().spawn_threshold, 200);
    }

    #[test]
    fn test_default_cpu_pool_threshold() {
        assert_eq!(HelixConfig::default().cpu_pool_threshold, 500);
    }

    #[test]
    fn test_default_cpu_parallelism() {
        assert_eq!(HelixConfig::default().cpu_parallelism, 4);
    }

    #[test]
    fn test_default_batch_size() {
        assert_eq!(HelixConfig::default().batch_size, 10);
    }

    #[test]
    fn test_default_adaptive_enabled() {
        assert!(HelixConfig::default().adaptive.enabled);
    }

    #[test]
    fn test_default_ema_alpha() {
        assert!((HelixConfig::default().adaptive.ema_alpha - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_pressure_weights_sum_to_one() {
        let p = PressureConfig::default();
        let sum = p.weight_queue_depth + p.weight_latency_trend + p.weight_drop_rate;
        assert!((sum - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_drop_threshold() {
        assert!((PressureConfig::default().drop_threshold - 0.8).abs() < f64::EPSILON);
    }

    // -- serde -----------------------------------------------------------

    #[test]
    fn test_config_toml_roundtrip() {
        let cfg = HelixConfig::default();
        let toml_str = toml::to_string_pretty(&cfg)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: serialize: {e}")));
        let parsed: HelixConfig = toml::from_str(&toml_str)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: deserialize: {e}")));
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn test_config_json_roundtrip() {
        let cfg = HelixConfig::default();
        let json = serde_json::to_string(&cfg)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: serialize: {e}")));
        let parsed: HelixConfig = serde_json::from_str(&json)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: deserialize: {e}")));
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn test_config_deserializes_with_defaults() {
        let cfg: HelixConfig = toml::from_str("")
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: deser: {e}")));
        assert_eq!(cfg.inline_threshold, 50);
        assert!(cfg.adaptive.enabled);
    }

    // -- validation ------------------------------------------------------

    #[test]
    fn test_validate_inline_gte_spawn_fails() {
        let mut cfg = HelixConfig::default();
        cfg.inline_threshold = 200;
        cfg.spawn_threshold = 200;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("inline_threshold")));
    }

    #[test]
    fn test_validate_spawn_gte_cpu_pool_fails() {
        let mut cfg = HelixConfig::default();
        cfg.spawn_threshold = 500;
        cfg.cpu_pool_threshold = 500;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("spawn_threshold")));
    }

    #[test]
    fn test_validate_zero_cpu_parallelism_fails() {
        let mut cfg = HelixConfig::default();
        cfg.cpu_parallelism = 0;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("cpu_parallelism")));
    }

    #[test]
    fn test_validate_backpressure_exceeds_parallelism_fails() {
        let mut cfg = HelixConfig::default();
        cfg.backpressure_busy_threshold = 5;
        cfg.cpu_parallelism = 4;
        let errors = validate(&cfg);
        assert!(errors
            .iter()
            .any(|e| e.contains("backpressure_busy_threshold")));
    }

    #[test]
    fn test_validate_zero_batch_size_fails() {
        let mut cfg = HelixConfig::default();
        cfg.batch_size = 0;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("batch_size")));
    }

    #[test]
    fn test_validate_zero_batch_timeout_fails() {
        let mut cfg = HelixConfig::default();
        cfg.batch_timeout_ms = 0;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("batch_timeout_ms")));
    }

    #[test]
    fn test_validate_ema_alpha_out_of_range_fails() {
        let mut cfg = HelixConfig::default();
        cfg.adaptive.ema_alpha = 0.0;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("ema_alpha")));

        cfg.adaptive.ema_alpha = 1.5;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("ema_alpha")));
    }

    #[test]
    fn test_validate_zero_adaptive_step_fails() {
        let mut cfg = HelixConfig::default();
        cfg.adaptive.adaptive_step = 0;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("adaptive_step")));
    }

    #[test]
    fn test_validate_drop_threshold_out_of_range_fails() {
        let mut cfg = HelixConfig::default();
        cfg.pressure.drop_threshold = 0.0;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("drop_threshold")));

        cfg.pressure.drop_threshold = 1.5;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("drop_threshold")));
    }

    #[test]
    fn test_validate_collects_multiple_errors() {
        let cfg = HelixConfig {
            inline_threshold: 999,
            spawn_threshold: 100,
            cpu_pool_threshold: 50,
            cpu_parallelism: 0,
            batch_size: 0,
            ..HelixConfig::default()
        };
        let errors = validate(&cfg);
        assert!(
            errors.len() >= 3,
            "expected >=3 errors, got {}: {errors:?}",
            errors.len()
        );
    }

    // -- ConfigReloader --------------------------------------------------

    #[test]
    fn test_config_reloader_current_returns_initial() {
        let cfg = HelixConfig::default();
        let reloader = ConfigReloader::new(cfg.clone());
        assert_eq!(reloader.current(), cfg);
    }

    #[test]
    fn test_config_reloader_update_valid_config_succeeds() {
        let reloader = ConfigReloader::new(HelixConfig::default());
        let mut new_cfg = HelixConfig::default();
        new_cfg.inline_threshold = 30;
        assert!(reloader.update(new_cfg.clone()).is_ok());
        assert_eq!(reloader.current().inline_threshold, 30);
    }

    #[test]
    fn test_config_reloader_update_invalid_config_fails() {
        let reloader = ConfigReloader::new(HelixConfig::default());
        let mut bad_cfg = HelixConfig::default();
        bad_cfg.cpu_parallelism = 0;
        let result = reloader.update(bad_cfg);
        assert!(result.is_err());
        // Config should not have changed
        assert_eq!(
            reloader.current().cpu_parallelism,
            HelixConfig::default().cpu_parallelism
        );
    }

    #[tokio::test]
    async fn test_config_reloader_subscribe_receives_update() {
        let reloader = ConfigReloader::new(HelixConfig::default());
        let mut rx = reloader.subscribe();

        let mut new_cfg = HelixConfig::default();
        new_cfg.inline_threshold = 25;
        reloader
            .update(new_cfg)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: update failed: {e:?}")));

        rx.changed()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: changed failed: {e}")));
        assert_eq!(rx.borrow().inline_threshold, 25);
    }

    #[test]
    fn test_config_reloader_debug_does_not_panic() {
        let reloader = ConfigReloader::new(HelixConfig::default());
        let _ = format!("{reloader:?}");
    }

    // -- Additional validation tests ----------------------------------------

    #[test]
    fn test_validate_equal_thresholds_fails() {
        let mut cfg = HelixConfig::default();
        cfg.inline_threshold = 50;
        cfg.spawn_threshold = 50;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.contains("inline_threshold")),
            "equal inline and spawn thresholds should fail validation"
        );
    }

    #[test]
    fn test_validate_inverted_spawn_cpu_pool_fails() {
        let mut cfg = HelixConfig::default();
        cfg.spawn_threshold = 600;
        cfg.cpu_pool_threshold = 500;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.contains("spawn_threshold")),
            "spawn > cpu_pool should fail validation"
        );
    }

    #[test]
    fn test_validate_zero_batch_size_fails_explicit() {
        let cfg = HelixConfig {
            batch_size: 0,
            ..HelixConfig::default()
        };
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.contains("batch_size")),
            "batch_size=0 should fail validation"
        );
    }

    #[test]
    fn test_validate_ema_alpha_above_one_fails() {
        let mut cfg = HelixConfig::default();
        cfg.adaptive.ema_alpha = 1.5;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.contains("ema_alpha")),
            "ema_alpha > 1.0 should fail validation"
        );
    }

    #[test]
    fn test_validate_pressure_weights_negative_fails() {
        let mut cfg = HelixConfig::default();
        cfg.pressure.weight_queue_depth = -1.0;
        cfg.pressure.weight_latency_trend = -1.0;
        cfg.pressure.weight_drop_rate = -1.0;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.contains("pressure weights")),
            "all-negative pressure weights should fail (sum <= 0)"
        );
    }

    #[test]
    fn test_validate_drop_threshold_above_one_fails() {
        let mut cfg = HelixConfig::default();
        cfg.pressure.drop_threshold = 1.5;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.contains("drop_threshold")),
            "drop_threshold > 1.0 should fail validation"
        );
    }

    #[test]
    fn test_config_serde_roundtrip_with_custom_values() {
        let cfg = HelixConfig {
            inline_threshold: 25,
            spawn_threshold: 150,
            cpu_pool_threshold: 400,
            cpu_parallelism: 8,
            backpressure_busy_threshold: 6,
            batch_size: 20,
            batch_timeout_ms: 250,
            adaptive: AdaptiveConfig {
                enabled: false,
                p95_budget_ms: 100,
                adaptive_step: 50,
                ema_alpha: 0.5,
            },
            pressure: PressureConfig {
                weight_queue_depth: 0.5,
                weight_latency_trend: 0.3,
                weight_drop_rate: 0.2,
                drop_threshold: 0.9,
            },
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: HelixConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed, "custom config should survive JSON roundtrip");
    }

    #[test]
    fn test_adaptive_config_default() {
        let ac = AdaptiveConfig::default();
        assert!(ac.enabled, "adaptive should be enabled by default");
        assert_eq!(ac.p95_budget_ms, 50);
        assert_eq!(ac.adaptive_step, 25);
        assert!((ac.ema_alpha - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pressure_config_default() {
        let pc = PressureConfig::default();
        assert!((pc.weight_queue_depth - 0.4).abs() < f64::EPSILON);
        assert!((pc.weight_latency_trend - 0.35).abs() < f64::EPSILON);
        assert!((pc.weight_drop_rate - 0.25).abs() < f64::EPSILON);
        assert!((pc.drop_threshold - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_reloader_update_validates() {
        let reloader = ConfigReloader::new(HelixConfig::default());
        // Try to push config with inverted thresholds
        let bad_cfg = HelixConfig {
            inline_threshold: 999,
            spawn_threshold: 10,
            ..HelixConfig::default()
        };
        let result = reloader.update(bad_cfg);
        assert!(
            result.is_err(),
            "invalid config should be rejected by reloader"
        );
        // Original config should be preserved
        assert_eq!(
            reloader.current().inline_threshold,
            HelixConfig::default().inline_threshold,
            "config should not change after rejected update"
        );
    }
}
