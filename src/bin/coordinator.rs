//! # Coordinator CLI — agent fleet launcher
//!
//! Spawns a fleet of agent processes that work through a task queue
//! defined in a TOML file.
//!
//! ## Usage
//!
//! ```bash
//! # Spawn 10 agents working through tasks.toml
//! cargo run --bin coordinator --features self-improving -- --agents 10 --tasks tasks.toml
//!
//! # Also start the self-improvement background loop
//! cargo run --bin coordinator --features self-improving -- \
//!   --agents 10 --tasks tasks.toml --self-improve
//!
//! # Show current status of task queue
//! cargo run --bin coordinator -- --status --tasks tasks.toml
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use tokio_prompt_orchestrator::coordination::{
    config::CoordinationConfig,
    monitor::AgentMonitor,
    queue::TaskQueue,
    spawner::{await_fleet, AgentSpawner},
};

/// Parsed CLI arguments.
struct Args {
    /// Number of agents to spawn.
    agent_count: usize,
    /// Path to the tasks TOML file.
    task_file: PathBuf,
    /// Show status only (don't spawn agents).
    status_only: bool,
    /// Task timeout in seconds.
    timeout_secs: u64,
    /// Enable the self-improvement background loop (requires `self-improving` feature).
    self_improve: bool,
}

/// Parse command-line arguments manually (no external arg parser dependency).
///
/// # Returns
///
/// - `Ok(Args)` on success
/// - `Err(String)` with a usage message on failure
fn parse_args() -> Result<Args, String> {
    let args: Vec<String> = std::env::args().collect();
    let mut agent_count: usize = 4;
    let mut task_file = PathBuf::from("tasks.toml");
    let mut status_only = false;
    let mut timeout_secs: u64 = 600;
    let mut self_improve = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--agents" | "-a" => {
                i += 1;
                if i >= args.len() {
                    return Err("--agents requires a value".to_string());
                }
                agent_count = args[i]
                    .parse()
                    .map_err(|_| format!("invalid agent count: {}", args[i]))?;
            }
            "--tasks" | "-t" => {
                i += 1;
                if i >= args.len() {
                    return Err("--tasks requires a value".to_string());
                }
                task_file = PathBuf::from(&args[i]);
            }
            "--status" | "-s" => {
                status_only = true;
            }
            "--self-improve" => {
                self_improve = true;
            }
            "--timeout" => {
                i += 1;
                if i >= args.len() {
                    return Err("--timeout requires a value".to_string());
                }
                timeout_secs = args[i]
                    .parse()
                    .map_err(|_| format!("invalid timeout: {}", args[i]))?;
            }
            "--help" | "-h" => {
                return Err(usage());
            }
            other => {
                return Err(format!("unknown argument: {other}\n{}", usage()));
            }
        }
        i += 1;
    }

    Ok(Args {
        agent_count,
        task_file,
        status_only,
        timeout_secs,
        self_improve,
    })
}

/// Print usage information.
fn usage() -> String {
    [
        "Usage: coordinator [OPTIONS]",
        "",
        "Options:",
        "  --agents, -a <N>      Number of agents to spawn (default: 4)",
        "  --self-improve        Enable self-improvement background loop",
        "  --tasks, -t <FILE>    Path to tasks TOML file (default: tasks.toml)",
        "  --status, -s          Show task queue status and exit",
        "  --timeout <SECS>      Per-task timeout in seconds (default: 600)",
        "  --help, -h            Show this help message",
    ]
    .join("\n")
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    let _ = tokio_prompt_orchestrator::init_tracing();

    let args = match parse_args() {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };

    let config = Arc::new(CoordinationConfig {
        agent_count: args.agent_count,
        task_file: args.task_file,
        timeout_secs: args.timeout_secs,
        ..CoordinationConfig::default()
    });

    if let Err(e) = config.validate() {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    }

    // Load task queue
    let queue = match TaskQueue::new(config.clone()).await {
        Ok(q) => Arc::new(q),
        Err(e) => {
            eprintln!("Failed to load task queue: {e}");
            std::process::exit(1);
        }
    };

    if args.status_only {
        print_status(&queue).await;
        return;
    }

    // Create monitor
    let monitor = AgentMonitor::new(config.clone(), Arc::clone(&queue));

    // Start monitoring
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let monitor_handle = monitor.start_monitoring(shutdown_rx.clone());

    // Optional: self-improvement loop (requires self-improving feature flag)
    #[cfg(all(feature = "self-tune", feature = "self-modify"))]
    let _sil_handle = if args.self_improve {
        use std::sync::Arc as StdArc;
        use tokio_prompt_orchestrator::{
            self_improve_loop::{LoopConfig, SelfImprovementLoop},
            self_tune::telemetry_bus::{PipelineCounters, TelemetryBus, TelemetryBusConfig},
        };
        let counters = PipelineCounters::new();
        let bus = StdArc::new(TelemetryBus::new(TelemetryBusConfig::default(), counters));
        bus.start();
        let sil = StdArc::new(SelfImprovementLoop::new(LoopConfig::default(), bus));
        let sil_clone = StdArc::clone(&sil);
        let sil_rx = shutdown_rx.clone();
        eprintln!("Self-improvement loop enabled");
        Some(tokio::spawn(async move { sil_clone.run(sil_rx).await }))
    } else {
        None
    };
    #[cfg(not(all(feature = "self-tune", feature = "self-modify")))]
    if args.self_improve {
        eprintln!("Warning: --self-improve requires --features self-improving at compile time");
    }

    // Register agents
    for i in 0..config.agent_count {
        monitor.register_agent(&format!("agent-{i}")).await;
    }

    eprintln!(
        "Spawning {} agents to work through tasks...",
        config.agent_count
    );

    // Spawn fleet
    let spawner = AgentSpawner::new(config.clone());
    let handles = match spawner.spawn_fleet(Arc::clone(&queue)).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Failed to spawn fleet: {e}");
            let _ = shutdown_tx.send(true);
            std::process::exit(1);
        }
    };

    // Wait for all agents to finish
    let stats = await_fleet(handles).await;

    // Signal monitor to stop
    let _ = shutdown_tx.send(true);
    let _ = monitor_handle.await;

    // Print final summary
    eprintln!("\n--- Fleet Summary ---");
    let mut total_completed = 0;
    let mut total_failed = 0;
    for s in &stats {
        eprintln!(
            "  {}: {} completed, {} failed ({} attempted)",
            s.agent_id, s.tasks_completed, s.tasks_failed, s.tasks_attempted
        );
        total_completed += s.tasks_completed;
        total_failed += s.tasks_failed;
    }
    eprintln!(
        "Total: {} completed, {} failed",
        total_completed, total_failed
    );

    // Print final queue status
    eprintln!("\n--- Task Queue Status ---");
    print_status(&queue).await;

    if total_failed > 0 {
        std::process::exit(1);
    }
}

/// Print the current status of the task queue.
async fn print_status(queue: &TaskQueue) {
    let (pending, claimed, completed, failed) = queue.summary().await;
    eprintln!("  Pending:   {pending}");
    eprintln!("  Claimed:   {claimed}");
    eprintln!("  Completed: {completed}");
    eprintln!("  Failed:    {failed}");

    let tasks = queue.status().await;
    if !tasks.is_empty() {
        eprintln!("\n  Tasks:");
        for task in &tasks {
            let claimed_info = if task.claimed_by.is_empty() {
                String::new()
            } else {
                format!(" ({})", task.claimed_by)
            };
            eprintln!(
                "    [{:>9}] {} (priority {}){claimed_info}",
                task.status, task.id, task.priority
            );
        }
    }
}
