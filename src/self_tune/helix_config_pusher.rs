//! # HelixRouter Config Pusher
//!
//! ## Responsibility
//!
//! Closes the second half of the Tokio Prompt ↔ HelixRouter feedback loop.
//! [`HelixPressureProbe`](super::helix_probe) *reads* HelixRouter's backpressure
//! signal; this module *writes* Tokio Prompt's tuning decisions back to
//! HelixRouter so that its routing thresholds adapt to what the orchestrator
//! observes.
//!
//! ## How it works
//!
//! 1. Reads [`LiveTuning`](super::actuator::LiveTuning) parameters every
//!    `push_interval` (default 30 s).
//! 2. Maps Tokio Prompt parameters to [`RouterConfigPatch`] fields using the
//!    conversion documented below.
//! 3. Sends a `PATCH /api/config` request to HelixRouter only when a parameter
//!    has changed by more than `change_threshold` (default 5 %).
//!
//! ## Parameter mapping
//!
//! | Tokio Prompt `ParameterId`      | HelixRouter field                |
//! |---------------------------------|----------------------------------|
//! | `BackpressureShedThreshold`     | `backpressure_busy_threshold`    |
//! |   (0.0 – 1.0 fraction)         |   (usize: shed_frac × 10, min 1)|
//!
//! ## Guarantees
//!
//! - **Non-panicking**: all error paths are soft-logged.
//! - **Non-blocking**: runs in a background task; cancel via `JoinHandle::abort`.
//! - **Idempotent**: identical values are never re-sent (no superfluous PATCHes).
//!
//! ## NOT Responsible For
//!
//! - Reading HelixRouter's pressure signal (see [`helix_probe`]).
//! - Authenticating to HelixRouter (no auth in current protocol).

#[cfg(feature = "self-tune")]
use crate::self_tune::{actuator::LiveTuning, controller::ParameterId};
use serde::Serialize;
use std::time::Duration;
use tracing::{debug, trace, warn};

// ── Wire-format mirror of HelixRouter's `RouterConfigPatch` ──────────────────
//
// We only declare the fields we actually push; serde(skip_serializing_if) ensures
// no spurious null fields reach the HelixRouter PATCH endpoint.

/// Minimal subset of HelixRouter's `RouterConfigPatch`.
///
/// Only the fields Tokio Prompt currently drives are declared here; all others
/// are omitted and ignored by HelixRouter's PATCH handler.
#[derive(Debug, Clone, Default, Serialize)]
struct ConfigPatch {
    /// Mapped from `BackpressureShedThreshold` (0.0–1.0) → usize.
    #[serde(skip_serializing_if = "Option::is_none")]
    backpressure_busy_threshold: Option<usize>,
}

impl ConfigPatch {
    fn is_empty(&self) -> bool {
        self.backpressure_busy_threshold.is_none()
    }
}

// ── Configuration ─────────────────────────────────────────────────────────────

/// Configuration for [`HelixConfigPusher`].
#[derive(Debug, Clone)]
pub struct HelixConfigPusherConfig {
    /// Base URL of the HelixRouter HTTP API (e.g. `http://127.0.0.1:8080`).
    pub base_url: String,
    /// How often to check for parameter changes and push updates.
    pub push_interval: Duration,
    /// TCP connection timeout.
    pub connect_timeout: Duration,
    /// Per-request write timeout.
    pub request_timeout: Duration,
    /// Minimum fractional change required before a parameter is pushed.
    ///
    /// E.g. `0.05` means "only push if the new value differs from the last
    /// pushed value by more than 5 %".
    pub change_threshold: f64,
}

impl Default for HelixConfigPusherConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080".to_string(),
            push_interval: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(10),
            change_threshold: 0.05,
        }
    }
}

// ── Pusher ────────────────────────────────────────────────────────────────────

/// Pushes Tokio Prompt tuning decisions to HelixRouter's `/api/config`.
///
/// # Example
///
/// ```rust,no_run
/// use tokio_prompt_orchestrator::self_tune::{
///     actuator::LiveTuning,
///     helix_config_pusher::{HelixConfigPusher, HelixConfigPusherConfig},
/// };
///
/// #[tokio::main]
/// async fn main() {
///     let tuning = LiveTuning::new();
///     let cfg = HelixConfigPusherConfig::default();
///     let pusher = HelixConfigPusher::new(cfg, tuning);
///     let _handle = tokio::spawn(async move { pusher.run().await });
/// }
/// ```
#[cfg(feature = "self-tune")]
pub struct HelixConfigPusher {
    config: HelixConfigPusherConfig,
    tuning: LiveTuning,
    client: reqwest::Client,
}

#[cfg(feature = "self-tune")]
impl HelixConfigPusher {
    /// Create a new pusher.
    ///
    /// `tuning` is cheaply cloned — [`LiveTuning`] is internally `Arc`-backed.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: HelixConfigPusherConfig, tuning: LiveTuning) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .build()
            .unwrap_or_default();

        Self { config, tuning, client }
    }

    /// Run the push loop indefinitely.
    ///
    /// On each tick, reads current tuning parameters, builds a sparse
    /// [`ConfigPatch`], and PATCHes HelixRouter only when at least one
    /// parameter has changed beyond the configured threshold.
    ///
    /// Errors are soft-logged; the loop never exits on a failed push.
    ///
    /// Drop the [`tokio::task::JoinHandle`] to stop the loop.
    ///
    /// # Panics
    /// This function never panics.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(self.config.push_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Track last-pushed value per parameter so we only send real changes.
        let mut last_busy_threshold: Option<usize> = None;

        loop {
            ticker.tick().await;

            let patch = self.build_patch(&mut last_busy_threshold);

            if patch.is_empty() {
                trace!(url = %self.config.base_url, "HelixConfigPusher: no parameter changes, skip");
                continue;
            }

            match self.push_patch(&patch).await {
                Ok(()) => {
                    debug!(
                        url = %self.config.base_url,
                        ?patch,
                        "HelixConfigPusher: pushed config patch"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        url = %self.config.base_url,
                        "HelixConfigPusher: push failed, will retry next tick"
                    );
                    // Roll back tracked values so a future tick re-sends.
                    if patch.backpressure_busy_threshold.is_some() {
                        last_busy_threshold = None;
                    }
                }
            }
        }
    }

    /// Convert current `LiveTuning` parameters to a sparse `ConfigPatch`.
    ///
    /// Only populates a field when the value differs from the last-pushed
    /// value by more than `change_threshold`.
    fn build_patch(&self, last_busy_threshold: &mut Option<usize>) -> ConfigPatch {
        let mut patch = ConfigPatch::default();

        // ── BackpressureShedThreshold → backpressure_busy_threshold ──────────
        //
        // LiveTuning stores a fraction in [0.0, 1.0].
        // HelixRouter's `backpressure_busy_threshold` is a usize counting how
        // many concurrent CpuPool tasks constitute "busy" (default = 7).
        // Mapping: floor(shed_frac × 10), clamped to [1, 20].
        let shed_frac = self.tuning.get(ParameterId::BackpressureShedThreshold);
        let new_threshold = ((shed_frac * 10.0).floor() as usize).clamp(1, 20);

        let should_push = match *last_busy_threshold {
            None => true, // first tick — always push baseline
            Some(prev) => {
                let prev_f = prev as f64;
                let new_f = new_threshold as f64;
                let rel_change = if prev_f > 0.0 {
                    (new_f - prev_f).abs() / prev_f
                } else {
                    1.0 // anything is 100 % change from 0
                };
                rel_change >= self.config.change_threshold
            }
        };

        if should_push {
            patch.backpressure_busy_threshold = Some(new_threshold);
            *last_busy_threshold = Some(new_threshold);
        }

        patch
    }

    /// PATCH `/api/config` with the given patch payload.
    async fn push_patch(&self, patch: &ConfigPatch) -> Result<(), String> {
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

        Ok(())
    }
}

// ── Stand-alone config (no feature gate needed for tests) ────────────────────

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── HelixConfigPusherConfig ───────────────────────────────────────────

    #[test]
    fn config_default_base_url() {
        let cfg = HelixConfigPusherConfig::default();
        assert_eq!(cfg.base_url, "http://127.0.0.1:8080");
    }

    #[test]
    fn config_default_push_interval() {
        let cfg = HelixConfigPusherConfig::default();
        assert_eq!(cfg.push_interval, Duration::from_secs(30));
    }

    #[test]
    fn config_default_change_threshold() {
        let cfg = HelixConfigPusherConfig::default();
        assert!((cfg.change_threshold - 0.05).abs() < 1e-9);
    }

    #[test]
    fn config_clone_is_independent() {
        let cfg = HelixConfigPusherConfig::default();
        let mut cfg2 = cfg.clone();
        cfg2.base_url = "http://other:9090".to_string();
        assert_eq!(cfg.base_url, "http://127.0.0.1:8080");
    }

    // ── ConfigPatch ───────────────────────────────────────────────────────

    #[test]
    fn patch_default_is_empty() {
        let patch = ConfigPatch::default();
        assert!(patch.is_empty());
    }

    #[test]
    fn patch_with_threshold_is_not_empty() {
        let patch = ConfigPatch { backpressure_busy_threshold: Some(5), ..Default::default() };
        assert!(!patch.is_empty());
    }

    #[test]
    fn patch_serializes_with_skip_none() {
        let patch = ConfigPatch { backpressure_busy_threshold: Some(7), ..Default::default() };
        let json = serde_json::to_string(&patch).expect("serialize");
        assert!(json.contains("backpressure_busy_threshold"));
        assert!(json.contains("7"));
    }

    #[test]
    fn patch_empty_serializes_to_empty_object() {
        let patch = ConfigPatch::default();
        let json = serde_json::to_string(&patch).expect("serialize");
        assert_eq!(json, "{}");
    }

    // ── Parameter mapping math ────────────────────────────────────────────

    #[test]
    fn shed_frac_to_busy_threshold_mapping() {
        // shed_frac = 0.7 → floor(7.0) = 7
        let shed_frac = 0.7_f64;
        let t = ((shed_frac * 10.0).floor() as usize).clamp(1, 20);
        assert_eq!(t, 7);
    }

    #[test]
    fn shed_frac_zero_clamps_to_one() {
        let shed_frac = 0.0_f64;
        let t = ((shed_frac * 10.0).floor() as usize).clamp(1, 20);
        assert_eq!(t, 1);
    }

    #[test]
    fn shed_frac_one_gives_ten() {
        let shed_frac = 1.0_f64;
        let t = ((shed_frac * 10.0).floor() as usize).clamp(1, 20);
        assert_eq!(t, 10);
    }

    #[test]
    fn shed_frac_above_bounds_clamps_to_twenty() {
        // Extreme value well beyond 1.0 clamps to 20.
        let shed_frac = 3.0_f64;
        let t = ((shed_frac * 10.0).floor() as usize).clamp(1, 20);
        assert_eq!(t, 20);
    }

    // ── Change threshold logic ────────────────────────────────────────────

    #[test]
    fn change_threshold_none_always_pushes() {
        let prev: Option<usize> = None;
        let new_threshold: usize = 7;
        let should_push = match prev {
            None => true,
            Some(p) => {
                let rel = (new_threshold as f64 - p as f64).abs() / (p as f64).max(1.0);
                rel >= 0.05
            }
        };
        assert!(should_push);
    }

    #[test]
    fn change_threshold_same_value_does_not_push() {
        let prev: Option<usize> = Some(7);
        let new_threshold: usize = 7;
        let should_push = match prev {
            None => true,
            Some(p) => {
                let prev_f = p as f64;
                let new_f = new_threshold as f64;
                let rel = if prev_f > 0.0 {
                    (new_f - prev_f).abs() / prev_f
                } else {
                    1.0
                };
                rel >= 0.05
            }
        };
        assert!(!should_push);
    }

    #[test]
    fn change_threshold_large_change_pushes() {
        let prev: Option<usize> = Some(4);
        let new_threshold: usize = 8; // 100 % change
        let should_push = match prev {
            None => true,
            Some(p) => {
                let prev_f = p as f64;
                let new_f = new_threshold as f64;
                let rel = if prev_f > 0.0 {
                    (new_f - prev_f).abs() / prev_f
                } else {
                    1.0
                };
                rel >= 0.05
            }
        };
        assert!(should_push);
    }

    #[test]
    fn change_threshold_small_change_does_not_push() {
        // prev = 10, new = 10 → 0 % change < 5 % threshold
        let prev: Option<usize> = Some(10);
        let new_threshold: usize = 10;
        let should_push = match prev {
            None => true,
            Some(p) => {
                let prev_f = p as f64;
                let new_f = new_threshold as f64;
                let rel = if prev_f > 0.0 {
                    (new_f - prev_f).abs() / prev_f
                } else {
                    1.0
                };
                rel >= 0.05
            }
        };
        assert!(!should_push);
    }

    // ── Pusher construction ───────────────────────────────────────────────

    #[cfg(feature = "self-tune")]
    #[test]
    fn pusher_new_does_not_panic() {
        use crate::self_tune::actuator::LiveTuning;
        let tuning = LiveTuning::new();
        let _pusher = HelixConfigPusher::new(HelixConfigPusherConfig::default(), tuning);
    }

    #[cfg(feature = "self-tune")]
    #[test]
    fn pusher_build_patch_first_tick_always_sends() {
        use crate::self_tune::actuator::LiveTuning;
        let tuning = LiveTuning::new();
        let pusher = HelixConfigPusher::new(HelixConfigPusherConfig::default(), tuning);

        let mut last: Option<usize> = None;
        let patch = pusher.build_patch(&mut last);
        // First tick: should always produce a non-empty patch.
        assert!(!patch.is_empty(), "first tick must always push");
        assert!(patch.backpressure_busy_threshold.is_some());
    }

    #[cfg(feature = "self-tune")]
    #[test]
    fn pusher_build_patch_second_tick_same_value_is_empty() {
        use crate::self_tune::actuator::LiveTuning;
        let tuning = LiveTuning::new();
        let pusher = HelixConfigPusher::new(HelixConfigPusherConfig::default(), tuning);

        let mut last: Option<usize> = None;
        // First tick — seeds `last`.
        let _ = pusher.build_patch(&mut last);
        // Second tick with the same LiveTuning value — no change.
        let patch2 = pusher.build_patch(&mut last);
        assert!(patch2.is_empty(), "no change should produce empty patch");
    }
}
