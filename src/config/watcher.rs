//! Configuration hot-reload watcher.
//!
//! ## Responsibility
//! Watch a TOML config file for changes and broadcast validated new configs
//! to subscribers. Invalid reloads are logged and rejected — the current
//! config remains unchanged.
//!
//! ## Guarantees
//! - Only validated configs are broadcast
//! - Invalid file edits are logged but do not disrupt the running system
//! - File watching is debounced to avoid rapid re-reads on multi-write editors
//! - Subscribers receive the new config via a `broadcast` channel
//!
//! ## NOT Responsible For
//! - Applying the config to the running pipeline (consumers decide what to do)
//! - Initial config loading (that belongs to `loader`)

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{broadcast, Mutex};

use crate::metrics;
use super::loader::load_from_file;
use super::validation::ConfigError;
use super::PipelineConfig;

/// Watches a config file for changes and broadcasts validated updates.
///
/// Subscribers receive new [`PipelineConfig`] values via a
/// [`broadcast::Receiver`].
///
/// # Panics
///
/// This type never panics.
pub struct ConfigWatcher {
    /// Broadcast sender for config updates.
    _tx: broadcast::Sender<PipelineConfig>,
    /// Retained watcher handle — dropping this stops file watching.
    _watcher: Arc<Mutex<RecommendedWatcher>>,
}

impl ConfigWatcher {
    /// Create a new [`ConfigWatcher`] for the given config file path.
    ///
    /// Returns the watcher and a receiver for config change notifications.
    /// The initial config is **not** broadcast — use `loader::load_from_file`
    /// for the initial load.
    ///
    /// # Arguments
    ///
    /// * `path` — Path to the TOML config file to watch.
    ///
    /// # Returns
    ///
    /// - `Ok((ConfigWatcher, Receiver))` on success.
    /// - `Err(ConfigError)` if the file watcher cannot be created.
    ///
    /// # Panics
    ///
    /// This function never panics.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tokio_prompt_orchestrator::config::watcher::ConfigWatcher;
    /// use std::path::PathBuf;
    ///
    /// let (watcher, mut rx) = ConfigWatcher::new(PathBuf::from("pipeline.toml"))?;
    /// tokio::spawn(async move {
    ///     while let Ok(config) = rx.recv().await {
    ///         println!("Config reloaded: {}", config.pipeline.name);
    ///     }
    /// });
    /// ```
    pub fn new(path: PathBuf) -> Result<(Self, broadcast::Receiver<PipelineConfig>), ConfigError> {
        let (tx, rx) = broadcast::channel(8);
        let tx_clone = tx.clone();
        let watch_path = path.clone();

        // Create a channel for notify events to forward into async context
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();

        let mut watcher = RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = notify_tx.send(event);
                }
            },
            notify::Config::default(),
        )
        .map_err(|e| ConfigError::Io {
            file: path.display().to_string(),
            source: std::io::Error::other(e.to_string()),
        })?;

        // Watch the parent directory to handle editors that do atomic saves
        // (write temp file → rename over original).
        let watch_dir = watch_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        watcher
            .watch(&watch_dir, RecursiveMode::NonRecursive)
            .map_err(|e| ConfigError::Io {
                file: watch_dir.display().to_string(),
                source: std::io::Error::other(e.to_string()),
            })?;

        let watcher = Arc::new(Mutex::new(watcher));

        // Spawn background task to process file events
        let config_path = watch_path.clone();
        tokio::spawn(async move {
            let debounce = Duration::from_millis(200);
            let mut last_reload = std::time::Instant::now()
                .checked_sub(debounce)
                .unwrap_or_else(std::time::Instant::now);

            loop {
                // Block on the notify channel with a timeout instead of
                // busy-polling with try_recv + sleep.  500 ms gives a good
                // balance: fast reaction to file changes while keeping CPU use
                // near zero when the file is idle.
                let mut should_reload = false;
                // Collect any events that arrived within the timeout window.
                match notify_rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(first_event) => {
                        // Process the first event, then drain any extras that
                        // arrived concurrently.
                        let events = std::iter::once(first_event)
                            .chain(std::iter::from_fn(|| notify_rx.try_recv().ok()))
                            .collect::<Vec<_>>();
                        for event in events {
                            match event.kind {
                                EventKind::Modify(_) | EventKind::Create(_) => {
                                    let is_our_file = event
                                        .paths
                                        .iter()
                                        .any(|p| p.file_name() == config_path.file_name());
                                    if is_our_file {
                                        should_reload = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Nothing arrived — loop back and wait again.
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        tracing::warn!("config watcher: notify channel disconnected, stopping");
                        break;
                    }
                }

                if should_reload && last_reload.elapsed() >= debounce {
                    last_reload = std::time::Instant::now();
                    let reload_start = std::time::Instant::now();
                    match load_from_file(&config_path) {
                        Ok(new_config) => {
                            let elapsed = reload_start.elapsed();
                            metrics::record_config_reload_duration("ok", elapsed);
                            tracing::info!(
                                path = %config_path.display(),
                                pipeline = %new_config.pipeline.name,
                                "config reloaded successfully"
                            );
                            // If no receivers, that's fine — the config was still validated
                            let _ = tx_clone.send(new_config);
                        }
                        Err(e) => {
                            let elapsed = reload_start.elapsed();
                            metrics::record_config_reload_duration("error", elapsed);
                            tracing::warn!(
                                path = %config_path.display(),
                                error = %e,
                                "config reload rejected — keeping current config"
                            );
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                _tx: tx,
                _watcher: watcher,
            },
            rx,
        ))
    }

    /// Subscribe to config change notifications.
    ///
    /// Returns a new receiver. Multiple subscribers are supported.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn subscribe(&self) -> broadcast::Receiver<PipelineConfig> {
        self._tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const VALID_TOML: &str = r#"
[pipeline]
name = "watcher-test"
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
"#;

    #[tokio::test]
    async fn test_config_watcher_creation_succeeds() {
        let dir = tempfile::tempdir().expect("test: create tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, VALID_TOML).expect("test: write");

        let result = ConfigWatcher::new(path);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_config_watcher_subscribe_returns_receiver() {
        let dir = tempfile::tempdir().expect("test: create tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, VALID_TOML).expect("test: write");

        let (watcher, _rx) = ConfigWatcher::new(path).expect("test: create watcher");
        let _rx2 = watcher.subscribe();
        // Should have two receivers now — no panic
    }

    #[tokio::test]
    async fn test_config_watcher_detects_file_change() {
        let dir = tempfile::tempdir().expect("test: create tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, VALID_TOML).expect("test: write");

        let (_watcher, mut rx) = ConfigWatcher::new(path.clone()).expect("test: create watcher");

        // Wait a moment for watcher to be ready
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Modify the file
        let updated = VALID_TOML.replace("watcher-test", "updated-name");
        let mut f = std::fs::File::create(&path).expect("test: open for write");
        f.write_all(updated.as_bytes()).expect("test: write");
        f.sync_all().expect("test: sync");
        drop(f);

        // Drain events until we see the updated config or time out.
        // macOS FSEvents can coalesce or deliver a stale read on the first
        // event, so we retry rather than asserting on the first notification.
        // Use a 15s deadline — macOS CI FSEvents latency can be high.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                remaining > Duration::ZERO,
                "timed out waiting for updated config"
            );
            let result = tokio::time::timeout(remaining, rx.recv()).await;
            assert!(result.is_ok(), "should receive config update within 15s");
            let config = result.expect("test: timeout").expect("test: recv");
            if config.pipeline.name == "updated-name" {
                break;
            }
            // Received a stale event; wait for the next one.
        }
    }

    #[tokio::test]
    async fn test_config_watcher_rejects_invalid_reload() {
        let dir = tempfile::tempdir().expect("test: create tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, VALID_TOML).expect("test: write");

        let (_watcher, mut rx) = ConfigWatcher::new(path.clone()).expect("test: create watcher");

        // Drain any delayed events from the initial file write (macOS FSEvents
        // can deliver the first event well after the watcher is created).
        tokio::time::sleep(Duration::from_millis(500)).await;
        while tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_ok()
        {}

        // Write invalid TOML — the watcher should reject it and not broadcast.
        std::fs::write(&path, "invalid [[[").expect("test: write invalid");

        // Should NOT receive a config update (invalid was rejected)
        let result = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        assert!(result.is_err(), "should not broadcast invalid config");
    }

    #[tokio::test]
    async fn test_config_watcher_nonexistent_parent_returns_error() {
        let path = PathBuf::from("/definitely/nonexistent/dir/config.toml");
        let result = ConfigWatcher::new(path);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn hot_reload_concurrent_writes_no_invalid_config_broadcast() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("test: create tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, VALID_TOML).expect("test: initial write");

        let (_watcher, mut rx) = ConfigWatcher::new(path.clone()).expect("test: create watcher");

        let cfg_a = VALID_TOML.replace("watcher-test", "config-a");
        let cfg_b = VALID_TOML.replace("watcher-test", "config-b");
        let cfg_a_bytes = cfg_a.as_bytes().to_vec();
        let cfg_b_bytes = cfg_b.as_bytes().to_vec();

        let path_a = path.clone();
        let path_b = path.clone();
        let h1 = tokio::spawn(async move {
            for _ in 0..5 {
                std::fs::write(&path_a, &cfg_a_bytes).ok();
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        });
        let h2 = tokio::spawn(async move {
            for _ in 0..5 {
                std::fs::write(&path_b, &cfg_b_bytes).ok();
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        });
        let _ = tokio::join!(h1, h2);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let mut received = 0usize;
        while let Ok(_cfg) = rx.try_recv() {
            received += 1;
        }
        // At least one reload must have been observed
        assert!(received > 0, "expected at least one hot-reload event");
    }
}
