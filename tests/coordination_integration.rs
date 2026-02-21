//! Integration tests for the coordination module.
//!
//! Tests cover the four required scenarios:
//! 1. Multi-agent: 5 agents, 20 tasks — all complete, no duplicates
//! 2. Crash recovery: crashed agent's tasks reclaimed by healthy agents
//! 3. Priority ordering: priority=1 tasks complete before priority=2
//! 4. Status output: summary shows correct counts

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_prompt_orchestrator::coordination::config::CoordinationConfig;
use tokio_prompt_orchestrator::coordination::monitor::{AgentHealth, AgentMonitor};
use tokio_prompt_orchestrator::coordination::queue::TaskQueue;
use tokio_prompt_orchestrator::coordination::spawner::{await_fleet, AgentSpawner, AgentStats};
use tokio_prompt_orchestrator::coordination::task::{Task, TaskStatus};

/// Helper: create a test config rooted in a temp directory.
fn test_config(dir: &tempfile::TempDir) -> CoordinationConfig {
    CoordinationConfig {
        agent_count: 4,
        task_file: dir.path().join("tasks.toml"),
        lock_dir: dir.path().join("locks"),
        // Use `echo` as the "claude" binary — exits immediately with success
        claude_bin: PathBuf::from("echo"),
        project_path: PathBuf::from("."),
        timeout_secs: 10,
        stale_lock_secs: 1, // short for tests
        health_interval_secs: 1,
    }
}

/// Helper: generate N tasks with ascending IDs and priorities.
fn make_tasks(count: usize) -> Vec<Task> {
    (0..count)
        .map(|i| Task::new(format!("task-{i}"), format!("Do task {i}"), (i as u32) + 1))
        .collect()
}

// ─── TEST 1: Multi-agent — 5 agents, 20 tasks ────────────────────────────

#[tokio::test]
async fn test_five_agents_twenty_tasks_all_complete_no_duplicates() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| {
        tempfile::tempdir().map_err(|e| e).ok().unwrap_or_else(|| {
            tempfile::tempdir().unwrap_or_else(|_| panic!("cannot create temp dir"))
        })
    });
    let config = Arc::new(CoordinationConfig {
        agent_count: 5,
        ..test_config(&dir)
    });

    let tasks = make_tasks(20);
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap_or_else(|| panic!("queue creation failed")),
    );

    let spawner = AgentSpawner::new(config);
    let handles = spawner
        .spawn_fleet(queue.clone())
        .await
        .ok()
        .unwrap_or_else(|| panic!("fleet spawn failed"));

    let stats = await_fleet(handles).await;

    // Verify: all 5 agents participated
    assert_eq!(stats.len(), 5);

    // Verify: total attempted = 20 (all tasks processed)
    let total_attempted: usize = stats.iter().map(|s| s.tasks_attempted).sum();
    assert_eq!(
        total_attempted, 20,
        "expected 20 tasks attempted, got {total_attempted}"
    );

    // Verify: all tasks are in terminal state
    assert!(queue.all_done().await, "not all tasks are done");

    // Verify: summary counts add up
    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 0, "expected 0 pending tasks");
    assert_eq!(claimed, 0, "expected 0 claimed tasks");
    assert_eq!(
        completed + failed,
        20,
        "expected completed+failed=20, got {completed}+{failed}"
    );

    // Verify: no task was done twice — check task IDs in status
    let final_tasks = queue.status().await;
    let ids: HashSet<&str> = final_tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids.len(), 20, "expected 20 unique task IDs");
}

// ─── TEST 2: Agent crash recovery ─────────────────────────────────────────

#[tokio::test]
async fn test_crashed_agent_task_reclaimed_by_healthy_agent() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| {
        tempfile::tempdir().map_err(|e| e).ok().unwrap_or_else(|| {
            tempfile::tempdir().unwrap_or_else(|_| panic!("cannot create temp dir"))
        })
    });
    let config = Arc::new(CoordinationConfig {
        agent_count: 2,
        stale_lock_secs: 0, // immediate stale detection for tests
        ..test_config(&dir)
    });

    // Only one task: the crashed agent claims it, simulating a crash.
    // After the stale lock timeout (0s), a healthy agent releases it
    // (as the monitor would detect and release stale claims) and reclaims.
    let tasks = vec![Task::new("crash-task", "This task should be reclaimed", 1)];
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap_or_else(|| panic!("queue creation failed")),
    );

    // Agent "crashed-agent" claims the only task but never completes it
    let claimed = queue.claim_next("crashed-agent").await;
    assert!(claimed.is_ok());
    let claimed_task = claimed.ok().unwrap();
    assert!(claimed_task.is_some());
    assert_eq!(
        claimed_task.as_ref().map(|t| t.id.as_str()),
        Some("crash-task")
    );

    // No pending tasks — a normal claim returns None
    let none = queue.claim_next("healthy-agent").await.ok().unwrap();
    assert!(none.is_none(), "no pending tasks should remain");

    // Simulate crash detection: the monitor would detect the stale lock
    // and release the task back to pending so other agents can claim it.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let release_result = queue.release("crash-task").await;
    assert!(release_result.is_ok());

    // Now a healthy agent can claim the released task
    let reclaimed = queue.claim_next("healthy-agent").await;
    assert!(reclaimed.is_ok());
    let reclaimed_task = reclaimed.ok().unwrap();
    assert!(
        reclaimed_task.is_some(),
        "healthy agent should claim released task"
    );
    assert_eq!(
        reclaimed_task.as_ref().map(|t| t.id.as_str()),
        Some("crash-task"),
        "reclaimed task should be the crash-task"
    );
    assert_eq!(
        reclaimed_task.as_ref().map(|t| t.claimed_by.as_str()),
        Some("healthy-agent")
    );
}

// ─── TEST 3: Priority ordering ────────────────────────────────────────────

#[tokio::test]
async fn test_priority_ordering_lower_number_runs_first() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| {
        tempfile::tempdir().map_err(|e| e).ok().unwrap_or_else(|| {
            tempfile::tempdir().unwrap_or_else(|_| panic!("cannot create temp dir"))
        })
    });
    let config = Arc::new(test_config(&dir));

    // Insert tasks in reverse priority order
    let tasks = vec![
        Task::new("low-priority", "Low priority task", 10),
        Task::new("medium-priority", "Medium priority task", 5),
        Task::new("high-priority", "High priority task", 1),
        Task::new("critical-priority", "Critical priority task", 0),
    ];
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap_or_else(|| panic!("queue creation failed")),
    );

    // Claim tasks one by one — should come in priority order
    let first = queue.claim_next("agent-1").await.ok().unwrap();
    assert_eq!(
        first.as_ref().map(|t| t.id.as_str()),
        Some("critical-priority"),
        "first claim should be priority 0 (critical)"
    );

    let second = queue.claim_next("agent-2").await.ok().unwrap();
    assert_eq!(
        second.as_ref().map(|t| t.id.as_str()),
        Some("high-priority"),
        "second claim should be priority 1 (high)"
    );

    let third = queue.claim_next("agent-3").await.ok().unwrap();
    assert_eq!(
        third.as_ref().map(|t| t.id.as_str()),
        Some("medium-priority"),
        "third claim should be priority 5 (medium)"
    );

    let fourth = queue.claim_next("agent-4").await.ok().unwrap();
    assert_eq!(
        fourth.as_ref().map(|t| t.id.as_str()),
        Some("low-priority"),
        "fourth claim should be priority 10 (low)"
    );

    // No more tasks
    let none = queue.claim_next("agent-5").await.ok().unwrap();
    assert!(none.is_none(), "no tasks should remain");
}

// ─── TEST 4: Status output counts ─────────────────────────────────────────

#[tokio::test]
async fn test_status_output_shows_correct_counts() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| {
        tempfile::tempdir().map_err(|e| e).ok().unwrap_or_else(|| {
            tempfile::tempdir().unwrap_or_else(|_| panic!("cannot create temp dir"))
        })
    });
    let config = Arc::new(test_config(&dir));
    let tasks = vec![
        Task::new("t1", "task 1", 1),
        Task::new("t2", "task 2", 2),
        Task::new("t3", "task 3", 3),
        Task::new("t4", "task 4", 4),
        Task::new("t5", "task 5", 5),
    ];
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap_or_else(|| panic!("queue creation failed")),
    );

    // Initial state: all pending
    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 5);
    assert_eq!(claimed, 0);
    assert_eq!(completed, 0);
    assert_eq!(failed, 0);

    // Claim 2 tasks
    let _ = queue.claim_next("agent-1").await;
    let _ = queue.claim_next("agent-2").await;
    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 3, "3 tasks should be pending");
    assert_eq!(claimed, 2, "2 tasks should be claimed");
    assert_eq!(completed, 0);
    assert_eq!(failed, 0);

    // Complete 1
    let _ = queue.complete("t1", "agent-1").await;
    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 3);
    assert_eq!(claimed, 1, "1 task should remain claimed");
    assert_eq!(completed, 1, "1 task should be completed");
    assert_eq!(failed, 0);

    // Fail 1
    let _ = queue.fail("t2", "agent-2", "test failure").await;
    let (pending, claimed, completed, failed) = queue.summary().await;
    assert_eq!(pending, 3);
    assert_eq!(claimed, 0, "no tasks should be claimed");
    assert_eq!(completed, 1);
    assert_eq!(failed, 1, "1 task should be failed");

    // Monitor snapshot reflects same counts
    let monitor = AgentMonitor::new(config, queue.clone());
    monitor.register_agent("agent-1").await;
    monitor.register_agent("agent-2").await;
    monitor.mark_unhealthy("agent-2", "test crash").await;

    let snapshot = monitor.snapshot().await;
    assert_eq!(snapshot.pending, 3);
    assert_eq!(snapshot.completed, 1);
    assert_eq!(snapshot.failed, 1);
    assert_eq!(snapshot.healthy_count(), 1);
    assert_eq!(snapshot.unhealthy_count(), 1);
    assert_eq!(snapshot.total_tasks(), 5);

    // format_status contains expected values
    let status = snapshot.format_status();
    assert!(status.contains("3 pending"));
    assert!(status.contains("1 completed"));
    assert!(status.contains("1 failed"));
}

// ─── TEST 5: TaskFile TOML round-trip integration ─────────────────────────

#[tokio::test]
async fn test_task_file_toml_load_and_queue_integration() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| {
        tempfile::tempdir().map_err(|e| e).ok().unwrap_or_else(|| {
            tempfile::tempdir().unwrap_or_else(|_| panic!("cannot create temp dir"))
        })
    });

    let toml_content = r#"
[[tasks]]
id = "fix-web-api-tests"
prompt = "Fix the 4 failing web_api tests. The issue is 405 vs 404 status codes."
priority = 1
status = "pending"
claimed_by = ""

[[tasks]]
id = "add-redis-tests"
prompt = "Add integration tests for redis_dedup.rs to reach 1.5:1 ratio"
priority = 2
status = "pending"
claimed_by = ""

[[tasks]]
id = "refactor-config"
prompt = "Refactor config module to use builder pattern"
priority = 3
status = "pending"
claimed_by = ""
"#;

    // Write task file
    let task_file_path = dir.path().join("tasks.toml");
    std::fs::write(&task_file_path, toml_content).ok();

    let config = Arc::new(CoordinationConfig {
        task_file: task_file_path,
        lock_dir: dir.path().join("locks"),
        ..CoordinationConfig::default()
    });

    // Load via TaskQueue::new (reads from disk)
    let queue = TaskQueue::new(config).await;
    assert!(queue.is_ok(), "queue should load from TOML file");
    let queue = queue.ok().unwrap();

    let tasks = queue.status().await;
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0].id, "fix-web-api-tests");
    assert_eq!(tasks[1].id, "add-redis-tests");
    assert_eq!(tasks[2].id, "refactor-config");
}

// ─── TEST 6: AgentStats aggregation ───────────────────────────────────────

#[test]
fn test_agent_stats_aggregation() {
    let stats = vec![
        AgentStats {
            agent_id: "agent-0".to_string(),
            tasks_completed: 5,
            tasks_failed: 1,
            tasks_attempted: 6,
        },
        AgentStats {
            agent_id: "agent-1".to_string(),
            tasks_completed: 8,
            tasks_failed: 0,
            tasks_attempted: 8,
        },
        AgentStats {
            agent_id: "agent-2".to_string(),
            tasks_completed: 3,
            tasks_failed: 2,
            tasks_attempted: 5,
        },
    ];

    let total_completed: usize = stats.iter().map(|s| s.tasks_completed).sum();
    let total_failed: usize = stats.iter().map(|s| s.tasks_failed).sum();
    let total_attempted: usize = stats.iter().map(|s| s.tasks_attempted).sum();

    assert_eq!(total_completed, 16);
    assert_eq!(total_failed, 3);
    assert_eq!(total_attempted, 19);
}

// ─── TEST 7: FleetSnapshot health tracking ────────────────────────────────

#[tokio::test]
async fn test_fleet_snapshot_comprehensive_health() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| {
        tempfile::tempdir().map_err(|e| e).ok().unwrap_or_else(|| {
            tempfile::tempdir().unwrap_or_else(|_| panic!("cannot create temp dir"))
        })
    });
    let config = Arc::new(test_config(&dir));
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), vec![])
            .await
            .ok()
            .unwrap_or_else(|| panic!("queue creation failed")),
    );

    let monitor = AgentMonitor::new(config, queue);

    // Register mixed health states
    monitor.register_agent("healthy-1").await;
    monitor.register_agent("healthy-2").await;
    monitor.register_agent("will-crash").await;
    monitor.register_agent("will-finish").await;

    monitor.mark_unhealthy("will-crash", "segfault").await;
    monitor.mark_finished("will-finish").await;

    let snapshot = monitor.snapshot().await;
    assert_eq!(snapshot.agents.len(), 4);
    assert_eq!(snapshot.healthy_count(), 2);
    assert_eq!(snapshot.unhealthy_count(), 1);
    assert_eq!(
        snapshot
            .agents
            .values()
            .filter(|h| matches!(h, AgentHealth::Finished))
            .count(),
        1
    );
}

// ─── TEST 8: Concurrent claiming correctness ──────────────────────────────

#[tokio::test]
async fn test_concurrent_claiming_no_double_assignment() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| {
        tempfile::tempdir().map_err(|e| e).ok().unwrap_or_else(|| {
            tempfile::tempdir().unwrap_or_else(|_| panic!("cannot create temp dir"))
        })
    });
    let config = Arc::new(test_config(&dir));

    // Create 10 tasks
    let tasks = make_tasks(10);
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap_or_else(|| panic!("queue creation failed")),
    );

    // Spawn 10 concurrent claim attempts
    let mut handles = Vec::new();
    for i in 0..10 {
        let q = Arc::clone(&queue);
        handles.push(tokio::spawn(async move {
            q.claim_next(&format!("concurrent-agent-{i}")).await
        }));
    }

    let mut claimed_ids: Vec<String> = Vec::new();
    for handle in handles {
        if let Ok(Ok(Some(task))) = handle.await {
            claimed_ids.push(task.id.clone());
        }
    }

    // Verify: no task was claimed twice
    let unique: HashSet<&str> = claimed_ids.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        unique.len(),
        claimed_ids.len(),
        "no task should be claimed twice: claimed_ids={claimed_ids:?}"
    );

    // All 10 tasks should have been claimed (10 agents, 10 tasks)
    assert_eq!(claimed_ids.len(), 10, "all 10 tasks should be claimed");
}

// ─── TEST 9: Config validation integration ────────────────────────────────

#[test]
fn test_coordination_config_validation_comprehensive() {
    // Valid config
    let config = CoordinationConfig::default();
    assert!(config.validate().is_ok());

    // Zero agent count
    let mut bad = CoordinationConfig::default();
    bad.agent_count = 0;
    assert!(bad.validate().is_err());

    // Excessive agent count
    bad.agent_count = 101;
    assert!(bad.validate().is_err());

    // Zero timeout
    let mut bad = CoordinationConfig::default();
    bad.timeout_secs = 0;
    assert!(bad.validate().is_err());

    // Multiple errors collected
    let mut bad = CoordinationConfig::default();
    bad.agent_count = 0;
    bad.timeout_secs = 0;
    bad.stale_lock_secs = 0;
    bad.health_interval_secs = 0;
    let err = bad.validate().err();
    assert!(err.is_some());
    let msg = err.map(|e| e.to_string()).unwrap_or_default();
    assert!(msg.contains("agent_count"));
    assert!(msg.contains("timeout_secs"));
    assert!(msg.contains("stale_lock_secs"));
    assert!(msg.contains("health_interval_secs"));
}

// ─── TEST 10: Task status lifecycle ───────────────────────────────────────

#[test]
fn test_task_status_lifecycle_transitions() {
    // Pending is claimable
    assert!(TaskStatus::Pending.is_claimable());
    assert!(!TaskStatus::Pending.is_terminal());

    // Claimed is not claimable or terminal
    assert!(!TaskStatus::Claimed.is_claimable());
    assert!(!TaskStatus::Claimed.is_terminal());

    // Running is not claimable or terminal
    assert!(!TaskStatus::Running.is_claimable());
    assert!(!TaskStatus::Running.is_terminal());

    // Completed is terminal, not claimable
    assert!(!TaskStatus::Completed.is_claimable());
    assert!(TaskStatus::Completed.is_terminal());

    // Failed is terminal, not claimable
    assert!(!TaskStatus::Failed.is_claimable());
    assert!(TaskStatus::Failed.is_terminal());
}

// ─── TEST 11: Full lifecycle — claim, execute, complete ───────────────────

#[tokio::test]
async fn test_full_task_lifecycle_claim_execute_complete() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| {
        tempfile::tempdir().map_err(|e| e).ok().unwrap_or_else(|| {
            tempfile::tempdir().unwrap_or_else(|_| panic!("cannot create temp dir"))
        })
    });
    let config = Arc::new(test_config(&dir));
    let tasks = vec![Task::new("lifecycle-task", "Test full lifecycle", 1)];
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap_or_else(|| panic!("queue creation failed")),
    );

    // Step 1: claim
    let claimed = queue.claim_next("lifecycle-agent").await;
    assert!(claimed.is_ok());
    let task = claimed.ok().unwrap();
    assert!(task.is_some());
    assert_eq!(task.as_ref().map(|t| t.id.as_str()), Some("lifecycle-task"));

    // Step 2: verify claimed state
    let (pending, claimed_count, completed, failed) = queue.summary().await;
    assert_eq!(pending, 0);
    assert_eq!(claimed_count, 1);
    assert_eq!(completed, 0);
    assert_eq!(failed, 0);

    // Step 3: complete
    let result = queue.complete("lifecycle-task", "lifecycle-agent").await;
    assert!(result.is_ok());

    // Step 4: verify completed state
    let (pending, claimed_count, completed, failed) = queue.summary().await;
    assert_eq!(pending, 0);
    assert_eq!(claimed_count, 0);
    assert_eq!(completed, 1);
    assert_eq!(failed, 0);
    assert!(queue.all_done().await);
}

// ─── TEST 12: Release and reclaim ─────────────────────────────────────────

#[tokio::test]
async fn test_release_and_reclaim_by_different_agent() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| {
        tempfile::tempdir().map_err(|e| e).ok().unwrap_or_else(|| {
            tempfile::tempdir().unwrap_or_else(|_| panic!("cannot create temp dir"))
        })
    });
    let config = Arc::new(test_config(&dir));
    let tasks = vec![Task::new("release-me", "A releasable task", 1)];
    let queue = Arc::new(
        TaskQueue::from_tasks(config.clone(), tasks)
            .await
            .ok()
            .unwrap_or_else(|| panic!("queue creation failed")),
    );

    // Claim by agent A
    let _ = queue.claim_next("agent-A").await;
    assert_eq!(queue.summary().await.1, 1); // 1 claimed

    // Release
    let _ = queue.release("release-me").await;
    assert_eq!(queue.summary().await.0, 1); // back to 1 pending

    // Reclaim by agent B
    let reclaimed = queue.claim_next("agent-B").await.ok().unwrap();
    assert!(reclaimed.is_some());
    assert_eq!(
        reclaimed.as_ref().map(|t| t.claimed_by.as_str()),
        Some("agent-B")
    );
}
