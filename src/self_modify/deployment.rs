//! # Staged Deployment Pipeline (Task 2.4)
//!
//! ## Responsibility
//! Manage the lifecycle of agent-proposed code changes through a multi-stage
//! deployment pipeline: validate → canary → progressive rollout → promote/rollback.
//! Each stage has configurable success criteria and automatic progression.
//!
//! ## Guarantees
//! - Thread-safe: all operations safe under concurrent access
//! - Bounded: deployment history capped at configurable limit
//! - Idempotent: advancing a deployment that's already in the target stage is a no-op
//! - Non-blocking: stage transitions are synchronous state changes
//!
//! ## NOT Responsible For
//! - Running validation gates (see gate.rs)
//! - Applying code changes (see agent coordinator)
//! - Observing metrics (see telemetry_bus)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Error ────────────────────────────────────────────────────────────────────

/// Errors produced by the deployment pipeline.
#[derive(Debug, Error)]
pub enum DeploymentError {
    /// Internal lock poisoned.
    #[error("deployment lock poisoned")]
    LockPoisoned,

    /// The requested deployment was not found.
    #[error("deployment not found: {0}")]
    DeploymentNotFound(String),

    /// A deployment with this ID already exists.
    #[error("deployment already exists: {0}")]
    DeploymentAlreadyExists(String),

    /// The requested stage transition is not valid.
    #[error("invalid transition for deployment '{deployment_id}': {from:?} -> {to:?}")]
    InvalidTransition {
        /// The deployment that was targeted.
        deployment_id: String,
        /// The current stage.
        from: DeploymentStage,
        /// The requested stage.
        to: DeploymentStage,
    },

    /// Canary deployment failed health checks.
    #[error("canary failed for deployment '{deployment_id}': {reason}")]
    CanaryFailed {
        /// The deployment that failed canary.
        deployment_id: String,
        /// Why the canary failed.
        reason: String,
    },

    /// Progressive rollout failed health checks.
    #[error("rollout failed for deployment '{deployment_id}': {reason}")]
    RolloutFailed {
        /// The deployment that failed rollout.
        deployment_id: String,
        /// Why the rollout failed.
        reason: String,
    },
}

// ─── Deployment stage ─────────────────────────────────────────────────────────

/// The current stage of a deployment in the pipeline.
///
/// Stages progress linearly from `Pending` through to `Promoted`, with
/// `RolledBack` and `Failed` as terminal error states reachable from
/// most active stages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DeploymentStage {
    /// Awaiting validation — initial state.
    Pending,
    /// Running through validation gates.
    Validating,
    /// Deployed to canary (small traffic percentage).
    Canary,
    /// Progressive rollout in progress.
    Rolling {
        /// Current rollout progress percentage (0–100).
        progress_pct: u8,
    },
    /// Fully deployed — terminal success state.
    Promoted,
    /// Rolled back from an active stage.
    RolledBack {
        /// Why the deployment was rolled back.
        reason: String,
    },
    /// Failed at some stage — terminal error state.
    Failed {
        /// Why the deployment failed.
        reason: String,
    },
}

impl DeploymentStage {
    /// Return `true` if this stage is terminal (no further transitions allowed
    /// except rollback which is handled separately).
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            DeploymentStage::Promoted
                | DeploymentStage::RolledBack { .. }
                | DeploymentStage::Failed { .. }
        )
    }
}

// ─── Configuration ────────────────────────────────────────────────────────────

/// Configuration for the deployment pipeline.
///
/// Controls canary duration, rollout steps, and health-check thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentConfig {
    /// How long the canary stage runs before evaluation, in seconds.
    pub canary_duration_secs: u64,
    /// Percentage of traffic directed to the canary.
    pub canary_traffic_pct: u8,
    /// Progressive rollout percentages (must be sorted ascending, ending at 100).
    pub rollout_steps: Vec<u8>,
    /// Time between rollout steps, in seconds.
    pub step_duration_secs: u64,
    /// Maximum tolerable error rate during rollout (fraction, e.g. 0.05 = 5%).
    pub max_error_rate: f64,
    /// Maximum tolerable latency regression during rollout (fraction, e.g. 0.10 = 10%).
    pub max_latency_regression_pct: f64,
    /// Whether to auto-promote after a successful 100% rollout.
    pub auto_promote: bool,
}

impl Default for DeploymentConfig {
    fn default() -> Self {
        Self {
            canary_duration_secs: 300,
            canary_traffic_pct: 5,
            rollout_steps: vec![10, 25, 50, 75, 100],
            step_duration_secs: 120,
            max_error_rate: 0.05,
            max_latency_regression_pct: 0.10,
            auto_promote: false,
        }
    }
}

// ─── Stage transition record ──────────────────────────────────────────────────

/// A recorded stage transition in a deployment's audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageTransition {
    /// The stage before the transition.
    pub from: DeploymentStage,
    /// The stage after the transition.
    pub to: DeploymentStage,
    /// Human-readable reason for the transition.
    pub reason: String,
    /// Unix timestamp when the transition occurred.
    pub timestamp_secs: u64,
}

// ─── Deployment ───────────────────────────────────────────────────────────────

/// A single deployment tracked by the pipeline.
///
/// Contains the full lifecycle state including stage history and metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    /// Unique deployment identifier.
    pub id: String,
    /// Human-readable description of what this deployment changes.
    pub description: String,
    /// Links to the validation gate proposal that approved this change.
    pub proposal_id: String,
    /// Current stage in the deployment pipeline.
    pub stage: DeploymentStage,
    /// Audit trail of all stage transitions.
    pub stage_history: Vec<StageTransition>,
    /// Pipeline configuration for this deployment.
    pub config: DeploymentConfig,
    /// Baseline metrics captured at deployment creation.
    pub metrics_at_start: HashMap<String, f64>,
    /// Latest observed metrics (updated during rollout).
    pub current_metrics: HashMap<String, f64>,
    /// Unix timestamp when the deployment was created.
    pub created_at_secs: u64,
    /// Unix timestamp of the last state change.
    pub updated_at_secs: u64,
}

// ─── Deployment summary ───────────────────────────────────────────────────────

/// Lightweight summary of a deployment for listing views.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentSummary {
    /// Unique deployment identifier.
    pub id: String,
    /// Human-readable description.
    pub description: String,
    /// Current stage.
    pub stage: DeploymentStage,
    /// Unix timestamp when the deployment was created.
    pub created_at_secs: u64,
    /// Unix timestamp of the last state change.
    pub updated_at_secs: u64,
}

impl Deployment {
    /// Create a summary view of this deployment.
    fn summarize(&self) -> DeploymentSummary {
        DeploymentSummary {
            id: self.id.clone(),
            description: self.description.clone(),
            stage: self.stage.clone(),
            created_at_secs: self.created_at_secs,
            updated_at_secs: self.updated_at_secs,
        }
    }

    /// Record a stage transition and update timestamps.
    fn transition_to(&mut self, to: DeploymentStage, reason: &str) {
        let from = self.stage.clone();
        let now = unix_now();
        self.stage_history.push(StageTransition {
            from,
            to: to.clone(),
            reason: reason.to_string(),
            timestamp_secs: now,
        });
        self.stage = to;
        self.updated_at_secs = now;
    }
}

// ─── Pipeline inner state ─────────────────────────────────────────────────────

struct PipelineInner {
    deployments: HashMap<String, Deployment>,
    completed: Vec<DeploymentSummary>,
    default_config: DeploymentConfig,
    max_history: usize,
}

// ─── Deployment pipeline ──────────────────────────────────────────────────────

/// Manages the lifecycle of agent-proposed code changes through a multi-stage
/// deployment pipeline.
///
/// Cheaply cloneable via `Arc<Mutex<_>>` — all clones share the same state.
#[derive(Clone)]
pub struct DeploymentPipeline {
    inner: Arc<Mutex<PipelineInner>>,
}

impl DeploymentPipeline {
    /// Create a new pipeline with the given default configuration.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: DeploymentConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PipelineInner {
                deployments: HashMap::new(),
                completed: Vec::new(),
                default_config: config,
                max_history: 1000,
            })),
        }
    }

    /// Create a new pipeline with default configuration.
    ///
    /// # Panics
    /// This function never panics.
    pub fn with_defaults() -> Self {
        Self::new(DeploymentConfig::default())
    }

    /// Create a new deployment in `Pending` stage.
    ///
    /// # Arguments
    /// * `id` — Unique identifier for the deployment
    /// * `description` — Human-readable description of the change
    /// * `proposal_id` — Links to the validation gate proposal
    /// * `metrics_at_start` — Baseline metrics for health-check comparisons
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentAlreadyExists`] if a deployment
    /// with this ID already exists.
    ///
    /// # Panics
    /// This function never panics.
    pub fn create_deployment(
        &self,
        id: impl Into<String>,
        description: impl Into<String>,
        proposal_id: impl Into<String>,
        metrics_at_start: HashMap<String, f64>,
    ) -> Result<(), DeploymentError> {
        let id = id.into();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;

        if inner.deployments.contains_key(&id) {
            return Err(DeploymentError::DeploymentAlreadyExists(id));
        }

        let now = unix_now();
        let deployment = Deployment {
            id: id.clone(),
            description: description.into(),
            proposal_id: proposal_id.into(),
            stage: DeploymentStage::Pending,
            stage_history: Vec::new(),
            config: inner.default_config.clone(),
            metrics_at_start,
            current_metrics: HashMap::new(),
            created_at_secs: now,
            updated_at_secs: now,
        };

        inner.deployments.insert(id, deployment);
        Ok(())
    }

    /// Transition a deployment from `Pending` to `Validating`.
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    /// Returns [`DeploymentError::InvalidTransition`] if not in `Pending` stage.
    ///
    /// # Panics
    /// This function never panics.
    pub fn start_validation(&self, deployment_id: &str) -> Result<(), DeploymentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        let deployment = inner
            .deployments
            .get_mut(deployment_id)
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))?;

        if deployment.stage != DeploymentStage::Pending {
            return Err(DeploymentError::InvalidTransition {
                deployment_id: deployment_id.to_string(),
                from: deployment.stage.clone(),
                to: DeploymentStage::Validating,
            });
        }

        deployment.transition_to(DeploymentStage::Validating, "starting validation");
        Ok(())
    }

    /// Transition a deployment from `Validating` to `Canary` after validation passes.
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    /// Returns [`DeploymentError::InvalidTransition`] if not in `Validating` stage.
    ///
    /// # Panics
    /// This function never panics.
    pub fn validation_passed(&self, deployment_id: &str) -> Result<(), DeploymentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        let deployment = inner
            .deployments
            .get_mut(deployment_id)
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))?;

        if deployment.stage != DeploymentStage::Validating {
            return Err(DeploymentError::InvalidTransition {
                deployment_id: deployment_id.to_string(),
                from: deployment.stage.clone(),
                to: DeploymentStage::Canary,
            });
        }

        deployment.transition_to(DeploymentStage::Canary, "validation passed");
        Ok(())
    }

    /// Transition a deployment from `Validating` to `Failed` after validation fails.
    ///
    /// # Arguments
    /// * `deployment_id` — The deployment to fail
    /// * `reason` — Human-readable reason for the failure
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    /// Returns [`DeploymentError::InvalidTransition`] if not in `Validating` stage.
    ///
    /// # Panics
    /// This function never panics.
    pub fn validation_failed(
        &self,
        deployment_id: &str,
        reason: &str,
    ) -> Result<(), DeploymentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        let deployment = inner
            .deployments
            .get_mut(deployment_id)
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))?;

        if deployment.stage != DeploymentStage::Validating {
            return Err(DeploymentError::InvalidTransition {
                deployment_id: deployment_id.to_string(),
                from: deployment.stage.clone(),
                to: DeploymentStage::Failed {
                    reason: reason.to_string(),
                },
            });
        }

        let fail_stage = DeploymentStage::Failed {
            reason: reason.to_string(),
        };
        deployment.transition_to(fail_stage, reason);
        Self::maybe_archive_deployment(&mut inner, deployment_id);
        Ok(())
    }

    /// Transition a deployment from `Canary` to `Rolling{0}` after canary passes.
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    /// Returns [`DeploymentError::InvalidTransition`] if not in `Canary` stage.
    ///
    /// # Panics
    /// This function never panics.
    pub fn canary_passed(&self, deployment_id: &str) -> Result<(), DeploymentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        let deployment = inner
            .deployments
            .get_mut(deployment_id)
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))?;

        if deployment.stage != DeploymentStage::Canary {
            return Err(DeploymentError::InvalidTransition {
                deployment_id: deployment_id.to_string(),
                from: deployment.stage.clone(),
                to: DeploymentStage::Rolling { progress_pct: 0 },
            });
        }

        deployment.transition_to(
            DeploymentStage::Rolling { progress_pct: 0 },
            "canary passed, starting rollout",
        );
        Ok(())
    }

    /// Transition a deployment from `Canary` to `RolledBack` after canary fails.
    ///
    /// # Arguments
    /// * `deployment_id` — The deployment to roll back
    /// * `reason` — Human-readable reason for the rollback
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    /// Returns [`DeploymentError::InvalidTransition`] if not in `Canary` stage.
    ///
    /// # Panics
    /// This function never panics.
    pub fn canary_failed(&self, deployment_id: &str, reason: &str) -> Result<(), DeploymentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        let deployment = inner
            .deployments
            .get_mut(deployment_id)
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))?;

        if deployment.stage != DeploymentStage::Canary {
            return Err(DeploymentError::InvalidTransition {
                deployment_id: deployment_id.to_string(),
                from: deployment.stage.clone(),
                to: DeploymentStage::RolledBack {
                    reason: reason.to_string(),
                },
            });
        }

        let rb_stage = DeploymentStage::RolledBack {
            reason: reason.to_string(),
        };
        deployment.transition_to(rb_stage, reason);
        Self::maybe_archive_deployment(&mut inner, deployment_id);
        Ok(())
    }

    /// Advance a rolling deployment to the next rollout step.
    ///
    /// Checks current metrics against baseline:
    /// - If any metric whose key contains `"error_rate"` exceeds `max_error_rate`,
    ///   the deployment is rolled back.
    /// - If any metric whose key contains `"latency"` has regressed more than
    ///   `max_latency_regression_pct` from its baseline value, the deployment
    ///   is rolled back.
    ///
    /// If all checks pass, advances to the next configured rollout step.
    /// If already at 100% and `auto_promote` is enabled, promotes automatically.
    ///
    /// # Arguments
    /// * `deployment_id` — The deployment to advance
    /// * `current_metrics` — Latest observed metrics for health checking
    ///
    /// # Returns
    /// The new [`DeploymentStage`] after advancement.
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    /// Returns [`DeploymentError::InvalidTransition`] if not in a `Rolling` stage.
    /// Returns [`DeploymentError::RolloutFailed`] if metrics checks fail.
    ///
    /// # Panics
    /// This function never panics.
    pub fn advance_rollout(
        &self,
        deployment_id: &str,
        current_metrics: HashMap<String, f64>,
    ) -> Result<DeploymentStage, DeploymentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        let deployment = inner
            .deployments
            .get_mut(deployment_id)
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))?;

        let current_pct = match &deployment.stage {
            DeploymentStage::Rolling { progress_pct } => *progress_pct,
            other => {
                return Err(DeploymentError::InvalidTransition {
                    deployment_id: deployment_id.to_string(),
                    from: other.clone(),
                    to: DeploymentStage::Rolling { progress_pct: 0 },
                });
            }
        };

        let max_error_rate = deployment.config.max_error_rate;
        let max_latency_regression = deployment.config.max_latency_regression_pct;

        // Check error rate metrics
        for (key, &value) in &current_metrics {
            if key.contains("error_rate") && value > max_error_rate {
                let reason =
                    format!("error rate {key}={value:.4} exceeds threshold {max_error_rate:.4}");
                let rb_stage = DeploymentStage::RolledBack {
                    reason: reason.clone(),
                };
                deployment.transition_to(rb_stage.clone(), &reason);
                deployment.current_metrics = current_metrics;
                let dep_id = deployment_id.to_string();
                Self::maybe_archive_deployment(&mut inner, deployment_id);
                return Err(DeploymentError::RolloutFailed {
                    deployment_id: dep_id,
                    reason,
                });
            }
        }

        // Check latency regression metrics
        for (key, &current_value) in &current_metrics {
            if key.contains("latency") {
                if let Some(&baseline_value) = deployment.metrics_at_start.get(key) {
                    if baseline_value > 0.0 {
                        let regression = (current_value - baseline_value) / baseline_value;
                        if regression > max_latency_regression {
                            let reason = format!(
                                "latency regression {key}: baseline={baseline_value:.2}, \
                                 current={current_value:.2}, regression={regression:.4} \
                                 exceeds threshold {max_latency_regression:.4}"
                            );
                            let rb_stage = DeploymentStage::RolledBack {
                                reason: reason.clone(),
                            };
                            deployment.transition_to(rb_stage.clone(), &reason);
                            deployment.current_metrics = current_metrics;
                            let dep_id = deployment_id.to_string();
                            Self::maybe_archive_deployment(&mut inner, deployment_id);
                            return Err(DeploymentError::RolloutFailed {
                                deployment_id: dep_id,
                                reason,
                            });
                        }
                    }
                }
            }
        }

        // Update current metrics
        deployment.current_metrics = current_metrics;

        // Find next rollout step
        let rollout_steps = deployment.config.rollout_steps.clone();
        let auto_promote = deployment.config.auto_promote;

        let next_step = rollout_steps.iter().find(|&&step| step > current_pct);

        match next_step {
            Some(&next_pct) => {
                let new_stage = DeploymentStage::Rolling {
                    progress_pct: next_pct,
                };
                deployment.transition_to(
                    new_stage.clone(),
                    &format!("advancing rollout to {next_pct}%"),
                );
                Ok(new_stage)
            }
            None => {
                // At or beyond 100%
                if auto_promote {
                    deployment.transition_to(
                        DeploymentStage::Promoted,
                        "rollout complete, auto-promoting",
                    );
                    let dep_id = deployment_id.to_string();
                    Self::maybe_archive_deployment(&mut inner, &dep_id);
                    Ok(DeploymentStage::Promoted)
                } else {
                    // Stay at current percentage, caller must explicitly promote
                    Ok(DeploymentStage::Rolling {
                        progress_pct: current_pct,
                    })
                }
            }
        }
    }

    /// Roll back a deployment from any active (non-terminal) stage.
    ///
    /// # Arguments
    /// * `deployment_id` — The deployment to roll back
    /// * `reason` — Human-readable reason for the rollback
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    /// Returns [`DeploymentError::InvalidTransition`] if already in a terminal stage.
    ///
    /// # Panics
    /// This function never panics.
    pub fn rollback(&self, deployment_id: &str, reason: &str) -> Result<(), DeploymentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        let deployment = inner
            .deployments
            .get_mut(deployment_id)
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))?;

        if deployment.stage.is_terminal() {
            return Err(DeploymentError::InvalidTransition {
                deployment_id: deployment_id.to_string(),
                from: deployment.stage.clone(),
                to: DeploymentStage::RolledBack {
                    reason: reason.to_string(),
                },
            });
        }

        let rb_stage = DeploymentStage::RolledBack {
            reason: reason.to_string(),
        };
        deployment.transition_to(rb_stage, reason);
        Self::maybe_archive_deployment(&mut inner, deployment_id);
        Ok(())
    }

    /// Promote a deployment that has completed 100% rollout.
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    /// Returns [`DeploymentError::InvalidTransition`] if not at `Rolling{100}`.
    ///
    /// # Panics
    /// This function never panics.
    pub fn promote(&self, deployment_id: &str) -> Result<(), DeploymentError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        let deployment = inner
            .deployments
            .get_mut(deployment_id)
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))?;

        let is_rolling_100 = matches!(
            &deployment.stage,
            DeploymentStage::Rolling { progress_pct: 100 }
        );

        if !is_rolling_100 {
            return Err(DeploymentError::InvalidTransition {
                deployment_id: deployment_id.to_string(),
                from: deployment.stage.clone(),
                to: DeploymentStage::Promoted,
            });
        }

        deployment.transition_to(DeploymentStage::Promoted, "manually promoted");
        Self::maybe_archive_deployment(&mut inner, deployment_id);
        Ok(())
    }

    /// Retrieve a full clone of a deployment by ID.
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    ///
    /// # Panics
    /// This function never panics.
    pub fn get_deployment(&self, deployment_id: &str) -> Result<Deployment, DeploymentError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        inner
            .deployments
            .get(deployment_id)
            .cloned()
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))
    }

    /// Return summaries of all active (non-terminal) deployments.
    ///
    /// # Panics
    /// This function never panics.
    pub fn active_deployments(&self) -> Vec<DeploymentSummary> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .deployments
                    .values()
                    .filter(|d| !d.stage.is_terminal())
                    .map(|d| d.summarize())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return summaries of all completed (archived) deployments.
    ///
    /// # Panics
    /// This function never panics.
    pub fn completed_deployments(&self) -> Vec<DeploymentSummary> {
        self.inner
            .lock()
            .map(|inner| inner.completed.clone())
            .unwrap_or_default()
    }

    /// Return the current stage of a deployment.
    ///
    /// # Errors
    /// Returns [`DeploymentError::DeploymentNotFound`] if the deployment does not exist.
    ///
    /// # Panics
    /// This function never panics.
    pub fn stage_of(&self, deployment_id: &str) -> Result<DeploymentStage, DeploymentError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| DeploymentError::LockPoisoned)?;
        inner
            .deployments
            .get(deployment_id)
            .map(|d| d.stage.clone())
            .ok_or_else(|| DeploymentError::DeploymentNotFound(deployment_id.to_string()))
    }

    // ── Private helpers ──────────────────────────────────────────────────

    /// If the deployment is in a terminal state, move it from active to completed.
    fn maybe_archive_deployment(inner: &mut PipelineInner, deployment_id: &str) {
        let should_archive = inner
            .deployments
            .get(deployment_id)
            .map(|d| d.stage.is_terminal())
            .unwrap_or(false);

        if should_archive {
            if let Some(deployment) = inner.deployments.remove(deployment_id) {
                let summary = deployment.summarize();
                inner.completed.push(summary);
                // Trim history to max_history
                while inner.completed.len() > inner.max_history {
                    inner.completed.remove(0);
                }
            }
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Return the current Unix timestamp in seconds.
///
/// Returns 0 if the system clock is before the Unix epoch (should never
/// happen in practice).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pipeline() -> DeploymentPipeline {
        DeploymentPipeline::with_defaults()
    }

    fn make_pipeline_auto_promote() -> DeploymentPipeline {
        DeploymentPipeline::new(DeploymentConfig {
            auto_promote: true,
            ..Default::default()
        })
    }

    fn baseline_metrics() -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("error_rate".to_string(), 0.01);
        m.insert("latency_p50_ms".to_string(), 100.0);
        m.insert("latency_p99_ms".to_string(), 250.0);
        m
    }

    fn healthy_metrics() -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("error_rate".to_string(), 0.02);
        m.insert("latency_p50_ms".to_string(), 105.0);
        m.insert("latency_p99_ms".to_string(), 260.0);
        m
    }

    fn high_error_metrics() -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("error_rate".to_string(), 0.10); // 10% > 5% threshold
        m.insert("latency_p50_ms".to_string(), 105.0);
        m
    }

    fn high_latency_metrics() -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("error_rate".to_string(), 0.01);
        m.insert("latency_p50_ms".to_string(), 200.0); // 100% regression > 10% threshold
        m
    }

    /// Drive a deployment through to Rolling{0} for advance_rollout tests.
    fn drive_to_rolling(pipeline: &DeploymentPipeline, id: &str) {
        pipeline
            .create_deployment(id, "test deploy", "proposal-1", baseline_metrics())
            .ok();
        pipeline.start_validation(id).ok();
        pipeline.validation_passed(id).ok();
        pipeline.canary_passed(id).ok();
    }

    // ── test_create_deployment ────────────────────────────────────────────

    #[test]
    fn test_create_deployment() {
        let pipeline = make_pipeline();
        let result =
            pipeline.create_deployment("dep-1", "first deploy", "proposal-1", baseline_metrics());
        assert!(result.is_ok());

        let dep = pipeline.get_deployment("dep-1").unwrap();
        assert_eq!(dep.id, "dep-1");
        assert_eq!(dep.description, "first deploy");
        assert_eq!(dep.proposal_id, "proposal-1");
        assert_eq!(dep.stage, DeploymentStage::Pending);
        assert!(dep.stage_history.is_empty());
    }

    // ── test_create_duplicate_fails ───────────────────────────────────────

    #[test]
    fn test_create_duplicate_fails() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("dep-1", "first", "p-1", HashMap::new())
            .unwrap();
        let result = pipeline.create_deployment("dep-1", "second", "p-2", HashMap::new());
        assert!(matches!(
            result,
            Err(DeploymentError::DeploymentAlreadyExists(ref id)) if id == "dep-1"
        ));
    }

    // ── test_start_validation ─────────────────────────────────────────────

    #[test]
    fn test_start_validation() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("dep-1", "test", "p-1", HashMap::new())
            .unwrap();
        let result = pipeline.start_validation("dep-1");
        assert!(result.is_ok());
        assert_eq!(
            pipeline.stage_of("dep-1").unwrap(),
            DeploymentStage::Validating
        );
    }

    // ── test_invalid_transition_pending_to_canary ─────────────────────────

    #[test]
    fn test_invalid_transition_pending_to_canary() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("dep-1", "test", "p-1", HashMap::new())
            .unwrap();
        // Try to skip Validating and go straight to Canary
        let result = pipeline.validation_passed("dep-1");
        assert!(matches!(
            result,
            Err(DeploymentError::InvalidTransition { .. })
        ));
    }

    // ── test_validation_passed_moves_to_canary ────────────────────────────

    #[test]
    fn test_validation_passed_moves_to_canary() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("dep-1", "test", "p-1", HashMap::new())
            .unwrap();
        pipeline.start_validation("dep-1").unwrap();
        pipeline.validation_passed("dep-1").unwrap();
        assert_eq!(pipeline.stage_of("dep-1").unwrap(), DeploymentStage::Canary);
    }

    // ── test_validation_failed_moves_to_failed ────────────────────────────

    #[test]
    fn test_validation_failed_moves_to_failed() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("dep-1", "test", "p-1", HashMap::new())
            .unwrap();
        pipeline.start_validation("dep-1").unwrap();
        pipeline
            .validation_failed("dep-1", "clippy warnings")
            .unwrap();
        // Deployment should be archived (terminal)
        let result = pipeline.stage_of("dep-1");
        assert!(matches!(
            result,
            Err(DeploymentError::DeploymentNotFound(_))
        ));
        // But it should be in completed
        let completed = pipeline.completed_deployments();
        assert_eq!(completed.len(), 1);
        assert!(matches!(completed[0].stage, DeploymentStage::Failed { .. }));
    }

    // ── test_canary_passed_starts_rollout ─────────────────────────────────

    #[test]
    fn test_canary_passed_starts_rollout() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("dep-1", "test", "p-1", HashMap::new())
            .unwrap();
        pipeline.start_validation("dep-1").unwrap();
        pipeline.validation_passed("dep-1").unwrap();
        pipeline.canary_passed("dep-1").unwrap();
        assert_eq!(
            pipeline.stage_of("dep-1").unwrap(),
            DeploymentStage::Rolling { progress_pct: 0 }
        );
    }

    // ── test_canary_failed_rolls_back ─────────────────────────────────────

    #[test]
    fn test_canary_failed_rolls_back() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("dep-1", "test", "p-1", HashMap::new())
            .unwrap();
        pipeline.start_validation("dep-1").unwrap();
        pipeline.validation_passed("dep-1").unwrap();
        pipeline
            .canary_failed("dep-1", "error spike in canary")
            .unwrap();
        // Should be archived
        let completed = pipeline.completed_deployments();
        assert_eq!(completed.len(), 1);
        assert!(matches!(
            completed[0].stage,
            DeploymentStage::RolledBack { .. }
        ));
    }

    // ── test_advance_rollout_progresses ───────────────────────────────────

    #[test]
    fn test_advance_rollout_progresses() {
        let pipeline = make_pipeline();
        drive_to_rolling(&pipeline, "dep-1");

        // Default steps: [10, 25, 50, 75, 100]
        let stage = pipeline
            .advance_rollout("dep-1", healthy_metrics())
            .unwrap();
        assert_eq!(stage, DeploymentStage::Rolling { progress_pct: 10 });

        let stage = pipeline
            .advance_rollout("dep-1", healthy_metrics())
            .unwrap();
        assert_eq!(stage, DeploymentStage::Rolling { progress_pct: 25 });

        let stage = pipeline
            .advance_rollout("dep-1", healthy_metrics())
            .unwrap();
        assert_eq!(stage, DeploymentStage::Rolling { progress_pct: 50 });
    }

    // ── test_advance_rollout_checks_error_rate ────────────────────────────

    #[test]
    fn test_advance_rollout_checks_error_rate() {
        let pipeline = make_pipeline();
        drive_to_rolling(&pipeline, "dep-1");

        let result = pipeline.advance_rollout("dep-1", high_error_metrics());
        assert!(matches!(result, Err(DeploymentError::RolloutFailed { .. })));
        // Deployment should be archived as rolled back
        let completed = pipeline.completed_deployments();
        assert!(completed
            .iter()
            .any(|d| d.id == "dep-1" && matches!(d.stage, DeploymentStage::RolledBack { .. })));
    }

    // ── test_advance_rollout_checks_latency ───────────────────────────────

    #[test]
    fn test_advance_rollout_checks_latency() {
        let pipeline = make_pipeline();
        drive_to_rolling(&pipeline, "dep-1");

        let result = pipeline.advance_rollout("dep-1", high_latency_metrics());
        assert!(matches!(result, Err(DeploymentError::RolloutFailed { .. })));
    }

    // ── test_advance_rollout_auto_promote ─────────────────────────────────

    #[test]
    fn test_advance_rollout_auto_promote() {
        let pipeline = make_pipeline_auto_promote();
        drive_to_rolling(&pipeline, "dep-1");

        // Advance through all steps: 0→10→25→50→75→100→Promoted
        for _ in 0..5 {
            pipeline
                .advance_rollout("dep-1", healthy_metrics())
                .unwrap();
        }

        // Next advance at 100% with auto_promote → Promoted
        let stage = pipeline
            .advance_rollout("dep-1", healthy_metrics())
            .unwrap();
        assert_eq!(stage, DeploymentStage::Promoted);
    }

    // ── test_advance_rollout_no_auto_promote ──────────────────────────────

    #[test]
    fn test_advance_rollout_no_auto_promote() {
        let pipeline = make_pipeline(); // auto_promote = false
        drive_to_rolling(&pipeline, "dep-1");

        // Advance through all steps: 0→10→25→50→75→100
        for _ in 0..5 {
            pipeline
                .advance_rollout("dep-1", healthy_metrics())
                .unwrap();
        }

        // At 100% with no auto_promote — stays at 100
        let stage = pipeline
            .advance_rollout("dep-1", healthy_metrics())
            .unwrap();
        assert_eq!(stage, DeploymentStage::Rolling { progress_pct: 100 });
    }

    // ── test_rollback_from_any_active_stage ───────────────────────────────

    #[test]
    fn test_rollback_from_any_active_stage() {
        // Rollback from Pending
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("d1", "test", "p-1", HashMap::new())
            .unwrap();
        assert!(pipeline.rollback("d1", "changed our mind").is_ok());

        // Rollback from Validating
        pipeline
            .create_deployment("d2", "test", "p-1", HashMap::new())
            .unwrap();
        pipeline.start_validation("d2").unwrap();
        assert!(pipeline.rollback("d2", "found issue").is_ok());

        // Rollback from Canary
        pipeline
            .create_deployment("d3", "test", "p-1", HashMap::new())
            .unwrap();
        pipeline.start_validation("d3").unwrap();
        pipeline.validation_passed("d3").unwrap();
        assert!(pipeline.rollback("d3", "canary bad").is_ok());

        // Rollback from Rolling
        drive_to_rolling(&pipeline, "d4");
        assert!(pipeline.rollback("d4", "metrics degraded").is_ok());
    }

    // ── test_rollback_terminal_fails ──────────────────────────────────────

    #[test]
    fn test_rollback_terminal_fails() {
        let pipeline = make_pipeline_auto_promote();
        drive_to_rolling(&pipeline, "dep-1");

        // Drive to Promoted
        for _ in 0..5 {
            pipeline
                .advance_rollout("dep-1", healthy_metrics())
                .unwrap();
        }
        pipeline
            .advance_rollout("dep-1", healthy_metrics())
            .unwrap();

        // dep-1 is now archived (Promoted), so it's not found
        let result = pipeline.rollback("dep-1", "too late");
        assert!(matches!(
            result,
            Err(DeploymentError::DeploymentNotFound(_))
        ));
    }

    // ── test_promote_from_rolling_100 ─────────────────────────────────────

    #[test]
    fn test_promote_from_rolling_100() {
        let pipeline = make_pipeline(); // auto_promote = false
        drive_to_rolling(&pipeline, "dep-1");

        // Advance to 100%
        for _ in 0..5 {
            pipeline
                .advance_rollout("dep-1", healthy_metrics())
                .unwrap();
        }

        assert_eq!(
            pipeline.stage_of("dep-1").unwrap(),
            DeploymentStage::Rolling { progress_pct: 100 }
        );

        let result = pipeline.promote("dep-1");
        assert!(result.is_ok());

        // Should be archived
        let completed = pipeline.completed_deployments();
        assert!(completed
            .iter()
            .any(|d| d.id == "dep-1" && d.stage == DeploymentStage::Promoted));
    }

    // ── test_promote_from_non_rolling_fails ───────────────────────────────

    #[test]
    fn test_promote_from_non_rolling_fails() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("dep-1", "test", "p-1", HashMap::new())
            .unwrap();
        let result = pipeline.promote("dep-1");
        assert!(matches!(
            result,
            Err(DeploymentError::InvalidTransition { .. })
        ));
    }

    // ── test_get_deployment ───────────────────────────────────────────────

    #[test]
    fn test_get_deployment() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("dep-1", "my deploy", "p-1", baseline_metrics())
            .unwrap();
        let dep = pipeline.get_deployment("dep-1").unwrap();
        assert_eq!(dep.id, "dep-1");
        assert_eq!(dep.description, "my deploy");
        assert!(!dep.metrics_at_start.is_empty());
    }

    // ── test_get_nonexistent ──────────────────────────────────────────────

    #[test]
    fn test_get_nonexistent() {
        let pipeline = make_pipeline();
        let result = pipeline.get_deployment("nope");
        assert!(matches!(
            result,
            Err(DeploymentError::DeploymentNotFound(ref id)) if id == "nope"
        ));
    }

    // ── test_active_deployments ───────────────────────────────────────────

    #[test]
    fn test_active_deployments() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("d1", "active one", "p-1", HashMap::new())
            .unwrap();
        pipeline
            .create_deployment("d2", "active two", "p-2", HashMap::new())
            .unwrap();

        let active = pipeline.active_deployments();
        assert_eq!(active.len(), 2);
    }

    // ── test_completed_deployments ────────────────────────────────────────

    #[test]
    fn test_completed_deployments() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("d1", "will fail", "p-1", HashMap::new())
            .unwrap();
        pipeline.start_validation("d1").unwrap();
        pipeline.validation_failed("d1", "tests broke").unwrap();

        let completed = pipeline.completed_deployments();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].id, "d1");
    }

    // ── test_stage_of ─────────────────────────────────────────────────────

    #[test]
    fn test_stage_of() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("d1", "test", "p-1", HashMap::new())
            .unwrap();
        assert_eq!(pipeline.stage_of("d1").unwrap(), DeploymentStage::Pending);
        pipeline.start_validation("d1").unwrap();
        assert_eq!(
            pipeline.stage_of("d1").unwrap(),
            DeploymentStage::Validating
        );
    }

    // ── test_stage_history_recorded ───────────────────────────────────────

    #[test]
    fn test_stage_history_recorded() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("d1", "test", "p-1", HashMap::new())
            .unwrap();
        pipeline.start_validation("d1").unwrap();
        pipeline.validation_passed("d1").unwrap();

        let dep = pipeline.get_deployment("d1").unwrap();
        assert_eq!(dep.stage_history.len(), 2);
        assert_eq!(dep.stage_history[0].from, DeploymentStage::Pending);
        assert_eq!(dep.stage_history[0].to, DeploymentStage::Validating);
        assert_eq!(dep.stage_history[1].from, DeploymentStage::Validating);
        assert_eq!(dep.stage_history[1].to, DeploymentStage::Canary);
    }

    // ── test_config_defaults ──────────────────────────────────────────────

    #[test]
    fn test_config_defaults() {
        let cfg = DeploymentConfig::default();
        assert_eq!(cfg.canary_duration_secs, 300);
        assert_eq!(cfg.canary_traffic_pct, 5);
        assert_eq!(cfg.rollout_steps, vec![10, 25, 50, 75, 100]);
        assert_eq!(cfg.step_duration_secs, 120);
        assert!((cfg.max_error_rate - 0.05).abs() < f64::EPSILON);
        assert!((cfg.max_latency_regression_pct - 0.10).abs() < f64::EPSILON);
        assert!(!cfg.auto_promote);
    }

    // ── test_clone_shares_state ───────────────────────────────────────────

    #[test]
    fn test_clone_shares_state() {
        let pipeline = make_pipeline();
        let pipeline2 = pipeline.clone();

        pipeline
            .create_deployment("d1", "test", "p-1", HashMap::new())
            .unwrap();

        // Second clone should see the deployment
        let dep = pipeline2.get_deployment("d1");
        assert!(dep.is_ok());
        assert_eq!(dep.unwrap().id, "d1");
    }

    // ── test_deployment_stage_serialization ────────────────────────────────

    #[test]
    fn test_deployment_stage_serialization() {
        let stages = vec![
            DeploymentStage::Pending,
            DeploymentStage::Validating,
            DeploymentStage::Canary,
            DeploymentStage::Rolling { progress_pct: 50 },
            DeploymentStage::Promoted,
            DeploymentStage::RolledBack {
                reason: "bad metrics".to_string(),
            },
            DeploymentStage::Failed {
                reason: "tests broke".to_string(),
            },
        ];

        for stage in &stages {
            let json = serde_json::to_string(stage).unwrap();
            let back: DeploymentStage = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, stage);
        }
    }

    // ── test_full_lifecycle_happy_path ─────────────────────────────────────

    #[test]
    fn test_full_lifecycle_happy_path() {
        let pipeline = make_pipeline(); // auto_promote = false
        let metrics = baseline_metrics();

        // Create
        pipeline
            .create_deployment("dep-1", "full lifecycle test", "proposal-42", metrics)
            .unwrap();
        assert_eq!(
            pipeline.stage_of("dep-1").unwrap(),
            DeploymentStage::Pending
        );

        // Validate
        pipeline.start_validation("dep-1").unwrap();
        assert_eq!(
            pipeline.stage_of("dep-1").unwrap(),
            DeploymentStage::Validating
        );

        // Canary
        pipeline.validation_passed("dep-1").unwrap();
        assert_eq!(pipeline.stage_of("dep-1").unwrap(), DeploymentStage::Canary);

        // Start rollout
        pipeline.canary_passed("dep-1").unwrap();
        assert_eq!(
            pipeline.stage_of("dep-1").unwrap(),
            DeploymentStage::Rolling { progress_pct: 0 }
        );

        // Advance through all steps: 10, 25, 50, 75, 100
        let expected_steps = [10u8, 25, 50, 75, 100];
        for &expected in &expected_steps {
            let stage = pipeline
                .advance_rollout("dep-1", healthy_metrics())
                .unwrap();
            assert_eq!(
                stage,
                DeploymentStage::Rolling {
                    progress_pct: expected
                }
            );
        }

        // Promote manually
        pipeline.promote("dep-1").unwrap();

        // Should be in completed
        let completed = pipeline.completed_deployments();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].id, "dep-1");
        assert_eq!(completed[0].stage, DeploymentStage::Promoted);

        // Active should be empty
        assert!(pipeline.active_deployments().is_empty());
    }

    // ── Additional edge-case tests ────────────────────────────────────────

    #[test]
    fn test_promote_from_rolling_50_fails() {
        let pipeline = make_pipeline();
        drive_to_rolling(&pipeline, "dep-1");

        // Advance to 10% only
        pipeline
            .advance_rollout("dep-1", healthy_metrics())
            .unwrap();

        let result = pipeline.promote("dep-1");
        assert!(matches!(
            result,
            Err(DeploymentError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn test_start_validation_on_nonexistent_fails() {
        let pipeline = make_pipeline();
        let result = pipeline.start_validation("ghost");
        assert!(matches!(
            result,
            Err(DeploymentError::DeploymentNotFound(_))
        ));
    }

    #[test]
    fn test_advance_rollout_on_pending_fails() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("d1", "test", "p-1", HashMap::new())
            .unwrap();
        let result = pipeline.advance_rollout("d1", HashMap::new());
        assert!(matches!(
            result,
            Err(DeploymentError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn test_stage_transition_has_timestamps() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("d1", "test", "p-1", HashMap::new())
            .unwrap();
        pipeline.start_validation("d1").unwrap();

        let dep = pipeline.get_deployment("d1").unwrap();
        assert_eq!(dep.stage_history.len(), 1);
        assert!(dep.stage_history[0].timestamp_secs > 0);
        assert!(dep.updated_at_secs >= dep.created_at_secs);
    }

    #[test]
    fn test_deployment_error_display() {
        let err = DeploymentError::DeploymentNotFound("xyz".to_string());
        assert!(err.to_string().contains("xyz"));

        let err = DeploymentError::InvalidTransition {
            deployment_id: "d1".to_string(),
            from: DeploymentStage::Pending,
            to: DeploymentStage::Canary,
        };
        assert!(err.to_string().contains("d1"));

        let err = DeploymentError::CanaryFailed {
            deployment_id: "d2".to_string(),
            reason: "errors too high".to_string(),
        };
        assert!(err.to_string().contains("errors too high"));
    }

    #[test]
    fn test_multiple_deployments_independent() {
        let pipeline = make_pipeline();
        pipeline
            .create_deployment("d1", "first", "p-1", HashMap::new())
            .unwrap();
        pipeline
            .create_deployment("d2", "second", "p-2", HashMap::new())
            .unwrap();

        pipeline.start_validation("d1").unwrap();

        // d2 should still be Pending
        assert_eq!(pipeline.stage_of("d2").unwrap(), DeploymentStage::Pending);
        assert_eq!(
            pipeline.stage_of("d1").unwrap(),
            DeploymentStage::Validating
        );
    }
}
