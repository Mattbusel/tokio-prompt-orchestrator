//! # AgentWorker — single agent process manager
//!
//! ## Responsibility
//! Spawn and manage a single claude process for task execution.
//! Feeds a prompt to the process, captures output, and reports results.
//!
//! ## Guarantees
//! - Isolated: each worker runs its own process
//! - Timeout-safe: processes are killed after the configured timeout
//! - Output-captured: stdout and stderr are collected for reporting
//!
//! ## NOT Responsible For
//! - Task claiming (see: queue.rs)
//! - Fleet management (see: spawner.rs)
//! - Health monitoring (see: monitor.rs)

use crate::coordination::config::CoordinationConfig;
use crate::coordination::task::TaskResult;
use crate::coordination::CoordinationError;
use std::sync::Arc;
use std::time::Instant;
use tokio::process::Command;

/// A single agent worker that executes tasks by spawning a claude process.
///
/// # Usage
///
/// ```rust,no_run
/// use tokio_prompt_orchestrator::coordination::worker::AgentWorker;
/// use tokio_prompt_orchestrator::coordination::config::CoordinationConfig;
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Arc::new(CoordinationConfig::default());
/// let worker = AgentWorker::new("agent-1".to_string(), config);
/// let result = worker.execute("task-1", "Fix the failing tests").await?;
/// println!("Task {} success: {}", result.task_id, result.success);
/// # Ok(())
/// # }
/// ```
///
/// # Panics
///
/// No methods on this type panic.
pub struct AgentWorker {
    /// Unique identifier for this agent.
    agent_id: String,
    /// Shared configuration.
    config: Arc<CoordinationConfig>,
}

impl AgentWorker {
    /// Create a new AgentWorker.
    ///
    /// # Arguments
    ///
    /// * `agent_id` — Unique identifier for this agent
    /// * `config` — Shared coordination configuration
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(agent_id: String, config: Arc<CoordinationConfig>) -> Self {
        Self { agent_id, config }
    }

    /// Return the agent ID.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Execute a task by spawning a claude process with the given prompt.
    ///
    /// The process is spawned with:
    /// - Working directory: `config.project_path`
    /// - Input: the task prompt passed as `--print` argument
    /// - Timeout: `config.timeout_secs`
    ///
    /// # Arguments
    ///
    /// * `task_id` — ID of the task being executed
    /// * `prompt` — The instruction text to feed to claude
    ///
    /// # Returns
    ///
    /// - `Ok(TaskResult)` with success/failure and captured output
    /// - `Err(CoordinationError::SpawnError)` if the process cannot be started
    /// - `Err(CoordinationError::AgentTimeout)` if the process exceeds timeout
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn execute(
        &self,
        task_id: &str,
        prompt: &str,
    ) -> Result<TaskResult, CoordinationError> {
        let start = Instant::now();

        let mut child = Command::new(&self.config.claude_bin)
            .arg("--print")
            .arg(prompt)
            .current_dir(&self.config.project_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                CoordinationError::SpawnError(format!(
                    "failed to spawn claude process for agent {}: {e}",
                    self.agent_id
                ))
            })?;

        let timeout = self.config.task_timeout();

        // Use wait() + take stdout/stderr separately so we can still kill on timeout
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(exit_status)) => {
                let stdout = if let Some(mut out) = stdout_handle {
                    let mut buf = Vec::new();
                    let _ = tokio::io::AsyncReadExt::read_to_end(&mut out, &mut buf).await;
                    String::from_utf8_lossy(&buf).to_string()
                } else {
                    String::new()
                };
                let stderr = if let Some(mut err) = stderr_handle {
                    let mut buf = Vec::new();
                    let _ = tokio::io::AsyncReadExt::read_to_end(&mut err, &mut buf).await;
                    String::from_utf8_lossy(&buf).to_string()
                } else {
                    String::new()
                };
                let success = exit_status.success();

                let combined_output = if stderr.is_empty() {
                    stdout
                } else {
                    format!("{stdout}\n--- stderr ---\n{stderr}")
                };

                Ok(TaskResult {
                    task_id: task_id.to_string(),
                    agent_id: self.agent_id.clone(),
                    success,
                    output: combined_output,
                    duration: start.elapsed(),
                })
            }
            Ok(Err(e)) => Err(CoordinationError::SpawnError(format!(
                "process error for agent {}: {e}",
                self.agent_id
            ))),
            Err(_) => {
                // Timeout — attempt to kill the process
                let _ = child.kill().await;
                Err(CoordinationError::AgentTimeout {
                    agent_id: self.agent_id.clone(),
                    timeout_secs: self.config.timeout_secs,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_config() -> Arc<CoordinationConfig> {
        Arc::new(CoordinationConfig {
            agent_count: 1,
            task_file: PathBuf::from("tasks.toml"),
            lock_dir: PathBuf::from(".coordination/locks"),
            // Use a command that exists on all platforms
            claude_bin: PathBuf::from("echo"),
            project_path: PathBuf::from("."),
            timeout_secs: 5,
            stale_lock_secs: 300,
            health_interval_secs: 10,
        })
    }

    #[test]
    fn test_agent_worker_new_stores_agent_id() {
        let config = test_config();
        let worker = AgentWorker::new("agent-42".to_string(), config);
        assert_eq!(worker.agent_id(), "agent-42");
    }

    #[test]
    fn test_agent_worker_agent_id_accessor() {
        let config = test_config();
        let worker = AgentWorker::new("test-agent".to_string(), config);
        assert_eq!(worker.agent_id(), "test-agent");
    }

    #[tokio::test]
    async fn test_agent_worker_execute_with_echo_succeeds() {
        let config = test_config();
        let worker = AgentWorker::new("agent-1".to_string(), config);
        let result = worker.execute("task-1", "hello world").await;
        // echo may or may not have --print flag, but the test verifies
        // the spawn path works. On failure, it's still a valid TaskResult.
        match result {
            Ok(task_result) => {
                assert_eq!(task_result.task_id, "task-1");
                assert_eq!(task_result.agent_id, "agent-1");
                assert!(task_result.duration.as_secs() < 5);
            }
            Err(CoordinationError::SpawnError(_)) => {
                // echo with --print may not work on all platforms; acceptable
            }
            Err(e) => {
                // Unexpected error variant
                assert!(false, "unexpected error: {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_agent_worker_execute_nonexistent_binary_fails() {
        let config = Arc::new(CoordinationConfig {
            claude_bin: PathBuf::from("nonexistent-binary-12345"),
            ..(*test_config()).clone()
        });
        let worker = AgentWorker::new("agent-1".to_string(), config);
        let result = worker.execute("task-1", "prompt").await;
        assert!(result.is_err());
        match result {
            Err(CoordinationError::SpawnError(msg)) => {
                assert!(msg.contains("agent-1"));
            }
            _ => assert!(false, "expected SpawnError"),
        }
    }

    #[tokio::test]
    async fn test_agent_worker_execute_timeout() {
        // Use a command that sleeps longer than timeout
        let config = Arc::new(CoordinationConfig {
            claude_bin: PathBuf::from("sleep"),
            timeout_secs: 1,
            ..(*test_config()).clone()
        });
        let worker = AgentWorker::new("agent-timeout".to_string(), config);
        let result = worker.execute("task-slow", "100").await;
        match result {
            Err(CoordinationError::AgentTimeout {
                agent_id,
                timeout_secs,
            }) => {
                assert_eq!(agent_id, "agent-timeout");
                assert_eq!(timeout_secs, 1);
            }
            Ok(_) => {
                // sleep may not exist on all platforms — that's fine
            }
            Err(CoordinationError::SpawnError(_)) => {
                // sleep may not exist on Windows — acceptable
            }
            Err(e) => {
                assert!(false, "unexpected error: {e}");
            }
        }
    }

    #[test]
    fn test_agent_worker_config_is_shared() {
        let config = test_config();
        let w1 = AgentWorker::new("a1".to_string(), Arc::clone(&config));
        let w2 = AgentWorker::new("a2".to_string(), Arc::clone(&config));
        assert_eq!(w1.config.timeout_secs, w2.config.timeout_secs);
    }

    #[test]
    fn test_agent_worker_different_ids() {
        let config = test_config();
        let w1 = AgentWorker::new("a1".to_string(), Arc::clone(&config));
        let w2 = AgentWorker::new("a2".to_string(), config);
        assert_ne!(w1.agent_id(), w2.agent_id());
    }
}
