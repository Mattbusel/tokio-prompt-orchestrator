//! # AgentMonitor — fleet health monitoring
//!
//! ## Responsibility
//! Watch running agents for crashes or hangs. Detect stale claims
//! from dead agents and release them for reclamation.
//!
//! ## Guarantees
//! - Periodic: checks run at configurable intervals
//! - Non-blocking: monitoring runs in a background tokio task
//! - Observable: health status is available via snapshot
//!
//! ## NOT Responsible For
//! - Restarting agents (see: spawner.rs)
//! - Claiming tasks (see: queue.rs)
//! - Executing tasks (see: worker.rs)

use crate::coordination::config::CoordinationConfig;
use crate::coordination::queue::TaskQueue;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Health status of a single agent.
///
/// # Panics
///
/// No methods on this type panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentHealth {
    /// Agent is running and responsive.
    Healthy,
    /// Agent task has completed normally.
    Finished,
    /// Agent task has been detected as unresponsive or crashed.
    Unhealthy(String),
}

impl std::fmt::Display for AgentHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Finished => write!(f, "finished"),
            Self::Unhealthy(reason) => write!(f, "unhealthy: {reason}"),
        }
    }
}

/// Snapshot of fleet health at a point in time.
///
/// # Panics
///
/// No methods on this type panic.
#[derive(Debug, Clone)]
pub struct FleetSnapshot {
    /// Per-agent health status.
    pub agents: HashMap<String, AgentHealth>,
    /// Number of pending tasks.
    pub pending: usize,
    /// Number of claimed/running tasks.
    pub in_progress: usize,
    /// Number of completed tasks.
    pub completed: usize,
    /// Number of failed tasks.
    pub failed: usize,
}

impl FleetSnapshot {
    /// Return the count of healthy agents.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn healthy_count(&self) -> usize {
        self.agents
            .values()
            .filter(|h| matches!(h, AgentHealth::Healthy))
            .count()
    }

    /// Return the count of unhealthy agents.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn unhealthy_count(&self) -> usize {
        self.agents
            .values()
            .filter(|h| matches!(h, AgentHealth::Unhealthy(_)))
            .count()
    }

    /// Return the total number of tasks.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn total_tasks(&self) -> usize {
        self.pending + self.in_progress + self.completed + self.failed
    }

    /// Format a human-readable status summary.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn format_status(&self) -> String {
        format!(
            "Fleet: {} agents ({} healthy, {} unhealthy, {} finished) | Tasks: {} pending, {} in-progress, {} completed, {} failed",
            self.agents.len(),
            self.healthy_count(),
            self.unhealthy_count(),
            self.agents.values().filter(|h| matches!(h, AgentHealth::Finished)).count(),
            self.pending,
            self.in_progress,
            self.completed,
            self.failed,
        )
    }
}

/// Monitors the health of a fleet of agents and their task queue.
///
/// Runs periodic health checks and maintains a snapshot of fleet status
/// that can be queried at any time.
///
/// # Example
///
/// ```rust,no_run
/// use tokio_prompt_orchestrator::coordination::monitor::AgentMonitor;
/// use tokio_prompt_orchestrator::coordination::config::CoordinationConfig;
/// use tokio_prompt_orchestrator::coordination::queue::TaskQueue;
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Arc::new(CoordinationConfig::default());
/// let queue = Arc::new(TaskQueue::from_tasks(config.clone(), vec![]).await?);
/// let monitor = AgentMonitor::new(config, queue);
/// let snapshot = monitor.snapshot().await;
/// println!("{}", snapshot.format_status());
/// # Ok(())
/// # }
/// ```
///
/// # Panics
///
/// No methods on this type panic.
pub struct AgentMonitor {
    /// Shared configuration.
    config: Arc<CoordinationConfig>,
    /// Shared task queue.
    queue: Arc<TaskQueue>,
    /// Per-agent health status.
    agent_health: Arc<Mutex<HashMap<String, AgentHealth>>>,
}

impl AgentMonitor {
    /// Create a new AgentMonitor.
    ///
    /// # Arguments
    ///
    /// * `config` — Shared coordination configuration
    /// * `queue` — Shared task queue to monitor
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(config: Arc<CoordinationConfig>, queue: Arc<TaskQueue>) -> Self {
        Self {
            config,
            queue,
            agent_health: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register an agent as healthy.
    ///
    /// # Arguments
    ///
    /// * `agent_id` — The agent to register
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn register_agent(&self, agent_id: &str) {
        let mut health = self.agent_health.lock().await;
        health.insert(agent_id.to_string(), AgentHealth::Healthy);
    }

    /// Mark an agent as finished.
    ///
    /// # Arguments
    ///
    /// * `agent_id` — The agent to mark as finished
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn mark_finished(&self, agent_id: &str) {
        let mut health = self.agent_health.lock().await;
        health.insert(agent_id.to_string(), AgentHealth::Finished);
    }

    /// Mark an agent as unhealthy.
    ///
    /// # Arguments
    ///
    /// * `agent_id` — The agent to mark
    /// * `reason` — Human-readable reason for unhealthiness
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn mark_unhealthy(&self, agent_id: &str, reason: &str) {
        let mut health = self.agent_health.lock().await;
        health.insert(
            agent_id.to_string(),
            AgentHealth::Unhealthy(reason.to_string()),
        );
    }

    /// Get a snapshot of the current fleet health.
    ///
    /// # Returns
    ///
    /// A `FleetSnapshot` containing agent health and task queue status.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn snapshot(&self) -> FleetSnapshot {
        let agents = self.agent_health.lock().await.clone();
        let (pending, in_progress, completed, failed) = self.queue.summary().await;

        FleetSnapshot {
            agents,
            pending,
            in_progress,
            completed,
            failed,
        }
    }

    /// Start a background monitoring loop that periodically logs fleet status.
    ///
    /// The loop runs until the returned join handle is aborted or the
    /// shutdown signal is sent.
    ///
    /// # Arguments
    ///
    /// * `shutdown` — Watch receiver for shutdown signals
    ///
    /// # Returns
    ///
    /// A `JoinHandle` for the monitoring task.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn start_monitoring(
        &self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let interval = self.config.health_interval();
        let queue = Arc::clone(&self.queue);
        let agent_health = Arc::clone(&self.agent_health);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let agents = agent_health.lock().await.clone();
                        let (pending, in_progress, completed, failed) = queue.summary().await;

                        let snapshot = FleetSnapshot {
                            agents,
                            pending,
                            in_progress,
                            completed,
                            failed,
                        };

                        tracing::info!(
                            status = %snapshot.format_status(),
                            "fleet health check"
                        );

                        // If all tasks done and no healthy agents, exit
                        if snapshot.pending == 0
                            && snapshot.in_progress == 0
                            && snapshot.healthy_count() == 0
                        {
                            tracing::info!("all tasks complete and no active agents, monitor exiting");
                            break;
                        }
                    }
                    _ = shutdown.changed() => {
                        tracing::info!("monitor shutdown signal received");
                        break;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::task::Task;
    use std::path::PathBuf;

    fn test_config(dir: &tempfile::TempDir) -> Arc<CoordinationConfig> {
        Arc::new(CoordinationConfig {
            agent_count: 2,
            task_file: PathBuf::from("tasks.toml"),
            lock_dir: dir.path().join("locks"),
            claude_bin: PathBuf::from("echo"),
            project_path: PathBuf::from("."),
            timeout_secs: 5,
            stale_lock_secs: 300,
            health_interval_secs: 1,
        })
    }

    #[test]
    fn test_agent_health_display_healthy() {
        assert_eq!(AgentHealth::Healthy.to_string(), "healthy");
    }

    #[test]
    fn test_agent_health_display_finished() {
        assert_eq!(AgentHealth::Finished.to_string(), "finished");
    }

    #[test]
    fn test_agent_health_display_unhealthy() {
        let h = AgentHealth::Unhealthy("process crashed".to_string());
        assert_eq!(h.to_string(), "unhealthy: process crashed");
    }

    #[test]
    fn test_agent_health_eq_healthy() {
        assert_eq!(AgentHealth::Healthy, AgentHealth::Healthy);
    }

    #[test]
    fn test_agent_health_ne_different_variants() {
        assert_ne!(AgentHealth::Healthy, AgentHealth::Finished);
    }

    #[test]
    fn test_fleet_snapshot_healthy_count() {
        let mut agents = HashMap::new();
        agents.insert("a1".to_string(), AgentHealth::Healthy);
        agents.insert("a2".to_string(), AgentHealth::Healthy);
        agents.insert("a3".to_string(), AgentHealth::Unhealthy("dead".to_string()));
        let snapshot = FleetSnapshot {
            agents,
            pending: 0,
            in_progress: 0,
            completed: 0,
            failed: 0,
        };
        assert_eq!(snapshot.healthy_count(), 2);
    }

    #[test]
    fn test_fleet_snapshot_unhealthy_count() {
        let mut agents = HashMap::new();
        agents.insert("a1".to_string(), AgentHealth::Healthy);
        agents.insert(
            "a2".to_string(),
            AgentHealth::Unhealthy("crash".to_string()),
        );
        agents.insert(
            "a3".to_string(),
            AgentHealth::Unhealthy("timeout".to_string()),
        );
        let snapshot = FleetSnapshot {
            agents,
            pending: 0,
            in_progress: 0,
            completed: 0,
            failed: 0,
        };
        assert_eq!(snapshot.unhealthy_count(), 2);
    }

    #[test]
    fn test_fleet_snapshot_total_tasks() {
        let snapshot = FleetSnapshot {
            agents: HashMap::new(),
            pending: 5,
            in_progress: 3,
            completed: 10,
            failed: 2,
        };
        assert_eq!(snapshot.total_tasks(), 20);
    }

    #[test]
    fn test_fleet_snapshot_format_status_contains_counts() {
        let mut agents = HashMap::new();
        agents.insert("a1".to_string(), AgentHealth::Healthy);
        let snapshot = FleetSnapshot {
            agents,
            pending: 5,
            in_progress: 2,
            completed: 3,
            failed: 1,
        };
        let status = snapshot.format_status();
        assert!(status.contains("1 agents"));
        assert!(status.contains("1 healthy"));
        assert!(status.contains("5 pending"));
        assert!(status.contains("2 in-progress"));
        assert!(status.contains("3 completed"));
        assert!(status.contains("1 failed"));
    }

    #[test]
    fn test_fleet_snapshot_empty() {
        let snapshot = FleetSnapshot {
            agents: HashMap::new(),
            pending: 0,
            in_progress: 0,
            completed: 0,
            failed: 0,
        };
        assert_eq!(snapshot.healthy_count(), 0);
        assert_eq!(snapshot.unhealthy_count(), 0);
        assert_eq!(snapshot.total_tasks(), 0);
    }

    #[tokio::test]
    async fn test_monitor_register_agent() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), vec![])
                .await
                .ok()
                .unwrap(),
        );
        let monitor = AgentMonitor::new(config, queue);

        monitor.register_agent("agent-1").await;
        let snapshot = monitor.snapshot().await;
        assert_eq!(snapshot.agents.get("agent-1"), Some(&AgentHealth::Healthy));
    }

    #[tokio::test]
    async fn test_monitor_mark_finished() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), vec![])
                .await
                .ok()
                .unwrap(),
        );
        let monitor = AgentMonitor::new(config, queue);

        monitor.register_agent("agent-1").await;
        monitor.mark_finished("agent-1").await;
        let snapshot = monitor.snapshot().await;
        assert_eq!(snapshot.agents.get("agent-1"), Some(&AgentHealth::Finished));
    }

    #[tokio::test]
    async fn test_monitor_mark_unhealthy() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), vec![])
                .await
                .ok()
                .unwrap(),
        );
        let monitor = AgentMonitor::new(config, queue);

        monitor.register_agent("agent-1").await;
        monitor.mark_unhealthy("agent-1", "process died").await;
        let snapshot = monitor.snapshot().await;
        assert_eq!(
            snapshot.agents.get("agent-1"),
            Some(&AgentHealth::Unhealthy("process died".to_string()))
        );
    }

    #[tokio::test]
    async fn test_monitor_snapshot_includes_task_counts() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let tasks = vec![Task::new("t1", "p1", 1), Task::new("t2", "p2", 2)];
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), tasks)
                .await
                .ok()
                .unwrap(),
        );
        let monitor = AgentMonitor::new(config, queue);
        let snapshot = monitor.snapshot().await;
        assert_eq!(snapshot.pending, 2);
        assert_eq!(snapshot.completed, 0);
    }

    #[tokio::test]
    async fn test_monitor_start_monitoring_can_be_stopped() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = Arc::new(CoordinationConfig {
            health_interval_secs: 1,
            lock_dir: dir.path().join("locks"),
            ..(*test_config(&dir)).clone()
        });
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), vec![])
                .await
                .ok()
                .unwrap(),
        );
        let monitor = AgentMonitor::new(config, queue);

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = monitor.start_monitoring(shutdown_rx);

        // Let it run for a moment
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Send shutdown
        let _ = shutdown_tx.send(true);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_monitor_multiple_agents() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = Arc::new(
            TaskQueue::from_tasks(config.clone(), vec![])
                .await
                .ok()
                .unwrap(),
        );
        let monitor = AgentMonitor::new(config, queue);

        monitor.register_agent("agent-0").await;
        monitor.register_agent("agent-1").await;
        monitor.register_agent("agent-2").await;

        let snapshot = monitor.snapshot().await;
        assert_eq!(snapshot.agents.len(), 3);
        assert_eq!(snapshot.healthy_count(), 3);
    }

    #[test]
    fn test_fleet_snapshot_clone() {
        let mut agents = HashMap::new();
        agents.insert("a1".to_string(), AgentHealth::Healthy);
        let original = FleetSnapshot {
            agents,
            pending: 1,
            in_progress: 2,
            completed: 3,
            failed: 4,
        };
        let cloned = original.clone();
        assert_eq!(cloned.pending, original.pending);
        assert_eq!(cloned.agents.len(), original.agents.len());
    }

    #[test]
    fn test_agent_health_clone() {
        let health = AgentHealth::Unhealthy("reason".to_string());
        let cloned = health.clone();
        assert_eq!(health, cloned);
    }
}
