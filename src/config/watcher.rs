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
            let debounce = Duration::from_millis(500);
            let mut last_reload = std::time::Instant::now()
                .checked_sub(debounce)
                .unwrap_or_else(std::time::Instant::now);

            loop {
                // Check for events in a non-blocking loop with a small sleep
                tokio::time::sleep(Duration::from_millis(100)).await;

                let mut should_reload = false;
                while let Ok(event) = notify_rx.try_recv() {
                    // Only react to modify/create events for our config file
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

                if should_reload && last_reload.elapsed() >= debounce {
                    last_reload = std::time::Instant::now();
                    match load_from_file(&config_path) {
                        Ok(new_config) => {
                            tracing::info!(
                                path = %config_path.display(),
                                pipeline = %new_config.pipeline.name,
                                "config reloaded successfully"
                            );
                            // If no receivers, that's fine — the config was still validated
                            let _ = tx_clone.send(new_config);
                        }
                        Err(e) => {
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

        // Wait for the watcher to detect and reload
        let result = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
        assert!(result.is_ok(), "should receive config update within 3s");
        let config = result.expect("test: timeout").expect("test: recv");
        assert_eq!(config.pipeline.name, "updated-name");
    }

    #[tokio::test]
    async fn test_config_watcher_rejects_invalid_reload() {
        let dir = tempfile::tempdir().expect("test: create tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, VALID_TOML).expect("test: write");

        let (_watcher, mut rx) = ConfigWatcher::new(path.clone()).expect("test: create watcher");

        // Wait for watcher
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Write invalid TOML
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
}
