//! Independent integration tests for each self-improvement tier.
//!
//! Tests that each of the four tiers (self-tune, self-modify, intelligence,
//! evolution) can be constructed and exercised in isolation, without requiring
//! the full `self-improving` feature bundle.

// ── self-tune tier ────────────────────────────────────────────────────────

#[cfg(feature = "self-tune")]
mod self_tune_tier {
    use std::time::Duration;
    use tokio_prompt_orchestrator::self_tune::{
        anomaly::AnomalyDetector,
        controller::TuningController,
        snapshot::SnapshotStore,
        telemetry_bus::{PipelineCounters, TelemetryBus, TelemetryBusConfig},
    };

    #[test]
    fn test_self_tune_telemetry_bus_construction() {
        let counters = PipelineCounters::new();
        let cfg = TelemetryBusConfig {
            sample_interval: Duration::from_millis(100),
            ..TelemetryBusConfig::default()
        };
        let _bus = TelemetryBus::new(cfg, counters);
    }

    #[test]
    fn test_self_tune_anomaly_detector_construction() {
        let detector = AnomalyDetector::with_defaults();
        // Initially no anomalies detected
        assert!(detector.recent_anomalies(10).is_empty());
    }

    #[test]
    fn test_self_tune_tuning_controller_construction_and_params() {
        let controller = TuningController::new(150.0, 20.0);
        // get() should return a valid value for any parameter
        use tokio_prompt_orchestrator::self_tune::controller::ParameterId;
        let val = controller.get(ParameterId::Concurrency);
        assert!(val >= 0.0, "concurrency param must be non-negative");
    }

    #[test]
    fn test_self_tune_snapshot_store_starts_empty() {
        let store = SnapshotStore::new(10);
        assert_eq!(store.version_count(), 0, "new store has zero versions");
    }

    #[tokio::test]
    async fn test_self_tune_bus_broadcasts_at_least_once() {
        let counters = PipelineCounters::new();
        let cfg = TelemetryBusConfig {
            sample_interval: Duration::from_millis(50),
            ..TelemetryBusConfig::default()
        };
        let bus = TelemetryBus::new(cfg, counters);
        let mut rx = bus.subscribe();
        bus.start();

        // Wait for at least one broadcast (50ms interval, 300ms budget)
        let result = tokio::time::timeout(Duration::from_millis(300), rx.changed()).await;
        assert!(result.is_ok(), "telemetry bus must broadcast within 300ms");
    }
}

// ── self-modify tier ──────────────────────────────────────────────────────

#[cfg(feature = "self-modify")]
mod self_modify_tier {
    use tokio_prompt_orchestrator::self_modify::{
        AgentMemory, GateConfig, MetaTaskGenerator, ValidationGate,
    };

    #[test]
    fn test_self_modify_agent_memory_starts_empty() {
        let mem = AgentMemory::new(100);
        let summary = mem.summary();
        assert_eq!(summary.modification_count, 0);
    }

    #[test]
    fn test_self_modify_meta_task_generator_starts_empty() {
        let gen = MetaTaskGenerator::new();
        assert_eq!(gen.task_count(), 0);
    }

    #[test]
    fn test_self_modify_validation_gate_default_trust_level() {
        let gate = ValidationGate::new(GateConfig::default());
        // Default trust level is 0 (human review required for all changes)
        assert_eq!(gate.trust_level(), 0);
    }

    #[test]
    fn test_self_modify_validation_gate_set_trust_level() {
        let gate = ValidationGate::new(GateConfig::default());
        let result = gate.set_trust_level(1);
        assert!(result.is_ok(), "setting trust_level to 1 should succeed");
        assert_eq!(gate.trust_level(), 1);
    }

    #[test]
    fn test_self_modify_gate_config_default_is_valid() {
        let cfg = GateConfig::default();
        assert!(cfg.test_timeout.as_secs() > 0);
        assert!(cfg.clippy_timeout.as_secs() > 0);
    }
}

// ── intelligence tier ─────────────────────────────────────────────────────

#[cfg(feature = "intelligence")]
mod intelligence_tier {
    use tokio_prompt_orchestrator::intelligence::{
        autoscale::{Autoscaler, AutoscalerConfig},
        feedback::FeedbackCollector,
        router::{LearnedRouter, RouterConfig},
    };

    #[test]
    fn test_intelligence_learned_router_starts_with_no_models() {
        let router = LearnedRouter::new(RouterConfig::default());
        assert_eq!(router.model_count(), 0);
    }

    #[test]
    fn test_intelligence_learned_router_register_and_count() {
        let router = LearnedRouter::new(RouterConfig::default());
        router.register_model("openai").expect("register openai");
        router
            .register_model("anthropic")
            .expect("register anthropic");
        assert_eq!(router.model_count(), 2);
    }

    #[test]
    fn test_intelligence_autoscaler_starts_at_zero_rps() {
        let autoscaler = Autoscaler::new(AutoscalerConfig::default(), 3_600);
        assert_eq!(autoscaler.current_avg_rps(), 0.0);
    }

    #[test]
    fn test_intelligence_feedback_collector_starts_empty() {
        let collector = FeedbackCollector::new(1_000);
        assert_eq!(collector.entry_count(), 0);
    }

    #[test]
    fn test_intelligence_feedback_collector_average_score_empty() {
        let collector = FeedbackCollector::new(100);
        // average_score on empty collector must not panic
        let avg = collector.average_score();
        assert_eq!(avg, 0.0, "empty collector average should be 0.0");
    }

    #[test]
    fn test_intelligence_quality_estimator_estimate_does_not_panic() {
        use tokio_prompt_orchestrator::intelligence::quality::{
            QualityEstimator, QualityEstimatorConfig,
        };
        let estimator = QualityEstimator::new(QualityEstimatorConfig::default());
        // Must not panic regardless of input content
        let result = estimator.estimate("What is Rust?", "Rust is a systems language.", "openai");
        assert!(result.is_ok(), "estimate must succeed for valid input");
        if let Ok(est) = result {
            assert!(est.overall_score >= 0.0 && est.overall_score <= 1.0);
        }
    }
}

// ── evolution tier ────────────────────────────────────────────────────────

#[cfg(feature = "evolution")]
mod evolution_tier {
    use std::collections::HashMap;
    use tokio_prompt_orchestrator::evolution::{
        brain::{ConsensusBrain, ConsensusConfig},
        plugin::PluginManager,
        search::{EvolutionConfig, EvolutionarySearch},
        transfer::{TransferConfig, TransferLearning},
    };

    fn simple_config() -> EvolutionConfig {
        let mut bounds = HashMap::new();
        bounds.insert("lr".to_string(), (0.001_f64, 0.1_f64));
        EvolutionConfig {
            population_size: 5,
            elite_count: 1,
            mutation_rate: 0.1,
            mutation_strength: 0.05,
            crossover_rate: 0.7,
            max_generations: 10,
            parameter_bounds: bounds,
        }
    }

    #[test]
    fn test_evolution_search_starts_at_generation_zero() {
        let search = EvolutionarySearch::new(simple_config());
        assert_eq!(search.current_generation(), 0);
        assert!(search.population().is_empty());
    }

    #[test]
    fn test_evolution_search_initialize_creates_population() {
        let search = EvolutionarySearch::new(simple_config());
        search.initialize_population().expect("initialize");
        assert_eq!(search.population().len(), 5);
    }

    #[test]
    fn test_evolution_search_evolve_increments_generation() {
        let search = EvolutionarySearch::new(simple_config());
        search.initialize_population().expect("initialize");
        for ind in search.population() {
            search.evaluate(&ind.id, 1.0).expect("evaluate");
        }
        let result = search.evolve().expect("evolve");
        assert_eq!(result.generation, 1);
        assert_eq!(search.current_generation(), 1);
    }

    #[test]
    fn test_evolution_consensus_brain_starts_with_no_nodes() {
        let brain = ConsensusBrain::new(ConsensusConfig::default())
            .expect("ConsensusBrain::new with default config");
        assert_eq!(brain.node_count(), 0);
    }

    #[test]
    fn test_evolution_consensus_brain_register_node() {
        let brain = ConsensusBrain::new(ConsensusConfig::default()).expect("ConsensusBrain::new");
        brain.register_node("node-1").expect("register node-1");
        assert_eq!(brain.node_count(), 1);
    }

    #[test]
    fn test_evolution_transfer_learning_starts_empty() {
        let tl = TransferLearning::new(TransferConfig::default());
        assert_eq!(tl.packet_count(), 0);
    }

    #[test]
    fn test_evolution_plugin_manager_starts_empty() {
        let pm = PluginManager::new();
        assert_eq!(pm.plugin_count(), 0);
    }
}
