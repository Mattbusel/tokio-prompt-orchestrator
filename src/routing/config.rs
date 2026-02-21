//! Routing configuration types.
//!
//! Provides [`RoutingConfig`] for tuning complexity thresholds, cost-per-token
//! rates, and adaptive threshold behaviour. All fields have sensible defaults
//! and are (de)serialisable via serde for TOML/JSON config files.

use serde::{Deserialize, Serialize};

// ── Default value functions ────────────────────────────────────────────

/// Default complexity threshold below which prompts route to the local model.
fn default_local_threshold() -> f64 {
    0.4
}

/// Default complexity threshold above which prompts route directly to cloud.
fn default_cloud_threshold() -> f64 {
    0.7
}

/// Default cost per 1K input tokens for local inference (effectively free).
fn default_local_cost_per_1k_tokens() -> f64 {
    0.0
}

/// Default cost per 1K input tokens for cloud inference (Claude API pricing).
fn default_cloud_cost_per_1k_tokens() -> f64 {
    0.015
}

/// Default adaptive threshold adjustment step size.
fn default_adaptive_step() -> f64 {
    0.05
}

/// Default minimum number of outcomes before adapting thresholds.
fn default_adaptive_min_samples() -> u64 {
    20
}

/// Default failure rate above which the fallback threshold rises.
fn default_adaptive_failure_ceiling() -> f64 {
    0.3
}

/// Default enabled state for adaptive threshold adjustment.
fn default_adaptive_enabled() -> bool {
    true
}

// ── RoutingConfig ──────────────────────────────────────────────────────

/// Configuration for the model routing layer.
///
/// Controls how the [`super::ComplexityScorer`] output maps to a
/// [`super::RoutingDecision`] and how per-token costs are tracked.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutingConfig {
    /// Complexity score below which requests route to the local model.
    ///
    /// Range: `0.0..=1.0`.  Default: `0.4`.
    #[serde(default = "default_local_threshold")]
    pub local_threshold: f64,

    /// Complexity score above which requests route directly to the cloud model.
    ///
    /// Range: `0.0..=1.0`, must be `>= local_threshold`.  Default: `0.7`.
    #[serde(default = "default_cloud_threshold")]
    pub cloud_threshold: f64,

    /// Estimated cost per 1 000 input tokens for the local worker.
    #[serde(default = "default_local_cost_per_1k_tokens")]
    pub local_cost_per_1k_tokens: f64,

    /// Estimated cost per 1 000 input tokens for the cloud worker.
    #[serde(default = "default_cloud_cost_per_1k_tokens")]
    pub cloud_cost_per_1k_tokens: f64,

    /// Adaptive threshold tuning settings.
    #[serde(default)]
    pub adaptive: AdaptiveConfig,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            local_threshold: default_local_threshold(),
            cloud_threshold: default_cloud_threshold(),
            local_cost_per_1k_tokens: default_local_cost_per_1k_tokens(),
            cloud_cost_per_1k_tokens: default_cloud_cost_per_1k_tokens(),
            adaptive: AdaptiveConfig::default(),
        }
    }
}

/// Adaptive threshold adjustment configuration.
///
/// When enabled the router raises or lowers the local/cloud boundary based
/// on observed local-model failure rates in the fallback zone.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdaptiveConfig {
    /// Whether adaptive threshold adjustment is enabled.
    #[serde(default = "default_adaptive_enabled")]
    pub enabled: bool,

    /// How much to raise/lower the cloud threshold per adaptation step.
    #[serde(default = "default_adaptive_step")]
    pub step: f64,

    /// Minimum completed fallback outcomes before the first adaptation.
    #[serde(default = "default_adaptive_min_samples")]
    pub min_samples: u64,

    /// Local failure rate ceiling. If the observed failure rate in the
    /// fallback zone exceeds this value the threshold is raised.
    #[serde(default = "default_adaptive_failure_ceiling")]
    pub failure_ceiling: f64,
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            enabled: default_adaptive_enabled(),
            step: default_adaptive_step(),
            min_samples: default_adaptive_min_samples(),
            failure_ceiling: default_adaptive_failure_ceiling(),
        }
    }
}

/// Validate a [`RoutingConfig`], returning a list of human-readable errors.
///
/// # Arguments
///
/// * `config` — The routing configuration to validate.
///
/// # Returns
///
/// An empty `Vec` on success, or one error string per violated constraint.
///
/// # Panics
///
/// This function never panics.
pub fn validate(config: &RoutingConfig) -> Vec<String> {
    let mut errors = Vec::new();

    if config.local_threshold < 0.0 || config.local_threshold > 1.0 {
        errors.push(format!(
            "local_threshold must be in [0.0, 1.0], got {}",
            config.local_threshold
        ));
    }

    if config.cloud_threshold < 0.0 || config.cloud_threshold > 1.0 {
        errors.push(format!(
            "cloud_threshold must be in [0.0, 1.0], got {}",
            config.cloud_threshold
        ));
    }

    if config.cloud_threshold < config.local_threshold {
        errors.push(format!(
            "cloud_threshold ({}) must be >= local_threshold ({})",
            config.cloud_threshold, config.local_threshold
        ));
    }

    if config.local_cost_per_1k_tokens < 0.0 {
        errors.push(format!(
            "local_cost_per_1k_tokens must be >= 0, got {}",
            config.local_cost_per_1k_tokens
        ));
    }

    if config.cloud_cost_per_1k_tokens < 0.0 {
        errors.push(format!(
            "cloud_cost_per_1k_tokens must be >= 0, got {}",
            config.cloud_cost_per_1k_tokens
        ));
    }

    if config.adaptive.step <= 0.0 || config.adaptive.step > 0.5 {
        errors.push(format!(
            "adaptive.step must be in (0.0, 0.5], got {}",
            config.adaptive.step
        ));
    }

    if config.adaptive.failure_ceiling <= 0.0 || config.adaptive.failure_ceiling > 1.0 {
        errors.push(format!(
            "adaptive.failure_ceiling must be in (0.0, 1.0], got {}",
            config.adaptive.failure_ceiling
        ));
    }

    errors
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- defaults --------------------------------------------------------

    #[test]
    fn test_default_local_threshold_returns_0_4() {
        assert!((default_local_threshold() - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_cloud_threshold_returns_0_7() {
        assert!((default_cloud_threshold() - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_local_cost_returns_zero() {
        assert!((default_local_cost_per_1k_tokens()).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_cloud_cost_returns_0_015() {
        assert!((default_cloud_cost_per_1k_tokens() - 0.015).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_adaptive_step_returns_0_05() {
        assert!((default_adaptive_step() - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_adaptive_min_samples_returns_20() {
        assert_eq!(default_adaptive_min_samples(), 20);
    }

    #[test]
    fn test_default_adaptive_failure_ceiling_returns_0_3() {
        assert!((default_adaptive_failure_ceiling() - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_adaptive_enabled_returns_true() {
        assert!(default_adaptive_enabled());
    }

    // -- Default trait ---------------------------------------------------

    #[test]
    fn test_routing_config_default_matches_function_defaults() {
        let cfg = RoutingConfig::default();
        assert!((cfg.local_threshold - 0.4).abs() < f64::EPSILON);
        assert!((cfg.cloud_threshold - 0.7).abs() < f64::EPSILON);
        assert!((cfg.local_cost_per_1k_tokens).abs() < f64::EPSILON);
        assert!((cfg.cloud_cost_per_1k_tokens - 0.015).abs() < f64::EPSILON);
        assert!(cfg.adaptive.enabled);
    }

    #[test]
    fn test_adaptive_config_default_matches_function_defaults() {
        let cfg = AdaptiveConfig::default();
        assert!(cfg.enabled);
        assert!((cfg.step - 0.05).abs() < f64::EPSILON);
        assert_eq!(cfg.min_samples, 20);
        assert!((cfg.failure_ceiling - 0.3).abs() < f64::EPSILON);
    }

    // -- serde -----------------------------------------------------------

    #[test]
    fn test_routing_config_toml_roundtrip() {
        let cfg = RoutingConfig::default();
        let toml_str = toml::to_string_pretty(&cfg)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: serialize: {e}")));
        let parsed: RoutingConfig = toml::from_str(&toml_str)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: deserialize: {e}")));
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn test_routing_config_json_roundtrip() {
        let cfg = RoutingConfig::default();
        let json = serde_json::to_string(&cfg)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: serialize: {e}")));
        let parsed: RoutingConfig = serde_json::from_str(&json)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: deserialize: {e}")));
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn test_routing_config_deserializes_with_defaults() {
        // Empty table → all defaults
        let cfg: RoutingConfig = toml::from_str("")
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: deserialize: {e}")));
        assert!((cfg.local_threshold - 0.4).abs() < f64::EPSILON);
        assert!(cfg.adaptive.enabled);
    }

    // -- validation ------------------------------------------------------

    #[test]
    fn test_validate_default_config_passes() {
        let errors = validate(&RoutingConfig::default());
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
    }

    #[test]
    fn test_validate_local_threshold_above_1_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.local_threshold = 1.1;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("local_threshold")));
    }

    #[test]
    fn test_validate_local_threshold_negative_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.local_threshold = -0.1;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("local_threshold")));
    }

    #[test]
    fn test_validate_cloud_threshold_above_1_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.cloud_threshold = 1.5;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("cloud_threshold")));
    }

    #[test]
    fn test_validate_cloud_below_local_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.local_threshold = 0.8;
        cfg.cloud_threshold = 0.3;
        let errors = validate(&cfg);
        assert!(errors
            .iter()
            .any(|e| e.contains("cloud_threshold") && e.contains(">=")));
    }

    #[test]
    fn test_validate_negative_local_cost_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.local_cost_per_1k_tokens = -1.0;
        let errors = validate(&cfg);
        assert!(errors
            .iter()
            .any(|e| e.contains("local_cost_per_1k_tokens")));
    }

    #[test]
    fn test_validate_negative_cloud_cost_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.cloud_cost_per_1k_tokens = -0.5;
        let errors = validate(&cfg);
        assert!(errors
            .iter()
            .any(|e| e.contains("cloud_cost_per_1k_tokens")));
    }

    #[test]
    fn test_validate_adaptive_step_zero_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.adaptive.step = 0.0;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("adaptive.step")));
    }

    #[test]
    fn test_validate_adaptive_step_too_large_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.adaptive.step = 0.6;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("adaptive.step")));
    }

    #[test]
    fn test_validate_adaptive_failure_ceiling_zero_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.adaptive.failure_ceiling = 0.0;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("failure_ceiling")));
    }

    #[test]
    fn test_validate_adaptive_failure_ceiling_above_1_fails() {
        let mut cfg = RoutingConfig::default();
        cfg.adaptive.failure_ceiling = 1.1;
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("failure_ceiling")));
    }

    #[test]
    fn test_validate_collects_multiple_errors() {
        let cfg = RoutingConfig {
            local_threshold: -1.0,
            cloud_threshold: 2.0,
            local_cost_per_1k_tokens: -5.0,
            cloud_cost_per_1k_tokens: -3.0,
            adaptive: AdaptiveConfig::default(),
        };
        let errors = validate(&cfg);
        assert!(
            errors.len() >= 4,
            "expected >=4 errors, got {}",
            errors.len()
        );
    }

    #[test]
    fn test_validate_equal_thresholds_passes() {
        let cfg = RoutingConfig {
            local_threshold: 0.5,
            cloud_threshold: 0.5,
            ..RoutingConfig::default()
        };
        let errors = validate(&cfg);
        assert!(
            errors.is_empty(),
            "equal thresholds should pass: {errors:?}"
        );
    }

    #[test]
    fn test_validate_boundary_thresholds_pass() {
        // Both at 0.0
        let cfg = RoutingConfig {
            local_threshold: 0.0,
            cloud_threshold: 0.0,
            ..RoutingConfig::default()
        };
        assert!(validate(&cfg).is_empty());

        // Both at 1.0
        let cfg = RoutingConfig {
            local_threshold: 1.0,
            cloud_threshold: 1.0,
            ..RoutingConfig::default()
        };
        assert!(validate(&cfg).is_empty());
    }
}
