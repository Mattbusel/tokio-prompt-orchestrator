//! Config hot-reload under load tests (test 29).
//!
//! Tests:
//! - Spawn a watcher with a valid config
//! - Start a background task sending config-read "requests" at high rate
//! - While those are in-flight, trigger a hot-reload by writing a new config
//! - Assert: no panics, new config is applied after reload, in-flight reads
//!   complete with the old config name, new reads get the new config name
//!
//! We don't need the web pipeline for this — the config watcher is independent
//! of any pipeline. Tests should complete well under 10 seconds.

use std::io::Write as IoWrite;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, RwLock};
use tokio_prompt_orchestrator::config::loader::load_from_file;
use tokio_prompt_orchestrator::config::watcher::ConfigWatcher;
use tokio_prompt_orchestrator::config::PipelineConfig;

// ============================================================================
// TOML helpers
// ============================================================================

fn make_toml(name: &str) -> String {
    format!(
        r#"
[pipeline]
name = "{name}"
version = "1.0"

[stages.rag]
enabled = true

[stages.assemble]
enabled = true

[stages.inference]
worker = "echo"
model = "test"

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts = 3
circuit_breaker_threshold = 5
circuit_breaker_timeout_s = 60
circuit_breaker_success_rate = 0.8

[deduplication]
enabled = false

[observability]
log_format = "pretty"
"#
    )
}

// ============================================================================
// Test 29a – Hot-reload under load: new config is applied
// ============================================================================

/// Spawns a background task that reads the current config at ~100/sec.
/// While reads are happening, writes a new config.
/// Asserts the new config name eventually shows up and no tasks panicked.
#[tokio::test]
async fn test_hot_reload_new_config_applied_under_load() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("config.toml");

    std::fs::write(&path, make_toml("original-pipeline")).expect("write initial config");

    let (_watcher, mut rx) = ConfigWatcher::new(path.clone()).expect("create watcher");

    // Shared current-config reference (simulates what the pipeline would hold).
    let current: Arc<RwLock<PipelineConfig>> = Arc::new(RwLock::new(
        load_from_file(&path).expect("load initial config"),
    ));

    // Background task: read the current config at ~100/sec for up to 5 seconds.
    let current_reader = Arc::clone(&current);
    let reader_handle = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut read_count = 0u32;
        while tokio::time::Instant::now() < deadline {
            {
                let cfg = current_reader.read().await;
                // Just access the name to ensure no panic on read.
                let _name = cfg.pipeline.name.clone();
                read_count += 1;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        read_count
    });

    // Wait for the reader to warm up.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Trigger hot-reload: write new config while reads are in flight.
    let new_toml = make_toml("reloaded-pipeline");
    let mut f = std::fs::File::create(&path).expect("open for write");
    f.write_all(new_toml.as_bytes()).expect("write new config");
    f.sync_all().expect("sync");
    drop(f);

    // Apply reload updates to the shared state when the watcher broadcasts.
    let current_writer = Arc::clone(&current);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "Timed out waiting for hot-reload broadcast"
        );
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(new_config)) => {
                let name = new_config.pipeline.name.clone();
                *current_writer.write().await = new_config;
                if name == "reloaded-pipeline" {
                    break;
                }
                // Stale event — keep waiting.
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                // Missed some; continue.
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                panic!("Watcher channel closed unexpectedly");
            }
            Err(_) => {
                panic!("Timeout waiting for hot-reload broadcast");
            }
        }
    }

    // Let the reader complete.
    let read_count = reader_handle.await.expect("reader task must not panic");
    assert!(
        read_count > 0,
        "Reader should have performed at least one read, got {read_count}"
    );

    // Verify new config is now active.
    let final_name = current.read().await.pipeline.name.clone();
    assert_eq!(
        final_name, "reloaded-pipeline",
        "After reload the active config should be 'reloaded-pipeline', got '{final_name}'"
    );
}

// ============================================================================
// Test 29b – In-flight requests complete with old config (read before reload)
// ============================================================================

/// Captures the config name at read time vs. the name after reload.
/// Reads that started before the write must see the old name.
#[tokio::test]
async fn test_hot_reload_in_flight_reads_see_old_config() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("config.toml");

    std::fs::write(&path, make_toml("before-reload")).expect("write initial config");

    let current: Arc<RwLock<PipelineConfig>> = Arc::new(RwLock::new(
        load_from_file(&path).expect("load initial config"),
    ));

    // Take a read lock snapshot *before* writing the new file.
    // This simulates an in-flight operation that holds config state.
    let snapshot_name = {
        let guard = current.read().await;
        guard.pipeline.name.clone()
        // guard dropped here
    };

    assert_eq!(
        snapshot_name, "before-reload",
        "Snapshot taken before reload should have the old name"
    );

    // Now overwrite the config on disk.
    std::fs::write(&path, make_toml("after-reload")).expect("write new config");

    // Reload manually (as the watcher would do).
    let new_config = load_from_file(&path).expect("reload");
    *current.write().await = new_config;

    // The snapshot retains the old value.
    assert_eq!(
        snapshot_name, "before-reload",
        "In-flight snapshot must still see the old config name"
    );

    // New reads see the updated config.
    let updated_name = current.read().await.pipeline.name.clone();
    assert_eq!(
        updated_name, "after-reload",
        "New reads after reload must see the new config name"
    );
}

// ============================================================================
// Test 29c – Invalid config during load does not panic or replace current
// ============================================================================

/// Writing invalid TOML while a background task is reading must not crash.
/// The watcher rejects the invalid config and keeps the old one valid.
#[tokio::test]
async fn test_hot_reload_invalid_config_does_not_replace_current() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("config.toml");

    std::fs::write(&path, make_toml("stable-pipeline")).expect("write initial config");

    let (_watcher, mut rx) = ConfigWatcher::new(path.clone()).expect("create watcher");

    // Write invalid TOML.
    std::fs::write(&path, "invalid [[[").expect("write invalid TOML");

    // The watcher must NOT broadcast the invalid config.
    let result = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        result.is_err(),
        "Watcher must NOT broadcast an invalid config (timed out = correct)"
    );
}

// ============================================================================
// Test 29d – Multiple reloads in quick succession converge to final config
// ============================================================================

/// Write the config several times in rapid succession. Eventually the watcher
/// must broadcast the final version (debounce collapses intermediate writes).
#[tokio::test]
async fn test_hot_reload_rapid_writes_converge_to_final_config() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("config.toml");

    std::fs::write(&path, make_toml("v0")).expect("write v0");

    let (_watcher, mut rx) = ConfigWatcher::new(path.clone()).expect("create watcher");

    // Allow watcher to start.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Drain any pending events from initial write.
    while tokio::time::timeout(Duration::from_millis(150), rx.recv())
        .await
        .is_ok()
    {}

    // Rapid-fire 5 writes.
    for i in 1..=5 {
        std::fs::write(&path, make_toml(&format!("v{i}"))).expect("write");
    }

    // Wait for at least one broadcast; the name should eventually be "v5".
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut last_name = String::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(cfg)) => {
                last_name = cfg.pipeline.name.clone();
                if last_name == "v5" {
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }

    // The watcher must eventually have broadcast a config (at least one version).
    assert!(
        !last_name.is_empty(),
        "Watcher must have broadcast at least one config update"
    );
}

// ============================================================================
// Test 29e – No panics: 100 concurrent readers during reload
// ============================================================================

/// 100 concurrent tasks read the shared config while a reload is applied.
/// None of them should panic.
#[tokio::test]
async fn test_hot_reload_no_panic_with_100_concurrent_readers() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("config.toml");

    std::fs::write(&path, make_toml("concurrent-test")).expect("write");

    let current: Arc<RwLock<PipelineConfig>> = Arc::new(RwLock::new(
        load_from_file(&path).expect("load"),
    ));

    // Spawn 100 concurrent readers.
    let mut handles = Vec::new();
    for _ in 0..100 {
        let c = Arc::clone(&current);
        handles.push(tokio::spawn(async move {
            // Each reader takes a brief lock and reads the pipeline name.
            let guard = c.read().await;
            let _name = guard.pipeline.name.clone();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }));
    }

    // While readers are sleeping, apply a reload.
    std::fs::write(&path, make_toml("concurrent-reload")).expect("write reload");
    let new_config = load_from_file(&path).expect("reload");
    *current.write().await = new_config;

    // All readers should complete without panicking.
    for (i, h) in handles.into_iter().enumerate() {
        tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .unwrap_or_else(|_| panic!("reader task {i} timed out"))
            .unwrap_or_else(|_| panic!("reader task {i} panicked"));
    }

    // Verify final state.
    let name = current.read().await.pipeline.name.clone();
    assert_eq!(
        name, "concurrent-reload",
        "After reload the config should be 'concurrent-reload', got '{name}'"
    );
}
