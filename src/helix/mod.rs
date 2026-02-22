//! # HelixRouter — Adaptive Async Compute Router
//!
//! ## Responsibility
//! Route compute jobs into the optimal execution strategy based on
//! cost thresholds and real-time system pressure:
//!
//! | Strategy | When | Overhead |
//! |----------|------|----------|
//! | `Inline` | cost < inline_threshold | Zero (current task) |
//! | `Spawn` | cost < spawn_threshold | Tokio task spawn |
//! | `CpuPool` | cost < cpu_pool_threshold | Channel + spawn_blocking |
//! | `Batch` | cost >= cpu_pool_threshold | Queue + batch flush |
//! | `Drop` | pressure > drop_threshold | None (job shed) |
//!
//! ## Guarantees
//! - Thread-safe: `HelixRouter` is `Clone + Send + Sync`.
//! - Adaptive: thresholds adjust based on observed p95 latency.
//! - Deterministic: same job kind + cost always produces the same compute result.
//! - Graceful degradation: under extreme load, jobs are shed rather than queued.
//!
//! ## NOT Responsible For
//! - LLM pipeline orchestration (see `stages`, `worker`)
//! - Model routing intelligence (see `routing`)
//! - Configuration file watching (see `config`)

pub mod config;
pub mod distributed;
pub mod metrics;
pub mod router;
pub mod simulator;
pub mod strategies;
pub mod types;
#[cfg(feature = "web-api")]
pub mod web;

// Re-exports for convenience
pub use config::{ConfigReloader, HelixConfig};
pub use metrics::MetricsStore;
pub use router::{HelixError, HelixRouter};
pub use simulator::{PressureProfile, SimulatorConfig};
pub use types::{Job, JobId, JobKind, Output, Strategy};
