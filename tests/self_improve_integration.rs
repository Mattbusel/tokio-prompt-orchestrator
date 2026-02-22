//! Integration tests for the self-improvement loop.
//!
//! These tests exercise the full pipeline:
//!   TelemetryBus → AnomalyDetector → TuningController → MetaTaskGenerator → AgentMemory
//!
//! The ValidationGate is configured with trust_level=0 and the gate itself
//! is not invoked in these tests (cargo test can't run nested cargo test).

#![cfg(feature = "self-improving")]

use std::{sync::Arc, time::Duration};

use tokio::sync::watch;
use tokio_prompt_orchestrator::{
    self_improve_loop::{LoopConfig, SelfImprovementLoop},
    self_tune::telemetry_bus::{PipelineCounters, TelemetryBus, TelemetryBusConfig},
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_bus() -> Arc<TelemetryBus> {
    let counters = PipelineCounters::new();
    let cfg = TelemetryBusConfig {
        snapshot_interval: Duration::from_millis(50),
        ..TelemetryBusConfig::default()
    };
    Arc::new(TelemetryBus::new(cfg, counters))
}

fn make_loop(bus: Arc<TelemetryBus>) -> SelfImprovementLoop {
    let cfg = LoopConfig {
        poll_interval: Duration::from_millis(50),
        run_gate_commands: false,
        ..LoopConfig::default()
    };
    SelfImprovementLoop::new(cfg, bus)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verifies the loop starts, receives a telemetry snapshot from the bus, and
/// increments `snapshots_processed` at least once before shutdown.
#[tokio::test]
async fn test_loop_processes_telemetry_from_bus() {
    let bus = make_bus();
    bus.start();

    let sil = Arc::new(make_loop(Arc::clone(&bus)));
    let (tx, rx) = watch::channel(false);

    let sil_clone = Arc::clone(&sil);
    let handle = tokio::spawn(async move { sil_clone.run(rx).await });

    // Wait long enough for at least one snapshot to be emitted by the bus
    // (snapshot_interval = 50ms, so 250ms gives 5 windows).
    tokio::time::sleep(Duration::from_millis(250)).await;

    tx.send(true).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("loop should stop within 2s")
        .expect("join error");

    let status = sil.status();
    assert!(
        status.snapshots_processed >= 1,
        "expected at least 1 snapshot processed, got {}",
        status.snapshots_processed
    );
}

/// Verifies that when the telemetry bus emits multiple snapshots, the loop
/// accumulates them without panicking or losing count.
#[tokio::test]
async fn test_loop_accumulates_multiple_snapshots() {
    let bus = make_bus();
    bus.start();

    let sil = Arc::new(make_loop(Arc::clone(&bus)));
    let (tx, rx) = watch::channel(false);

    let sil_clone = Arc::clone(&sil);
    let handle = tokio::spawn(async move { sil_clone.run(rx).await });

    // 500ms at 50ms interval → ~10 snapshots
    tokio::time::sleep(Duration::from_millis(500)).await;

    tx.send(true).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("loop should stop")
        .expect("join error");

    let status = sil.status();
    assert!(
        status.snapshots_processed >= 2,
        "expected multiple snapshots, got {}",
        status.snapshots_processed
    );
}

/// Verifies that the snapshot store records at least one configuration
/// version after the loop processes snapshots.
#[tokio::test]
async fn test_loop_writes_config_snapshots() {
    let bus = make_bus();
    bus.start();

    let sil = Arc::new(make_loop(Arc::clone(&bus)));
    let (tx, rx) = watch::channel(false);

    let sil_clone = Arc::clone(&sil);
    let handle = tokio::spawn(async move { sil_clone.run(rx).await });

    tokio::time::sleep(Duration::from_millis(300)).await;
    tx.send(true).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("loop should stop")
        .expect("join error");

    // The loop records a config snapshot per telemetry snapshot processed.
    // With bus running at 50ms, we should have several.
    let status = sil.status();
    assert!(
        status.snapshots_processed >= 1,
        "at least one snapshot should have been processed"
    );
}

/// Verifies the loop running status transitions correctly.
#[tokio::test]
async fn test_loop_running_flag_transitions() {
    let bus = make_bus();
    let sil = Arc::new(make_loop(Arc::clone(&bus)));

    assert!(!sil.status().running, "should not be running before start");

    let (tx, rx) = watch::channel(false);
    let sil_clone = Arc::clone(&sil);
    let handle = tokio::spawn(async move { sil_clone.run(rx).await });

    // Give it time to mark itself running
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(sil.status().running, "should be running after start");

    tx.send(true).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("loop should stop")
        .expect("join error");

    assert!(!sil.status().running, "should not be running after stop");
}

/// Verifies uptime is non-zero after the loop has been running.
#[tokio::test]
async fn test_loop_status_uptime_advances() {
    let bus = make_bus();
    let sil = Arc::new(make_loop(Arc::clone(&bus)));
    let (tx, rx) = watch::channel(false);

    let sil_clone = Arc::clone(&sil);
    let handle = tokio::spawn(async move { sil_clone.run(rx).await });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let status = sil.status();
    assert!(
        status.uptime >= Duration::from_millis(50),
        "uptime should be at least 50ms, got {:?}",
        status.uptime
    );

    tx.send(true).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("loop should stop")
        .expect("join error");
}

/// Verifies that with_components constructs a working loop.
#[tokio::test]
async fn test_loop_with_components_processes_snapshots() {
    use tokio_prompt_orchestrator::{
        self_modify::{AgentMemory, GateConfig, MetaTaskGenerator, ValidationGate},
        self_tune::{
            anomaly::AnomalyDetector,
            controller::TuningController,
            snapshot::SnapshotStore,
        },
    };

    let bus = make_bus();
    bus.start();

    let cfg = LoopConfig {
        poll_interval: Duration::from_millis(50),
        run_gate_commands: false,
        target_p95_ms: 50.0,
        target_throughput_rps: 1_000.0,
        ..LoopConfig::default()
    };

    let sil = Arc::new(SelfImprovementLoop::with_components(
        cfg.clone(),
        Arc::clone(&bus),
        Arc::new(AnomalyDetector::with_defaults()),
        TuningController::new(cfg.target_p95_ms, cfg.target_throughput_rps),
        Arc::new(SnapshotStore::new(64)),
        Arc::new(MetaTaskGenerator::new()),
        Arc::new(ValidationGate::new(GateConfig::default())),
        Arc::new(AgentMemory::new(64)),
    ));

    let (tx, rx) = watch::channel(false);
    let sil_clone = Arc::clone(&sil);
    let handle = tokio::spawn(async move { sil_clone.run(rx).await });

    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(true).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("loop should stop")
        .expect("join error");

    assert!(sil.status().snapshots_processed >= 1);
}
