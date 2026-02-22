//! # Self-Modifying Agent Loop
//!
//! Modules that enable the system to generate, validate, deploy, and document
//! its own modifications autonomously.
//!
//! ## Feature flag: `self-modify`
//! Implies `self-tune`.  The pipeline operates identically with this feature
//! disabled.
//!
//! ## Module map
//! - [`task_gen`]  — converts telemetry signals into agent work orders
//! - [`gate`]      — validates proposed changes through cargo/clippy/benchmark gates
//! - [`memory`]    — persistent knowledge base for the agent fleet
//! - [`docs`]      — auto-generates changelogs, diagrams, and impact reports
//! - [`discover`]  — periodic codebase scanning for improvement opportunities

pub mod deployment;
pub mod discover;
pub mod docs;
pub mod gate;
pub mod memory;
pub mod task_gen;

pub use deployment::{
    Deployment, DeploymentConfig, DeploymentError, DeploymentPipeline, DeploymentStage,
    DeploymentSummary, StageTransition,
};
pub use discover::{CapabilityDiscovery, DiscoveryConfig, DiscoveryFinding, FindingCategory};
pub use docs::{ChangelogEntry, MetricDelta, SelfDocGenerator};
pub use gate::{
    BenchmarkSnapshot, GateConfig, GateOutcome, GateReport, RecommendedAction, SmokeTestRunner,
    ValidationGate,
};
pub use memory::{AgentMemory, ModificationOutcome, ModificationRecord};
pub use task_gen::{Complexity, GeneratedTask, MetaTaskGenerator, TaskPriority};
