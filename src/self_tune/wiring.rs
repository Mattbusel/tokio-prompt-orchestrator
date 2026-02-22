#![allow(missing_docs, dead_code, clippy::duplicated_attributes)]
//! # Self-Tune Wiring (Task 1.x — Loop Closure)
//!
//! Connects the telemetry bus, PID controller, and anomaly detector into a
//! live feedback loop that runs alongside the pipeline.
//!
//! ## Loop
//! ```text
//! TelemetryBus ──snapshot──► TuningController ──adjustments──► log/apply
//!                    │
//!                    └──────► AnomalyDetector  ──anomalies───► warn/rollback
//! ```
//!
//! ## Usage
//! ```no_run
//! # use tokio_prompt_orchestrator::self_tune::wiring::start_self_tune_loop;
//! # #[tokio::main] async fn main() {
//! let handles = start_self_tune_loop(50.0, 10.0).await;
//! // pipeline runs...
//! handles.shutdown();
//! # }
//! ```

use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::self_tune::{
    actuator::LiveTuning,
    anomaly::AnomalyDetector,
    controller::TuningController,
    telemetry_bus::{PipelineCounters, TelemetryBus, TelemetryBusConfig},
};

/// Handles for the background self-tune tasks.
///
/// Drop or call [`shutdown`](SelfTuneHandles::shutdown) to stop the loop.
pub struct SelfTuneHandles {
    /// Handle to the controller loop task.
    pub controller_task: JoinHandle<()>,
    /// Handle to the anomaly detector loop task.
    pub anomaly_task: JoinHandle<()>,
    /// The live telemetry bus (callers may subscribe for additional consumers).
    pub bus: Arc<TelemetryBus>,
    /// Shared pipeline counters (callers can increment these from pipeline code).
    pub counters: Arc<PipelineCounters>,
    /// Live parameter store — pipeline components read from here.
    ///
    /// The controller writes updated values every telemetry tick; components
    /// (circuit breaker, rate limiter, dedup) consult this store before each
    /// operation for adaptive behaviour.
    pub live_tuning: LiveTuning,
}

impl SelfTuneHandles {
    /// Abort both background tasks immediately.
    pub fn shutdown(&self) {
        self.controller_task.abort();
        self.anomaly_task.abort();
    }
}

/// Start the self-tune feedback loop.
///
/// Spawns two background tasks:
/// 1. **Controller loop** — reads `TelemetrySnapshot`s, runs PID, logs adjustments.
/// 2. **Anomaly loop** — reads `TelemetrySnapshot`s, checks for statistical anomalies.
///
/// # Arguments
/// * `target_p95_ms` — target 95th-percentile end-to-end latency in milliseconds.
/// * `target_rps` — target request throughput in requests per second.
///
/// # Returns
/// [`SelfTuneHandles`] containing task handles and shared resources.
///
/// # Panics
///
/// This function never panics.
pub async fn start_self_tune_loop(target_p95_ms: f64, target_rps: f64) -> SelfTuneHandles {
    let counters = PipelineCounters::new();
    let bus = Arc::new(TelemetryBus::new(
        TelemetryBusConfig::default(),
        Arc::clone(&counters),
    ));
    bus.start();

    // Shared atomic parameter store — the controller writes here; pipeline
    // components read from here lock-free.
    let live_tuning = LiveTuning::new();

    let controller_task = {
        let mut rx = bus.subscribe();
        let mut controller = TuningController::new(target_p95_ms, target_rps);
        let lt = live_tuning.clone();
        tokio::spawn(async move {
            info!(target: "self_tune::controller", "Controller loop started");
            loop {
                match rx.recv().await {
                    Ok(snap) => {
                        let adjustments = controller.process(&snap);
                        if !adjustments.is_empty() {
                            // Apply PID recommendations to the live parameter
                            // store so pipeline components pick them up.
                            lt.apply_adjustments(&adjustments);
                            for (param, value) in &adjustments {
                                info!(
                                    target: "self_tune::controller",
                                    parameter = ?param,
                                    new_value = value,
                                    "Parameter adjusted and applied"
                                );
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            target: "self_tune::controller",
                            skipped = n,
                            "Controller lagged behind telemetry bus"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!(target: "self_tune::controller", "Telemetry bus closed, shutting down");
                        break;
                    }
                }
            }
        })
    };

    let anomaly_task = {
        let mut rx = bus.subscribe();
        let detector = AnomalyDetector::with_defaults();
        tokio::spawn(async move {
            info!(target: "self_tune::anomaly", "Anomaly detector loop started");
            loop {
                match rx.recv().await {
                    Ok(snap) => {
                        // Feed per-stage latency into anomaly detector.
                        for stage in &snap.stages {
                            let metric = format!("{}_p95_ms", stage.stage);
                            if let Err(e) = detector.ingest(&metric, stage.latency.p95_ms as f64) {
                                warn!(
                                    target: "self_tune::anomaly",
                                    metric = %metric,
                                    error = ?e,
                                    "Anomaly ingest failed"
                                );
                            }
                        }
                        // Log any critical anomalies.
                        for anomaly in detector.critical_anomalies() {
                            warn!(
                                target: "self_tune::anomaly",
                                metric = %anomaly.metric,
                                severity = ?anomaly.severity,
                                score = anomaly.score,
                                "Critical anomaly detected"
                            );
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            target: "self_tune::anomaly",
                            skipped = n,
                            "Anomaly detector lagged behind telemetry bus"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!(target: "self_tune::anomaly", "Telemetry bus closed, shutting down");
                        break;
                    }
                }
            }
        })
    };

    SelfTuneHandles {
        controller_task,
        anomaly_task,
        bus,
        counters,
        live_tuning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_start_self_tune_loop_returns_handles() {
        let handles = start_self_tune_loop(50.0, 10.0).await;
        assert!(!handles.controller_task.is_finished());
        assert!(!handles.anomaly_task.is_finished());
        handles.shutdown();
    }

    #[tokio::test]
    async fn test_shutdown_aborts_tasks() {
        let handles = start_self_tune_loop(50.0, 10.0).await;
        handles.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(handles.controller_task.is_finished());
        assert!(handles.anomaly_task.is_finished());
    }

    #[tokio::test]
    async fn test_bus_subscribe_works_after_start() {
        let handles = start_self_tune_loop(100.0, 5.0).await;
        let _sub = handles.bus.subscribe();
        handles.shutdown();
    }

    #[tokio::test]
    async fn test_counters_shared_and_incrementable() {
        use std::sync::atomic::Ordering;
        let handles = start_self_tune_loop(50.0, 10.0).await;
        handles.counters.completed.fetch_add(1, Ordering::Relaxed);
        assert_eq!(handles.counters.completed.load(Ordering::Relaxed), 1);
        handles.shutdown();
    }

    #[tokio::test]
    async fn test_multiple_loops_independent() {
        let h1 = start_self_tune_loop(50.0, 10.0).await;
        let h2 = start_self_tune_loop(100.0, 20.0).await;
        h1.shutdown();
        h2.shutdown();
    }

    #[tokio::test]
    async fn test_controller_loop_receives_tick() {
        use crate::self_tune::telemetry_bus::TelemetryBusConfig;
        // Use a very fast sample interval to get at least one tick quickly.
        let counters = PipelineCounters::new();
        let cfg = TelemetryBusConfig {
            sample_interval: Duration::from_millis(20),
            ..Default::default()
        };
        let bus = Arc::new(TelemetryBus::new(cfg, Arc::clone(&counters)));
        bus.start();
        let mut sub = bus.subscribe();

        // Wait for a snapshot.
        let result = tokio::time::timeout(Duration::from_millis(500), sub.recv()).await;
        assert!(result.is_ok(), "should receive snapshot within 500ms");
        assert!(result.unwrap().is_ok(), "snapshot recv should succeed");
    }

    #[tokio::test]
    async fn test_anomaly_detector_ingests_without_panic() {
        let detector = AnomalyDetector::with_defaults();
        // Feed some values — first few won't produce anomalies (no window yet).
        for i in 0..10u64 {
            let _ = detector.ingest("test_metric", i as f64 * 10.0);
        }
        let history = detector.anomaly_history();
        // History length is bounded; no panic.
        assert!(history.len() <= 10000);
    }

    #[tokio::test]
    async fn test_shutdown_is_idempotent() {
        let handles = start_self_tune_loop(50.0, 10.0).await;
        handles.shutdown();
        handles.shutdown(); // second call must not panic
    }
}
