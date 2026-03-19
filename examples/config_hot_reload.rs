//! # Example: Config Hot-Reload
//!
//! Demonstrates: Watching a TOML config file for changes and applying the new
//! config to a running pipeline without restarting.
//!
//! Run with: `cargo run --example config_hot_reload`
//! Features needed: none
//!
//! What you will see:
//!   1. Pipeline starts with an initial config (name = "initial").
//!   2. After 2 seconds the example writes a new config (name = "hot-reloaded").
//!   3. The ConfigWatcher detects the file change and broadcasts the new config.
//!   4. The pipeline reads the new config and logs the change.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_prompt_orchestrator::{
    config::{loader::load_from_file, watcher::ConfigWatcher, PipelineConfig},
    spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

/// Write a minimal valid pipeline config to `path`, using `pipeline_name` as
/// the pipeline name so we can tell configs apart in the log output.
fn write_config(path: &std::path::Path, pipeline_name: &str, retry_attempts: u32) {
    let toml = format!(
        r#"[pipeline]
name = "{pipeline_name}"
version = "1.0"
description = "Hot-reload demo config"

[stages.rag]
enabled = true

[stages.assemble]
enabled = true

[stages.inference]
worker = "echo"
model = "echo"

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts = {retry_attempts}
retry_base_ms = 100
retry_max_ms = 5000
circuit_breaker_threshold = 5
circuit_breaker_timeout_s = 60
circuit_breaker_success_rate = 0.8

[deduplication]
enabled = true
window_s = 300
max_entries = 10000

[observability]
log_format = "pretty"
metrics_port = 9090
"#
    );

    let mut f = std::fs::File::create(path).expect("could not create config file");
    f.write_all(toml.as_bytes())
        .expect("could not write config file");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Config Hot-Reload Demo");
    info!("======================");
    info!("");

    // Create a temporary config file using the `tempfile` crate (available in dev-deps).
    let tmp_dir = tempfile::tempdir()?;
    let config_path: PathBuf = tmp_dir.path().join("pipeline.toml");

    // ── Step 1: Write and load the initial config ─────────────────────────
    write_config(&config_path, "initial", 3);
    let initial_config = load_from_file(&config_path)?;

    info!(
        "Loaded initial config: name={} retry_attempts={}",
        initial_config.pipeline.name, initial_config.resilience.retry_attempts
    );

    // Store the current active config in a shared RwLock so the pipeline
    // (and any request handler) can always read the latest version.
    let active_config: Arc<RwLock<PipelineConfig>> =
        Arc::new(RwLock::new(initial_config));

    // ── Step 2: Start the file watcher ───────────────────────────────────
    let (watcher, mut config_rx) = ConfigWatcher::new(config_path.clone())?;
    // Keep the watcher alive for the duration of the demo.
    let _watcher = watcher;

    // Spawn a task that applies config updates to the shared state.
    let active_config_clone = active_config.clone();
    tokio::spawn(async move {
        loop {
            match config_rx.recv().await {
                Ok(new_config) => {
                    info!(
                        "CONFIG RELOAD: new name={} retry_attempts={}",
                        new_config.pipeline.name, new_config.resilience.retry_attempts
                    );
                    let mut guard = active_config_clone.write().await;
                    *guard = new_config;
                    info!("Active config updated successfully (no restart required).");
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Config watcher lagged by {n} updates");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!("Config watcher channel closed — stopping reload task.");
                    break;
                }
            }
        }
    });

    // ── Step 3: Start the pipeline ────────────────────────────────────────
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(50));
    let handles = spawn_pipeline(worker);

    // Send a few requests with the initial config active.
    info!("");
    info!("Sending 3 requests with initial config...");
    for i in 0..3usize {
        let cfg = active_config.read().await;
        let req = PromptRequest {
            session: SessionId::new(format!("session-{i}")),
            request_id: format!("req-init-{i}"),
            input: format!("Hello from pipeline '{}' request {i}", cfg.pipeline.name),
            meta: std::collections::HashMap::new(),
            deadline: None,
        };
        drop(cfg); // release read lock before async send
        handles.input_tx.send(req).await.ok();
    }

    // ── Step 4: Write the updated config (simulates operator changing config) ─
    info!("");
    info!("Waiting 2 seconds, then writing updated config to disk...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    write_config(&config_path, "hot-reloaded", 5);
    info!("Updated config written to: {}", config_path.display());

    // Allow time for the watcher to detect the change and broadcast it.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Step 5: Send requests with the reloaded config ────────────────────
    info!("");
    info!("Sending 3 more requests — active config should now be 'hot-reloaded'...");
    for i in 0..3usize {
        let cfg = active_config.read().await;
        let req = PromptRequest {
            session: SessionId::new(format!("session-reload-{i}")),
            request_id: format!("req-reload-{i}"),
            input: format!("Hello from pipeline '{}' request {i}", cfg.pipeline.name),
            meta: std::collections::HashMap::new(),
            deadline: None,
        };
        drop(cfg);
        handles.input_tx.send(req).await.ok();
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Summary ───────────────────────────────────────────────────────────
    let final_cfg = active_config.read().await;
    info!("");
    info!("Summary");
    info!("=======");
    info!("  Final active config: name={}", final_cfg.pipeline.name);
    info!("  Final retry_attempts: {}", final_cfg.resilience.retry_attempts);
    info!("  Pipeline restarted: NO");
    info!("  Config update was applied live via ConfigWatcher + broadcast channel.");

    drop(handles.input_tx);
    Ok(())
}
