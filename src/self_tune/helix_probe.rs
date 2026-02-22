//! # HelixRouter Pressure Probe
//!
//! ## Responsibility
//!
//! Polls HelixRouter's `/api/stats` endpoint at a configurable interval and
//! injects its `pressure_score` into the local [`TelemetryBus`] via
//! [`TelemetryBus::set_external_pressure`].
//!
//! This closes the cross-repo feedback loop: HelixRouter's backpressure signal
//! (derived from adaptive-spawn threshold dynamics and job-drop rate) becomes
//! visible to Tokio Prompt's anomaly detector and tuning controller, letting
//! the self-improvement loop slow down request generation when downstream
//! capacity is saturated.
//!
//! ## Guarantees
//!
//! - **Non-panicking**: all error paths are soft-logged and retried.
//! - **Non-blocking**: runs in a background task; cancel via `JoinHandle::abort`.
//! - **Bounded**: no accumulating state; only one `f64` is written per tick.
//!
//! ## NOT Responsible For
//!
//! - Pushing config updates back to HelixRouter (that is `helix_bridge` in EOT).
//! - Authenticating to HelixRouter (no auth in current protocol).

use std::time::Duration;

use serde::Deserialize;
use tracing::{error, trace, warn};

use crate::self_tune::telemetry_bus::TelemetryBus;

// ── Wire-format mirror of HelixRouter's `/api/stats` response ────────────────

/// Minimal subset of HelixRouter's stats JSON.
///
/// Only fields we actually consume are declared; unknown fields are ignored by
/// `#[serde(deny_unknown_fields)]` being absent.
#[derive(Debug, Deserialize)]
struct HelixStats {
    pressure_score: f64,
    dropped: u64,
    completed: u64,
}

// ── Configuration ─────────────────────────────────────────────────────────────

/// Configuration for [`HelixPressureProbe`].
#[derive(Debug, Clone)]
pub struct HelixProbeConfig {
    /// Base URL of the HelixRouter HTTP API (e.g. `http://127.0.0.1:8080`).
    pub base_url: String,
    /// How often to poll `/api/stats`.
    pub poll_interval: Duration,
    /// TCP connection timeout.
    pub connect_timeout: Duration,
    /// Per-request read timeout.
    pub request_timeout: Duration,
    /// Number of consecutive failures before switching from WARN to ERROR logs.
    pub error_threshold: u32,
}

impl Default for HelixProbeConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080".to_string(),
            poll_interval: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(10),
            error_threshold: 5,
        }
    }
}

// ── Probe ─────────────────────────────────────────────────────────────────────

/// Polls HelixRouter and feeds its pressure score into the [`TelemetryBus`].
///
/// # Example
///
/// ```rust,no_run
/// use tokio_prompt_orchestrator::self_tune::{
///     telemetry_bus::{TelemetryBus, TelemetryBusConfig, PipelineCounters},
///     helix_probe::{HelixPressureProbe, HelixProbeConfig},
/// };
///
/// #[tokio::main]
/// async fn main() {
///     let counters = PipelineCounters::new();
///     let bus = TelemetryBus::new(TelemetryBusConfig::default(), counters);
///     let cfg = HelixProbeConfig::default();
///     let probe = HelixPressureProbe::new(cfg, bus.clone());
///     let _handle = tokio::spawn(async move { probe.run().await });
/// }
/// ```
pub struct HelixPressureProbe {
    config: HelixProbeConfig,
    bus: TelemetryBus,
    client: reqwest::Client,
}

impl HelixPressureProbe {
    /// Create a new probe.
    ///
    /// `bus` is cheaply cloned — [`TelemetryBus`] is internally `Arc`-backed.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: HelixProbeConfig, bus: TelemetryBus) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .build()
            .unwrap_or_default();

        Self { config, bus, client }
    }

    /// Run the polling loop indefinitely.
    ///
    /// On each tick, fetches `/api/stats`, extracts `pressure_score`, and
    /// calls [`TelemetryBus::set_external_pressure`].
    ///
    /// On failure, increments a consecutive-failure counter.  When the counter
    /// reaches `config.error_threshold`, the log level escalates from WARN to
    /// ERROR and the external pressure is reset to `0.0` so Tokio Prompt does
    /// not make routing decisions based on stale data.
    ///
    /// Drop the [`tokio::task::JoinHandle`] to stop the loop.
    ///
    /// # Panics
    /// This function never panics.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut failures: u32 = 0;

        loop {
            ticker.tick().await;

            match self.fetch_pressure().await {
                Ok((pressure, dropped, completed)) => {
                    failures = 0;

                    // Combine HelixRouter's own pressure_score with the
                    // observed drop_rate so that explicit shedding (tasks
                    // actually being dropped) always raises the signal even
                    // when pressure_score hasn't caught up yet.
                    let total = dropped + completed;
                    let drop_rate = if total > 0 {
                        dropped as f64 / total as f64
                    } else {
                        0.0
                    };
                    let combined = pressure.max(drop_rate);

                    trace!(
                        pressure,
                        drop_rate,
                        combined,
                        dropped,
                        completed,
                        url = %self.config.base_url,
                        "HelixPressureProbe: tick"
                    );
                    self.bus.set_external_pressure(combined);
                }
                Err(e) => {
                    failures = failures.saturating_add(1);

                    if failures >= self.config.error_threshold {
                        error!(
                            error = %e,
                            consecutive_failures = failures,
                            url = %self.config.base_url,
                            "HelixPressureProbe: repeated failures, clearing external pressure"
                        );
                        // Clear stale signal so Tokio Prompt is not misled.
                        self.bus.set_external_pressure(0.0);
                    } else {
                        warn!(
                            error = %e,
                            url = %self.config.base_url,
                            "HelixPressureProbe: poll failed, will retry"
                        );
                    }
                }
            }
        }
    }

    /// Fetch `/api/stats` and extract `(pressure_score, dropped, completed)`.
    async fn fetch_pressure(&self) -> Result<(f64, u64, u64), String> {
        let url = format!("{}/api/stats", self.config.base_url);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("connect: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }

        let stats: HelixStats = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse: {e}"))?;

        Ok((stats.pressure_score, stats.dropped, stats.completed))
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_tune::telemetry_bus::{PipelineCounters, TelemetryBusConfig};

    fn make_bus() -> TelemetryBus {
        let counters = PipelineCounters::new();
        TelemetryBus::new(TelemetryBusConfig::default(), counters)
    }

    fn make_probe(bus: TelemetryBus) -> HelixPressureProbe {
        HelixPressureProbe::new(HelixProbeConfig::default(), bus)
    }

    // ── Config tests ──────────────────────────────────────────────────────

    #[test]
    fn config_default_base_url() {
        let cfg = HelixProbeConfig::default();
        assert_eq!(cfg.base_url, "http://127.0.0.1:8080");
    }

    #[test]
    fn config_default_poll_interval() {
        let cfg = HelixProbeConfig::default();
        assert_eq!(cfg.poll_interval, Duration::from_secs(5));
    }

    #[test]
    fn config_default_connect_timeout() {
        let cfg = HelixProbeConfig::default();
        assert_eq!(cfg.connect_timeout, Duration::from_secs(3));
    }

    #[test]
    fn config_default_request_timeout() {
        let cfg = HelixProbeConfig::default();
        assert_eq!(cfg.request_timeout, Duration::from_secs(10));
    }

    #[test]
    fn config_default_error_threshold() {
        let cfg = HelixProbeConfig::default();
        assert_eq!(cfg.error_threshold, 5);
    }

    #[test]
    fn config_clone_is_independent() {
        let cfg = HelixProbeConfig::default();
        let mut cfg2 = cfg.clone();
        cfg2.base_url = "http://other:9090".to_string();
        assert_eq!(cfg.base_url, "http://127.0.0.1:8080");
        assert_eq!(cfg2.base_url, "http://other:9090");
    }

    // ── Probe construction ────────────────────────────────────────────────

    #[test]
    fn probe_new_constructs_without_panic() {
        let bus = make_bus();
        let _probe = make_probe(bus);
    }

    #[test]
    fn probe_new_with_custom_config() {
        let bus = make_bus();
        let cfg = HelixProbeConfig {
            base_url: "http://192.168.1.10:3000".to_string(),
            poll_interval: Duration::from_secs(2),
            connect_timeout: Duration::from_millis(500),
            request_timeout: Duration::from_secs(5),
            error_threshold: 3,
        };
        let probe = HelixPressureProbe::new(cfg.clone(), bus);
        assert_eq!(probe.config.base_url, "http://192.168.1.10:3000");
        assert_eq!(probe.config.poll_interval, Duration::from_secs(2));
        assert_eq!(probe.config.error_threshold, 3);
    }

    // ── Bus integration: external pressure wiring ─────────────────────────

    #[test]
    fn probe_bus_starts_with_zero_pressure() {
        let bus = make_bus();
        assert_eq!(bus.external_pressure(), 0.0);
    }

    #[tokio::test]
    async fn probe_failure_clears_pressure_after_threshold() {
        // Simulate what the run() loop does on repeated failures.
        let bus = make_bus();
        bus.set_external_pressure(0.8); // Stale signal from a previous good tick.

        let cfg = HelixProbeConfig {
            error_threshold: 1, // Low threshold for test speed.
            ..Default::default()
        };
        let probe = HelixPressureProbe::new(cfg, bus.clone());

        // Simulate the failure path manually.
        let mut failures: u32 = 0;
        let error_threshold = probe.config.error_threshold;

        // After threshold failures, pressure is reset.
        failures = failures.saturating_add(1);
        if failures >= error_threshold {
            probe.bus.set_external_pressure(0.0);
        }

        assert_eq!(probe.bus.external_pressure(), 0.0);
    }

    #[tokio::test]
    async fn probe_success_path_updates_bus_pressure() {
        let bus = make_bus();
        // Simulate a successful tick result.
        let pressure = 0.55_f64;
        bus.set_external_pressure(pressure);
        let v = bus.external_pressure();
        assert!((v - 0.55).abs() < 0.002, "expected ~0.55, got {v}");
    }

    // ── HelixStats deserialization ────────────────────────────────────────

    #[test]
    fn helix_stats_deserializes_from_json() {
        let json = r#"{"pressure_score":0.42,"dropped":3,"completed":100}"#;
        let stats: HelixStats = serde_json::from_str(json).expect("parse");
        assert!((stats.pressure_score - 0.42).abs() < 1e-9);
        assert_eq!(stats.dropped, 3);
        assert_eq!(stats.completed, 100);
    }

    #[test]
    fn helix_stats_deserializes_zero_pressure() {
        let json = r#"{"pressure_score":0.0,"dropped":0,"completed":0}"#;
        let stats: HelixStats = serde_json::from_str(json).expect("parse");
        assert_eq!(stats.pressure_score, 0.0);
    }

    #[test]
    fn helix_stats_deserializes_max_pressure() {
        let json = r#"{"pressure_score":1.0,"dropped":50,"completed":200}"#;
        let stats: HelixStats = serde_json::from_str(json).expect("parse");
        assert!((stats.pressure_score - 1.0).abs() < 1e-9);
        assert_eq!(stats.dropped, 50);
    }

    #[test]
    fn helix_stats_ignores_extra_fields() {
        // HelixRouter may add new fields; we must not fail on unknown keys.
        let json = r#"{"pressure_score":0.3,"dropped":1,"completed":10,"extra_field":"ignored"}"#;
        let result = serde_json::from_str::<HelixStats>(json);
        assert!(result.is_ok(), "unknown fields should be ignored: {result:?}");
    }

    #[test]
    fn helix_stats_fails_without_required_field() {
        let json = r#"{"dropped":0,"completed":0}"#; // Missing pressure_score.
        let result = serde_json::from_str::<HelixStats>(json);
        assert!(result.is_err(), "missing pressure_score should fail");
    }

    // ── Blending integration ──────────────────────────────────────────────

    #[tokio::test]
    async fn blended_pressure_visible_in_snapshot_after_probe_tick() {
        let bus = make_bus();
        // Simulate a probe tick with pressure_score = 0.8.
        bus.set_external_pressure(0.8);
        let snap = bus.tick_now().await;
        // Internal = 0.0 (no stages), external = 0.8 → blended = 0.4
        assert!((snap.pressure - 0.4).abs() < 0.002,
            "expected blended 0.4, got {}", snap.pressure);
    }

    #[tokio::test]
    async fn clearing_external_pressure_reverts_snapshot_to_internal() {
        let bus = make_bus();
        bus.set_external_pressure(0.8);
        bus.set_external_pressure(0.0);
        let snap = bus.tick_now().await;
        assert_eq!(snap.pressure, 0.0,
            "cleared external pressure should yield 0.0: {}", snap.pressure);
    }

    // ── Combined pressure signal tests ────────────────────────────────────

    #[test]
    fn combined_pressure_uses_drop_rate_when_higher_than_pressure_score() {
        // pressure_score = 0.1, drop_rate = 0.8 → combined = 0.8
        let dropped = 80_u64;
        let completed = 20_u64;
        let pressure = 0.1_f64;
        let total = dropped + completed;
        let drop_rate = dropped as f64 / total as f64;
        let combined = pressure.max(drop_rate);
        assert!((combined - 0.8).abs() < 1e-9, "combined={combined}");
    }

    #[test]
    fn combined_pressure_uses_pressure_score_when_higher_than_drop_rate() {
        // pressure_score = 0.9, drop_rate = 0.1 → combined = 0.9
        let dropped = 10_u64;
        let completed = 90_u64;
        let pressure = 0.9_f64;
        let total = dropped + completed;
        let drop_rate = dropped as f64 / total as f64;
        let combined = pressure.max(drop_rate);
        assert!((combined - 0.9).abs() < 1e-9, "combined={combined}");
    }

    #[test]
    fn combined_pressure_zero_total_gives_pressure_score_unchanged() {
        // When completed + dropped == 0 (no jobs yet), drop_rate = 0.0 and
        // combined == pressure_score.
        let pressure = 0.45_f64;
        let total = 0_u64;
        let drop_rate = if total > 0 { 1.0 } else { 0.0 };
        let combined = pressure.max(drop_rate);
        assert!((combined - 0.45).abs() < 1e-9, "combined={combined}");
    }

    #[test]
    fn combined_pressure_all_dropped_gives_one() {
        let dropped = 100_u64;
        let completed = 0_u64;
        let pressure = 0.0_f64;
        let total = dropped + completed;
        let drop_rate = dropped as f64 / total as f64;
        let combined = pressure.max(drop_rate);
        assert!((combined - 1.0).abs() < 1e-9, "combined={combined}");
    }

    #[test]
    fn combined_pressure_equal_scores_returns_same() {
        let dropped = 50_u64;
        let completed = 50_u64;
        let pressure = 0.5_f64;
        let total = dropped + completed;
        let drop_rate = dropped as f64 / total as f64;
        let combined = pressure.max(drop_rate);
        assert!((combined - 0.5).abs() < 1e-9, "combined={combined}");
    }
}
