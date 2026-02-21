//! # TaskQueue — file-locked concurrent task claiming
//!
//! ## Responsibility
//! Provide atomic task claiming using filesystem lock files.
//! Multiple agent processes can safely claim tasks without races.
//!
//! ## Guarantees
//! - Atomic: task claiming uses `create_new(true)` for exclusivity
//! - Stale-safe: locks older than configured timeout are reclaimable
//! - Crash-safe: dead agent locks are detectable and stealable
//! - Non-blocking: all file I/O runs via `spawn_blocking`
//!
//! ## NOT Responsible For
//! - Process management (see: spawner.rs)
//! - Task definitions (see: task.rs)
//! - Health monitoring (see: monitor.rs)

use crate::coordination::config::CoordinationConfig;
use crate::coordination::task::{Task, TaskFile, TaskStatus};
use crate::coordination::CoordinationError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Lock file contents: agent ID and timestamp.
#[derive(Debug, Clone)]
struct LockInfo {
    /// Agent that holds this lock.
    agent_id: String,
    /// When the lock was created (Unix timestamp seconds).
    timestamp: u64,
}

impl std::fmt::Display for LockInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\n{}", self.agent_id, self.timestamp)
    }
}

impl LockInfo {
    /// Parse from lock file contents.
    ///
    /// # Returns
    ///
    /// - `Some(LockInfo)` if parsing succeeds
    /// - `None` if the format is invalid
    fn from_str(s: &str) -> Option<Self> {
        let mut lines = s.lines();
        let agent_id = lines.next()?.to_string();
        let timestamp: u64 = lines.next()?.parse().ok()?;
        Some(Self {
            agent_id,
            timestamp,
        })
    }
}

/// File-locked task queue for concurrent agent coordination.
///
/// The queue reads tasks from a TOML file and uses per-task lock files
/// in a dedicated directory for atomic claiming. Lock file presence
/// determines task status at runtime:
///
/// - `{lock_dir}/{task_id}.claimed` — task is claimed by an agent
/// - `{lock_dir}/{task_id}.completed` — task finished successfully
/// - `{lock_dir}/{task_id}.failed` — task failed (contents: reason)
///
/// # Thread Safety
///
/// The queue is safe for concurrent access from multiple tasks within
/// a single process. Cross-process safety comes from the filesystem locks.
///
/// # Panics
///
/// No methods on this type panic.
///
/// # Example
///
/// ```rust,no_run
/// use tokio_prompt_orchestrator::coordination::queue::TaskQueue;
/// use tokio_prompt_orchestrator::coordination::config::CoordinationConfig;
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = CoordinationConfig::default();
/// let queue = TaskQueue::new(Arc::new(config)).await?;
/// if let Some(task) = queue.claim_next("agent-1").await? {
///     println!("Claimed task: {}", task.id);
/// }
/// # Ok(())
/// # }
/// ```
pub struct TaskQueue {
    /// Shared configuration.
    config: Arc<CoordinationConfig>,
    /// In-memory task list (loaded from TOML, updated by claim/complete/fail).
    tasks: Mutex<Vec<Task>>,
    /// Lock directory path.
    lock_dir: PathBuf,
}

impl TaskQueue {
    /// Create a new TaskQueue, loading tasks from the configured TOML file.
    ///
    /// Creates the lock directory if it does not exist.
    ///
    /// # Arguments
    ///
    /// * `config` — Shared coordination configuration
    ///
    /// # Returns
    ///
    /// - `Ok(TaskQueue)` on success
    /// - `Err(CoordinationError::TaskFileNotFound)` if the task file doesn't exist
    /// - `Err(CoordinationError::TaskFileParse)` if the TOML is invalid
    /// - `Err(CoordinationError::Io)` on filesystem errors
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn new(config: Arc<CoordinationConfig>) -> Result<Self, CoordinationError> {
        let task_file_path = config.task_file.clone();
        let lock_dir = config.lock_dir.clone();

        // Read task file
        let content = tokio::task::spawn_blocking(move || std::fs::read_to_string(&task_file_path))
            .await
            .map_err(|e| CoordinationError::Io(std::io::Error::other(e)))?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    CoordinationError::TaskFileNotFound {
                        path: config.task_file.clone(),
                    }
                } else {
                    CoordinationError::Io(e)
                }
            })?;

        let task_file = TaskFile::from_toml(&content)?;

        // Create lock directory
        let lock_dir_clone = lock_dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&lock_dir_clone))
            .await
            .map_err(|e| CoordinationError::Io(std::io::Error::other(e)))??;

        // Reconcile task statuses with existing lock files
        let mut tasks = task_file.tasks;
        for task in &mut tasks {
            let status = Self::read_lock_status_sync(&lock_dir, &task.id);
            match status {
                Some((TaskStatus::Claimed, agent)) | Some((TaskStatus::Running, agent)) => {
                    task.status = TaskStatus::Claimed;
                    task.claimed_by = agent;
                }
                Some((TaskStatus::Completed, agent)) => {
                    task.status = TaskStatus::Completed;
                    task.claimed_by = agent;
                }
                Some((TaskStatus::Failed, agent)) => {
                    task.status = TaskStatus::Failed;
                    task.claimed_by = agent;
                }
                _ => {}
            }
        }

        Ok(Self {
            config,
            tasks: Mutex::new(tasks),
            lock_dir,
        })
    }

    /// Create a TaskQueue from an existing task list (for testing).
    ///
    /// # Arguments
    ///
    /// * `config` — Shared coordination configuration
    /// * `tasks` — Pre-loaded task list
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn from_tasks(
        config: Arc<CoordinationConfig>,
        tasks: Vec<Task>,
    ) -> Result<Self, CoordinationError> {
        let lock_dir = config.lock_dir.clone();

        let lock_dir_clone = lock_dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&lock_dir_clone))
            .await
            .map_err(|e| CoordinationError::Io(std::io::Error::other(e)))??;

        Ok(Self {
            config,
            tasks: Mutex::new(tasks),
            lock_dir,
        })
    }

    /// Atomically claim the next available task by priority.
    ///
    /// Writes a lock file `{lock_dir}/{task_id}.claimed` with the agent ID.
    /// If a lock file already exists and is stale (older than `stale_lock_secs`),
    /// the lock is stolen and the task is reclaimed.
    ///
    /// # Arguments
    ///
    /// * `agent_id` — Identifier of the claiming agent
    ///
    /// # Returns
    ///
    /// - `Ok(Some(task))` if a task was successfully claimed
    /// - `Ok(None)` if no tasks are available
    /// - `Err(CoordinationError)` on I/O failure
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn claim_next(&self, agent_id: &str) -> Result<Option<Task>, CoordinationError> {
        let mut tasks = self.tasks.lock().await;

        // Consider both pending and claimed tasks (stale locks can be stolen)
        let mut candidate_indices: Vec<usize> = tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.status.is_terminal())
            .map(|(i, _)| i)
            .collect();

        candidate_indices.sort_by_key(|&i| tasks[i].priority);

        for idx in candidate_indices {
            let task_id = tasks[idx].id.clone();
            let lock_path = self.claimed_path(&task_id);

            match self.try_create_lock(&lock_path, agent_id).await {
                Ok(true) => {
                    // Successfully claimed (no lock file existed)
                    tasks[idx].status = TaskStatus::Claimed;
                    tasks[idx].claimed_by = agent_id.to_string();
                    return Ok(Some(tasks[idx].clone()));
                }
                Ok(false) => {
                    // Lock exists — check if stale
                    if self.is_lock_stale(&lock_path).await? {
                        // Steal the lock from a dead/stale agent
                        self.steal_lock(&lock_path, agent_id).await?;
                        tasks[idx].status = TaskStatus::Claimed;
                        tasks[idx].claimed_by = agent_id.to_string();
                        return Ok(Some(tasks[idx].clone()));
                    }
                    // Not stale — skip this task, try next
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(None)
    }

    /// Mark a task as completed.
    ///
    /// Creates `{lock_dir}/{task_id}.completed` and removes the `.claimed` file.
    ///
    /// # Arguments
    ///
    /// * `task_id` — ID of the completed task
    /// * `agent_id` — ID of the agent that completed the task
    ///
    /// # Returns
    ///
    /// - `Ok(())` on success
    /// - `Err(CoordinationError::TaskNotFound)` if the task doesn't exist
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn complete(&self, task_id: &str, agent_id: &str) -> Result<(), CoordinationError> {
        let mut tasks = self.tasks.lock().await;

        let task = tasks.iter_mut().find(|t| t.id == task_id).ok_or_else(|| {
            CoordinationError::TaskNotFound {
                id: task_id.to_string(),
            }
        })?;

        task.status = TaskStatus::Completed;

        // Write completion marker
        let completed_path = self.completed_path(task_id);
        let content = format!("{agent_id}\n{}", current_timestamp());
        let claimed_path = self.claimed_path(task_id);

        tokio::task::spawn_blocking(move || {
            std::fs::write(&completed_path, content)?;
            // Remove claimed lock (best-effort)
            let _ = std::fs::remove_file(&claimed_path);
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|e| CoordinationError::Io(std::io::Error::other(e)))??;

        Ok(())
    }

    /// Mark a task as failed.
    ///
    /// Creates `{lock_dir}/{task_id}.failed` with the failure reason.
    ///
    /// # Arguments
    ///
    /// * `task_id` — ID of the failed task
    /// * `agent_id` — ID of the agent that reported the failure
    /// * `reason` — Human-readable failure reason
    ///
    /// # Returns
    ///
    /// - `Ok(())` on success
    /// - `Err(CoordinationError::TaskNotFound)` if the task doesn't exist
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn fail(
        &self,
        task_id: &str,
        agent_id: &str,
        reason: &str,
    ) -> Result<(), CoordinationError> {
        let mut tasks = self.tasks.lock().await;

        let task = tasks.iter_mut().find(|t| t.id == task_id).ok_or_else(|| {
            CoordinationError::TaskNotFound {
                id: task_id.to_string(),
            }
        })?;

        task.status = TaskStatus::Failed;
        task.failure_reason = reason.to_string();

        let failed_path = self.failed_path(task_id);
        let content = format!("{agent_id}\n{}\n{reason}", current_timestamp());
        let claimed_path = self.claimed_path(task_id);

        tokio::task::spawn_blocking(move || {
            std::fs::write(&failed_path, content)?;
            let _ = std::fs::remove_file(&claimed_path);
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|e| CoordinationError::Io(std::io::Error::other(e)))??;

        Ok(())
    }

    /// Release a claimed task back to pending status.
    ///
    /// Removes the `.claimed` lock file so other agents can claim it.
    ///
    /// # Arguments
    ///
    /// * `task_id` — ID of the task to release
    ///
    /// # Returns
    ///
    /// - `Ok(())` on success
    /// - `Err(CoordinationError::TaskNotFound)` if the task doesn't exist
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn release(&self, task_id: &str) -> Result<(), CoordinationError> {
        let mut tasks = self.tasks.lock().await;

        let task = tasks.iter_mut().find(|t| t.id == task_id).ok_or_else(|| {
            CoordinationError::TaskNotFound {
                id: task_id.to_string(),
            }
        })?;

        task.status = TaskStatus::Pending;
        task.claimed_by.clear();

        let claimed_path = self.claimed_path(task_id);
        tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_file(&claimed_path);
        })
        .await
        .map_err(|e| CoordinationError::Io(std::io::Error::other(e)))?;

        Ok(())
    }

    /// Get a snapshot of all tasks and their current statuses.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn status(&self) -> Vec<Task> {
        let tasks = self.tasks.lock().await;
        tasks.clone()
    }

    /// Get summary counts: (pending, claimed, completed, failed).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn summary(&self) -> (usize, usize, usize, usize) {
        let tasks = self.tasks.lock().await;
        let pending = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Pending)
            .count();
        let claimed = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Claimed || t.status == TaskStatus::Running)
            .count();
        let completed = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count();
        let failed = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .count();
        (pending, claimed, completed, failed)
    }

    /// Check whether all tasks are in terminal state.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn all_done(&self) -> bool {
        let tasks = self.tasks.lock().await;
        tasks.iter().all(|t| t.status.is_terminal())
    }

    // ── Private helpers ──────────────────────────────────────────────

    /// Path for a claimed lock file.
    fn claimed_path(&self, task_id: &str) -> PathBuf {
        self.lock_dir.join(format!("{task_id}.claimed"))
    }

    /// Path for a completed marker file.
    fn completed_path(&self, task_id: &str) -> PathBuf {
        self.lock_dir.join(format!("{task_id}.completed"))
    }

    /// Path for a failed marker file.
    fn failed_path(&self, task_id: &str) -> PathBuf {
        self.lock_dir.join(format!("{task_id}.failed"))
    }

    /// Try to atomically create a lock file. Returns Ok(true) if created,
    /// Ok(false) if the file already exists.
    async fn try_create_lock(
        &self,
        path: &Path,
        agent_id: &str,
    ) -> Result<bool, CoordinationError> {
        let path = path.to_path_buf();
        let content = LockInfo {
            agent_id: agent_id.to_string(),
            timestamp: current_timestamp(),
        }
        .to_string();

        let result = tokio::task::spawn_blocking(move || {
            use std::fs::OpenOptions;
            use std::io::Write;

            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(content.as_bytes())?;
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e),
            }
        })
        .await
        .map_err(|e| CoordinationError::Io(std::io::Error::other(e)))??;

        Ok(result)
    }

    /// Check if a lock file is stale (older than stale_lock_secs).
    async fn is_lock_stale(&self, path: &Path) -> Result<bool, CoordinationError> {
        let path = path.to_path_buf();
        let stale_secs = self.config.stale_lock_secs;

        let result = tokio::task::spawn_blocking(move || -> Result<bool, std::io::Error> {
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e),
            };

            if let Some(info) = LockInfo::from_str(&content) {
                let now = current_timestamp();
                Ok(now.saturating_sub(info.timestamp) > stale_secs)
            } else {
                // Corrupt lock file — treat as stale
                Ok(true)
            }
        })
        .await
        .map_err(|e| CoordinationError::Io(std::io::Error::other(e)))??;

        Ok(result)
    }

    /// Steal a stale lock by removing and recreating it.
    async fn steal_lock(&self, path: &Path, agent_id: &str) -> Result<(), CoordinationError> {
        let path = path.to_path_buf();
        let content = LockInfo {
            agent_id: agent_id.to_string(),
            timestamp: current_timestamp(),
        }
        .to_string();

        tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_file(&path);
            std::fs::write(&path, content)
        })
        .await
        .map_err(|e| CoordinationError::Io(std::io::Error::other(e)))??;

        Ok(())
    }

    /// Read lock status from the filesystem (synchronous, for init).
    fn read_lock_status_sync(lock_dir: &Path, task_id: &str) -> Option<(TaskStatus, String)> {
        // Check completed first
        let completed_path = lock_dir.join(format!("{task_id}.completed"));
        if let Ok(content) = std::fs::read_to_string(&completed_path) {
            let agent = content.lines().next().unwrap_or_default().to_string();
            return Some((TaskStatus::Completed, agent));
        }

        // Check failed
        let failed_path = lock_dir.join(format!("{task_id}.failed"));
        if let Ok(content) = std::fs::read_to_string(&failed_path) {
            let agent = content.lines().next().unwrap_or_default().to_string();
            return Some((TaskStatus::Failed, agent));
        }

        // Check claimed
        let claimed_path = lock_dir.join(format!("{task_id}.claimed"));
        if let Ok(content) = std::fs::read_to_string(&claimed_path) {
            if let Some(info) = LockInfo::from_str(&content) {
                return Some((TaskStatus::Claimed, info.agent_id));
            }
        }

        None
    }
}

/// Get the current Unix timestamp in seconds.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::task::Task;
    use tempfile::TempDir;

    fn test_config(dir: &TempDir) -> Arc<CoordinationConfig> {
        Arc::new(CoordinationConfig {
            agent_count: 4,
            task_file: dir.path().join("tasks.toml"),
            lock_dir: dir.path().join("locks"),
            claude_bin: PathBuf::from("claude"),
            project_path: PathBuf::from("."),
            timeout_secs: 600,
            stale_lock_secs: 300,
            health_interval_secs: 10,
        })
    }

    fn test_tasks() -> Vec<Task> {
        vec![
            Task::new("task-1", "Do task 1", 1),
            Task::new("task-2", "Do task 2", 2),
            Task::new("task-3", "Do task 3", 3),
        ]
    }

    #[tokio::test]
    async fn test_queue_from_tasks_creates_lock_dir() {
        let dir = TempDir::new().ok().unwrap_or_else(|| {
            TempDir::new()
                .ok()
                .unwrap_or_else(|| panic!("cannot create temp dir"))
        });
        let config = test_config(&dir);
        let lock_dir = config.lock_dir.clone();
        let queue = TaskQueue::from_tasks(config, test_tasks()).await;
        assert!(queue.is_ok());
        assert!(lock_dir.exists());
    }

    #[tokio::test]
    async fn test_queue_claim_next_returns_highest_priority() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let tasks = vec![
            Task::new("low", "low priority", 10),
            Task::new("high", "high priority", 1),
            Task::new("mid", "mid priority", 5),
        ];
        let queue = TaskQueue::from_tasks(config, tasks).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();
        let claimed = queue.claim_next("agent-1").await;
        assert!(claimed.is_ok());
        let task = claimed.ok().unwrap();
        assert!(task.is_some());
        assert_eq!(task.as_ref().map(|t| t.id.as_str()), Some("high"));
    }

    #[tokio::test]
    async fn test_queue_claim_next_returns_none_when_all_claimed() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let tasks = vec![Task::new("only-task", "the only task", 1)];
        let queue = TaskQueue::from_tasks(config, tasks).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        // Claim the only task
        let first = queue.claim_next("agent-1").await;
        assert!(first.is_ok());
        assert!(first.ok().unwrap().is_some());

        // Second claim should return None
        let second = queue.claim_next("agent-2").await;
        assert!(second.is_ok());
        assert!(second.ok().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_queue_claim_next_empty_queue_returns_none() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = TaskQueue::from_tasks(config, vec![]).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();
        let claimed = queue.claim_next("agent-1").await;
        assert!(claimed.is_ok());
        assert!(claimed.ok().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_queue_complete_updates_status() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = TaskQueue::from_tasks(config, test_tasks()).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        let claimed = queue.claim_next("agent-1").await;
        assert!(claimed.is_ok());
        let task = claimed.ok().unwrap();
        assert!(task.is_some());
        let task = task.as_ref().unwrap();

        let result = queue.complete(&task.id, "agent-1").await;
        assert!(result.is_ok());

        let tasks = queue.status().await;
        let completed_task = tasks.iter().find(|t| t.id == task.id);
        assert!(completed_task.is_some());
        assert_eq!(
            completed_task.map(|t| &t.status),
            Some(&TaskStatus::Completed)
        );
    }

    #[tokio::test]
    async fn test_queue_fail_updates_status_and_reason() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = TaskQueue::from_tasks(config, test_tasks()).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        let claimed = queue.claim_next("agent-1").await;
        assert!(claimed.is_ok());
        let task = claimed.ok().unwrap();
        assert!(task.is_some());
        let task = task.as_ref().unwrap();

        let result = queue.fail(&task.id, "agent-1", "compilation error").await;
        assert!(result.is_ok());

        let tasks = queue.status().await;
        let failed_task = tasks.iter().find(|t| t.id == task.id);
        assert!(failed_task.is_some());
        assert_eq!(failed_task.map(|t| &t.status), Some(&TaskStatus::Failed));
    }

    #[tokio::test]
    async fn test_queue_complete_nonexistent_task_returns_error() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = TaskQueue::from_tasks(config, test_tasks()).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        let result = queue.complete("nonexistent", "agent-1").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_queue_fail_nonexistent_task_returns_error() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = TaskQueue::from_tasks(config, test_tasks()).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        let result = queue.fail("nonexistent", "agent-1", "reason").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_queue_release_makes_task_claimable_again() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let tasks = vec![Task::new("releasable", "release me", 1)];
        let queue = TaskQueue::from_tasks(config, tasks).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        // Claim
        let claimed = queue.claim_next("agent-1").await;
        assert!(claimed.is_ok());
        assert!(claimed.ok().unwrap().is_some());

        // Release
        let release_result = queue.release("releasable").await;
        assert!(release_result.is_ok());

        // Re-claim by different agent
        let reclaimed = queue.claim_next("agent-2").await;
        assert!(reclaimed.is_ok());
        let task = reclaimed.ok().unwrap();
        assert!(task.is_some());
        assert_eq!(
            task.as_ref().map(|t| t.claimed_by.as_str()),
            Some("agent-2")
        );
    }

    #[tokio::test]
    async fn test_queue_release_nonexistent_task_returns_error() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = TaskQueue::from_tasks(config, vec![]).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        let result = queue.release("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_queue_summary_counts_correctly() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = TaskQueue::from_tasks(config, test_tasks()).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        let (pending, claimed, completed, failed) = queue.summary().await;
        assert_eq!(pending, 3);
        assert_eq!(claimed, 0);
        assert_eq!(completed, 0);
        assert_eq!(failed, 0);

        // Claim one
        let _ = queue.claim_next("agent-1").await;
        let (pending, claimed, completed, failed) = queue.summary().await;
        assert_eq!(pending, 2);
        assert_eq!(claimed, 1);
        assert_eq!(completed, 0);
        assert_eq!(failed, 0);
    }

    #[tokio::test]
    async fn test_queue_all_done_initially_false() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = TaskQueue::from_tasks(config, test_tasks()).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        assert!(!queue.all_done().await);
    }

    #[tokio::test]
    async fn test_queue_all_done_empty_queue_returns_true() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let queue = TaskQueue::from_tasks(config, vec![]).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        assert!(queue.all_done().await);
    }

    #[tokio::test]
    async fn test_queue_all_done_after_completion() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let tasks = vec![Task::new("single", "one task", 1)];
        let queue = TaskQueue::from_tasks(config, tasks).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();

        let _ = queue.claim_next("agent-1").await;
        let _ = queue.complete("single", "agent-1").await;
        assert!(queue.all_done().await);
    }

    #[tokio::test]
    async fn test_queue_from_toml_file() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let toml_content = r#"
[[tasks]]
id = "task-from-file"
prompt = "Do something"
priority = 1
"#;
        let task_file = dir.path().join("tasks.toml");
        std::fs::write(&task_file, toml_content).ok();

        let config = Arc::new(CoordinationConfig {
            task_file,
            lock_dir: dir.path().join("locks"),
            ..CoordinationConfig::default()
        });

        let queue = TaskQueue::new(config).await;
        assert!(queue.is_ok());
        let queue = queue.ok().unwrap();
        let tasks = queue.status().await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "task-from-file");
    }

    #[tokio::test]
    async fn test_queue_new_missing_file_returns_error() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = Arc::new(CoordinationConfig {
            task_file: dir.path().join("nonexistent.toml"),
            lock_dir: dir.path().join("locks"),
            ..CoordinationConfig::default()
        });
        let result = TaskQueue::new(config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_queue_multiple_agents_no_double_claim() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let tasks = vec![Task::new("t1", "task 1", 1), Task::new("t2", "task 2", 2)];
        let queue = Arc::new(TaskQueue::from_tasks(config, tasks).await.ok().unwrap());

        // Agent 1 claims first
        let t1 = queue.claim_next("agent-1").await;
        assert!(t1.is_ok());
        assert!(t1.ok().unwrap().is_some());

        // Agent 2 claims second
        let t2 = queue.claim_next("agent-2").await;
        assert!(t2.is_ok());
        assert!(t2.ok().unwrap().is_some());

        // Agent 3 gets nothing
        let t3 = queue.claim_next("agent-3").await;
        assert!(t3.is_ok());
        assert!(t3.ok().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_queue_priority_ordering_across_claims() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let tasks = vec![
            Task::new("p3", "priority 3", 3),
            Task::new("p1", "priority 1", 1),
            Task::new("p2", "priority 2", 2),
        ];
        let queue = TaskQueue::from_tasks(config, tasks).await.ok().unwrap();

        let first = queue.claim_next("a1").await.ok().unwrap();
        assert_eq!(first.as_ref().map(|t| t.id.as_str()), Some("p1"));

        let second = queue.claim_next("a2").await.ok().unwrap();
        assert_eq!(second.as_ref().map(|t| t.id.as_str()), Some("p2"));

        let third = queue.claim_next("a3").await.ok().unwrap();
        assert_eq!(third.as_ref().map(|t| t.id.as_str()), Some("p3"));
    }

    #[tokio::test]
    async fn test_queue_claimed_lock_file_exists_on_disk() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let lock_dir = config.lock_dir.clone();
        let tasks = vec![Task::new("disk-check", "check disk", 1)];
        let queue = TaskQueue::from_tasks(config, tasks).await.ok().unwrap();

        let _ = queue.claim_next("agent-1").await;
        let lock_file = lock_dir.join("disk-check.claimed");
        assert!(lock_file.exists());
    }

    #[tokio::test]
    async fn test_queue_completed_marker_exists_on_disk() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let lock_dir = config.lock_dir.clone();
        let tasks = vec![Task::new("complete-check", "check complete", 1)];
        let queue = TaskQueue::from_tasks(config, tasks).await.ok().unwrap();

        let _ = queue.claim_next("agent-1").await;
        let _ = queue.complete("complete-check", "agent-1").await;
        let marker = lock_dir.join("complete-check.completed");
        assert!(marker.exists());
    }

    #[tokio::test]
    async fn test_queue_failed_marker_exists_on_disk() {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|_| tempfile::tempdir().map_err(|e| e).ok().unwrap());
        let config = test_config(&dir);
        let lock_dir = config.lock_dir.clone();
        let tasks = vec![Task::new("fail-check", "check fail", 1)];
        let queue = TaskQueue::from_tasks(config, tasks).await.ok().unwrap();

        let _ = queue.claim_next("agent-1").await;
        let _ = queue.fail("fail-check", "agent-1", "broken").await;
        let marker = lock_dir.join("fail-check.failed");
        assert!(marker.exists());
    }

    #[test]
    fn test_lock_info_roundtrip() {
        let info = LockInfo {
            agent_id: "agent-42".to_string(),
            timestamp: 1700000000,
        };
        let serialized = info.to_string();
        let parsed = LockInfo::from_str(&serialized);
        assert!(parsed.is_some());
        let parsed = parsed.unwrap();
        assert_eq!(parsed.agent_id, "agent-42");
        assert_eq!(parsed.timestamp, 1700000000);
    }

    #[test]
    fn test_lock_info_from_str_invalid_returns_none() {
        assert!(LockInfo::from_str("").is_none());
        assert!(LockInfo::from_str("agent-only").is_none());
        assert!(LockInfo::from_str("agent\nnot-a-number").is_none());
    }

    #[test]
    fn test_current_timestamp_returns_nonzero() {
        let ts = current_timestamp();
        assert!(ts > 0);
    }

    #[test]
    fn test_current_timestamp_is_monotonic() {
        let t1 = current_timestamp();
        let t2 = current_timestamp();
        assert!(t2 >= t1);
    }
}
