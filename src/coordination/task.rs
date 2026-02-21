//! # Task — coordination work unit
//!
//! ## Responsibility
//! Define the Task struct, TaskStatus, and priority ordering for the
//! agent coordination layer. Tasks are loaded from TOML and claimed
//! atomically by agent workers via filesystem lock files.
//!
//! ## Guarantees
//! - Deterministic: same TOML input always produces the same task set
//! - Serializable: round-trips through serde (TOML ↔ Rust)
//! - Ordered: tasks sort by priority (lower number = higher priority)
//! - Non-panicking: all operations return Result
//!
//! ## NOT Responsible For
//! - Concurrency control (see: queue.rs)
//! - Process management (see: spawner.rs)
//! - Persistence of lock state (see: queue.rs)

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Status of a coordination task in its lifecycle.
///
/// Tasks progress: `Pending` → `Claimed` → `Running` → `Completed` | `Failed`
///
/// # Panics
///
/// No methods on this type panic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task is waiting to be claimed by an agent.
    Pending,
    /// Task has been claimed by an agent but not yet started.
    Claimed,
    /// Task is actively being executed by an agent.
    Running,
    /// Task completed successfully.
    Completed,
    /// Task failed after execution.
    Failed,
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Claimed => write!(f, "claimed"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

impl TaskStatus {
    /// Returns `true` if the task is available for claiming.
    pub fn is_claimable(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// Returns `true` if the task has reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// A coordination task to be claimed and executed by an agent.
///
/// # Fields
///
/// * `id` — Unique task identifier (e.g. `"fix-web-api-tests"`)
/// * `prompt` — The instruction to feed to the agent process
/// * `priority` — Numeric priority (1 = highest). Lower numbers run first.
/// * `status` — Current lifecycle status
/// * `claimed_by` — Agent ID that claimed this task (empty if unclaimed)
/// * `failure_reason` — Reason for failure (empty if not failed)
///
/// # Panics
///
/// No methods on this type panic.
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::coordination::task::{Task, TaskStatus};
/// let task = Task::new("fix-tests", "Fix the failing tests", 1);
/// assert_eq!(task.status, TaskStatus::Pending);
/// assert!(task.claimed_by.is_empty());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique task identifier.
    pub id: String,
    /// The instruction to feed to the agent process.
    pub prompt: String,
    /// Numeric priority (1 = highest, lower numbers = higher priority).
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Current lifecycle status.
    #[serde(default = "default_status")]
    pub status: TaskStatus,
    /// Agent ID that claimed this task (empty if unclaimed).
    #[serde(default)]
    pub claimed_by: String,
    /// Reason for failure (empty if not failed).
    #[serde(default)]
    pub failure_reason: String,
}

impl Task {
    /// Create a new pending task with the given ID, prompt, and priority.
    ///
    /// # Arguments
    ///
    /// * `id` — Unique identifier for this task
    /// * `prompt` — The instruction text for the agent
    /// * `priority` — Numeric priority (1 = highest)
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(id: impl Into<String>, prompt: impl Into<String>, priority: u32) -> Self {
        Self {
            id: id.into(),
            prompt: prompt.into(),
            priority,
            status: TaskStatus::Pending,
            claimed_by: String::new(),
            failure_reason: String::new(),
        }
    }
}

/// Default priority for deserialization (priority 1 = highest).
fn default_priority() -> u32 {
    1
}

/// Default status for deserialization (Pending).
fn default_status() -> TaskStatus {
    TaskStatus::Pending
}

/// A TOML file containing a list of tasks.
///
/// # Format
///
/// ```toml
/// [[tasks]]
/// id = "fix-web-api-tests"
/// prompt = "Fix the 4 failing web_api tests."
/// priority = 1
/// status = "pending"
/// claimed_by = ""
/// ```
///
/// # Panics
///
/// No methods on this type panic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskFile {
    /// The list of tasks in this file.
    pub tasks: Vec<Task>,
}

impl TaskFile {
    /// Parse a task file from a TOML string.
    ///
    /// # Arguments
    ///
    /// * `content` — Raw TOML content
    ///
    /// # Returns
    ///
    /// - `Ok(TaskFile)` on successful parse
    /// - `Err(CoordinationError::TaskFileParse)` on invalid TOML
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn from_toml(content: &str) -> Result<Self, super::CoordinationError> {
        toml::from_str(content)
            .map_err(|e| super::CoordinationError::TaskFileParse(e.to_string()))
    }

    /// Serialize this task file to a TOML string.
    ///
    /// # Returns
    ///
    /// - `Ok(String)` with the TOML representation
    /// - `Err(CoordinationError::TaskFileParse)` if serialization fails
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn to_toml(&self) -> Result<String, super::CoordinationError> {
        toml::to_string_pretty(self)
            .map_err(|e| super::CoordinationError::TaskFileParse(e.to_string()))
    }

    /// Return tasks sorted by priority (lowest number first).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn sorted_by_priority(&self) -> Vec<&Task> {
        let mut tasks: Vec<&Task> = self.tasks.iter().collect();
        tasks.sort_by_key(|t| t.priority);
        tasks
    }

    /// Return only tasks that are still pending.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn pending_tasks(&self) -> Vec<&Task> {
        self.tasks.iter().filter(|t| t.status.is_claimable()).collect()
    }
}

/// Result of a task execution by an agent.
///
/// # Panics
///
/// No methods on this type panic.
#[derive(Debug, Clone)]
pub struct TaskResult {
    /// ID of the task that was executed.
    pub task_id: String,
    /// ID of the agent that executed the task.
    pub agent_id: String,
    /// Whether the task completed successfully.
    pub success: bool,
    /// Output produced by the agent (stdout).
    pub output: String,
    /// How long the task took to execute.
    pub duration: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_status_display_pending() {
        assert_eq!(TaskStatus::Pending.to_string(), "pending");
    }

    #[test]
    fn test_task_status_display_claimed() {
        assert_eq!(TaskStatus::Claimed.to_string(), "claimed");
    }

    #[test]
    fn test_task_status_display_running() {
        assert_eq!(TaskStatus::Running.to_string(), "running");
    }

    #[test]
    fn test_task_status_display_completed() {
        assert_eq!(TaskStatus::Completed.to_string(), "completed");
    }

    #[test]
    fn test_task_status_display_failed() {
        assert_eq!(TaskStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn test_task_status_is_claimable_pending_returns_true() {
        assert!(TaskStatus::Pending.is_claimable());
    }

    #[test]
    fn test_task_status_is_claimable_claimed_returns_false() {
        assert!(!TaskStatus::Claimed.is_claimable());
    }

    #[test]
    fn test_task_status_is_claimable_running_returns_false() {
        assert!(!TaskStatus::Running.is_claimable());
    }

    #[test]
    fn test_task_status_is_claimable_completed_returns_false() {
        assert!(!TaskStatus::Completed.is_claimable());
    }

    #[test]
    fn test_task_status_is_claimable_failed_returns_false() {
        assert!(!TaskStatus::Failed.is_claimable());
    }

    #[test]
    fn test_task_status_is_terminal_completed_returns_true() {
        assert!(TaskStatus::Completed.is_terminal());
    }

    #[test]
    fn test_task_status_is_terminal_failed_returns_true() {
        assert!(TaskStatus::Failed.is_terminal());
    }

    #[test]
    fn test_task_status_is_terminal_pending_returns_false() {
        assert!(!TaskStatus::Pending.is_terminal());
    }

    #[test]
    fn test_task_status_is_terminal_claimed_returns_false() {
        assert!(!TaskStatus::Claimed.is_terminal());
    }

    #[test]
    fn test_task_status_is_terminal_running_returns_false() {
        assert!(!TaskStatus::Running.is_terminal());
    }

    #[test]
    fn test_task_new_creates_pending_task() {
        let task = Task::new("test-id", "test prompt", 1);
        assert_eq!(task.id, "test-id");
        assert_eq!(task.prompt, "test prompt");
        assert_eq!(task.priority, 1);
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.claimed_by.is_empty());
        assert!(task.failure_reason.is_empty());
    }

    #[test]
    fn test_task_new_accepts_string_types() {
        let task = Task::new(String::from("id"), String::from("prompt"), 5);
        assert_eq!(task.id, "id");
        assert_eq!(task.prompt, "prompt");
        assert_eq!(task.priority, 5);
    }

    #[test]
    fn test_task_default_priority_is_1() {
        assert_eq!(default_priority(), 1);
    }

    #[test]
    fn test_task_default_status_is_pending() {
        assert_eq!(default_status(), TaskStatus::Pending);
    }

    #[test]
    fn test_task_file_from_toml_valid_input() {
        let toml = r#"
[[tasks]]
id = "fix-tests"
prompt = "Fix the failing tests"
priority = 1
status = "pending"
claimed_by = ""

[[tasks]]
id = "add-docs"
prompt = "Add documentation"
priority = 2
status = "pending"
claimed_by = ""
"#;
        let result = TaskFile::from_toml(toml);
        assert!(result.is_ok());
        let file = result.ok().unwrap_or_else(|| TaskFile { tasks: vec![] });
        assert_eq!(file.tasks.len(), 2);
        assert_eq!(file.tasks[0].id, "fix-tests");
        assert_eq!(file.tasks[1].id, "add-docs");
    }

    #[test]
    fn test_task_file_from_toml_invalid_input_returns_error() {
        let result = TaskFile::from_toml("not valid toml {{{");
        assert!(result.is_err());
    }

    #[test]
    fn test_task_file_from_toml_missing_required_field_returns_error() {
        let toml = r#"
[[tasks]]
prompt = "no id field"
"#;
        let result = TaskFile::from_toml(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_task_file_from_toml_defaults_applied() {
        let toml = r#"
[[tasks]]
id = "minimal"
prompt = "minimal task"
"#;
        let result = TaskFile::from_toml(toml);
        assert!(result.is_ok());
        let file = result.ok().unwrap_or_else(|| TaskFile { tasks: vec![] });
        assert_eq!(file.tasks[0].priority, 1);
        assert_eq!(file.tasks[0].status, TaskStatus::Pending);
        assert!(file.tasks[0].claimed_by.is_empty());
    }

    #[test]
    fn test_task_file_to_toml_roundtrip() {
        let file = TaskFile {
            tasks: vec![
                Task::new("t1", "prompt1", 1),
                Task::new("t2", "prompt2", 2),
            ],
        };
        let toml_str = file.to_toml();
        assert!(toml_str.is_ok());
        let reparsed = TaskFile::from_toml(
            &toml_str.unwrap_or_default(),
        );
        assert!(reparsed.is_ok());
        let reparsed_file = reparsed.ok().unwrap_or_else(|| TaskFile { tasks: vec![] });
        assert_eq!(reparsed_file.tasks.len(), 2);
        assert_eq!(reparsed_file.tasks[0].id, "t1");
        assert_eq!(reparsed_file.tasks[1].id, "t2");
    }

    #[test]
    fn test_task_file_sorted_by_priority_orders_correctly() {
        let file = TaskFile {
            tasks: vec![
                Task::new("low", "low priority", 10),
                Task::new("high", "high priority", 1),
                Task::new("mid", "mid priority", 5),
            ],
        };
        let sorted = file.sorted_by_priority();
        assert_eq!(sorted[0].id, "high");
        assert_eq!(sorted[1].id, "mid");
        assert_eq!(sorted[2].id, "low");
    }

    #[test]
    fn test_task_file_pending_tasks_filters_correctly() {
        let file = TaskFile {
            tasks: vec![
                Task::new("pending1", "p1", 1),
                Task {
                    id: "completed1".to_string(),
                    prompt: "c1".to_string(),
                    priority: 1,
                    status: TaskStatus::Completed,
                    claimed_by: "agent-1".to_string(),
                    failure_reason: String::new(),
                },
                Task::new("pending2", "p2", 2),
            ],
        };
        let pending = file.pending_tasks();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, "pending1");
        assert_eq!(pending[1].id, "pending2");
    }

    #[test]
    fn test_task_file_empty_tasks_list() {
        let file = TaskFile { tasks: vec![] };
        assert!(file.sorted_by_priority().is_empty());
        assert!(file.pending_tasks().is_empty());
    }

    #[test]
    fn test_task_status_serde_roundtrip_all_variants() {
        let statuses = vec![
            TaskStatus::Pending,
            TaskStatus::Claimed,
            TaskStatus::Running,
            TaskStatus::Completed,
            TaskStatus::Failed,
        ];
        for status in statuses {
            let json = serde_json::to_string(&status);
            assert!(json.is_ok());
            let deserialized: Result<TaskStatus, _> =
                serde_json::from_str(&json.unwrap_or_default());
            assert!(deserialized.is_ok());
            assert_eq!(deserialized.ok(), Some(status));
        }
    }

    #[test]
    fn test_task_result_captures_all_fields() {
        let result = TaskResult {
            task_id: "t1".to_string(),
            agent_id: "agent-1".to_string(),
            success: true,
            output: "done".to_string(),
            duration: Duration::from_secs(5),
        };
        assert_eq!(result.task_id, "t1");
        assert_eq!(result.agent_id, "agent-1");
        assert!(result.success);
        assert_eq!(result.output, "done");
        assert_eq!(result.duration, Duration::from_secs(5));
    }

    #[test]
    fn test_task_result_failure_case() {
        let result = TaskResult {
            task_id: "t2".to_string(),
            agent_id: "agent-2".to_string(),
            success: false,
            output: "error: compilation failed".to_string(),
            duration: Duration::from_millis(100),
        };
        assert!(!result.success);
        assert!(result.output.contains("compilation failed"));
    }

    #[test]
    fn test_task_file_from_toml_all_statuses() {
        let toml = r#"
[[tasks]]
id = "t1"
prompt = "p1"
status = "pending"

[[tasks]]
id = "t2"
prompt = "p2"
status = "claimed"

[[tasks]]
id = "t3"
prompt = "p3"
status = "running"

[[tasks]]
id = "t4"
prompt = "p4"
status = "completed"

[[tasks]]
id = "t5"
prompt = "p5"
status = "failed"
"#;
        let file = TaskFile::from_toml(toml);
        assert!(file.is_ok());
        let f = file.ok().unwrap_or_else(|| TaskFile { tasks: vec![] });
        assert_eq!(f.tasks[0].status, TaskStatus::Pending);
        assert_eq!(f.tasks[1].status, TaskStatus::Claimed);
        assert_eq!(f.tasks[2].status, TaskStatus::Running);
        assert_eq!(f.tasks[3].status, TaskStatus::Completed);
        assert_eq!(f.tasks[4].status, TaskStatus::Failed);
    }

    #[test]
    fn test_task_clone_independence() {
        let original = Task::new("orig", "original prompt", 1);
        let mut cloned = original.clone();
        cloned.status = TaskStatus::Completed;
        cloned.claimed_by = "agent-1".to_string();
        assert_eq!(original.status, TaskStatus::Pending);
        assert!(original.claimed_by.is_empty());
    }

    #[test]
    fn test_task_file_sorted_by_priority_same_priority_stable() {
        let file = TaskFile {
            tasks: vec![
                Task::new("a", "first", 1),
                Task::new("b", "second", 1),
                Task::new("c", "third", 1),
            ],
        };
        let sorted = file.sorted_by_priority();
        // Stable sort preserves insertion order for equal priorities
        assert_eq!(sorted[0].id, "a");
        assert_eq!(sorted[1].id, "b");
        assert_eq!(sorted[2].id, "c");
    }
}
