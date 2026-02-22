//! # HelixRouter Simulation Harness
//!
//! ## Responsibility
//! Generate synthetic job streams with configurable pressure profiles
//! and submit them to a HelixRouter for load testing and benchmarking.
//!
//! ## Guarantees
//! - Deterministic: same seed produces the same job sequence.
//! - Configurable: pressure profiles control CPU pressure over time.
//! - Non-blocking: runs as an async function, yields between jobs.
//!
//! ## NOT Responsible For
//! - Routing decisions (see `router`)
//! - Metric reporting (see `metrics`)

use crate::helix::router::HelixRouter;
use crate::helix::types::{Job, JobKind};
use serde::{Deserialize, Serialize};

/// Pressure profile for a simulation run.
///
/// Controls how CPU pressure varies over the simulation duration.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PressureProfile {
    /// Constant pressure throughout.
    Steady,
    /// Linearly increasing pressure from 0% to 100%.
    Ramp,
    /// Low pressure with periodic spikes.
    Spike,
    /// Random pressure fluctuations.
    Chaos,
}

impl std::fmt::Display for PressureProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Steady => write!(f, "steady"),
            Self::Ramp => write!(f, "ramp"),
            Self::Spike => write!(f, "spike"),
            Self::Chaos => write!(f, "chaos"),
        }
    }
}

/// Configuration for a simulation run.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulatorConfig {
    /// Total number of jobs to generate.
    pub total_jobs: u64,
    /// RNG seed for deterministic job generation.
    pub seed: u64,
    /// Pressure profile.
    pub profile: PressureProfile,
    /// Delay between jobs in microseconds.
    pub inter_job_delay_us: u64,
}

impl Default for SimulatorConfig {
    fn default() -> Self {
        Self {
            total_jobs: 1000,
            seed: 42,
            profile: PressureProfile::Steady,
            inter_job_delay_us: 100,
        }
    }
}

/// Summary of a completed simulation run.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// Total jobs submitted.
    pub total: u64,
    /// Jobs completed successfully.
    pub completed: u64,
    /// Jobs dropped.
    pub dropped: u64,
    /// Total wall-clock time in milliseconds.
    pub wall_time_ms: f64,
    /// Throughput in jobs per second.
    pub throughput_jps: f64,
}

/// Run a simulation against a HelixRouter.
///
/// Generates `config.total_jobs` jobs with costs determined by a
/// deterministic PRNG seeded with `config.seed`. Returns a summary
/// after all jobs have been processed.
///
/// # Arguments
///
/// * `router` — The HelixRouter to submit jobs to.
/// * `config` — Simulation parameters.
///
/// # Returns
///
/// A [`SimulationResult`] summarising the run.
///
/// # Panics
///
/// This function never panics.
pub async fn run_simulation(router: &HelixRouter, config: &SimulatorConfig) -> SimulationResult {
    let start = std::time::Instant::now();
    let mut rng_state = config.seed;
    let mut completed = 0u64;
    let mut dropped = 0u64;

    for _ in 0..config.total_jobs {
        // Deterministic PRNG (LCG)
        rng_state = rng_state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1442695040888963407);

        let kind = match rng_state % 3 {
            0 => JobKind::Hash,
            1 => JobKind::Prime,
            _ => JobKind::MonteCarlo,
        };

        // Cost varies based on pressure profile
        let base_cost = (rng_state >> 16) % 600;
        let cost = apply_pressure_profile(base_cost, config.profile, rng_state);

        let job = Job {
            id: router.next_job_id(),
            kind,
            cost,
        };

        match router.submit(job).await {
            Ok(_) => completed += 1,
            Err(_) => dropped += 1,
        }

        if config.inter_job_delay_us > 0 {
            tokio::time::sleep(tokio::time::Duration::from_micros(
                config.inter_job_delay_us,
            ))
            .await;
        }
    }

    let wall_time_ms = start.elapsed().as_secs_f64() * 1000.0;
    let throughput_jps = if wall_time_ms > 0.0 {
        config.total_jobs as f64 / (wall_time_ms / 1000.0)
    } else {
        0.0
    };

    SimulationResult {
        total: config.total_jobs,
        completed,
        dropped,
        wall_time_ms,
        throughput_jps,
    }
}

/// Apply a pressure profile modifier to a base cost.
fn apply_pressure_profile(base_cost: u64, profile: PressureProfile, rng: u64) -> u64 {
    match profile {
        PressureProfile::Steady => base_cost,
        PressureProfile::Ramp => {
            // Increase cost over time (simulated via rng position)
            let multiplier = 1 + (rng >> 48) % 3;
            base_cost.saturating_mul(multiplier)
        }
        PressureProfile::Spike => {
            // Every ~10th job gets 5x cost
            if rng % 10 == 0 {
                base_cost.saturating_mul(5)
            } else {
                base_cost
            }
        }
        PressureProfile::Chaos => {
            // Random multiplier 1x–10x
            let multiplier = 1 + (rng >> 40) % 10;
            base_cost.saturating_mul(multiplier)
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helix::config::HelixConfig;

    // -- PressureProfile ------------------------------------------------

    #[test]
    fn test_pressure_profile_display() {
        assert_eq!(format!("{}", PressureProfile::Steady), "steady");
        assert_eq!(format!("{}", PressureProfile::Ramp), "ramp");
        assert_eq!(format!("{}", PressureProfile::Spike), "spike");
        assert_eq!(format!("{}", PressureProfile::Chaos), "chaos");
    }

    #[test]
    fn test_pressure_profile_serde_roundtrip() {
        for profile in [
            PressureProfile::Steady,
            PressureProfile::Ramp,
            PressureProfile::Spike,
            PressureProfile::Chaos,
        ] {
            let json = serde_json::to_string(&profile)
                .unwrap_or_else(|e| std::panic::panic_any(format!("test: ser: {e}")));
            let back: PressureProfile = serde_json::from_str(&json)
                .unwrap_or_else(|e| std::panic::panic_any(format!("test: deser: {e}")));
            assert_eq!(profile, back);
        }
    }

    // -- SimulatorConfig ------------------------------------------------

    #[test]
    fn test_default_simulator_config() {
        let cfg = SimulatorConfig::default();
        assert_eq!(cfg.total_jobs, 1000);
        assert_eq!(cfg.seed, 42);
        assert_eq!(cfg.profile, PressureProfile::Steady);
        assert_eq!(cfg.inter_job_delay_us, 100);
    }

    #[test]
    fn test_simulator_config_serde_roundtrip() {
        let cfg = SimulatorConfig::default();
        let json = serde_json::to_string(&cfg)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: ser: {e}")));
        let back: SimulatorConfig = serde_json::from_str(&json)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test: deser: {e}")));
        assert_eq!(back.total_jobs, cfg.total_jobs);
        assert_eq!(back.seed, cfg.seed);
    }

    // -- apply_pressure_profile -----------------------------------------

    #[test]
    fn test_steady_profile_no_change() {
        assert_eq!(
            apply_pressure_profile(100, PressureProfile::Steady, 42),
            100
        );
    }

    #[test]
    fn test_ramp_profile_increases_cost() {
        let base = 100;
        let result = apply_pressure_profile(base, PressureProfile::Ramp, 42);
        assert!(result >= base);
    }

    #[test]
    fn test_spike_profile_usually_unchanged() {
        // For rng values not divisible by 10, cost should be unchanged
        let base = 100;
        let result = apply_pressure_profile(base, PressureProfile::Spike, 7);
        assert_eq!(result, base);
    }

    #[test]
    fn test_spike_profile_spikes_on_trigger() {
        let base = 100;
        let result = apply_pressure_profile(base, PressureProfile::Spike, 10);
        assert_eq!(result, 500);
    }

    #[test]
    fn test_chaos_profile_increases_cost() {
        let base = 100;
        let result = apply_pressure_profile(base, PressureProfile::Chaos, 42);
        assert!(result >= base);
    }

    // -- run_simulation ------------------------------------------------

    #[tokio::test]
    async fn test_run_simulation_steady_completes_all() {
        let router = HelixRouter::new(HelixConfig::default());
        let config = SimulatorConfig {
            total_jobs: 10,
            seed: 42,
            profile: PressureProfile::Steady,
            inter_job_delay_us: 0,
        };
        let result = run_simulation(&router, &config).await;
        assert_eq!(result.total, 10);
        assert_eq!(result.completed + result.dropped, 10);
        assert!(result.wall_time_ms >= 0.0);
        assert!(result.throughput_jps >= 0.0);
    }

    #[tokio::test]
    async fn test_run_simulation_deterministic() {
        let router1 = HelixRouter::new(HelixConfig::default());
        let router2 = HelixRouter::new(HelixConfig::default());
        let config = SimulatorConfig {
            total_jobs: 10,
            seed: 123,
            profile: PressureProfile::Steady,
            inter_job_delay_us: 0,
        };
        let r1 = run_simulation(&router1, &config).await;
        let r2 = run_simulation(&router2, &config).await;
        assert_eq!(r1.completed, r2.completed);
        assert_eq!(r1.dropped, r2.dropped);
    }

    #[tokio::test]
    async fn test_run_simulation_spike_profile() {
        let router = HelixRouter::new(HelixConfig::default());
        let config = SimulatorConfig {
            total_jobs: 20,
            seed: 99,
            profile: PressureProfile::Spike,
            inter_job_delay_us: 0,
        };
        let result = run_simulation(&router, &config).await;
        assert_eq!(result.total, 20);
        assert_eq!(result.completed + result.dropped, 20);
    }

    #[tokio::test]
    async fn test_run_simulation_chaos_profile() {
        let router = HelixRouter::new(HelixConfig::default());
        let config = SimulatorConfig {
            total_jobs: 10,
            seed: 7,
            profile: PressureProfile::Chaos,
            inter_job_delay_us: 0,
        };
        let result = run_simulation(&router, &config).await;
        assert_eq!(result.total, 10);
    }

    #[tokio::test]
    async fn test_run_simulation_zero_jobs() {
        let router = HelixRouter::new(HelixConfig::default());
        let config = SimulatorConfig {
            total_jobs: 0,
            seed: 42,
            profile: PressureProfile::Steady,
            inter_job_delay_us: 0,
        };
        let result = run_simulation(&router, &config).await;
        assert_eq!(result.total, 0);
        assert_eq!(result.completed, 0);
        assert_eq!(result.dropped, 0);
    }
}
