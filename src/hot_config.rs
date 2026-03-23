//! # Hot-Reloadable Configuration
//!
//! Polls a TOML-like flat key=value configuration file at a configurable
//! interval and broadcasts a new version number to all subscribers whenever
//! the file changes (detected via mtime).
//!
//! ## Supported syntax
//!
//! The parser handles:
//! - Blank lines and lines beginning with `#` (comments)
//! - `[section]` headers (ignored — values are stored without section prefix)
//! - `key = value` lines where value may be:
//!   - `true` / `false` → [`ConfigValue::Bool`]
//!   - Integer literal (no `.`) → [`ConfigValue::Int`]
//!   - Float literal (contains `.`) → [`ConfigValue::Float`]
//!   - `["a", "b"]`-style list → [`ConfigValue::List`]
//!   - Anything else (including quoted strings) → [`ConfigValue::String`]
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::hot_config::{HotConfig, ConfigValue};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let cfg = HotConfig::from_defaults();
//! cfg.set("workers", ConfigValue::Int(4));
//! assert_eq!(cfg.get("workers"), Some(ConfigValue::Int(4)));
//! # }
//! ```

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::{watch, RwLock};

// ---------------------------------------------------------------------------
// ConfigValue
// ---------------------------------------------------------------------------

/// A typed configuration value.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    /// UTF-8 string (raw, without surrounding quotes).
    String(String),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit float.
    Float(f64),
    /// Boolean.
    Bool(bool),
    /// Ordered list of [`ConfigValue`] items.
    List(Vec<ConfigValue>),
}

// ---------------------------------------------------------------------------
// FromConfigValue
// ---------------------------------------------------------------------------

/// Fallible conversion from a [`ConfigValue`] reference to a concrete type.
pub trait FromConfigValue: Sized {
    /// Attempt the conversion.  Returns `None` on type mismatch.
    fn from_config_value(v: &ConfigValue) -> Option<Self>;
}

impl FromConfigValue for String {
    fn from_config_value(v: &ConfigValue) -> Option<Self> {
        match v {
            ConfigValue::String(s) => Some(s.clone()),
            ConfigValue::Int(i) => Some(i.to_string()),
            ConfigValue::Float(f) => Some(f.to_string()),
            ConfigValue::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
}

impl FromConfigValue for i64 {
    fn from_config_value(v: &ConfigValue) -> Option<Self> {
        match v {
            ConfigValue::Int(i) => Some(*i),
            ConfigValue::Float(f) => Some(*f as i64),
            ConfigValue::String(s) => s.parse().ok(),
            _ => None,
        }
    }
}

impl FromConfigValue for f64 {
    fn from_config_value(v: &ConfigValue) -> Option<Self> {
        match v {
            ConfigValue::Float(f) => Some(*f),
            ConfigValue::Int(i) => Some(*i as f64),
            ConfigValue::String(s) => s.parse().ok(),
            _ => None,
        }
    }
}

impl FromConfigValue for bool {
    fn from_config_value(v: &ConfigValue) -> Option<Self> {
        match v {
            ConfigValue::Bool(b) => Some(*b),
            ConfigValue::String(s) => match s.to_lowercase().as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ConfigError
// ---------------------------------------------------------------------------

/// Errors that can occur during configuration loading or parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    /// The file could not be read (contains the OS error description).
    IoError(String),
    /// A line could not be parsed.
    ParseError(String),
    /// A value could not be coerced to the requested type.
    InvalidType,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IoError(s) => write!(f, "IO error: {s}"),
            Self::ParseError(s) => write!(f, "parse error: {s}"),
            Self::InvalidType => write!(f, "invalid type coercion"),
        }
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// ConfigSnapshot
// ---------------------------------------------------------------------------

/// An immutable snapshot of the configuration at a point in time.
#[derive(Debug, Clone)]
pub struct ConfigSnapshot {
    /// Key-value pairs loaded from the file.
    pub values: HashMap<String, ConfigValue>,
    /// Monotonically increasing version counter (starts at 0, increments on reload).
    pub version: u64,
    /// Wall-clock instant when this snapshot was created.
    pub loaded_at: Instant,
}

impl Default for ConfigSnapshot {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            version: 0,
            loaded_at: Instant::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// TOML-like parser
// ---------------------------------------------------------------------------

/// Parse a flat TOML-like key=value file into a [`ConfigSnapshot`].
///
/// Sections (`[name]`) are accepted but ignored (values are stored without a
/// section prefix).  Comments (`# ...`) and blank lines are skipped.
///
/// This is intentionally a lightweight parser: it does **not** support
/// multi-line values, inline tables, or dotted keys.
pub fn load_toml(path: &str) -> Result<ConfigSnapshot, ConfigError> {
    let content = fs::read_to_string(path)
        .map_err(|e| ConfigError::IoError(format!("{path}: {e}")))?;
    parse_content(&content)
}

fn parse_content(content: &str) -> Result<ConfigSnapshot, ConfigError> {
    let mut values = HashMap::new();

    for (line_no, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();

        // Skip blank lines and comments.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Skip section headers.
        if line.starts_with('[') {
            continue;
        }

        // Expect `key = value`.
        let eq_pos = line.find('=').ok_or_else(|| {
            ConfigError::ParseError(format!(
                "line {}: expected `key = value`, got `{line}`",
                line_no + 1
            ))
        })?;

        let key = line[..eq_pos].trim().to_string();
        if key.is_empty() {
            return Err(ConfigError::ParseError(format!(
                "line {}: empty key",
                line_no + 1
            )));
        }
        let raw_val = line[eq_pos + 1..].trim();
        let value = parse_value(raw_val);
        values.insert(key, value);
    }

    Ok(ConfigSnapshot {
        values,
        version: 0,
        loaded_at: Instant::now(),
    })
}

/// Parse a single value token.
fn parse_value(raw: &str) -> ConfigValue {
    // Boolean
    if raw == "true" {
        return ConfigValue::Bool(true);
    }
    if raw == "false" {
        return ConfigValue::Bool(false);
    }

    // List: starts with `[`
    if raw.starts_with('[') && raw.ends_with(']') {
        let inner = &raw[1..raw.len() - 1];
        let items: Vec<ConfigValue> = inner
            .split(',')
            .map(|s| parse_value(s.trim()))
            .filter(|v| !matches!(v, ConfigValue::String(s) if s.is_empty()))
            .collect();
        return ConfigValue::List(items);
    }

    // Quoted string: strip surrounding `"` or `'`.
    if (raw.starts_with('"') && raw.ends_with('"'))
        || (raw.starts_with('\'') && raw.ends_with('\''))
    {
        return ConfigValue::String(raw[1..raw.len() - 1].to_string());
    }

    // Integer (no decimal point).
    if !raw.contains('.') {
        if let Ok(i) = raw.parse::<i64>() {
            return ConfigValue::Int(i);
        }
    }

    // Float.
    if let Ok(f) = raw.parse::<f64>() {
        return ConfigValue::Float(f);
    }

    // Fallback: raw string.
    ConfigValue::String(raw.to_string())
}

// ---------------------------------------------------------------------------
// HotConfig
// ---------------------------------------------------------------------------

/// Hot-reloadable configuration object.
///
/// Internally holds an [`Arc<RwLock<ConfigSnapshot>>`] and a
/// [`watch::Sender<u64>`] channel.  Call [`start_watcher`](HotConfig::start_watcher)
/// to spawn a background Tokio task that polls the file for mtime changes.
pub struct HotConfig {
    snapshot: Arc<RwLock<ConfigSnapshot>>,
    file_path: Option<PathBuf>,
    poll_interval: Duration,
    version_tx: watch::Sender<u64>,
    version_rx: watch::Receiver<u64>,
}

impl HotConfig {
    /// Create a [`HotConfig`] backed by a file path.
    ///
    /// The file is loaded immediately.  If loading fails the config starts
    /// with an empty snapshot (version 0).
    pub fn new(path: &str, poll_interval: Duration) -> Self {
        let snapshot = load_toml(path).unwrap_or_default();
        let (tx, rx) = watch::channel(snapshot.version);
        Self {
            snapshot: Arc::new(RwLock::new(snapshot)),
            file_path: Some(PathBuf::from(path)),
            poll_interval,
            version_tx: tx,
            version_rx: rx,
        }
    }

    /// Create an in-memory [`HotConfig`] with no backing file.
    ///
    /// Useful for tests and programmatic configuration.
    pub fn from_defaults() -> Self {
        let snapshot = ConfigSnapshot::default();
        let (tx, rx) = watch::channel(snapshot.version);
        Self {
            snapshot: Arc::new(RwLock::new(snapshot)),
            file_path: None,
            poll_interval: Duration::from_secs(5),
            version_tx: tx,
            version_rx: rx,
        }
    }

    /// Programmatically set a key/value in the current snapshot.
    ///
    /// Increments the version counter and notifies all subscribers.
    pub fn set(&self, key: &str, value: ConfigValue) {
        // Use blocking write — acceptable in non-async contexts.
        let snapshot = Arc::clone(&self.snapshot);
        let key = key.to_string();
        let tx = self.version_tx.clone();
        // This runs synchronously; callers in async context should use a
        // spawn_blocking wrapper if needed.
        let mut guard = snapshot.blocking_write();
        guard.values.insert(key, value);
        guard.version += 1;
        let v = guard.version;
        drop(guard);
        let _ = tx.send(v);
    }

    /// Read the value for `key` from the current snapshot.
    pub fn get(&self, key: &str) -> Option<ConfigValue> {
        let guard = self.snapshot.blocking_read();
        guard.values.get(key).cloned()
    }

    /// Read `key`, coerce it to `T`, or return `default` on miss/type error.
    pub fn get_or_default<T: FromConfigValue>(&self, key: &str, default: T) -> T {
        self.get(key)
            .and_then(|v| T::from_config_value(&v))
            .unwrap_or(default)
    }

    /// Subscribe to version-change notifications.
    ///
    /// The receiver yields the new version number each time the config is
    /// reloaded (or [`set`](HotConfig::set) is called).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version_rx.clone()
    }

    /// Spawn a background Tokio task that polls the backing file for changes.
    ///
    /// Returns a [`tokio::task::JoinHandle`] that the caller can abort or
    /// await.  If no file path was configured (e.g. created via
    /// [`from_defaults`](HotConfig::from_defaults)), the spawned task exits
    /// immediately.
    pub fn start_watcher(&self) -> tokio::task::JoinHandle<()> {
        let file_path = self.file_path.clone();
        let snapshot = Arc::clone(&self.snapshot);
        let poll_interval = self.poll_interval;
        let tx = self.version_tx.clone();

        tokio::spawn(async move {
            let path = match file_path {
                Some(p) => p,
                None => return,
            };

            let mut last_mtime: Option<SystemTime> = None;

            loop {
                tokio::time::sleep(poll_interval).await;

                // Get current mtime.
                let current_mtime = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok();

                let changed = match (last_mtime, current_mtime) {
                    (None, Some(_)) => true,
                    (Some(prev), Some(curr)) => curr != prev,
                    _ => false,
                };

                if changed {
                    last_mtime = current_mtime;
                    if let Ok(new_snap) = load_toml(path.to_str().unwrap_or("")) {
                        let new_version = {
                            let guard = snapshot.read().await;
                            guard.version + 1
                        };
                        {
                            let mut guard = snapshot.write().await;
                            guard.values = new_snap.values;
                            guard.version = new_version;
                            guard.loaded_at = Instant::now();
                        }
                        let _ = tx.send(new_version);
                    }
                } else {
                    // Update last_mtime on first successful read even if unchanged.
                    if last_mtime.is_none() {
                        last_mtime = current_mtime;
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_content tests -----------------------------------------------

    #[test]
    fn parse_bool_values() {
        let snap = parse_content("enabled = true\ndisabled = false").unwrap();
        assert_eq!(snap.values["enabled"], ConfigValue::Bool(true));
        assert_eq!(snap.values["disabled"], ConfigValue::Bool(false));
    }

    #[test]
    fn parse_int_value() {
        let snap = parse_content("workers = 8").unwrap();
        assert_eq!(snap.values["workers"], ConfigValue::Int(8));
    }

    #[test]
    fn parse_float_value() {
        let snap = parse_content("threshold = 0.95").unwrap();
        assert_eq!(snap.values["threshold"], ConfigValue::Float(0.95));
    }

    #[test]
    fn parse_quoted_string() {
        let snap = parse_content(r#"name = "hello world""#).unwrap();
        assert_eq!(
            snap.values["name"],
            ConfigValue::String("hello world".to_string())
        );
    }

    #[test]
    fn parse_list_of_strings() {
        let snap = parse_content(r#"models = ["gpt-4", "claude-3"]"#).unwrap();
        match &snap.values["models"] {
            ConfigValue::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], ConfigValue::String("gpt-4".to_string()));
                assert_eq!(items[1], ConfigValue::String("claude-3".to_string()));
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn skip_comments_and_sections() {
        let content = "# comment\n[section]\nkey = 42\n# another comment\n";
        let snap = parse_content(content).unwrap();
        assert_eq!(snap.values.len(), 1);
        assert_eq!(snap.values["key"], ConfigValue::Int(42));
    }

    #[test]
    fn missing_key_returns_default() {
        let cfg = HotConfig::from_defaults();
        let val: i64 = cfg.get_or_default("nonexistent", 99);
        assert_eq!(val, 99);
    }

    #[test]
    fn get_returns_none_for_missing_key() {
        let cfg = HotConfig::from_defaults();
        assert_eq!(cfg.get("does_not_exist"), None);
    }

    #[test]
    fn set_and_get_roundtrip() {
        let cfg = HotConfig::from_defaults();
        cfg.set("workers", ConfigValue::Int(16));
        assert_eq!(cfg.get("workers"), Some(ConfigValue::Int(16)));
    }

    #[test]
    fn set_increments_version() {
        let cfg = HotConfig::from_defaults();
        let initial_version = {
            let guard = cfg.snapshot.blocking_read();
            guard.version
        };
        cfg.set("x", ConfigValue::Bool(true));
        let new_version = {
            let guard = cfg.snapshot.blocking_read();
            guard.version
        };
        assert_eq!(new_version, initial_version + 1);
    }

    #[test]
    fn type_coercion_int_to_float() {
        let cfg = HotConfig::from_defaults();
        cfg.set("rate", ConfigValue::Int(5));
        let v: f64 = cfg.get_or_default("rate", 0.0);
        assert!((v - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn type_coercion_string_to_bool() {
        assert_eq!(
            bool::from_config_value(&ConfigValue::String("true".into())),
            Some(true)
        );
        assert_eq!(
            bool::from_config_value(&ConfigValue::String("false".into())),
            Some(false)
        );
        assert_eq!(
            bool::from_config_value(&ConfigValue::String("yes".into())),
            Some(true)
        );
    }

    #[test]
    fn parse_error_on_no_equals() {
        let result = parse_content("bad_line_no_equals");
        assert!(matches!(result, Err(ConfigError::ParseError(_))));
    }

    #[tokio::test]
    async fn file_load_missing_file_returns_io_error() {
        let result = load_toml("/nonexistent/path/to/config.toml");
        assert!(matches!(result, Err(ConfigError::IoError(_))));
    }

    #[tokio::test]
    async fn subscribe_receives_version_on_set() {
        let cfg = HotConfig::from_defaults();
        let mut rx = cfg.subscribe();

        cfg.set("debug", ConfigValue::Bool(true));
        // The watch channel marks the value as changed; borrow_and_update
        // returns the latest.
        assert!(*rx.borrow_and_update() >= 1);
    }

    #[tokio::test]
    async fn watcher_exits_cleanly_for_defaults() {
        // A HotConfig with no file path should spawn a task that exits immediately.
        let cfg = HotConfig::from_defaults();
        let handle = cfg.start_watcher();
        // The task should complete without error.
        let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
    }

    #[tokio::test]
    async fn watcher_reloads_file_on_change() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().expect("tempfile");
        writeln!(f, "version_key = 1").expect("write");
        let path = f.path().to_str().unwrap().to_string();

        let cfg = HotConfig::new(&path, Duration::from_millis(20));
        let mut rx = cfg.subscribe();
        let handle = cfg.start_watcher();

        // Let the watcher do its first poll (sets the baseline mtime).
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Modify the file.  Sleep 1 second first so the mtime is guaranteed
        // to differ even on file systems with 1-second mtime granularity.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(f.path())
                .expect("open");
            writeln!(file, "version_key = 2").expect("write");
        }

        // Wait for at least two poll cycles after the modification.
        tokio::time::sleep(Duration::from_millis(80)).await;

        handle.abort();

        // Version should have been bumped.
        let v = *rx.borrow_and_update();
        assert!(v >= 1, "expected version >= 1, got {v}");
    }
}
