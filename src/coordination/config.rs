//! # CoordinationConfig — agent fleet configuration
//!
//! ## Responsibility
//! Define configuration for the agent coordination layer including
//! agent count, task file path, timeouts, and lock directory.
//!
//! ## Guarantees
//! - Validated: all fields are bounds-checked before use
//! - Defaulted: every field has a sensible default
//! - Serializable: round-trips through serde (TOML ↔ Rust)
//!
//! ## NOT Responsible For
//! - Runtime coordination (see: queue.rs, spawner.rs)
//! - Task definitions (see: task.rs)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for the agent coordination layer.
///
/// # Fields
///
/// * `agent_count` — Number of agent processes to spawn (default: 4)
/// * `task_file` — Path to the tasks TOML file (default: `tasks.toml`)
/// * `lock_dir` — Directory for per-task lock files (default: `.coordination/locks`)
/// * `claude_bin` — Path to the claude binary (default: `claude`)
/// * `project_path` — Root path of the project for agents (default: `.`)
/// * `timeout_secs` — Maximum seconds per task before timeout (default: 600)
/// * `stale_lock_secs` — Lock files older than this are reclaimable (default: 300)
/// * `health_interval_secs` — Seconds between health checks (default: 10)
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::coordination::config::CoordinationConfig;
/// let config = CoordinationConfig::default();
/// assert_eq!(config.agent_count, 4);
/// ```
///
/// # Panics
///
/// No methods on this type panic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinationConfig {
    /// Number of agent processes to spawn.
    #[serde(default = "default_agent_count")]
    pub agent_count: usize,

    /// Path to the tasks TOML file.
    #[serde(default = "default_task_file")]
    pub task_file: PathBuf,

    /// Directory for per-task lock files.
    #[serde(default = "default_lock_dir")]
    pub lock_dir: PathBuf,

    /// Path to the claude binary.
    #[serde(default = "default_claude_bin")]
    pub claude_bin: PathBuf,

    /// Root path of the project for agents to work in.
    #[serde(default = "default_project_path")]
    pub project_path: PathBuf,

    /// Maximum seconds per task before timeout.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,

    /// Lock files older than this (seconds) are reclaimable.
    #[serde(default = "default_stale_lock_secs")]
    pub stale_lock_secs: u64,

    /// Seconds between agent health checks.
    #[serde(default = "default_health_interval_secs")]
    pub health_interval_secs: u64,
}

impl Default for CoordinationConfig {
    fn default() -> Self {
        Self {
            agent_count: default_agent_count(),
            task_file: default_task_file(),
            lock_dir: default_lock_dir(),
            claude_bin: default_claude_bin(),
            project_path: default_project_path(),
            timeout_secs: default_timeout_secs(),
            stale_lock_secs: default_stale_lock_secs(),
            health_interval_secs: default_health_interval_secs(),
        }
    }
}

impl CoordinationConfig {
    /// Validate the configuration, returning a list of errors.
    ///
    /// # Returns
    ///
    /// - `Ok(())` if all fields are valid
    /// - `Err(CoordinationError::InvalidConfig)` with concatenated error messages
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn validate(&self) -> Result<(), super::CoordinationError> {
        let mut errors = Vec::new();

        if self.agent_count == 0 {
            errors.push("agent_count must be > 0".to_string());
        }
        if self.agent_count > 100 {
            errors.push("agent_count must be <= 100".to_string());
        }
        if self.timeout_secs == 0 {
            errors.push("timeout_secs must be > 0".to_string());
        }
        if self.stale_lock_secs == 0 {
            errors.push("stale_lock_secs must be > 0".to_string());
        }
        if self.health_interval_secs == 0 {
            errors.push("health_interval_secs must be > 0".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(super::CoordinationError::InvalidConfig(errors.join("; ")))
        }
    }

    /// Return the task timeout as a [`std::time::Duration`].
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn task_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs)
    }

    /// Return the stale lock timeout as a [`std::time::Duration`].
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn stale_lock_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.stale_lock_secs)
    }

    /// Return the health check interval as a [`std::time::Duration`].
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn health_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.health_interval_secs)
    }
}

/// Default agent count: 4.
fn default_agent_count() -> usize {
    4
}

/// Default task file path: `tasks.toml`.
fn default_task_file() -> PathBuf {
    PathBuf::from("tasks.toml")
}

/// Default lock directory: `.coordination/locks`.
fn default_lock_dir() -> PathBuf {
    PathBuf::from(".coordination/locks")
}

/// Default claude binary: `claude`.
fn default_claude_bin() -> PathBuf {
    PathBuf::from("claude")
}

/// Default project path: `.` (current directory).
fn default_project_path() -> PathBuf {
    PathBuf::from(".")
}

/// Default task timeout: 600 seconds (10 minutes).
fn default_timeout_secs() -> u64 {
    600
}

/// Default stale lock timeout: 300 seconds (5 minutes).
fn default_stale_lock_secs() -> u64 {
    300
}

/// Default health check interval: 10 seconds.
fn default_health_interval_secs() -> u64 {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_agent_count_is_4() {
        let config = CoordinationConfig::default();
        assert_eq!(config.agent_count, 4);
    }

    #[test]
    fn test_default_task_file_is_tasks_toml() {
        let config = CoordinationConfig::default();
        assert_eq!(config.task_file, PathBuf::from("tasks.toml"));
    }

    #[test]
    fn test_default_lock_dir() {
        let config = CoordinationConfig::default();
        assert_eq!(config.lock_dir, PathBuf::from(".coordination/locks"));
    }

    #[test]
    fn test_default_claude_bin() {
        let config = CoordinationConfig::default();
        assert_eq!(config.claude_bin, PathBuf::from("claude"));
    }

    #[test]
    fn test_default_project_path() {
        let config = CoordinationConfig::default();
        assert_eq!(config.project_path, PathBuf::from("."));
    }

    #[test]
    fn test_default_timeout_secs_is_600() {
        let config = CoordinationConfig::default();
        assert_eq!(config.timeout_secs, 600);
    }

    #[test]
    fn test_default_stale_lock_secs_is_300() {
        let config = CoordinationConfig::default();
        assert_eq!(config.stale_lock_secs, 300);
    }

    #[test]
    fn test_default_health_interval_secs_is_10() {
        let config = CoordinationConfig::default();
        assert_eq!(config.health_interval_secs, 10);
    }

    #[test]
    fn test_validate_default_config_passes() {
        let config = CoordinationConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_zero_agent_count_fails() {
        let mut config = CoordinationConfig::default();
        config.agent_count = 0;
        let result = config.validate();
        assert!(result.is_err());
        let err_msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err_msg.contains("agent_count must be > 0"));
    }

    #[test]
    fn test_validate_excessive_agent_count_fails() {
        let mut config = CoordinationConfig::default();
        config.agent_count = 101;
        let result = config.validate();
        assert!(result.is_err());
        let err_msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err_msg.contains("agent_count must be <= 100"));
    }

    #[test]
    fn test_validate_zero_timeout_fails() {
        let mut config = CoordinationConfig::default();
        config.timeout_secs = 0;
        let result = config.validate();
        assert!(result.is_err());
        let err_msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err_msg.contains("timeout_secs must be > 0"));
    }

    #[test]
    fn test_validate_zero_stale_lock_fails() {
        let mut config = CoordinationConfig::default();
        config.stale_lock_secs = 0;
        let result = config.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_zero_health_interval_fails() {
        let mut config = CoordinationConfig::default();
        config.health_interval_secs = 0;
        let result = config.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_collects_multiple_errors() {
        let mut config = CoordinationConfig::default();
        config.agent_count = 0;
        config.timeout_secs = 0;
        config.stale_lock_secs = 0;
        let result = config.validate();
        assert!(result.is_err());
        let err_msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err_msg.contains("agent_count"));
        assert!(err_msg.contains("timeout_secs"));
        assert!(err_msg.contains("stale_lock_secs"));
    }

    #[test]
    fn test_task_timeout_returns_duration() {
        let config = CoordinationConfig::default();
        assert_eq!(config.task_timeout(), std::time::Duration::from_secs(600));
    }

    #[test]
    fn test_stale_lock_timeout_returns_duration() {
        let config = CoordinationConfig::default();
        assert_eq!(
            config.stale_lock_timeout(),
            std::time::Duration::from_secs(300)
        );
    }

    #[test]
    fn test_health_interval_returns_duration() {
        let config = CoordinationConfig::default();
        assert_eq!(
            config.health_interval(),
            std::time::Duration::from_secs(10)
        );
    }

    #[test]
    fn test_config_serde_roundtrip_toml() {
        let config = CoordinationConfig::default();
        let toml_str = toml::to_string_pretty(&config);
        assert!(toml_str.is_ok());
        let reparsed: Result<CoordinationConfig, _> =
            toml::from_str(&toml_str.unwrap_or_default());
        assert!(reparsed.is_ok());
        let reparsed_config = reparsed.ok().unwrap_or_default();
        assert_eq!(reparsed_config.agent_count, config.agent_count);
        assert_eq!(reparsed_config.timeout_secs, config.timeout_secs);
    }

    #[test]
    fn test_config_serde_roundtrip_json() {
        let config = CoordinationConfig::default();
        let json = serde_json::to_string(&config);
        assert!(json.is_ok());
        let reparsed: Result<CoordinationConfig, _> =
            serde_json::from_str(&json.unwrap_or_default());
        assert!(reparsed.is_ok());
    }

    #[test]
    fn test_config_custom_values() {
        let config = CoordinationConfig {
            agent_count: 10,
            task_file: PathBuf::from("custom_tasks.toml"),
            lock_dir: PathBuf::from("/tmp/locks"),
            claude_bin: PathBuf::from("/usr/local/bin/claude"),
            project_path: PathBuf::from("/home/user/project"),
            timeout_secs: 1200,
            stale_lock_secs: 600,
            health_interval_secs: 5,
        };
        assert!(config.validate().is_ok());
        assert_eq!(config.task_timeout(), std::time::Duration::from_secs(1200));
    }

    #[test]
    fn test_validate_boundary_agent_count_1_passes() {
        let mut config = CoordinationConfig::default();
        config.agent_count = 1;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_boundary_agent_count_100_passes() {
        let mut config = CoordinationConfig::default();
        config.agent_count = 100;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_clone_independence() {
        let original = CoordinationConfig::default();
        let mut cloned = original.clone();
        cloned.agent_count = 99;
        assert_eq!(original.agent_count, 4);
        assert_eq!(cloned.agent_count, 99);
    }

    #[test]
    fn test_config_debug_format() {
        let config = CoordinationConfig::default();
        let debug = format!("{:?}", config);
        assert!(debug.contains("CoordinationConfig"));
        assert!(debug.contains("agent_count: 4"));
    }

    #[test]
    fn test_config_from_toml_with_missing_fields_uses_defaults() {
        let toml_str = r#"agent_count = 8"#;
        let config: Result<CoordinationConfig, _> = toml::from_str(toml_str);
        assert!(config.is_ok());
        let c = config.ok().unwrap_or_default();
        assert_eq!(c.agent_count, 8);
        assert_eq!(c.timeout_secs, 600); // default
    }
}
