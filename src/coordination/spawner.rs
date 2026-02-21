//! # AgentSpawner — fleet spawning and coordination
//!
//! ## Responsibility
//! Spawn N agent processes, each running a claim-execute-complete loop.
//! Manages the lifecycle of the agent fleet from startup to shutdown.
//!
//! ## Guarantees
//! - Concurrent: agents run in parallel tokio tasks
//! - Graceful: fleet shuts down cleanly when all tasks are done
//! - Observable: each agent logs its activity via tracing
//!
//! ## NOT Responsible For
//! - Task claiming logic (see: queue.rs)
//! - Process spawning details (see: worker.rs)
//! - Health monitoring (see: monitor.rs)

use crate::coordination::config::CoordinationConfig;
use crate::coordination::queue::TaskQueue;
use crate::coordination::worker::AgentWorker;
use crate::coordination::CoordinationError;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Handle to a running agent in the fleet.
///
/// # Panics
///
/// No methods on this type panic.
pub struct AgentHandle {
    /// Unique identifier for this agent.
    pub agent_id: String,
    /// Join handle for the agent's tokio task.
    pub join_handle: JoinHandle<Result<AgentStats, CoordinationError>>,
}

/// Statistics for a completed agent run.
///
/// # Panics
///
/// No methods on this type panic.
#[derive(Debug, Clone, Default)]
pub struct AgentStats {
    /// Agent identifier.
    pub agent_id: String,
    /// Number of tasks completed successfully.
    pub tasks_completed: usize,
    /// Number of tasks that failed.
    pub tasks_failed: usize,
    /// Total tasks attempted.
    pub tasks_attempted: usize,
}

/// Spawns and manages a fleet of agent workers.
///
/// Each agent runs an autonomous claim loop:
/// 1. Claim the highest-priority available task
/// 2. Execute it via a claude process
/// 3. Mark it as completed or failed
/// 4. Repeat until no tasks remain
///
/// # Example
///
/// ```rust,no_run
/// use tokio_prompt_orchestrator::coordination::spawner::AgentSpawner;
/// use tokio_prompt_orchestrator::coordination::config::CoordinationConfig;
/// use tokio_prompt_orchestrator::coordination::queue::TaskQueue;
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Arc::new(CoordinationConfig::default());
/// let queue = Arc::new(TaskQueue::from_tasks(config.clone(), vec![]).await?);
/// let spawner = AgentSpawner::new(config);
/// let handles = spawner.spawn_fleet(queue).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Panics
///
/// No methods on this type panic.
pub struct AgentSpawner {
    /// Shared configuration.
    config: Arc<CoordinationConfig>,
}

impl AgentSpawner {
    /// Create a new AgentSpawner.
    ///
    /// # Arguments
    ///
    /// * `config` — Shared coordination configuration
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(config: Arc<CoordinationConfig>) -> Self {
        Self { config }
    }

    /// Spawn the configured number of agents, each running a claim loop.
    ///
    /// # Arguments
    ///
    /// * `queue` — Shared task queue for claiming work
    ///
    /// # Returns
    ///
    /// A vector of `AgentHandle` for each spawned agent.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn spawn_fleet(
        &self,
        queue: Arc<TaskQueue>,
    ) -> Result<Vec<AgentHandle>, CoordinationError> {
        let count = self.config.agent_count;
        let (shutdown_tx, _) = watch::channel(false);
        let mut handles = Vec::with_capacity(count);

        for i in 0..count {
            let agent_id = format!("agent-{i}");
            let worker = AgentWorker::new(agent_id.clone(), Arc::clone(&self.config));
            let queue_clone = Arc::clone(&queue);
            let mut shutdown_rx = shutdown_tx.subscribe();
            let agent_id_clone = agent_id.clone();

            let join_handle = tokio::spawn(async move {
                Self::agent_loop(worker, queue_clone, &mut shutdown_rx, &agent_id_clone).await
            });

            handles.push(AgentHandle {
                agent_id,
                join_handle,
            });
        }

        Ok(handles)
    }

    /// Spawn a specific number of agents (overriding config).
    ///
    /// # Arguments
    ///
    /// * `count` — Number of agents to spawn
    /// * `queue` — Shared task queue for claiming work
    ///
    /// # Returns
    ///
    /// A vector of `AgentHandle` for each spawned agent.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn spawn_fleet_n(
        &self,
        count: usize,
        queue: Arc<TaskQueue>,
    ) -> Result<Vec<AgentHandle>, CoordinationError> {
        let (shutdown_tx, _) = watch::channel(false);
        let mut handles = Vec::with_capacity(count);

        for i in 0..count {
            let agent_id = format!("agent-{i}");
            let worker = AgentWorker::new(agent_id.clone(), Arc::clone(&self.config));
            let queue_clone = Arc::clone(&queue);
            let mut shutdown_rx = shutdown_tx.subscribe();
            let agent_id_clone = agent_id.clone();

            let join_handle = tokio::spawn(async move {
                Self::agent_loop(worker, queue_clone, &mut shutdown_rx, &agent_id_clone).await
            });

            handles.push(AgentHandle {
                agent_id,
                join_handle,
            });
        }

        Ok(handles)
    }

    /// The main loop for a single agent.
    ///
    /// Claims tasks, executes them, and reports results until no tasks remain
    /// or a shutdown signal is received.
    ///
    /// # Arguments
    ///
    /// * `worker` — The agent worker to execute tasks with
    /// * `queue` — The shared task queue
    /// * `shutdown_rx` — Watch receiver for shutdown signals
    /// * `agent_id` — The agent's identifier for logging
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn agent_loop(
        worker: AgentWorker,
        queue: Arc<TaskQueue>,
        shutdown_rx: &mut watch::Receiver<bool>,
        agent_id: &str,
    ) -> Result<AgentStats, CoordinationError> {
        let mut stats = AgentStats {
            agent_id: agent_id.to_string(),
            ..Default::default()
        };

        loop {
            // Check for shutdown
            if *shutdown_rx.borrow() {
                tracing::info!(agent = agent_id, "shutdown signal received");
                break;
            }

            // Try to claim a task
            let task = match queue.claim_next(agent_id).await {
                Ok(Some(task)) => task,
                Ok(None) => {
                    // Check if all tasks are done
                    if queue.all_done().await {
                        tracing::info!(agent = agent_id, "all tasks complete, shutting down");
                        break;
                    }
                    // No pending tasks but some in-flight — wait and retry
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                Err(e) => {
                    tracing::error!(agent = agent_id, error = %e, "failed to claim task");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            tracing::info!(
                agent = agent_id,
                task_id = %task.id,
                priority = task.priority,
                "claimed task"
            );

            stats.tasks_attempted += 1;

            // Execute the task
            match worker.execute(&task.id, &task.prompt).await {
                Ok(result) => {
                    if result.success {
                        tracing::info!(
                            agent = agent_id,
                            task_id = %task.id,
                            duration_secs = result.duration.as_secs(),
                            "task completed successfully"
                        );
                        let _ = queue.complete(&task.id, agent_id).await;
                        stats.tasks_completed += 1;
                    } else {
                        tracing::warn!(
                            agent = agent_id,
                            task_id = %task.id,
                            "task execution returned non-zero exit code"
                        );
                        let _ = queue.fail(&task.id, agent_id, "non-zero exit code").await;
                        stats.tasks_failed += 1;
                    }
                }
                Err(CoordinationError::AgentTimeout { .. }) => {
                    tracing::error!(
                        agent = agent_id,
                        task_id = %task.id,
                        "task timed out"
                    );
                    let _ = queue.fail(&task.id, agent_id, "timeout").await;
                    stats.tasks_failed += 1;
                }
                Err(e) => {
                    tracing::error!(
                        agent = agent_id,
                        task_id = %task.id,
                        error = %e,
                        "task execution failed"
                    );
                    let _ = queue.fail(&task.id, agent_id, &e.to_string()).await;
                    stats.tasks_failed += 1;
                }
            }
        }

        Ok(stats)
    }
}

/// Wait for all agent handles to complete and collect their stats.
///
/// # Arguments
///
/// * `handles` — The list of agent handles from `spawn_fleet`
///
/// # Returns
///
/// A vector of `AgentStats`, one per agent.
///
/// # Panics
///
/// This function never panics.
pub async fn await_fleet(handles: Vec<AgentHandle>) -> Vec<AgentStats> {
    let mut all_stats = Vec::with_capacity(handles.len());

    for handle in handles {
        match handle.join_handle.await {
            Ok(Ok(stats)) => {
                tracing::info!(
                    agent = %stats.agent_id,
                    completed = stats.tasks_completed,
                    failed = stats.tasks_failed,
                    "agent finished"
                );
                all_stats.push(stats);
            }
            Ok(Err(e)) => {
                tracing::error!(
                    agent = %handle.agent_id,
                    error = %e,
                    "agent errored"
                );
                all_stats.push(AgentStats {
                    agent_id: handle.agent_id,
                    ..Default::default()
                });
            }
            Err(e) => {
                tracing::error!(
                    agent = %handle.agent_id,
                    error = %e,
                    "agent task panicked"
                );
                all_stats.push(AgentStats {
                    agent_id: handle.agent_id,
                    ..Default::default()
                });
            }
        }
    }

    all_stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::task::Task;
    use std::path::PathBuf;

    fn test_config() -> Arc<CoordinationConfig> {
        Arc::new(CoordinationConfig {
            agent_count: 2,
            task_file: PathBuf::from("tasks.toml"),
            lock_dir: PathBuf::from(".coordination/locks"),
            claude_bin: PathBuf::from("echo"),
            project_path: PathBuf::from("."),
            timeout_secs: 5,
            stale_lock_secs: 300,
            health_interval_secs: 10,
        })
    }

    #[test]
    fn test_agent_spawner_new() {
        let config = test_config();
        let spawner = AgentSpawner::new(config);
        assert_eq!(spawner.config.agent_count, 2);
    }

    #[test]
    fn test_agent_stats_default() {
        let stats = AgentStats::default();
        assert!(stats.agent_id.is_empty());
        assert_eq!(stats.tasks_completed, 0);
        assert_eq!(stats.tasks_failed, 0);
        assert_eq!(stats.tasks_attempted, 0);
    }

    #[test]
    fn test_agent_stats_debug() {
        let stats = AgentStats {
            agent_id: "agent-0".to_string(),
            tasks_completed: 5,
            tasks_failed: 1,
            tasks_attempted: 6,
        };
        let debug = format!("{:?}", stats);
        assert!(debug.contains("agent-0"));
        assert!(debug.contains("5"));
    }

    #[test]
    fn test_agent_stats_clone() {
        let stats = AgentStats {
            agent_id: "agent-0".to_string(),
            tasks_completed: 3,
            tasks_failed: 0,
            tasks_attempted: 3,
        };
        let cloned = stats.clone();
        assert_eq!(cloned.agent_id, stats.agent_id);
        assert_eq!(cloned.tasks_completed, stats.tasks_completed);
    }

    #[tokio::test]
    async fn test_spawn_fleet_creates_correct_count() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = Arc::new(CoordinationConfig {
            agent_count: 3,
            lock_dir: dir.path().join("locks"),
            claude_bin: PathBuf::from("echo"),
            ..(*test_config()).clone()
        });
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), vec![])
                .await
                .ok()
                .unwrap(),
        );
        let spawner = AgentSpawner::new(config);
        let handles = spawner.spawn_fleet(queue).await;
        assert!(handles.is_ok());
        let handles = handles.ok().unwrap();
        assert_eq!(handles.len(), 3);
        assert_eq!(handles[0].agent_id, "agent-0");
        assert_eq!(handles[1].agent_id, "agent-1");
        assert_eq!(handles[2].agent_id, "agent-2");

        // Wait for handles to finish (they'll exit immediately — no tasks)
        let stats = await_fleet(handles).await;
        assert_eq!(stats.len(), 3);
    }

    #[tokio::test]
    async fn test_spawn_fleet_n_overrides_count() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = Arc::new(CoordinationConfig {
            agent_count: 2,
            lock_dir: dir.path().join("locks"),
            claude_bin: PathBuf::from("echo"),
            ..(*test_config()).clone()
        });
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), vec![])
                .await
                .ok()
                .unwrap(),
        );
        let spawner = AgentSpawner::new(config);
        let handles = spawner.spawn_fleet_n(5, queue).await;
        assert!(handles.is_ok());
        assert_eq!(handles.ok().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn test_agent_loop_with_no_tasks_exits_immediately() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = Arc::new(CoordinationConfig {
            lock_dir: dir.path().join("locks"),
            claude_bin: PathBuf::from("echo"),
            ..(*test_config()).clone()
        });
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), vec![])
                .await
                .ok()
                .unwrap(),
        );
        let worker = AgentWorker::new("test-agent".to_string(), config);
        let (_tx, mut rx) = watch::channel(false);
        let stats = AgentSpawner::agent_loop(worker, queue, &mut rx, "test-agent").await;
        assert!(stats.is_ok());
        let stats = stats.ok().unwrap();
        assert_eq!(stats.tasks_attempted, 0);
    }

    #[tokio::test]
    async fn test_await_fleet_empty_vec() {
        let stats = await_fleet(vec![]).await;
        assert!(stats.is_empty());
    }

    #[tokio::test]
    async fn test_agent_handle_agent_id() {
        let handle = AgentHandle {
            agent_id: "test-handle".to_string(),
            join_handle: tokio::spawn(async { Ok(AgentStats::default()) }),
        };
        assert_eq!(handle.agent_id, "test-handle");
        let _ = handle.join_handle.await;
    }

    #[tokio::test]
    async fn test_fleet_with_echo_tasks_completes() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = Arc::new(CoordinationConfig {
            agent_count: 2,
            lock_dir: dir.path().join("locks"),
            claude_bin: PathBuf::from("echo"),
            timeout_secs: 5,
            ..(*test_config()).clone()
        });
        let tasks = vec![
            Task::new("echo-1", "hello", 1),
            Task::new("echo-2", "world", 2),
        ];
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), tasks)
                .await
                .ok()
                .unwrap(),
        );
        let spawner = AgentSpawner::new(config);
        let handles = spawner.spawn_fleet(queue.clone()).await;
        assert!(handles.is_ok());
        let stats = await_fleet(handles.ok().unwrap()).await;

        // All tasks should have been attempted
        let total_attempted: usize = stats.iter().map(|s| s.tasks_attempted).sum();
        assert_eq!(total_attempted, 2);
    }
}
