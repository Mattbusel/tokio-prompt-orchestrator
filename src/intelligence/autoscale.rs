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
#![cfg(feature = "intelligence")]

//! # Stage: Autoscaler
//! Predictive load forecasting using exponential smoothing.
//! Recommends instance counts and buffer sizes ahead of traffic spikes.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum AutoscalerError {
    #[error("insufficient history: need {needed} points, have {have}")]
    InsufficientHistory { needed: usize, have: usize },
    #[error("forecast failed: {0}")]
    ForecastFailed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoscaleRecommendation {
    pub predicted_rps_5m: f64,
    pub predicted_rps_15m: f64,
    pub predicted_rps_60m: f64,
    pub recommended_local_instances: u32,
    pub recommended_channel_buffer: usize,
    pub prewarm_local: bool,
    pub rate_limit_ceiling: u32,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoscalerConfig {
    pub min_history_points: usize,
    pub smoothing_alpha: f64,
    pub spike_threshold_multiplier: f64,
    pub max_local_instances: u32,
}
impl Default for AutoscalerConfig {
    fn default() -> Self {
        Self {
            min_history_points: 12,
            smoothing_alpha: 0.3,
            spike_threshold_multiplier: 2.0,
            max_local_instances: 8,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadPoint {
    pub rps: f64,
    pub timestamp_ms: u64,
}

pub struct Autoscaler {
    history: Arc<Mutex<VecDeque<LoadPoint>>>,
    config: AutoscalerConfig,
    capacity: usize,
}

impl Autoscaler {
    pub fn new(config: AutoscalerConfig, history_capacity: usize) -> Self {
        Self {
            history: Arc::new(Mutex::new(VecDeque::with_capacity(history_capacity))),
            config,
            capacity: history_capacity,
        }
    }

    pub fn record_rps(&self, rps: f64) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let point = LoadPoint {
            rps,
            timestamp_ms: ts,
        };
        match self.history.lock() {
            Ok(mut guard) => {
                if guard.len() >= self.capacity {
                    guard.pop_front();
                }
                guard.push_back(point);
                debug!(rps, history_len = guard.len(), "rps recorded");
            }
            Err(e) => warn!(error = %e, "history lock poisoned in record_rps"),
        }
    }

    fn exponential_smooth(data: &[f64], alpha: f64) -> f64 {
        if data.is_empty() {
            return 0.0;
        }
        let mut smoothed = data[0];
        for &val in &data[1..] {
            smoothed = alpha * val + (1.0 - alpha) * smoothed;
        }
        smoothed
    }

    pub fn forecast(&self) -> Result<AutoscaleRecommendation, AutoscalerError> {
        let guard = match self.history.lock() {
            Ok(g) => g,
            Err(e) => return Err(AutoscalerError::ForecastFailed(e.to_string())),
        };
        let history_len = guard.len();
        if history_len < self.config.min_history_points {
            return Err(AutoscalerError::InsufficientHistory {
                needed: self.config.min_history_points,
                have: history_len,
            });
        }
        let all_rps: Vec<f64> = guard.iter().map(|p| p.rps).collect();
        let alpha = self.config.smoothing_alpha;
        let smoothed_5m = Self::exponential_smooth(&all_rps, alpha);
        let smoothed_15m = Self::exponential_smooth(&all_rps, alpha * 0.6);
        let smoothed_60m = Self::exponential_smooth(&all_rps, alpha * 0.2);
        let avg = all_rps.iter().sum::<f64>() / all_rps.len() as f64;
        let prewarm_local = smoothed_5m > avg * self.config.spike_threshold_multiplier;
        let raw_instances = (smoothed_5m / 10.0).ceil() as u32;
        let recommended_local_instances = raw_instances.min(self.config.max_local_instances).max(1);
        let recommended_channel_buffer = (smoothed_5m * 2.0).ceil() as usize + 64;
        let rate_limit_ceiling = (smoothed_60m * 3.0).ceil() as u32 + 100;
        let confidence =
            (history_len as f64 / (self.config.min_history_points as f64 * 2.0)).min(1.0);
        let rec = AutoscaleRecommendation {
            predicted_rps_5m: smoothed_5m,
            predicted_rps_15m: smoothed_15m,
            predicted_rps_60m: smoothed_60m,
            recommended_local_instances,
            recommended_channel_buffer,
            prewarm_local,
            rate_limit_ceiling,
            confidence,
        };
        info!(
            predicted_rps_5m = smoothed_5m,
            instances = recommended_local_instances,
            "autoscale forecast"
        );
        Ok(rec)
    }

    pub fn current_avg_rps(&self) -> f64 {
        match self.history.lock() {
            Ok(guard) => {
                if guard.is_empty() {
                    return 0.0;
                }
                guard.iter().map(|p| p.rps).sum::<f64>() / guard.len() as f64
            }
            Err(e) => {
                warn!(error = %e, "lock poisoned in current_avg_rps");
                0.0
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn make_scaler(min_pts: usize) -> Autoscaler {
        let config = AutoscalerConfig {
            min_history_points: min_pts,
            ..AutoscalerConfig::default()
        };
        Autoscaler::new(config, 200)
    }

    #[test]
    fn test_default_min_history_points_is_12() {
        assert_eq!(AutoscalerConfig::default().min_history_points, 12);
    }

    #[test]
    fn test_default_max_local_instances_is_8() {
        assert_eq!(AutoscalerConfig::default().max_local_instances, 8);
    }

    #[test]
    fn test_forecast_fails_with_insufficient_history() {
        let s = make_scaler(12);
        assert!(matches!(
            s.forecast(),
            Err(AutoscalerError::InsufficientHistory { .. })
        ));
    }

    #[test]
    fn test_record_rps_adds_to_history() {
        let s = make_scaler(1);
        s.record_rps(10.0);
        assert!(s.history.lock().unwrap().len() == 1);
    }

    #[test]
    fn test_record_rps_trims_to_capacity() {
        let s = Autoscaler::new(AutoscalerConfig::default(), 3);
        s.record_rps(1.0);
        s.record_rps(2.0);
        s.record_rps(3.0);
        s.record_rps(4.0);
        assert_eq!(s.history.lock().unwrap().len(), 3);
    }

    #[test]
    fn test_current_avg_rps_empty_returns_zero() {
        let s = make_scaler(12);
        assert_eq!(s.current_avg_rps(), 0.0);
    }

    #[test]
    fn test_current_avg_rps_computes_mean() {
        let s = make_scaler(1);
        s.record_rps(10.0);
        s.record_rps(20.0);
        assert!((s.current_avg_rps() - 15.0).abs() < 0.001);
    }

    #[test]
    fn test_forecast_succeeds_with_enough_history() {
        let s = make_scaler(3);
        for _ in 0..6 {
            s.record_rps(20.0);
        }
        assert!(s.forecast().is_ok());
    }

    #[test]
    fn test_forecast_confidence_in_unit_range() {
        let s = make_scaler(3);
        for _ in 0..6 {
            s.record_rps(10.0);
        }
        let rec = s.forecast().unwrap();
        assert!(rec.confidence >= 0.0 && rec.confidence <= 1.0);
    }

    #[test]
    fn test_forecast_instances_at_least_one() {
        let s = make_scaler(3);
        for _ in 0..6 {
            s.record_rps(0.1);
        }
        let rec = s.forecast().unwrap();
        assert!(rec.recommended_local_instances >= 1);
    }

    #[test]
    fn test_forecast_instances_capped_at_max() {
        let s = make_scaler(3);
        for _ in 0..6 {
            s.record_rps(10000.0);
        }
        let rec = s.forecast().unwrap();
        assert!(rec.recommended_local_instances <= AutoscalerConfig::default().max_local_instances);
    }

    #[test]
    fn test_forecast_prewarm_triggered_by_spike() {
        let s = make_scaler(3);
        // Low baseline first, then spike
        for _ in 0..5 {
            s.record_rps(1.0);
        }
        s.record_rps(1000.0);
        let rec = s.forecast().unwrap();
        assert!(rec.prewarm_local);
    }

    #[test]
    fn test_forecast_buffer_size_positive() {
        let s = make_scaler(3);
        for _ in 0..6 {
            s.record_rps(5.0);
        }
        let rec = s.forecast().unwrap();
        assert!(rec.recommended_channel_buffer > 0);
    }
}
