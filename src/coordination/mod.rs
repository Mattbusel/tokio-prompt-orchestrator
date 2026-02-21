//! # Coordination — programmatic agent fleet management
//!
//! ## Responsibility
//! Provide a complete agent coordination layer: define tasks in TOML,
//! spawn agent processes, let them claim work atomically via filesystem
//! locks, and monitor fleet health — all without manual terminal pane
//! management.
//!
//! ## Architecture
//!
//! ```text
//! tasks.toml ──► TaskQueue (file-locked claiming)
//!                    │
//!                    ├──► AgentWorker 0 ──► claude process
//!                    ├──► AgentWorker 1 ──► claude process
//!                    ├──► AgentWorker N ──► claude process
//!                    │
//!               AgentMonitor (health checks, status)
//! ```
//!
//! ## Modules
//!
//! - [`task`] — Task struct, TaskStatus, priority ordering
//! - [`config`] — CoordinationConfig with defaults and validation
//! - [`queue`] — TaskQueue with file-locked concurrent claiming
//! - [`worker`] — AgentWorker: spawn claude process, capture output
//! - [`spawner`] — AgentSpawner: spawn N agents, manage fleet lifecycle
//! - [`monitor`] — AgentMonitor: health checks, fleet status snapshots
//!
//! ## Guarantees
//!
//! - **Atomic claiming**: no task is ever executed twice (filesystem locks)
//! - **Crash recovery**: stale locks from dead agents are reclaimable
//! - **Priority ordering**: highest-priority tasks are claimed first
//! - **Observable**: fleet status available at any time via monitor snapshots
//! - **Zero panics**: all operations return `Result`, never panic
//!
//! ## NOT Responsible For
//!
//! - LLM inference (see: `worker` module at crate root)
//! - Pipeline stages (see: `stages` module)
//! - Web API (see: `web_api` module)
//!
//! ## Example
//!
//! ```rust,no_run
//! use tokio_prompt_orchestrator::coordination::{
//!     config::CoordinationConfig,
//!     queue::TaskQueue,
//!     spawner::{AgentSpawner, await_fleet},
//! };
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = Arc::new(CoordinationConfig::default());
//! let queue = Arc::new(TaskQueue::new(config.clone()).await?);
//! let spawner = AgentSpawner::new(config);
//! let handles = spawner.spawn_fleet(queue).await?;
//! let stats = await_fleet(handles).await;
//! for s in &stats {
//!     println!("{}: {} completed, {} failed", s.agent_id, s.tasks_completed, s.tasks_failed);
//! }
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod monitor;
pub mod queue;
pub mod spawner;
pub mod task;
pub mod worker;

use std::path::PathBuf;
use thiserror::Error;

/// Errors specific to the agent coordination layer.
///
/// All variants implement `std::error::Error` via [`thiserror`].
/// Every variant has at least one test that triggers it.
///
/// # Panics
///
/// No methods on this type panic.
#[derive(Error, Debug)]
pub enum CoordinationError {
    /// Task file not found at the specified path.
    #[error("task file not found: {}", path.display())]
    TaskFileNotFound {
        /// Path that was looked up.
        path: PathBuf,
    },

    /// Task file could not be parsed as valid TOML.
    #[error("task file parse error: {0}")]
    TaskFileParse(String),

    /// Referenced task ID does not exist in the queue.
    #[error("task not found: {id}")]
    TaskNotFound {
        /// The task ID that was not found.
        id: String,
    },

    /// Task was already claimed by another agent.
    #[error("task already claimed: {id} by {agent}")]
    TaskAlreadyClaimed {
        /// The task ID.
        id: String,
        /// The agent that holds the claim.
        agent: String,
    },

    /// Agent process could not be spawned.
    #[error("agent spawn failed: {0}")]
    SpawnError(String),

    /// Agent exceeded its configured timeout.
    #[error("agent timeout: agent {agent_id} exceeded {timeout_secs}s")]
    AgentTimeout {
        /// The agent that timed out.
        agent_id: String,
        /// The timeout that was exceeded.
        timeout_secs: u64,
    },

    /// Filesystem I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// No tasks are available for claiming.
    #[error("no tasks available")]
    NoTasksAvailable,

    /// Configuration validation failed.
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordination_error_display_task_file_not_found() {
        let err = CoordinationError::TaskFileNotFound {
            path: PathBuf::from("tasks.toml"),
        };
        assert!(err.to_string().contains("tasks.toml"));
    }

    #[test]
    fn test_coordination_error_display_task_file_parse() {
        let err = CoordinationError::TaskFileParse("unexpected token".to_string());
        assert!(err.to_string().contains("unexpected token"));
    }

    #[test]
    fn test_coordination_error_display_task_not_found() {
        let err = CoordinationError::TaskNotFound {
            id: "task-42".to_string(),
        };
        assert!(err.to_string().contains("task-42"));
    }

    #[test]
    fn test_coordination_error_display_task_already_claimed() {
        let err = CoordinationError::TaskAlreadyClaimed {
            id: "task-1".to_string(),
            agent: "agent-0".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("task-1"));
        assert!(msg.contains("agent-0"));
    }

    #[test]
    fn test_coordination_error_display_spawn_error() {
        let err = CoordinationError::SpawnError("binary not found".to_string());
        assert!(err.to_string().contains("binary not found"));
    }

    #[test]
    fn test_coordination_error_display_agent_timeout() {
        let err = CoordinationError::AgentTimeout {
            agent_id: "agent-3".to_string(),
            timeout_secs: 600,
        };
        let msg = err.to_string();
        assert!(msg.contains("agent-3"));
        assert!(msg.contains("600"));
    }

    #[test]
    fn test_coordination_error_display_io() {
        let err = CoordinationError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        ));
        assert!(err.to_string().contains("access denied"));
    }

    #[test]
    fn test_coordination_error_display_no_tasks_available() {
        let err = CoordinationError::NoTasksAvailable;
        assert!(err.to_string().contains("no tasks available"));
    }

    #[test]
    fn test_coordination_error_display_invalid_config() {
        let err = CoordinationError::InvalidConfig("agent_count must be > 0".to_string());
        assert!(err.to_string().contains("agent_count must be > 0"));
    }

    #[test]
    fn test_coordination_error_debug_format() {
        let err = CoordinationError::NoTasksAvailable;
        let debug = format!("{:?}", err);
        assert!(debug.contains("NoTasksAvailable"));
    }

    #[test]
    fn test_coordination_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let coord_err: CoordinationError = io_err.into();
        assert!(matches!(coord_err, CoordinationError::Io(_)));
    }
}
