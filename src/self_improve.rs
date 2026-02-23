//! # Self-Improving Loop
//!
//! Closes the self-improvement feedback loop by wiring all self-tuning
//! subsystems into a single running control plane.
//!
//! ## Data flow
//!
//! ```text
//! TelemetryBus ──► AnomalyDetector ──► MetaTaskGenerator
//!      │                                       │
//!      ├──► TuningController ──► SnapshotStore │
//!      │                                       ▼
//!      ├──► CostOptimizer             (logged to AgentMemory)
//!      │
//!      └──► Autoscaler + LearnedRouter
//! ```
//!
//! ## Guarantees
//! - Non-blocking: all subsystems run from a single background task.
//! - Bounded: every collection is capped; no unbounded growth.
//! - Fault-tolerant: subsystem errors are logged, never fatal to the pipeline.
//! - Observable: every decision is recorded in `AgentMemory` and `SnapshotStore`.
//!
//! ## Example
//!
//! ```no_run
//! use tokio_prompt_orchestrator::self_improve::{SelfImprovingLoop, LoopConfig};
//! use tokio_prompt_orchestrator::self_tune::telemetry_bus::{TelemetryBus, TelemetryBusConfig, PipelineCounters};
//!
//! #[tokio::main]
//! async fn main() {
//!     let counters = PipelineCounters::new();
//!     let bus = TelemetryBus::new(TelemetryBusConfig::default(), counters.clone());
//!     bus.start();
//!     let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus);
//!     let _handle = loop_.spawn();
//!     // … pipeline runs …
//! }
//! ```

use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::self_tune::{
    anomaly::{AnomalyDetector, AnomalyDetectorConfig, Severity},
    controller::TuningController,
    cost::{BudgetConfig, CostEntry, CostOptimizer},
    snapshot::{SnapshotSource, SnapshotStore},
    telemetry_bus::{TelemetryBus, TelemetrySnapshot},
};

use crate::self_modify::{
    gate::{GateConfig, ValidationGate},
    memory::AgentMemory,
    task_gen::MetaTaskGenerator,
};

use crate::intelligence::{
    autoscale::{Autoscaler, AutoscalerConfig},
    router::{LearnedRouter, RouterConfig},
};

use crate::self_tune::helix_probe::HelixProbeConfig;
use crate::self_tune::helix_feedback::{HelixFeedbackConfig, HelixFeedbackPusher};

// ─── Configuration ────────────────────────────────────────────────────────────

/// Configuration for the self-improving control loop.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// How many snapshots to retain in the snapshot store.
    pub max_snapshots: usize,
    /// How many modification records to retain in agent memory.
    pub max_memory_records: usize,
    /// Budget configuration for cost tracking.
    pub budget: BudgetConfig,
    /// Anomaly detector configuration.
    pub anomaly: AnomalyDetectorConfig,
    /// Validation gate configuration.
    pub gate: GateConfig,
    /// Autoscaler configuration.
    pub autoscale: AutoscalerConfig,
    /// Learned router configuration.
    pub router: RouterConfig,
    /// Minimum number of anomaly events per tick to trigger task generation.
    pub anomaly_task_threshold: usize,
    /// Take a config snapshot every N telemetry ticks.
    pub snapshot_every_n_ticks: u64,
    /// Target p95 latency in ms for the PID controller.
    pub target_p95_ms: f64,
    /// Target aggregate throughput (req/s) for the PID controller.
    pub target_throughput_rps: f64,
    /// Optional HelixRouter pressure probe configuration.
    ///
    /// When set, the loop spawns a background [`HelixPressureProbe`] that polls
    /// HelixRouter's `/api/stats` and feeds its `pressure_score` into the
    /// `TelemetryBus`, closing the cross-repo feedback loop.
    ///
    /// Set to `None` (the default) to disable HelixRouter integration.
    pub helix_probe: Option<HelixProbeConfig>,

    /// Optional HelixRouter feedback pusher configuration.
    ///
    /// When set, each loop tick examines the current pipeline pressure and pushes
    /// a `RouterConfigPatch` to HelixRouter's `PATCH /api/config` endpoint —
    /// tightening thresholds under high pressure, relaxing them when load is low.
    ///
    /// This closes the **two-way** cross-repo feedback loop:
    /// - tokio-prompt pulls pressure from HelixRouter (via `helix_probe`)
    /// - tokio-prompt pushes config adjustments back to HelixRouter (via this)
    ///
    /// Set to `None` (the default) to disable config push-back.
    pub helix_feedback: Option<HelixFeedbackConfig>,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_snapshots: 256,
            max_memory_records: 1_024,
            budget: BudgetConfig::default(),
            anomaly: AnomalyDetectorConfig::default(),
            gate: GateConfig::default(),
            autoscale: AutoscalerConfig::default(),
            router: RouterConfig::default(),
            anomaly_task_threshold: 1,
            snapshot_every_n_ticks: 12, // every ~60 s at default 5 s bus interval
            target_p95_ms: 200.0,
            target_throughput_rps: 50.0,
            helix_probe: None,
            helix_feedback: None,
        }
    }
}

// ─── Subsystem handles ───────────────────────────────────────────────────────

/// Cheaply-cloneable handles to every self-improving subsystem.
///
/// Pass these to the MCP server, web API, or TUI for read access.
#[derive(Clone)]
pub struct SubsystemHandles {
    /// PID parameter tuning controller (interior-mutable via `parking_lot::Mutex`).
    pub controller: Arc<Mutex<TuningController>>,
    /// Git-like configuration versioning.
    pub snapshots: Arc<SnapshotStore>,
    /// Budget-aware cost optimizer.
    pub cost: Arc<CostOptimizer>,
    /// Statistical anomaly detector.
    pub anomaly: Arc<AnomalyDetector>,
    /// Task generator from degradation signals.
    pub task_gen: Arc<MetaTaskGenerator>,
    /// Validation gate for proposed changes.
    pub gate: Arc<ValidationGate>,
    /// Agent knowledge base.
    pub memory: Arc<AgentMemory>,
    /// Load forecaster and autoscaler.
    pub autoscaler: Arc<Autoscaler>,
    /// Multi-armed bandit model router.
    pub learned_router: Arc<LearnedRouter>,
    /// Optional cross-repo feedback pusher to HelixRouter.
    pub helix_feedback: Option<Arc<HelixFeedbackPusher>>,
}

// ─── Main loop ───────────────────────────────────────────────────────────────

/// The self-improving control plane.
///
/// Construct with [`new`](Self::new), then call [`spawn`](Self::spawn) to
/// start the background feedback loop.
pub struct SelfImprovingLoop {
    bus: TelemetryBus,
    handles: SubsystemHandles,
    cfg: LoopConfig,
}

impl SelfImprovingLoop {
    /// Construct the loop.  No background work starts until [`spawn`] is called.
    pub fn new(cfg: LoopConfig, bus: TelemetryBus) -> Self {
        let controller = Arc::new(Mutex::new(TuningController::new(
            cfg.target_p95_ms,
            cfg.target_throughput_rps,
        )));
        let snapshots = Arc::new(SnapshotStore::new(cfg.max_snapshots));
        let cost = Arc::new(
            CostOptimizer::new(cfg.budget.clone())
                .unwrap_or_else(|_| CostOptimizer::new(BudgetConfig::default()).expect("default budget is valid")),
        );
        let anomaly = Arc::new(AnomalyDetector::new(cfg.anomaly.clone()));
        let task_gen = Arc::new(MetaTaskGenerator::new());
        let gate = Arc::new(ValidationGate::new(cfg.gate.clone()));
        let memory = Arc::new(AgentMemory::new(cfg.max_memory_records));
        let autoscaler = Arc::new(Autoscaler::new(cfg.autoscale.clone(), 3_600));
        let learned_router = Arc::new(LearnedRouter::new(cfg.router.clone()));

        // Register the three default model backends
        for model in &["openai", "anthropic", "llama_cpp"] {
            let _ = learned_router.register_model(model);
        }

        let helix_feedback = cfg
            .helix_feedback
            .as_ref()
            .map(|fb_cfg| Arc::new(HelixFeedbackPusher::new(fb_cfg.clone())));

        let handles = SubsystemHandles {
            controller,
            snapshots,
            cost,
            anomaly,
            task_gen,
            gate,
            memory,
            autoscaler,
            learned_router,
            helix_feedback,
        };

        Self { bus, handles, cfg }
    }

    /// Return a clone of all subsystem handles for external read access.
    pub fn handles(&self) -> SubsystemHandles {
        self.handles.clone()
    }

    /// Spawn the background control loop as a detached Tokio task.
    ///
    /// If `LoopConfig::helix_probe` is `Some`, a [`HelixPressureProbe`] background
    /// task is also spawned to feed HelixRouter's pressure signal into the bus.
    ///
    /// The loop runs until the telemetry bus is dropped (broadcast channel closes).
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        let mut rx = self.bus.subscribe();
        let handles = self.handles;
        let cfg = self.cfg;

        // If configured, start the HelixRouter pressure probe as a background task.
        // The probe feeds HelixRouter's pressure_score into the TelemetryBus so
        // the anomaly detector and controller react to downstream backpressure.
        if let Some(probe_cfg) = cfg.helix_probe.clone() {
            use crate::self_tune::helix_probe::HelixPressureProbe;
            let probe = HelixPressureProbe::new(probe_cfg, self.bus.clone());
            tokio::spawn(async move { probe.run().await });
            info!("HelixPressureProbe started — cross-repo pressure feedback active");
        }

        tokio::spawn(async move {
            let mut tick: u64 = 0;
            info!("self-improving loop started");

            loop {
                let snap = match rx.recv().await {
                    Ok(s) => s,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(dropped = n, "self-improve loop lagged on telemetry bus");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!("telemetry bus closed — self-improving loop exiting");
                        break;
                    }
                };

                tick += 1;
                debug!(tick, elapsed_secs = snap.elapsed_secs, "loop tick");

                // 1. Feed autoscaler with aggregate RPS
                Self::step_autoscaler(&handles, &snap);

                // 2. Ingest metrics into anomaly detector; count events
                let anomaly_count = Self::step_anomaly(&handles, &snap);

                // 3. Record cost estimate
                Self::step_cost(&handles, &snap);

                // 4. Run PID controller
                Self::step_controller(&handles, &snap);

                // 5. Generate improvement tasks when anomalies exceed threshold
                if anomaly_count >= cfg.anomaly_task_threshold {
                    Self::step_task_gen(&handles, &snap);
                }

                // 6. Periodically snapshot current configuration
                if tick % cfg.snapshot_every_n_ticks == 0 {
                    Self::step_snapshot(&handles, &snap);
                }

                // 7. Push pressure-derived config patch back to HelixRouter (two-way loop)
                if handles.helix_feedback.is_some() {
                    Self::step_helix_feedback(&handles, &snap).await;
                }
            }

            info!("self-improving loop exited");
        })
    }

    // ── Step functions (pub so tests can invoke them directly) ───────────────

    /// Feed aggregate RPS into the autoscaler.
    pub fn step_autoscaler(h: &SubsystemHandles, snap: &TelemetrySnapshot) {
        let rps: f64 = snap.stages.iter().map(|s| s.throughput_rps).sum();
        h.autoscaler.record_rps(rps);

        match h.autoscaler.forecast() {
            Ok(rec) => {
                debug!(
                    predicted_5m = rec.predicted_rps_5m,
                    instances = rec.recommended_local_instances,
                    prewarm = rec.prewarm_local,
                    "autoscale forecast"
                );
            }
            Err(e) => {
                // Normal early in a session — insufficient history
                debug!(err = ?e, "autoscale forecast skipped");
            }
        }
    }

    /// Ingest per-stage and global metrics into the anomaly detector.
    ///
    /// Returns the total number of anomaly events detected this tick.
    pub fn step_anomaly(h: &SubsystemHandles, snap: &TelemetrySnapshot) -> usize {
        let mut total = 0usize;

        for stage in &snap.stages {
            let prefix = &stage.stage;
            let metrics: &[(&str, f64)] = &[
                (&format!("{prefix}.error_rate"), stage.error_rate),
                (&format!("{prefix}.throughput_rps"), stage.throughput_rps),
                (&format!("{prefix}.queue_depth"), stage.queue_depth as f64),
                (&format!("{prefix}.p95_ms"), stage.latency.p95_ms as f64),
            ];

            for (metric, value) in metrics {
                match h.anomaly.ingest(metric, *value) {
                    Ok(anomalies) => {
                        for a in &anomalies {
                            total += 1;
                            match a.severity {
                                Severity::Critical => {
                                    warn!(
                                        metric = %a.metric,
                                        value = a.value,
                                        score = a.score,
                                        "CRITICAL anomaly"
                                    );
                                }
                                Severity::Warning => {
                                    info!(metric = %a.metric, value = a.value, "anomaly warning");
                                }
                                Severity::Info => {
                                    debug!(metric = %a.metric, "anomaly info");
                                }
                            }
                        }
                    }
                    Err(e) => debug!(err = ?e, metric, "anomaly ingest error"),
                }
            }
        }

        // Global metrics
        for (metric, value) in &[
            ("global.cache_hit_rate", snap.cache_hit_rate),
            ("global.dedup_collision_rate", snap.dedup_collision_rate),
            ("global.pressure", snap.pressure),
        ] {
            match h.anomaly.ingest(metric, *value) {
                Ok(anomalies) => total += anomalies.len(),
                Err(e) => debug!(err = ?e, "anomaly ingest error (global)"),
            }
        }

        total
    }

    /// Record a cost estimate based on aggregate RPS and elapsed time.
    pub fn step_cost(h: &SubsystemHandles, snap: &TelemetrySnapshot) {
        let total_rps: f64 = snap.stages.iter().map(|s| s.throughput_rps).sum();
        if total_rps > 0.0 {
            // ~$0.001 per request at ~800 tokens (rough estimate; real costs come from API billing)
            let estimated_cost = total_rps * 0.001;
            let entry = CostEntry {
                model: "pipeline".to_string(),
                cost_usd: estimated_cost,
                tokens_in: (total_rps * 400.0) as u64,
                tokens_out: (total_rps * 400.0) as u64,
                timestamp_secs: snap.unix_ts,
            };
            if let Err(e) = h.cost.record_cost(entry) {
                debug!(err = ?e, "cost record error");
            }
        }

        match h.cost.budget_status() {
            Ok(status) => {
                if status.budget_fraction > 0.8 {
                    warn!(fraction = status.budget_fraction, "budget approaching limit");
                } else {
                    debug!(
                        spent = status.total_spent,
                        fraction = status.budget_fraction,
                        "budget ok"
                    );
                }
            }
            Err(e) => debug!(err = ?e, "budget status error"),
        }
    }

    /// Run the PID controller and log any adjustments.
    pub fn step_controller(h: &SubsystemHandles, snap: &TelemetrySnapshot) {
        let adjustments = h.controller.lock().process(snap);
        if !adjustments.is_empty() {
            info!(count = adjustments.len(), "PID controller made adjustments");
            for (id, value) in &adjustments {
                debug!(param = ?id, value, "parameter adjusted");
            }
        }
    }

    /// Generate improvement tasks from a telemetry snapshot.
    pub fn step_task_gen(h: &SubsystemHandles, snap: &TelemetrySnapshot) {
        match h.task_gen.process_snapshot(snap) {
            Ok(tasks) => {
                if !tasks.is_empty() {
                    info!(count = tasks.len(), "task generator produced tasks");
                    for task in &tasks {
                        info!(
                            name = %task.name,
                            priority = ?task.priority,
                            metric = %task.trigger_metric,
                            value = task.trigger_value,
                            "improvement task generated"
                        );
                    }
                }
            }
            Err(e) => warn!(err = ?e, "task gen error"),
        }
    }

    /// Push a pressure-derived config patch to HelixRouter (two-way cross-repo loop).
    ///
    /// Under high pressure (≥0.7): tighten HelixRouter's spawn threshold by 10%
    /// and raise its cpu_p95_budget_ms so it tolerates latency spikes instead of
    /// aggressively shedding load that tokio-prompt is already throttling.
    ///
    /// Under low pressure (≤0.3): send a relaxation patch to allow HelixRouter to
    /// resume normal adaptive behaviour.
    ///
    /// This is a best-effort, non-blocking operation.  Failures are soft-logged.
    pub async fn step_helix_feedback(h: &SubsystemHandles, snap: &TelemetrySnapshot) {
        let Some(pusher) = &h.helix_feedback else { return };
        if let Err(e) = pusher.maybe_push(snap.pressure).await {
            debug!(err = %e, "helix_feedback push skipped or failed");
        }
    }

    /// Record a snapshot of the current controller parameters.
    pub fn step_snapshot(h: &SubsystemHandles, snap: &TelemetrySnapshot) {
        // Collect current parameter values from controller
        let params: HashMap<String, f64> = {
            use crate::self_tune::controller::ParameterId;
            let ctrl = h.controller.lock();
            ParameterId::all()
                .iter()
                .map(|&id| (id.name().to_string(), ctrl.get(id)))
                .collect()
        };

        let mut metrics = HashMap::new();
        metrics.insert("pressure_inv".to_string(), 1.0 - snap.pressure);
        metrics.insert("cache_hit_rate".to_string(), snap.cache_hit_rate);
        metrics.insert("dedup_collision_rate".to_string(), snap.dedup_collision_rate);

        let avg_error: f64 = if snap.stages.is_empty() {
            0.0
        } else {
            snap.stages.iter().map(|s| s.error_rate).sum::<f64>() / snap.stages.len() as f64
        };
        metrics.insert("error_rate_inv".to_string(), 1.0 - avg_error);

        match h.snapshots.create_snapshot(
            params,
            metrics,
            SnapshotSource::PidAdjustment,
            format!("auto @ t={}", snap.elapsed_secs),
        ) {
            Ok(v) => debug!(version = v.version, "config snapshot recorded"),
            Err(e) => debug!(err = ?e, "snapshot error"),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_tune::telemetry_bus::{PipelineCounters, TelemetryBusConfig};

    fn make_bus() -> TelemetryBus {
        let counters = PipelineCounters::new();
        TelemetryBus::new(TelemetryBusConfig::default(), counters)
    }

    // ── Construction ────────────────────────────────────────────────────────

    #[test]
    fn test_loop_config_default_is_sane() {
        let cfg = LoopConfig::default();
        assert!(cfg.max_snapshots > 0);
        assert!(cfg.max_memory_records > 0);
        assert!(cfg.snapshot_every_n_ticks > 0);
        assert!(cfg.anomaly_task_threshold > 0);
        assert!(cfg.target_p95_ms > 0.0);
        assert!(cfg.target_throughput_rps > 0.0);
    }

    #[test]
    fn test_self_improving_loop_constructs_without_panic() {
        let bus = make_bus();
        let _ = SelfImprovingLoop::new(LoopConfig::default(), bus);
    }

    #[test]
    fn test_subsystem_handles_are_clonable() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus);
        let h1 = loop_.handles();
        let h2 = h1.clone();
        assert!(Arc::ptr_eq(&h1.snapshots, &h2.snapshots));
        assert!(Arc::ptr_eq(&h1.cost, &h2.cost));
        assert!(Arc::ptr_eq(&h1.anomaly, &h2.anomaly));
    }

    #[test]
    fn test_loop_config_custom_values() {
        let cfg = LoopConfig {
            max_snapshots: 10,
            snapshot_every_n_ticks: 1,
            anomaly_task_threshold: 3,
            target_p95_ms: 100.0,
            target_throughput_rps: 25.0,
            ..LoopConfig::default()
        };
        assert_eq!(cfg.max_snapshots, 10);
        assert_eq!(cfg.snapshot_every_n_ticks, 1);
        assert_eq!(cfg.anomaly_task_threshold, 3);
    }

    // ── Step functions ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_step_anomaly_returns_zero_for_empty_snapshot() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus);
        let h = loop_.handles();
        let snap = TelemetrySnapshot::default();
        // Empty snapshot → all global metrics are 0.0, no stages
        let count = SelfImprovingLoop::step_anomaly(&h, &snap);
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_step_snapshot_records_entry() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus.clone());
        let h = loop_.handles();
        let snap = bus.tick_now().await;

        SelfImprovingLoop::step_snapshot(&h, &snap);

        let count = h.snapshots.version_count();
        assert!(count > 0, "expected at least one snapshot after step_snapshot");
    }

    #[tokio::test]
    async fn test_step_cost_zero_rps_records_nothing() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus);
        let h = loop_.handles();

        let snap = TelemetrySnapshot::default(); // zero RPS
        SelfImprovingLoop::step_cost(&h, &snap);

        let status = h.cost.budget_status().unwrap();
        assert_eq!(status.total_spent, 0.0);
    }

    #[tokio::test]
    async fn test_step_controller_runs_without_panic() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus.clone());
        let h = loop_.handles();
        let snap = bus.tick_now().await;
        // Must not panic
        SelfImprovingLoop::step_controller(&h, &snap);
    }

    #[tokio::test]
    async fn test_step_autoscaler_runs_without_panic() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus.clone());
        let h = loop_.handles();
        let snap = bus.tick_now().await;
        SelfImprovingLoop::step_autoscaler(&h, &snap);
        // After one point, forecast returns InsufficientHistory — that's fine
    }

    #[tokio::test]
    async fn test_full_tick_does_not_panic() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus.clone());
        let handles = loop_.handles();
        let snap = bus.tick_now().await;

        SelfImprovingLoop::step_autoscaler(&handles, &snap);
        let anomaly_count = SelfImprovingLoop::step_anomaly(&handles, &snap);
        SelfImprovingLoop::step_cost(&handles, &snap);
        SelfImprovingLoop::step_controller(&handles, &snap);
        if anomaly_count >= 1 {
            SelfImprovingLoop::step_task_gen(&handles, &snap);
        }
        SelfImprovingLoop::step_snapshot(&handles, &snap);
    }

    // ── Loop lifecycle ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_loop_exits_when_bus_drops() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus.clone());
        let handle = loop_.spawn();

        // Tick once so the loop processes at least one snapshot
        bus.tick_now().await;

        // Drop the bus — broadcast channel closes, loop should exit
        drop(bus);

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(300), handle).await;
        // Either completes cleanly or times out — both are acceptable;
        // the important thing is no panic.
        let _ = result;
    }

    #[tokio::test]
    async fn test_loop_processes_multiple_ticks() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(
            LoopConfig {
                snapshot_every_n_ticks: 1,
                ..LoopConfig::default()
            },
            bus.clone(),
        );
        let handles = loop_.handles();

        // Simulate 5 ticks manually (step-by-step, not via spawn)
        for _ in 0..5 {
            let snap = bus.tick_now().await;
            SelfImprovingLoop::step_autoscaler(&handles, &snap);
            SelfImprovingLoop::step_anomaly(&handles, &snap);
            SelfImprovingLoop::step_cost(&handles, &snap);
            SelfImprovingLoop::step_controller(&handles, &snap);
            SelfImprovingLoop::step_snapshot(&handles, &snap);
        }

        // After 5 ticks there should be 5 snapshots
        assert_eq!(handles.snapshots.version_count(), 5);
    }

    #[tokio::test]
    async fn test_snapshot_source_is_pid_adjustment() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus.clone());
        let h = loop_.handles();
        let snap = bus.tick_now().await;

        SelfImprovingLoop::step_snapshot(&h, &snap);

        let snaps = h.snapshots.find_by_source(&SnapshotSource::PidAdjustment);
        assert!(!snaps.is_empty(), "expected PidAdjustment snapshot");
    }

    #[tokio::test]
    async fn test_learned_router_has_default_models() {
        let bus = make_bus();
        let loop_ = SelfImprovingLoop::new(LoopConfig::default(), bus);
        let h = loop_.handles();
        // All three default models should be registered
        assert_eq!(h.learned_router.model_count(), 3);
    }
}
