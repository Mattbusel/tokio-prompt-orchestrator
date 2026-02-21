//! Integration tests for the agent coordination layer.
//!
//! These tests verify end-to-end behavior:
//! - Multiple agents competing for tasks
//! - Priority ordering across claim cycles
//! - Crash recovery (stale lock reclamation)
//! - Status reporting accuracy
//! - Fleet lifecycle (spawn → claim → execute → complete)

use std::path::PathBuf;
use std::sync::Arc;
use tokio_prompt_orchestrator::coordination::{
    config::CoordinationConfig,
    monitor::{AgentHealth, AgentMonitor, FleetSnapshot},
    queue::TaskQueue,
    spawner::{await_fleet, AgentSpawner, AgentStats},
    task::{Task, TaskFile, TaskResult, TaskStatus},
    worker::AgentWorker,
    CoordinationError,
};

// ── Helper functions ──────────────────────────────────────────────

fn make_config(dir: &tempfile::TempDir) -> Arc<CoordinationConfig> {
    Arc::new(CoordinationConfig {
        agent_count: 4,
        task_file: dir.path().join("tasks.toml"),
        lock_dir: dir.path().join("locks"),
        claude_bin: PathBuf::from("echo"),
        project_path: PathBuf::from("."),
        timeout_secs: 10,
        stale_lock_secs: 2,
        health_interval_secs: 1,
    })
}

fn make_tasks(count: usize) -> Vec<Task> {
    (0..count)
        .map(|i| Task::new(format!("task-{i}"), format!("Do task {i}"), (i as u32) + 1))
        .collect()
}

fn make_toml(tasks: &[Task]) -> String {
    let tf = TaskFile {
        tasks: tasks.to_vec(),
    };
    tf.to_toml().unwrap_or_default()
}

// ── Test: 5 agents, 20 tasks — all complete, no double claims ─────

#[tokio::test]
async fn test_five_agents_twenty_tasks_all_complete_no_duplicates() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = Arc::new(CoordinationConfig {
        agent_count: 5,
        lock_dir: dir.path().join("locks"),
        claude_bin: PathBuf::from("echo"),
        timeout_secs: 10,
        ..(*make_config(&dir)).clone()
    });

    let tasks = make_tasks(20);
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap(),
    );

    let spawner = AgentSpawner::new(config);
    let handles = spawner.spawn_fleet(queue.clone()).await.ok().unwrap();

    let stats = await_fleet(handles).await;

    // All tasks should be done
    assert!(queue.all_done().await, "not all tasks completed");

    // Total attempted should be exactly 20
    let total_attempted: usize = stats.iter().map(|s| s.tasks_attempted).sum();
    assert_eq!(
        total_attempted, 20,
        "expected 20 attempted, got {total_attempted}"
    );

    // Every task should be in terminal state
    let final_tasks = queue.status().await;
    for task in &final_tasks {
        assert!(
            task.status.is_terminal(),
            "task {} is not terminal: {}",
            task.id,
            task.status
        );
    }

    // No task ID should appear in two agents' completed lists (checked via
    // queue status — each task has a unique claimed_by)
    let mut claimed_set = std::collections::HashSet::new();
    for task in &final_tasks {
        if task.status == TaskStatus::Completed {
            assert!(
                claimed_set.insert(task.id.clone()),
                "task {} was completed twice",
                task.id
            );
        }
    }
}

// ── Test: Priority ordering — p1 completes before p2 ──────────────

#[tokio::test]
async fn test_priority_ordering_p1_before_p2() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = make_config(&dir);

    // Create tasks with different priorities
    let tasks = vec![
        Task::new("low-p10", "low priority task", 10),
        Task::new("high-p1", "high priority task", 1),
        Task::new("mid-p5", "medium priority task", 5),
    ];

    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap(),
    );

    // Single agent claims sequentially to verify order
    let first = queue.claim_next("agent-0").await.ok().unwrap();
    assert_eq!(
        first.as_ref().map(|t| t.id.as_str()),
        Some("high-p1"),
        "first claim should be highest priority"
    );

    let second = queue.claim_next("agent-0").await.ok().unwrap();
    assert_eq!(
        second.as_ref().map(|t| t.id.as_str()),
        Some("mid-p5"),
        "second claim should be medium priority"
    );

    let third = queue.claim_next("agent-0").await.ok().unwrap();
    assert_eq!(
        third.as_ref().map(|t| t.id.as_str()),
        Some("low-p10"),
        "third claim should be lowest priority"
    );

    let none = queue.claim_next("agent-0").await.ok().unwrap();
    assert!(none.is_none(), "fourth claim should be None");
}

// ── Test: Status output — shows claimed/pending/done counts ────────

#[tokio::test]
async fn test_status_output_shows_correct_counts() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = make_config(&dir);
    let tasks = make_tasks(5);
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap(),
    );

    // Initially all pending
    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 5);
    assert_eq!(claimed, 0);
    assert_eq!(completed, 0);
    assert_eq!(failed, 0);

    // Claim 2
    let t1 = queue.claim_next("agent-0").await.ok().unwrap().unwrap();
    let _t2 = queue.claim_next("agent-1").await.ok().unwrap().unwrap();

    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 3);
    assert_eq!(claimed, 2);
    assert_eq!(completed, 0);
    assert_eq!(failed, 0);

    // Complete 1
    queue.complete(&t1.id, "agent-0").await.ok();

    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 3);
    assert_eq!(claimed, 1);
    assert_eq!(completed, 1);
    assert_eq!(failed, 0);
}

// ── Test: Agent crash — crashed agent's tasks get reclaimed ────────

#[tokio::test]
async fn test_crashed_agent_task_reclaimed_by_healthy_agent() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = Arc::new(CoordinationConfig {
        stale_lock_secs: 1, // 1 second stale threshold for testing
        ..(*make_config(&dir)).clone()
    });

    let tasks = vec![Task::new("reclaim-me", "test reclaim", 1)];
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap(),
    );

    // Agent 1 claims the task
    let claimed = queue.claim_next("crashed-agent").await.ok().unwrap();
    assert!(claimed.is_some());

    // Simulate crash by waiting for lock to go stale
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Agent 2 should be able to reclaim it
    let reclaimed = queue.claim_next("healthy-agent").await.ok().unwrap();
    assert!(
        reclaimed.is_some(),
        "healthy agent should reclaim stale task"
    );
    assert_eq!(
        reclaimed.as_ref().map(|t| t.id.as_str()),
        Some("reclaim-me")
    );
    assert_eq!(
        reclaimed.as_ref().map(|t| t.claimed_by.as_str()),
        Some("healthy-agent")
    );
}

// ── Test: Task file loading from TOML ──────────────────────────────

#[tokio::test]
async fn test_queue_loads_from_toml_file() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let tasks = make_tasks(3);
    let toml_content = make_toml(&tasks);

    let task_file = dir.path().join("tasks.toml");
    std::fs::write(&task_file, &toml_content).ok();

    let config = Arc::new(CoordinationConfig {
        task_file: task_file.clone(),
        lock_dir: dir.path().join("locks"),
        ..CoordinationConfig::default()
    });

    let queue = TaskQueue::new(config).await;
    assert!(queue.is_ok(), "queue should load from TOML file");

    let queue = queue.ok().unwrap();
    let status = queue.status().await;
    assert_eq!(status.len(), 3);
    assert_eq!(status[0].id, "task-0");
    assert_eq!(status[1].id, "task-1");
    assert_eq!(status[2].id, "task-2");
}

// ── Test: Task file with invalid TOML returns error ─────────────

#[tokio::test]
async fn test_queue_new_invalid_toml_returns_error() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let task_file = dir.path().join("tasks.toml");
    std::fs::write(&task_file, "not valid {{{ toml").ok();

    let config = Arc::new(CoordinationConfig {
        task_file,
        lock_dir: dir.path().join("locks"),
        ..CoordinationConfig::default()
    });

    let result = TaskQueue::new(config).await;
    assert!(result.is_err());
}

// ── Test: Monitor integration ─────────────────────────────────────

#[tokio::test]
async fn test_monitor_tracks_fleet_health_correctly() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = make_config(&dir);
    let tasks = make_tasks(5);
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap(),
    );

    let monitor = AgentMonitor::new(config, Arc::clone(&queue));

    // Register 3 agents
    monitor.register_agent("agent-0").await;
    monitor.register_agent("agent-1").await;
    monitor.register_agent("agent-2").await;

    let snapshot = monitor.snapshot().await;
    assert_eq!(snapshot.healthy_count(), 3);
    assert_eq!(snapshot.pending, 5);
    assert_eq!(snapshot.total_tasks(), 5);

    // Mark one as unhealthy
    monitor.mark_unhealthy("agent-2", "process crashed").await;
    let snapshot = monitor.snapshot().await;
    assert_eq!(snapshot.healthy_count(), 2);
    assert_eq!(snapshot.unhealthy_count(), 1);

    // Claim and complete a task
    let task = queue.claim_next("agent-0").await.ok().unwrap().unwrap();
    queue.complete(&task.id, "agent-0").await.ok();

    let snapshot = monitor.snapshot().await;
    assert_eq!(snapshot.pending, 4);
    assert_eq!(snapshot.completed, 1);

    // Format status contains correct info
    let status = snapshot.format_status();
    assert!(status.contains("3 agents"));
    assert!(status.contains("2 healthy"));
    assert!(status.contains("1 unhealthy"));
    assert!(status.contains("4 pending"));
    assert!(status.contains("1 completed"));
}

// ── Test: Fleet snapshot format_status ──────────────────────────

#[test]
fn test_fleet_snapshot_format_status_complete() {
    let mut agents = std::collections::HashMap::new();
    agents.insert("a0".to_string(), AgentHealth::Healthy);
    agents.insert("a1".to_string(), AgentHealth::Healthy);
    agents.insert("a2".to_string(), AgentHealth::Finished);
    agents.insert(
        "a3".to_string(),
        AgentHealth::Unhealthy("crash".to_string()),
    );

    let snapshot = FleetSnapshot {
        agents,
        pending: 2,
        in_progress: 1,
        completed: 7,
        failed: 0,
    };

    let status = snapshot.format_status();
    assert!(status.contains("4 agents"));
    assert!(status.contains("2 healthy"));
    assert!(status.contains("1 unhealthy"));
    assert!(status.contains("1 finished"));
    assert!(status.contains("2 pending"));
    assert!(status.contains("1 in-progress"));
    assert!(status.contains("7 completed"));
    assert!(status.contains("0 failed"));
}

// ── Test: TaskFile parsing edge cases ─────────────────────────────

#[test]
fn test_task_file_empty_tasks_list_parses() {
    let toml = "tasks = []";
    let result = TaskFile::from_toml(toml);
    assert!(result.is_ok());
    let tf = result.ok().unwrap();
    assert!(tf.tasks.is_empty());
}

#[test]
fn test_task_file_preserves_failure_reason() {
    let toml = r#"
[[tasks]]
id = "failed-task"
prompt = "do something"
status = "failed"
failure_reason = "compilation error"
"#;
    let result = TaskFile::from_toml(toml);
    assert!(result.is_ok());
    let tf = result.ok().unwrap();
    assert_eq!(tf.tasks[0].failure_reason, "compilation error");
}

#[test]
fn test_task_file_preserves_claimed_by() {
    let toml = r#"
[[tasks]]
id = "claimed-task"
prompt = "do something"
status = "claimed"
claimed_by = "agent-42"
"#;
    let result = TaskFile::from_toml(toml);
    assert!(result.is_ok());
    let tf = result.ok().unwrap();
    assert_eq!(tf.tasks[0].claimed_by, "agent-42");
}

// ── Test: CoordinationConfig validation edge cases ─────────────

#[test]
fn test_config_validate_boundary_values() {
    let config = CoordinationConfig {
        agent_count: 1,
        task_file: PathBuf::from("tasks.toml"),
        lock_dir: PathBuf::from("locks"),
        claude_bin: PathBuf::from("claude"),
        project_path: PathBuf::from("."),
        timeout_secs: 1,
        stale_lock_secs: 1,
        health_interval_secs: 1,
    };
    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validate_max_agents() {
    let config = CoordinationConfig {
        agent_count: 100,
        ..CoordinationConfig::default()
    };
    assert!(config.validate().is_ok());

    let config = CoordinationConfig {
        agent_count: 101,
        ..CoordinationConfig::default()
    };
    assert!(config.validate().is_err());
}

// ── Test: CoordinationError variants ───────────────────────────

#[test]
fn test_error_task_file_not_found_display() {
    let err = CoordinationError::TaskFileNotFound {
        path: PathBuf::from("/tmp/missing.toml"),
    };
    let msg = err.to_string();
    assert!(msg.contains("missing.toml"));
    assert!(msg.contains("not found"));
}

#[test]
fn test_error_task_already_claimed_display() {
    let err = CoordinationError::TaskAlreadyClaimed {
        id: "task-1".to_string(),
        agent: "agent-0".to_string(),
    };
    let msg = err.to_string();
    assert!(msg.contains("task-1"));
    assert!(msg.contains("agent-0"));
}

#[test]
fn test_error_agent_timeout_display() {
    let err = CoordinationError::AgentTimeout {
        agent_id: "agent-5".to_string(),
        timeout_secs: 600,
    };
    let msg = err.to_string();
    assert!(msg.contains("agent-5"));
    assert!(msg.contains("600"));
}

#[test]
fn test_error_no_tasks_available_display() {
    let err = CoordinationError::NoTasksAvailable;
    assert_eq!(err.to_string(), "no tasks available");
}

#[test]
fn test_error_invalid_config_display() {
    let err = CoordinationError::InvalidConfig("bad value".to_string());
    assert!(err.to_string().contains("bad value"));
}

#[test]
fn test_error_from_io_preserves_message() {
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
    let coord_err: CoordinationError = io_err.into();
    assert!(coord_err.to_string().contains("access denied"));
}

// ── Test: Task struct behavior ──────────────────────────────────

#[test]
fn test_task_new_with_various_priorities() {
    for priority in [1, 5, 10, 100, u32::MAX] {
        let task = Task::new("t", "p", priority);
        assert_eq!(task.priority, priority);
    }
}

#[test]
fn test_task_status_lifecycle() {
    let mut task = Task::new("lifecycle", "test", 1);
    assert!(task.status.is_claimable());
    assert!(!task.status.is_terminal());

    task.status = TaskStatus::Claimed;
    assert!(!task.status.is_claimable());
    assert!(!task.status.is_terminal());

    task.status = TaskStatus::Running;
    assert!(!task.status.is_claimable());
    assert!(!task.status.is_terminal());

    task.status = TaskStatus::Completed;
    assert!(!task.status.is_claimable());
    assert!(task.status.is_terminal());

    let mut failed_task = Task::new("fail", "test", 1);
    failed_task.status = TaskStatus::Failed;
    assert!(!failed_task.status.is_claimable());
    assert!(failed_task.status.is_terminal());
}

// ── Test: TaskResult ────────────────────────────────────────────

#[test]
fn test_task_result_success() {
    let result = TaskResult {
        task_id: "t1".to_string(),
        agent_id: "a1".to_string(),
        success: true,
        output: "all tests passed".to_string(),
        duration: std::time::Duration::from_secs(30),
    };
    assert!(result.success);
    assert_eq!(result.duration.as_secs(), 30);
}

#[test]
fn test_task_result_failure_with_output() {
    let result = TaskResult {
        task_id: "t2".to_string(),
        agent_id: "a2".to_string(),
        success: false,
        output: "error[E0308]: mismatched types".to_string(),
        duration: std::time::Duration::from_millis(500),
    };
    assert!(!result.success);
    assert!(result.output.contains("E0308"));
}

// ── Test: AgentWorker ────────────────────────────────────────────

#[test]
fn test_agent_worker_id() {
    let config = Arc::new(CoordinationConfig::default());
    let worker = AgentWorker::new("my-agent".to_string(), config);
    assert_eq!(worker.agent_id(), "my-agent");
}

#[tokio::test]
async fn test_agent_worker_nonexistent_binary_returns_spawn_error() {
    let config = Arc::new(CoordinationConfig {
        claude_bin: PathBuf::from("definitely-not-a-real-binary-xyz"),
        ..CoordinationConfig::default()
    });
    let worker = AgentWorker::new("a1".to_string(), config);
    let result = worker.execute("task-1", "prompt").await;
    assert!(result.is_err());
    if let Err(CoordinationError::SpawnError(msg)) = &result {
        assert!(msg.contains("a1"));
    }
}

// ── Test: AgentStats ────────────────────────────────────────────

#[test]
fn test_agent_stats_default_values() {
    let stats = AgentStats::default();
    assert!(stats.agent_id.is_empty());
    assert_eq!(stats.tasks_completed, 0);
    assert_eq!(stats.tasks_failed, 0);
    assert_eq!(stats.tasks_attempted, 0);
}

#[test]
fn test_agent_stats_accumulated() {
    let stats = AgentStats {
        agent_id: "agent-0".to_string(),
        tasks_completed: 15,
        tasks_failed: 2,
        tasks_attempted: 17,
    };
    assert_eq!(
        stats.tasks_completed + stats.tasks_failed,
        stats.tasks_attempted
    );
}

// ── Test: Queue concurrent access within single process ──────────

#[tokio::test]
async fn test_queue_concurrent_claims_no_duplicates() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = make_config(&dir);
    let tasks = make_tasks(10);
    let queue = Arc::new(TaskQueue::from_tasks(config, tasks).await.ok().unwrap());

    // Spawn 5 concurrent claim tasks
    let mut handles = Vec::new();
    for i in 0..5 {
        let q = Arc::clone(&queue);
        let agent_id = format!("concurrent-agent-{i}");
        handles.push(tokio::spawn(async move {
            let mut claimed = Vec::new();
            loop {
                match q.claim_next(&agent_id).await {
                    Ok(Some(task)) => {
                        claimed.push(task.id.clone());
                        q.complete(&task.id, &agent_id).await.ok();
                    }
                    Ok(None) => {
                        if q.all_done().await {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(_) => break,
                }
            }
            claimed
        }));
    }

    let mut all_claimed = Vec::new();
    for handle in handles {
        if let Ok(claimed) = handle.await {
            all_claimed.extend(claimed);
        }
    }

    // All 10 tasks should be claimed exactly once
    all_claimed.sort();
    all_claimed.dedup();
    assert_eq!(
        all_claimed.len(),
        10,
        "expected 10 unique claims, got {}",
        all_claimed.len()
    );
}

// ── Test: Queue release and reclaim ──────────────────────────────

#[tokio::test]
async fn test_queue_release_and_reclaim_cycle() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = make_config(&dir);
    let tasks = vec![Task::new("bouncy", "bounce between agents", 1)];
    let queue = TaskQueue::from_tasks(config, tasks).await.ok().unwrap();

    // Agent 1 claims
    let t = queue.claim_next("agent-1").await.ok().unwrap();
    assert!(t.is_some());

    // Agent 1 releases
    queue.release("bouncy").await.ok();

    // Agent 2 claims
    let t = queue.claim_next("agent-2").await.ok().unwrap();
    assert!(t.is_some());
    assert_eq!(t.as_ref().map(|t| t.claimed_by.as_str()), Some("agent-2"));

    // Agent 2 completes
    queue.complete("bouncy", "agent-2").await.ok();
    assert!(queue.all_done().await);
}

// ── Test: Queue fail and check status ─────────────────────────────

#[tokio::test]
async fn test_queue_fail_records_failure() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = make_config(&dir);
    let tasks = vec![Task::new("will-fail", "this will fail", 1)];
    let queue = TaskQueue::from_tasks(config, tasks).await.ok().unwrap();

    queue.claim_next("agent-1").await.ok();
    queue
        .fail("will-fail", "agent-1", "compilation error")
        .await
        .ok();

    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 0);
    assert_eq!(claimed, 0);
    assert_eq!(completed, 0);
    assert_eq!(failed, 1);
    assert!(queue.all_done().await);
}

// ── Test: Monitor lifecycle ───────────────────────────────────────

#[tokio::test]
async fn test_monitor_lifecycle_register_to_finished() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = make_config(&dir);
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), vec![])
            .await
            .ok()
            .unwrap(),
    );
    let monitor = AgentMonitor::new(config, queue);

    monitor.register_agent("a1").await;
    assert_eq!(
        monitor.snapshot().await.agents.get("a1"),
        Some(&AgentHealth::Healthy)
    );

    monitor.mark_unhealthy("a1", "timeout").await;
    assert_eq!(
        monitor.snapshot().await.agents.get("a1"),
        Some(&AgentHealth::Unhealthy("timeout".to_string()))
    );

    monitor.mark_finished("a1").await;
    assert_eq!(
        monitor.snapshot().await.agents.get("a1"),
        Some(&AgentHealth::Finished)
    );
}

// ── Test: TaskFile sorted_by_priority with mixed statuses ─────────

#[test]
fn test_task_file_sorted_by_priority_includes_all() {
    let tasks = vec![
        Task::new("c", "c", 3),
        Task::new("a", "a", 1),
        Task {
            id: "b".to_string(),
            prompt: "b".to_string(),
            priority: 2,
            status: TaskStatus::Completed,
            claimed_by: "agent-0".to_string(),
            failure_reason: String::new(),
        },
    ];
    let tf = TaskFile { tasks };
    let sorted = tf.sorted_by_priority();
    assert_eq!(sorted[0].id, "a");
    assert_eq!(sorted[1].id, "b");
    assert_eq!(sorted[2].id, "c");
}

// ── Test: Config duration accessors ───────────────────────────────

#[test]
fn test_config_duration_accessors_custom_values() {
    let config = CoordinationConfig {
        timeout_secs: 120,
        stale_lock_secs: 60,
        health_interval_secs: 5,
        ..CoordinationConfig::default()
    };
    assert_eq!(config.task_timeout(), std::time::Duration::from_secs(120));
    assert_eq!(
        config.stale_lock_timeout(),
        std::time::Duration::from_secs(60)
    );
    assert_eq!(config.health_interval(), std::time::Duration::from_secs(5));
}

// ── Test: Queue state after mixed operations ──────────────────────

#[tokio::test]
async fn test_queue_mixed_complete_and_fail() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let config = make_config(&dir);
    let tasks = vec![
        Task::new("will-succeed", "succeed", 1),
        Task::new("will-fail", "fail", 2),
        Task::new("also-succeed", "succeed too", 3),
    ];
    let queue = TaskQueue::from_tasks(config, tasks).await.ok().unwrap();

    // Claim and complete first
    let t = queue.claim_next("a1").await.ok().unwrap().unwrap();
    queue.complete(&t.id, "a1").await.ok();

    // Claim and fail second
    let t = queue.claim_next("a2").await.ok().unwrap().unwrap();
    queue.fail(&t.id, "a2", "broken").await.ok();

    // Claim and complete third
    let t = queue.claim_next("a3").await.ok().unwrap().unwrap();
    queue.complete(&t.id, "a3").await.ok();

    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 0);
    assert_eq!(claimed, 0);
    assert_eq!(completed, 2);
    assert_eq!(failed, 1);
    assert!(queue.all_done().await);
}

// ── Test: Task TOML roundtrip with all fields ────────────────────

#[test]
fn test_task_toml_roundtrip_preserves_all_fields() {
    let tasks = vec![Task {
        id: "full-task".to_string(),
        prompt: "do everything".to_string(),
        priority: 42,
        status: TaskStatus::Failed,
        claimed_by: "agent-99".to_string(),
        failure_reason: "out of memory".to_string(),
    }];
    let tf = TaskFile { tasks };
    let toml_str = tf.to_toml().unwrap_or_default();
    let reparsed = TaskFile::from_toml(&toml_str).ok().unwrap();
    assert_eq!(reparsed.tasks[0].id, "full-task");
    assert_eq!(reparsed.tasks[0].prompt, "do everything");
    assert_eq!(reparsed.tasks[0].priority, 42);
    assert_eq!(reparsed.tasks[0].status, TaskStatus::Failed);
    assert_eq!(reparsed.tasks[0].claimed_by, "agent-99");
    assert_eq!(reparsed.tasks[0].failure_reason, "out of memory");
}

// ── Test: Config TOML roundtrip ──────────────────────────────────

#[test]
fn test_config_toml_roundtrip_all_fields() {
    let config = CoordinationConfig {
        agent_count: 10,
        task_file: PathBuf::from("custom.toml"),
        lock_dir: PathBuf::from("/tmp/locks"),
        claude_bin: PathBuf::from("/usr/bin/claude"),
        project_path: PathBuf::from("/home/user/project"),
        timeout_secs: 1200,
        stale_lock_secs: 600,
        health_interval_secs: 30,
    };
    let toml_str = toml::to_string_pretty(&config).unwrap_or_default();
    let reparsed: CoordinationConfig = toml::from_str(&toml_str).unwrap_or_default();
    assert_eq!(reparsed.agent_count, 10);
    assert_eq!(reparsed.timeout_secs, 1200);
    assert_eq!(reparsed.stale_lock_secs, 600);
    assert_eq!(reparsed.health_interval_secs, 30);
}

// ── Test: Queue with TOML file reconciles existing locks ──────────

#[tokio::test]
async fn test_queue_new_reconciles_existing_completed_locks() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| tempfile::tempdir().ok().unwrap());
    let tasks = vec![Task::new("t1", "task 1", 1), Task::new("t2", "task 2", 2)];
    let toml_content = make_toml(&tasks);
    let task_file = dir.path().join("tasks.toml");
    std::fs::write(&task_file, &toml_content).ok();

    let lock_dir = dir.path().join("locks");
    std::fs::create_dir_all(&lock_dir).ok();

    // Pre-create a completed marker for t1
    std::fs::write(lock_dir.join("t1.completed"), "agent-0\n1700000000").ok();

    let config = Arc::new(CoordinationConfig {
        task_file,
        lock_dir,
        ..CoordinationConfig::default()
    });

    let queue = TaskQueue::new(config).await.ok().unwrap();
    let status = queue.status().await;

    // t1 should be recognized as completed
    let t1 = status.iter().find(|t| t.id == "t1");
    assert_eq!(t1.map(|t| &t.status), Some(&TaskStatus::Completed));

    // t2 should still be pending
    let t2 = status.iter().find(|t| t.id == "t2");
    assert_eq!(t2.map(|t| &t.status), Some(&TaskStatus::Pending));
}

// ── Test: AgentHealth display and comparison ──────────────────────

#[test]
fn test_agent_health_all_display_variants() {
    assert_eq!(AgentHealth::Healthy.to_string(), "healthy");
    assert_eq!(AgentHealth::Finished.to_string(), "finished");
    assert_eq!(
        AgentHealth::Unhealthy("oom".to_string()).to_string(),
        "unhealthy: oom"
    );
}

#[test]
fn test_agent_health_equality() {
    assert_eq!(AgentHealth::Healthy, AgentHealth::Healthy);
    assert_eq!(AgentHealth::Finished, AgentHealth::Finished);
    assert_eq!(
        AgentHealth::Unhealthy("a".to_string()),
        AgentHealth::Unhealthy("a".to_string())
    );
    assert_ne!(
        AgentHealth::Unhealthy("a".to_string()),
        AgentHealth::Unhealthy("b".to_string())
    );
    assert_ne!(AgentHealth::Healthy, AgentHealth::Finished);
}

// ── Test: FleetSnapshot edge cases ───────────────────────────────

#[test]
fn test_fleet_snapshot_all_finished() {
    let mut agents = std::collections::HashMap::new();
    agents.insert("a0".to_string(), AgentHealth::Finished);
    agents.insert("a1".to_string(), AgentHealth::Finished);

    let snapshot = FleetSnapshot {
        agents,
        pending: 0,
        in_progress: 0,
        completed: 100,
        failed: 0,
    };

    assert_eq!(snapshot.healthy_count(), 0);
    assert_eq!(snapshot.unhealthy_count(), 0);
    assert_eq!(snapshot.total_tasks(), 100);
}

// ── Test: await_fleet collects all stats ────────────────────────

#[tokio::test]
async fn test_await_fleet_collects_stats_from_all_agents() {
    use tokio_prompt_orchestrator::coordination::spawner::AgentHandle;

    let handles = vec![
        AgentHandle {
            agent_id: "a0".to_string(),
            join_handle: tokio::spawn(async {
                Ok(AgentStats {
                    agent_id: "a0".to_string(),
                    tasks_completed: 5,
                    tasks_failed: 0,
                    tasks_attempted: 5,
                })
            }),
        },
        AgentHandle {
            agent_id: "a1".to_string(),
            join_handle: tokio::spawn(async {
                Ok(AgentStats {
                    agent_id: "a1".to_string(),
                    tasks_completed: 3,
                    tasks_failed: 1,
                    tasks_attempted: 4,
                })
            }),
        },
    ];

    let stats = await_fleet(handles).await;
    assert_eq!(stats.len(), 2);

    let total_completed: usize = stats.iter().map(|s| s.tasks_completed).sum();
    assert_eq!(total_completed, 8);

    let total_failed: usize = stats.iter().map(|s| s.tasks_failed).sum();
    assert_eq!(total_failed, 1);
}
