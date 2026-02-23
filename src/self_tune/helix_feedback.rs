//! # HelixRouter Feedback Pusher (pressure-driven)
//!
//! ## Responsibility
//!
//! Closes the **two-way** cross-repo feedback loop between Tokio Prompt and
//! HelixRouter.  [`HelixPressureProbe`](super::helix_probe) *reads* HelixRouter's
//! pressure signal into the [`TelemetryBus`]; this module *writes* back a derived
//! configuration adjustment whenever the observed pipeline pressure crosses
//! configured thresholds.
//!
//! ## How it works
//!
//! On each telemetry tick the [`SelfImprovingLoop`] calls
//! [`HelixFeedbackPusher::maybe_push`] with the current blended pressure value.
//!
//! | Condition          | Patch sent to HelixRouter                         |
//! |--------------------|--------------------------------------------------|
//! | pressure ≥ 0.7     | Tighten: reduce `spawn_threshold` by 10 %;       |
//! |                    | raise `cpu_p95_budget_ms` by 25 % (tolerate       |
//! |                    | latency spikes rather than shedding load that     |
//! |                    | the orchestrator is already throttling).          |
//! | pressure ≤ 0.3     | Relax: restore `spawn_threshold` and              |
//! |                    | `cpu_p95_budget_ms` toward their defaults.        |
//! | 0.3 < pressure < 0.7 | No push — hysteresis band avoids thrashing.    |
//!
//! ## Guarantees
//!
//! - **Non-panicking**: all error paths return `Err` or are soft-logged.
//! - **Idempotent**: the same pressure tier never triggers two consecutive pushes.
//! - **Bounded**: the spawn_threshold clamp prevents values outside \[10k, 500k\].
//!
//! ## NOT Responsible For
//!
//! - Reading HelixRouter's pressure signal (see [`helix_probe`]).
//! - Pushing per-parameter tuning decisions derived from LiveTuning
//!   (see [`helix_config_pusher`](super::helix_config_pusher)).

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, info};

// ── Wire-format patch ──────────────────────────────────────────────────────────

/// Sparse config patch for HelixRouter's `PATCH /api/config`.
///
/// Only the fields that are driven by pipeline pressure are declared here.
#[derive(Debug, Clone, Default, Serialize)]
struct PressurePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    spawn_threshold: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_p95_budget_ms: Option<u64>,
}

impl PressurePatch {
    fn is_empty(&self) -> bool {
        self.spawn_threshold.is_none() && self.cpu_p95_budget_ms.is_none()
    }
}

// ── Pressure tier ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressureTier {
    High,   // ≥ high_threshold: tighten HelixRouter
    Normal, // inside hysteresis band: no action
    Low,    // ≤ low_threshold: relax HelixRouter
}

// ── Configuration ─────────────────────────────────────────────────────────────

/// Configuration for [`HelixFeedbackPusher`].
#[derive(Debug, Clone)]
pub struct HelixFeedbackConfig {
    /// Base URL of the HelixRouter HTTP API (e.g. `http://127.0.0.1:8080`).
    pub base_url: String,
    /// Pressure level above which the tighten patch is sent.
    pub high_threshold: f64,
    /// Pressure level below which the relax patch is sent.
    pub low_threshold: f64,
    /// Baseline spawn_threshold (HelixRouter default = 60_000).
    /// Used as the reference value when computing scaled adjustments.
    pub baseline_spawn_threshold: u64,
    /// Baseline cpu_p95_budget_ms (HelixRouter default = 200).
    pub baseline_cpu_p95_budget_ms: u64,
    /// TCP connection timeout.
    pub connect_timeout: Duration,
    /// Per-request write timeout.
    pub request_timeout: Duration,
}

impl Default for HelixFeedbackConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080".to_string(),
            high_threshold: 0.7,
            low_threshold: 0.3,
            baseline_spawn_threshold: 60_000,
            baseline_cpu_p95_budget_ms: 200,
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(10),
        }
    }
}

// ── Pusher ────────────────────────────────────────────────────────────────────

/// Per-tick pressure-driven config pusher.
///
/// Call [`maybe_push`](Self::maybe_push) on each telemetry tick.  The pusher
/// tracks the last tier it pushed and suppresses duplicate pushes within the
/// same tier (hysteresis).
pub struct HelixFeedbackPusher {
    config: HelixFeedbackConfig,
    client: reqwest::Client,
    /// Last tier for which a push was sent (guards against tier thrashing).
    last_tier: Mutex<Option<PressureTier>>,
}

impl HelixFeedbackPusher {
    /// Create a new pusher.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: HelixFeedbackConfig) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .build()
            .unwrap_or_default();

        Self {
            config,
            client,
            last_tier: Mutex::new(None),
        }
    }

    /// Evaluate `pressure` and, if the tier has changed, push a config patch.
    ///
    /// Returns `Ok(())` if no push was needed or the push succeeded.
    /// Returns `Err(String)` on an HTTP or connectivity failure.
    ///
    /// # Panics
    /// This function never panics.
    pub async fn maybe_push(&self, pressure: f64) -> Result<(), String> {
        let tier = self.classify(pressure);

        // Hysteresis: only push when the tier changes.
        {
            let mut last = self.last_tier.lock().unwrap_or_else(|p| p.into_inner());
            if *last == Some(tier) {
                return Ok(());
            }
            *last = Some(tier);
        }

        let patch = match tier {
            PressureTier::Normal => {
                // Tier changed to Normal but we don't push anything — just reset tracking.
                debug!(pressure, "helix_feedback: pressure normal, no patch");
                return Ok(());
            }
            PressureTier::High => self.tighten_patch(),
            PressureTier::Low => self.relax_patch(),
        };

        if patch.is_empty() {
            return Ok(());
        }

        self.push_patch(&patch, tier, pressure).await
    }

    fn classify(&self, pressure: f64) -> PressureTier {
        if pressure >= self.config.high_threshold {
            PressureTier::High
        } else if pressure <= self.config.low_threshold {
            PressureTier::Low
        } else {
            PressureTier::Normal
        }
    }

    /// Build a tighten patch: reduce spawn_threshold by 10 %, raise cpu_p95_budget_ms by 25 %.
    fn tighten_patch(&self) -> PressurePatch {
        let spawn = (self.config.baseline_spawn_threshold as f64 * 0.90) as u64;
        // Clamp to [10_000, 500_000] to stay within HelixRouter's valid range.
        let spawn = spawn.clamp(10_000, 500_000);
        let p95 = (self.config.baseline_cpu_p95_budget_ms as f64 * 1.25) as u64;
        // Keep budget in a reasonable range.
        let p95 = p95.clamp(50, 5_000);
        PressurePatch {
            spawn_threshold: Some(spawn),
            cpu_p95_budget_ms: Some(p95),
        }
    }

    /// Build a relax patch: restore spawn_threshold and cpu_p95_budget_ms to baseline.
    fn relax_patch(&self) -> PressurePatch {
        PressurePatch {
            spawn_threshold: Some(self.config.baseline_spawn_threshold),
            cpu_p95_budget_ms: Some(self.config.baseline_cpu_p95_budget_ms),
        }
    }

    async fn push_patch(
        &self,
        patch: &PressurePatch,
        tier: PressureTier,
        pressure: f64,
    ) -> Result<(), String> {
        let url = format!("{}/api/config", self.config.base_url);
        let resp = self
            .client
            .patch(&url)
            .header("Content-Type", "application/json")
            .json(patch)
            .send()
            .await
            .map_err(|e| format!("connect: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }

        info!(
            pressure,
            tier = ?tier,
            spawn_threshold = ?patch.spawn_threshold,
            cpu_p95_budget_ms = ?patch.cpu_p95_budget_ms,
            "helix_feedback: pushed config patch to HelixRouter"
        );

        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pusher() -> HelixFeedbackPusher {
        HelixFeedbackPusher::new(HelixFeedbackConfig::default())
    }

    // ── Config ────────────────────────────────────────────────────────────

    #[test]
    fn config_default_thresholds() {
        let cfg = HelixFeedbackConfig::default();
        assert!((cfg.high_threshold - 0.7).abs() < 1e-9);
        assert!((cfg.low_threshold - 0.3).abs() < 1e-9);
        assert!(cfg.high_threshold > cfg.low_threshold);
    }

    #[test]
    fn config_default_base_url() {
        let cfg = HelixFeedbackConfig::default();
        assert_eq!(cfg.base_url, "http://127.0.0.1:8080");
    }

    #[test]
    fn config_default_baselines() {
        let cfg = HelixFeedbackConfig::default();
        assert_eq!(cfg.baseline_spawn_threshold, 60_000);
        assert_eq!(cfg.baseline_cpu_p95_budget_ms, 200);
    }

    #[test]
    fn config_clone_is_independent() {
        let cfg = HelixFeedbackConfig::default();
        let mut cfg2 = cfg.clone();
        cfg2.base_url = "http://other:9090".to_string();
        assert_eq!(cfg.base_url, "http://127.0.0.1:8080");
    }

    // ── PressureTier classification ───────────────────────────────────────

    #[test]
    fn classify_high_threshold() {
        let p = make_pusher();
        assert_eq!(p.classify(0.7), PressureTier::High);
        assert_eq!(p.classify(0.9), PressureTier::High);
        assert_eq!(p.classify(1.0), PressureTier::High);
    }

    #[test]
    fn classify_low_threshold() {
        let p = make_pusher();
        assert_eq!(p.classify(0.3), PressureTier::Low);
        assert_eq!(p.classify(0.1), PressureTier::Low);
        assert_eq!(p.classify(0.0), PressureTier::Low);
    }

    #[test]
    fn classify_normal_band() {
        let p = make_pusher();
        assert_eq!(p.classify(0.5), PressureTier::Normal);
        assert_eq!(p.classify(0.4), PressureTier::Normal);
        assert_eq!(p.classify(0.69), PressureTier::Normal);
        assert_eq!(p.classify(0.31), PressureTier::Normal);
    }

    // ── Patch construction ────────────────────────────────────────────────

    #[test]
    fn tighten_patch_reduces_spawn_threshold() {
        let p = make_pusher();
        let patch = p.tighten_patch();
        let baseline = p.config.baseline_spawn_threshold;
        let t = patch.spawn_threshold.expect("spawn_threshold set");
        assert!(t < baseline, "tighten should reduce spawn_threshold: {t} < {baseline}");
        // Should be ~90% of baseline
        let expected = (baseline as f64 * 0.90) as u64;
        assert_eq!(t, expected.clamp(10_000, 500_000));
    }

    #[test]
    fn tighten_patch_raises_cpu_p95_budget() {
        let p = make_pusher();
        let patch = p.tighten_patch();
        let baseline = p.config.baseline_cpu_p95_budget_ms;
        let b = patch.cpu_p95_budget_ms.expect("cpu_p95_budget_ms set");
        assert!(b > baseline, "tighten should raise cpu_p95_budget_ms: {b} > {baseline}");
    }

    #[test]
    fn relax_patch_restores_baselines() {
        let p = make_pusher();
        let patch = p.relax_patch();
        assert_eq!(patch.spawn_threshold, Some(p.config.baseline_spawn_threshold));
        assert_eq!(patch.cpu_p95_budget_ms, Some(p.config.baseline_cpu_p95_budget_ms));
    }

    #[test]
    fn tighten_patch_is_not_empty() {
        let p = make_pusher();
        assert!(!p.tighten_patch().is_empty());
    }

    #[test]
    fn relax_patch_is_not_empty() {
        let p = make_pusher();
        assert!(!p.relax_patch().is_empty());
    }

    #[test]
    fn patch_default_is_empty() {
        assert!(PressurePatch::default().is_empty());
    }

    // ── Tighten patch spawn_threshold clamping ────────────────────────────

    #[test]
    fn tighten_clamps_very_low_baseline() {
        // With a tiny baseline, 90% could go below 10_000 — must clamp.
        let mut cfg = HelixFeedbackConfig::default();
        cfg.baseline_spawn_threshold = 5_000;
        let p = HelixFeedbackPusher::new(cfg);
        let patch = p.tighten_patch();
        let t = patch.spawn_threshold.expect("set");
        assert!(t >= 10_000, "must not go below 10_000: {t}");
    }

    #[test]
    fn tighten_clamps_very_high_baseline() {
        let mut cfg = HelixFeedbackConfig::default();
        cfg.baseline_spawn_threshold = 10_000_000;
        let p = HelixFeedbackPusher::new(cfg);
        let patch = p.tighten_patch();
        let t = patch.spawn_threshold.expect("set");
        assert!(t <= 500_000, "must not exceed 500_000: {t}");
    }

    // ── Hysteresis / idempotency ──────────────────────────────────────────

    #[tokio::test]
    async fn maybe_push_same_tier_twice_skips_second_push() {
        // We can't easily test the HTTP push here, but we CAN verify that
        // calling maybe_push with a connection-refused URL returns Ok(()) on
        // the second identical call (same tier → no HTTP attempt made).
        let mut cfg = HelixFeedbackConfig::default();
        cfg.base_url = "http://127.0.0.1:1".to_string(); // will error if attempted
        cfg.connect_timeout = Duration::from_millis(100);
        cfg.request_timeout = Duration::from_millis(200);
        let p = HelixFeedbackPusher::new(cfg);

        // First high-pressure call: will attempt push and fail.
        let first = p.maybe_push(0.9).await;
        // May be Ok or Err depending on whether port 1 refuses or times out.
        // Either way: the tier is now recorded as High.

        // Second call at same pressure: same tier → skipped → always Ok(()).
        let second = p.maybe_push(0.9).await;
        assert!(second.is_ok(), "second same-tier push should be skipped: {second:?}");
    }

    #[tokio::test]
    async fn maybe_push_normal_tier_returns_ok_without_http() {
        // Normal pressure never makes an HTTP call, so even a bad URL returns Ok(()).
        let mut cfg = HelixFeedbackConfig::default();
        cfg.base_url = "http://127.0.0.1:1".to_string();
        let p = HelixFeedbackPusher::new(cfg);

        let result = p.maybe_push(0.5).await;
        assert!(result.is_ok(), "normal pressure should return Ok without HTTP: {result:?}");
    }

    // ── Patch serialisation ───────────────────────────────────────────────

    #[test]
    fn pressure_patch_serialises_both_fields() {
        let patch = PressurePatch { spawn_threshold: Some(54_000), cpu_p95_budget_ms: Some(250) };
        let json = serde_json::to_string(&patch).expect("serialize");
        assert!(json.contains("spawn_threshold"), "json: {json}");
        assert!(json.contains("cpu_p95_budget_ms"), "json: {json}");
        assert!(json.contains("54000"), "json: {json}");
        assert!(json.contains("250"), "json: {json}");
    }

    #[test]
    fn pressure_patch_skips_none_fields() {
        let patch = PressurePatch { spawn_threshold: Some(54_000), cpu_p95_budget_ms: None };
        let json = serde_json::to_string(&patch).expect("serialize");
        assert!(!json.contains("cpu_p95_budget_ms"), "None field must be absent: {json}");
    }

    // ── Mock-HTTP integration tests ───────────────────────────────────────

    async fn bind_mock_server() -> (u16, tokio::net::TcpListener) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let port = listener.local_addr().expect("local_addr").port();
        (port, listener)
    }

    async fn serve_once_status(listener: tokio::net::TcpListener, status: u16) {
        use tokio::io::AsyncWriteExt;
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4096];
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
        )
        .await;
        let body = r#"{"inline_threshold":8000,"spawn_threshold":54000}"#;
        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            len = body.len(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    }

    #[tokio::test]
    async fn push_patch_succeeds_on_200() {
        let (port, listener) = bind_mock_server().await;
        tokio::spawn(serve_once_status(listener, 200));

        let mut cfg = HelixFeedbackConfig::default();
        cfg.base_url = format!("http://127.0.0.1:{port}");
        let p = HelixFeedbackPusher::new(cfg);
        let patch = p.tighten_patch();
        let result = p.push_patch(&patch, PressureTier::High, 0.8).await;
        assert!(result.is_ok(), "push should succeed on HTTP 200: {result:?}");
    }

    #[tokio::test]
    async fn push_patch_fails_on_500() {
        let (port, listener) = bind_mock_server().await;
        tokio::spawn(serve_once_status(listener, 500));

        let mut cfg = HelixFeedbackConfig::default();
        cfg.base_url = format!("http://127.0.0.1:{port}");
        let p = HelixFeedbackPusher::new(cfg);
        let patch = p.tighten_patch();
        let result = p.push_patch(&patch, PressureTier::High, 0.8).await;
        assert!(result.is_err(), "push should fail on HTTP 500");
        let err = result.unwrap_err();
        assert!(err.contains("500"), "err: {err}");
    }

    #[tokio::test]
    async fn maybe_push_high_pressure_sends_patch() {
        let (port, listener) = bind_mock_server().await;
        tokio::spawn(serve_once_status(listener, 200));

        let mut cfg = HelixFeedbackConfig::default();
        cfg.base_url = format!("http://127.0.0.1:{port}");
        let p = HelixFeedbackPusher::new(cfg);

        // High pressure should trigger tighten patch.
        let result = p.maybe_push(0.9).await;
        assert!(result.is_ok(), "high pressure push should succeed: {result:?}");
    }

    #[tokio::test]
    async fn maybe_push_low_pressure_sends_relax_patch() {
        let (port, listener) = bind_mock_server().await;
        tokio::spawn(serve_once_status(listener, 200));

        let mut cfg = HelixFeedbackConfig::default();
        cfg.base_url = format!("http://127.0.0.1:{port}");
        let p = HelixFeedbackPusher::new(cfg);

        // Low pressure should trigger relax patch.
        let result = p.maybe_push(0.1).await;
        assert!(result.is_ok(), "low pressure push should succeed: {result:?}");
    }
}
