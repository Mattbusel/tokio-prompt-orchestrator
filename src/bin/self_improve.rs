//! # Self-Improving Orchestrator Binary
//!
//! Starts a full pipeline with the self-improving control loop active.
//!
//! ```bash
//! cargo run --bin self-improve --features self-improving
//! ```

use std::time::Duration;

use tokio_prompt_orchestrator::{
    self_improve::{LoopConfig, SelfImprovingLoop},
    self_tune::telemetry_bus::{PipelineCounters, TelemetryBus, TelemetryBusConfig},
    spawn_pipeline, EchoWorker,
};

#[tokio::main]
async fn main() {
    tokio_prompt_orchestrator::init_tracing();

    // ── Build pipeline ───────────────────────────────────────────────────────
    let worker = std::sync::Arc::new(EchoWorker::new());
    let pipeline = spawn_pipeline(worker);
    let counters = PipelineCounters::new();

    // ── Start telemetry bus ──────────────────────────────────────────────────
    let bus_cfg = TelemetryBusConfig {
        sample_interval: Duration::from_secs(5),
        ..TelemetryBusConfig::default()
    };
    let bus = TelemetryBus::new(bus_cfg, counters.clone());
    bus.start();

    tracing::info!("telemetry bus started");

    // ── Start self-improving loop ────────────────────────────────────────────
    let loop_cfg = LoopConfig {
        snapshot_every_n_ticks: 6, // snapshot every ~30 s
        target_p95_ms: 150.0,
        target_throughput_rps: 20.0,
        ..LoopConfig::default()
    };

    let self_loop = SelfImprovingLoop::new(loop_cfg, bus.clone());
    let handles = self_loop.handles();
    let _loop_handle = self_loop.spawn();

    tracing::info!("self-improving loop started");
    tracing::info!(
        snapshots = handles.snapshots.version_count(),
        models = handles.learned_router.model_count(),
        "subsystems ready"
    );

    // ── Send demo requests ───────────────────────────────────────────────────
    use tokio_prompt_orchestrator::{PromptRequest, SessionId};
    use std::collections::HashMap;

    for i in 0u64..50 {
        let req = PromptRequest {
            session: SessionId::new(format!("demo-{}", i % 5)),
            request_id: format!("req-{i:04}"),
            input: format!("What is {} squared?", i),
            meta: HashMap::new(),
        };

        if let Err(e) = pipeline.input_tx.send(req).await {
            tracing::warn!(err = ?e, "pipeline send failed — shutting down");
            break;
        }

        // Small delay to spread load
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Periodically report state
        if i % 10 == 9 {
            let status = handles.cost.budget_status();
            let snap_count = handles.snapshots.version_count();
            let avg_rps = handles.autoscaler.current_avg_rps();

            tracing::info!(
                requests_sent = i + 1,
                config_snapshots = snap_count,
                avg_rps,
                budget_ok = status.is_ok(),
                "progress"
            );
        }
    }

    // Let the loop process remaining telemetry ticks
    tokio::time::sleep(Duration::from_secs(12)).await;

    // ── Final report ─────────────────────────────────────────────────────────
    let snap_count = handles.snapshots.version_count();
    let tasks = handles.task_gen.task_count();
    let avg_rps = handles.autoscaler.current_avg_rps();
    let anomalies = handles.anomaly.recent_anomalies(100);

    tracing::info!(
        config_snapshots = snap_count,
        tasks_generated = tasks,
        avg_rps,
        anomalies_detected = anomalies.len(),
        "self-improving loop summary"
    );

    if let Ok(status) = handles.cost.budget_status() {
        tracing::info!(
            spent_usd = status.total_spent,
            budget_fraction = status.budget_fraction,
            "cost summary"
        );
    }

    // Print best config snapshot if any were taken
    if let Ok(best) = handles.snapshots.best_in_window(300) {
        tracing::info!(
            version = best.version,
            score_keys = best.metric_scores.len(),
            "best config snapshot in last 5 minutes"
        );
    }
}
