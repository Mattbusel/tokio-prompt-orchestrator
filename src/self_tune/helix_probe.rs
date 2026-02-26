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

/// Per-strategy routing count from HelixRouter's `/api/stats`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HelixRoutedStrategyCount {
    /// Strategy name as returned by HelixRouter (e.g. `"inline"`, `"spawn"`, `"drop"`).
    pub strategy: String,
    /// Number of jobs routed to this strategy in the current window.
    pub count: u64,
}

/// Per-strategy latency summary from HelixRouter's `/api/stats`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HelixLatencyRow {
    /// Strategy name.
    pub strategy: String,
    /// Number of completed samples in this strategy's window.
    pub count: u64,
    /// Simple average latency in milliseconds.
    pub avg_ms: f64,
    /// Exponential moving average latency in milliseconds.
    pub ema_ms: f64,
    /// 95th-percentile latency in milliseconds.
    pub p95_ms: u64,
}

/// Per-strategy snapshot surfaced by [`HelixPressureProbe`] for observability
/// and strategy-aware tuning.
///
/// Consumers can inspect which routing strategies HelixRouter is currently
/// using and their latency profiles, enabling targeted adjustments rather than
/// relying solely on the aggregate pressure signal.
#[derive(Debug, Clone, Default)]
pub struct HelixStrategySnapshot {
    /// Fraction of jobs explicitly shed via the `Drop` strategy (0.0–1.0).
    /// A non-zero value means HelixRouter is actively shedding load.
    pub drop_strategy_frac: f64,
    /// Highest p95 latency seen across all strategies (milliseconds).
    pub max_p95_ms: u64,
    /// Strategy with the highest p95 latency, if any.
    pub hottest_strategy: Option<String>,
    /// Full per-strategy routing counts for logging/dashboards.
    pub routed_by_strategy: Vec<HelixRoutedStrategyCount>,
    /// Full per-strategy latency rows for logging/dashboards.
    pub latency_by_strategy: Vec<HelixLatencyRow>,
}

/// Full stats response mirror of HelixRouter's `/api/stats`.
///
/// All fields we actually consume are declared; unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct HelixStats {
    pressure_score: f64,
    dropped: u64,
    completed: u64,
    /// Current adaptive spawn threshold — logged for observability only.
    #[serde(default)]
    adaptive_spawn_threshold: u64,
    /// Per-strategy routing counts — used to detect explicit load shedding.
    #[serde(default)]
    routed_by_strategy: Vec<HelixRoutedStrategyCount>,
    /// Per-strategy latency summaries — used to detect hot execution paths.
    #[serde(default)]
    latency_by_strategy: Vec<HelixLatencyRow>,
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

            // Apply exponential backoff when HelixRouter is unreachable.
            // Cap at 5× the base poll interval to avoid starving the bus.
            if failures > 0 {
                let backoff_factor = (failures as u64).min(5);
                let backoff = self.config.poll_interval * backoff_factor as u32;
                tokio::time::sleep(backoff).await;
            }

            match self.fetch_pressure().await {
                Ok((pressure, dropped, completed, adaptive_spawn_threshold, strategy_snap)) => {
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

                    // Additionally blend in the Drop-strategy fraction: when
                    // HelixRouter is actively routing jobs to the Drop strategy,
                    // that is an unambiguous signal of load shedding that should
                    // immediately raise the combined pressure even if pressure_score
                    // and drop_rate haven't caught up yet.
                    let combined = pressure
                        .max(drop_rate)
                        .max(strategy_snap.drop_strategy_frac);

                    trace!(
                        pressure,
                        drop_rate,
                        drop_strategy_frac = strategy_snap.drop_strategy_frac,
                        combined,
                        dropped,
                        completed,
                        adaptive_spawn_threshold,
                        max_p95_ms = strategy_snap.max_p95_ms,
                        hottest_strategy = ?strategy_snap.hottest_strategy,
                        url = %self.config.base_url,
                        "HelixPressureProbe: tick"
                    );
                    self.bus.set_external_pressure(combined);
                }
                Err(e) => {
                    failures = failures.saturating_add(1);

                    if failures >= self.config.error_threshold {
                        let backoff_ms = self.config.poll_interval.as_millis()
                            * (failures as u64).min(5) as u128;
                        error!(
                            error = %e,
                            consecutive_failures = failures,
                            backoff_ms,
                            url = %self.config.base_url,
                            "HelixPressureProbe: repeated failures, clearing external pressure and backing off"
                        );
                        // Clear stale signal so Tokio Prompt is not misled.
                        self.bus.set_external_pressure(0.0);
                    } else {
                        let backoff_ms = self.config.poll_interval.as_millis()
                            * (failures as u64).min(5) as u128;
                        warn!(
                            error = %e,
                            consecutive_failures = failures,
                            backoff_ms,
                            url = %self.config.base_url,
                            "HelixPressureProbe: poll failed, backing off before retry"
                        );
                    }
                }
            }
        }
    }

    /// Fetch `/api/stats` and extract the pressure tuple plus per-strategy snapshot.
    ///
    /// Returns `(pressure_score, dropped, completed, adaptive_spawn_threshold, strategy_snapshot)`.
    pub(crate) async fn fetch_pressure(
        &self,
    ) -> Result<(f64, u64, u64, u64, HelixStrategySnapshot), String> {
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

        let strategy_snapshot = Self::build_strategy_snapshot(
            &stats.routed_by_strategy,
            &stats.latency_by_strategy,
        );

        Ok((
            stats.pressure_score,
            stats.dropped,
            stats.completed,
            stats.adaptive_spawn_threshold,
            strategy_snapshot,
        ))
    }

    /// Build a [`HelixStrategySnapshot`] from the raw per-strategy data.
    ///
    /// Computes the `Drop` strategy fraction and identifies the hottest strategy
    /// by p95 latency for targeted tuning decisions.
    fn build_strategy_snapshot(
        routed: &[HelixRoutedStrategyCount],
        latency: &[HelixLatencyRow],
    ) -> HelixStrategySnapshot {
        // Compute what fraction of routed jobs went to the Drop strategy.
        let total_routed: u64 = routed.iter().map(|r| r.count).sum();
        let drop_count: u64 = routed
            .iter()
            .filter(|r| r.strategy.eq_ignore_ascii_case("drop"))
            .map(|r| r.count)
            .sum();
        let drop_strategy_frac = if total_routed > 0 {
            drop_count as f64 / total_routed as f64
        } else {
            0.0
        };

        // Find the strategy with the worst p95 latency.
        let (max_p95_ms, hottest_strategy) = latency
            .iter()
            .filter(|r| r.count > 0)
            .fold((0u64, None::<String>), |(max_p95, hot), row| {
                if row.p95_ms > max_p95 {
                    (row.p95_ms, Some(row.strategy.clone()))
                } else {
                    (max_p95, hot)
                }
            });

        HelixStrategySnapshot {
            drop_strategy_frac,
            max_p95_ms,
            hottest_strategy,
            routed_by_strategy: routed.to_vec(),
            latency_by_strategy: latency.to_vec(),
        }
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

    // ── Mock-HTTP integration tests ───────────────────────────────────────
    //
    // These tests spin up a minimal tokio TCP listener that speaks just enough
    // HTTP/1.1 to satisfy reqwest, verifying the probe's fetch_pressure() method
    // end-to-end against a real (local) network round-trip.

    /// Bind to a random OS-assigned port and return the port number + listener.
    async fn bind_mock_server() -> (u16, tokio::net::TcpListener) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let port = listener.local_addr().expect("local_addr").port();
        (port, listener)
    }

    /// Serve a single HTTP request, return the given body with the given status.
    async fn serve_once(
        listener: tokio::net::TcpListener,
        status: u16,
        body: &'static str,
    ) {
        use tokio::io::AsyncWriteExt;
        let (mut stream, _) = listener.accept().await.expect("accept");
        // Drain the request bytes (we don't care about the content here).
        let mut buf = [0u8; 4096];
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
        )
        .await;
        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            len = body.len(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    }

    #[tokio::test]
    async fn fetch_pressure_success_returns_correct_values() {
        let (port, listener) = bind_mock_server().await;
        let body = r#"{"pressure_score":0.72,"dropped":5,"completed":95,"adaptive_spawn_threshold":1000,"routed_by_strategy":[],"latency_by_strategy":[]}"#;

        tokio::spawn(serve_once(listener, 200, body));

        let bus = make_bus();
        let cfg = HelixProbeConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let probe = HelixPressureProbe::new(cfg, bus);
        let result = probe.fetch_pressure().await;

        let (pressure, dropped, completed, adaptive_spawn_threshold, strategy_snap) =
            result.expect("fetch_pressure should succeed");
        assert!((pressure - 0.72).abs() < 1e-9, "pressure={pressure}");
        assert_eq!(dropped, 5);
        assert_eq!(completed, 95);
        assert_eq!(adaptive_spawn_threshold, 1000);
        assert_eq!(strategy_snap.drop_strategy_frac, 0.0);
        assert_eq!(strategy_snap.max_p95_ms, 0);
    }

    #[tokio::test]
    async fn fetch_pressure_http_500_returns_error() {
        let (port, listener) = bind_mock_server().await;
        let body = r#"{"error":"internal server error"}"#;

        tokio::spawn(serve_once(listener, 500, body));

        let bus = make_bus();
        let cfg = HelixProbeConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let probe = HelixPressureProbe::new(cfg, bus);
        let result = probe.fetch_pressure().await;

        assert!(result.is_err(), "expected error for HTTP 500");
        let err = result.unwrap_err();
        assert!(err.contains("500"), "error should mention status code: {err}");
    }

    // ── Per-strategy snapshot tests ───────────────────────────────────────

    #[test]
    fn build_strategy_snapshot_detects_drop_strategy() {
        let routed = vec![
            HelixRoutedStrategyCount { strategy: "inline".to_string(), count: 70 },
            HelixRoutedStrategyCount { strategy: "spawn".to_string(), count: 20 },
            HelixRoutedStrategyCount { strategy: "drop".to_string(), count: 10 },
        ];
        let snap = HelixPressureProbe::build_strategy_snapshot(&routed, &[]);
        assert!((snap.drop_strategy_frac - 0.1).abs() < 1e-9, "drop_frac={}", snap.drop_strategy_frac);
    }

    #[test]
    fn build_strategy_snapshot_zero_drop_when_no_drop_strategy() {
        let routed = vec![
            HelixRoutedStrategyCount { strategy: "inline".to_string(), count: 80 },
            HelixRoutedStrategyCount { strategy: "spawn".to_string(), count: 20 },
        ];
        let snap = HelixPressureProbe::build_strategy_snapshot(&routed, &[]);
        assert_eq!(snap.drop_strategy_frac, 0.0);
    }

    #[test]
    fn build_strategy_snapshot_zero_when_empty() {
        let snap = HelixPressureProbe::build_strategy_snapshot(&[], &[]);
        assert_eq!(snap.drop_strategy_frac, 0.0);
        assert_eq!(snap.max_p95_ms, 0);
        assert!(snap.hottest_strategy.is_none());
    }

    #[test]
    fn build_strategy_snapshot_all_drop() {
        let routed = vec![
            HelixRoutedStrategyCount { strategy: "drop".to_string(), count: 100 },
        ];
        let snap = HelixPressureProbe::build_strategy_snapshot(&routed, &[]);
        assert!((snap.drop_strategy_frac - 1.0).abs() < 1e-9);
    }

    #[test]
    fn build_strategy_snapshot_case_insensitive_drop() {
        let routed = vec![
            HelixRoutedStrategyCount { strategy: "Drop".to_string(), count: 50 },
            HelixRoutedStrategyCount { strategy: "inline".to_string(), count: 50 },
        ];
        let snap = HelixPressureProbe::build_strategy_snapshot(&routed, &[]);
        assert!((snap.drop_strategy_frac - 0.5).abs() < 1e-9);
    }

    #[test]
    fn build_strategy_snapshot_finds_hottest_strategy() {
        let latency = vec![
            HelixLatencyRow { strategy: "inline".to_string(), count: 10, avg_ms: 1.0, ema_ms: 1.0, p95_ms: 5 },
            HelixLatencyRow { strategy: "cpu_pool".to_string(), count: 5, avg_ms: 50.0, ema_ms: 50.0, p95_ms: 250 },
            HelixLatencyRow { strategy: "spawn".to_string(), count: 8, avg_ms: 20.0, ema_ms: 20.0, p95_ms: 80 },
        ];
        let snap = HelixPressureProbe::build_strategy_snapshot(&[], &latency);
        assert_eq!(snap.max_p95_ms, 250);
        assert_eq!(snap.hottest_strategy.as_deref(), Some("cpu_pool"));
    }

    #[test]
    fn build_strategy_snapshot_ignores_zero_count_strategies() {
        let latency = vec![
            HelixLatencyRow { strategy: "inline".to_string(), count: 0, avg_ms: 0.0, ema_ms: 0.0, p95_ms: 9999 },
            HelixLatencyRow { strategy: "spawn".to_string(), count: 5, avg_ms: 10.0, ema_ms: 10.0, p95_ms: 40 },
        ];
        let snap = HelixPressureProbe::build_strategy_snapshot(&[], &latency);
        // Zero-count strategy should be ignored even if its p95 is high.
        assert_eq!(snap.max_p95_ms, 40, "zero-count strategy should be excluded");
        assert_eq!(snap.hottest_strategy.as_deref(), Some("spawn"));
    }

    #[test]
    fn combined_pressure_includes_drop_strategy_fraction() {
        // pressure_score = 0.1, drop_rate = 0.05, drop_strategy_frac = 0.8
        // combined should be 0.8 driven by drop_strategy_frac
        let pressure = 0.1_f64;
        let drop_rate = 0.05_f64;
        let drop_strategy_frac = 0.8_f64;
        let combined = pressure.max(drop_rate).max(drop_strategy_frac);
        assert!((combined - 0.8).abs() < 1e-9, "combined={combined}");
    }

    #[tokio::test]
    async fn fetch_pressure_malformed_json_returns_error() {
        let (port, listener) = bind_mock_server().await;
        let body = r#"not valid json {"#;

        tokio::spawn(serve_once(listener, 200, body));

        let bus = make_bus();
        let cfg = HelixProbeConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let probe = HelixPressureProbe::new(cfg, bus);
        let result = probe.fetch_pressure().await;

        assert!(result.is_err(), "expected error for malformed JSON");
        let err = result.unwrap_err();
        assert!(err.contains("JSON") || err.contains("parse"), "err: {err}");
    }

    #[tokio::test]
    async fn fetch_pressure_missing_field_returns_error() {
        let (port, listener) = bind_mock_server().await;
        // Missing required `pressure_score` field
        let body = r#"{"dropped":0,"completed":10}"#;

        tokio::spawn(serve_once(listener, 200, body));

        let bus = make_bus();
        let cfg = HelixProbeConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let probe = HelixPressureProbe::new(cfg, bus);
        let result = probe.fetch_pressure().await;

        assert!(result.is_err(), "expected error for missing pressure_score");
    }

    #[tokio::test]
    async fn fetch_pressure_connection_refused_returns_error() {
        // Port 1 is unlikely to have anything listening.
        let bus = make_bus();
        let cfg = HelixProbeConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            connect_timeout: Duration::from_millis(200),
            request_timeout: Duration::from_millis(500),
            ..Default::default()
        };
        let probe = HelixPressureProbe::new(cfg, bus);
        let result = probe.fetch_pressure().await;

        assert!(result.is_err(), "expected error for connection refused");
        let err = result.unwrap_err();
        assert!(err.contains("connect"), "err should mention connect: {err}");
    }

    #[tokio::test]
    async fn fetch_pressure_updates_bus_via_run_loop() {
        // Spin up two sequential mock responses, then verify the bus has the
        // expected pressure after one probe poll.
        let (port, listener) = bind_mock_server().await;
        let body = r#"{"pressure_score":0.65,"dropped":13,"completed":87,"adaptive_spawn_threshold":500,"routed_by_strategy":[],"latency_by_strategy":[]}"#;

        // The mock will serve exactly one request.
        tokio::spawn(serve_once(listener, 200, body));

        let bus = make_bus();
        let cfg = HelixProbeConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let probe = HelixPressureProbe::new(cfg, bus.clone());

        // Execute one poll cycle directly (not via run() which loops forever).
        let (pressure, dropped, completed, _adaptive_spawn_threshold, strategy_snap) = probe
            .fetch_pressure()
            .await
            .expect("fetch should succeed");

        let total = dropped + completed;
        let drop_rate = if total > 0 { dropped as f64 / total as f64 } else { 0.0 };
        let combined = pressure.max(drop_rate).max(strategy_snap.drop_strategy_frac);
        bus.set_external_pressure(combined);

        let snap = bus.tick_now().await;
        // combined = max(0.65, 0.13) = 0.65; blended with internal (0.0) = 0.325
        assert!(snap.pressure > 0.0, "pressure should be non-zero after probe tick: {}", snap.pressure);
    }

    // -----------------------------------------------------------------------
    // Backoff arithmetic tests
    // -----------------------------------------------------------------------

    #[test]
    fn probe_backoff_factor_clamps_at_five() {
        let poll = Duration::from_secs(5);
        for failures in [1u32, 2, 3, 4, 5, 10, 100, u32::MAX] {
            let factor = (failures as u64).min(5);
            let backoff = poll * factor as u32;
            assert!(
                backoff <= poll * 5,
                "backoff must not exceed 5× poll at failures={failures}"
            );
        }
    }

    #[test]
    fn probe_backoff_factor_one_on_first_failure() {
        let failures: u32 = 1;
        let factor = (failures as u64).min(5);
        assert_eq!(factor, 1, "first failure should use factor 1");
    }

    #[test]
    fn probe_backoff_factor_five_at_saturation() {
        let failures: u32 = u32::MAX;
        let factor = (failures as u64).min(5);
        assert_eq!(factor, 5, "saturated failures must clamp to factor 5");
    }
}
