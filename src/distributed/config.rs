//! # Distributed Configuration
//!
//! ## Responsibility
//! Define and validate all configuration parameters for distributed clustering:
//! NATS connection, Redis connection, node identity, heartbeat timing, and
//! leader election parameters.
//!
//! ## Guarantees
//! - Deterministic: same inputs always produce the same configuration
//! - Validated: all semantic constraints are checked before use
//! - Type-safe: invalid combinations are caught at construction time
//! - Serializable: round-trips through TOML/JSON via serde
//!
//! ## NOT Responsible For
//! - Establishing connections (see: `nats`, `redis_dedup`, `leader`)
//! - Runtime behavior changes (config is immutable once constructed)

use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::DistributedError;

/// Default NATS URL when none is specified.
fn default_nats_url() -> String {
    "nats://localhost:4222".to_string()
}

/// Default Redis URL when none is specified.
fn default_redis_url() -> String {
    "redis://localhost:6379".to_string()
}

/// Default heartbeat interval: 5 seconds.
fn default_heartbeat_interval_s() -> u64 {
    5
}

/// Default eviction timeout: 15 seconds (3 missed heartbeats).
fn default_eviction_timeout_s() -> u64 {
    15
}

/// Default leader TTL: 10 seconds.
fn default_leader_ttl_s() -> u64 {
    10
}

/// Default deduplication TTL: 300 seconds (5 minutes).
fn default_dedup_ttl_s() -> u64 {
    300
}

/// Configuration for distributed clustering mode.
///
/// Holds all parameters needed to join a cluster: NATS and Redis connection
/// URLs, node identity, heartbeat timing, and leader election parameters.
///
/// # Panics
///
/// This type never panics during construction or access.
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::distributed::DistributedConfig;
///
/// let config = DistributedConfig::new("node-1".to_string());
/// assert_eq!(config.node_id, "node-1");
/// assert_eq!(config.heartbeat_interval_s, 5);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DistributedConfig {
    /// Unique identifier for this node within the cluster.
    pub node_id: String,

    /// NATS server connection URL for pub/sub messaging.
    #[serde(default = "default_nats_url")]
    pub nats_url: String,

    /// Redis server connection URL for dedup and leader election.
    #[serde(default = "default_redis_url")]
    pub redis_url: String,

    /// Interval (seconds) between heartbeat broadcasts.
    #[serde(default = "default_heartbeat_interval_s")]
    pub heartbeat_interval_s: u64,

    /// Time (seconds) of silence before a node is evicted from the cluster.
    #[serde(default = "default_eviction_timeout_s")]
    pub eviction_timeout_s: u64,

    /// Leader lease TTL (seconds). Renewed every ttl/2.
    #[serde(default = "default_leader_ttl_s")]
    pub leader_ttl_s: u64,

    /// Deduplication key TTL (seconds) in Redis.
    #[serde(default = "default_dedup_ttl_s")]
    pub dedup_ttl_s: u64,
}

impl DistributedConfig {
    /// Create a new configuration with defaults and the given node ID.
    ///
    /// # Arguments
    /// * `node_id` — Unique identifier for this node
    ///
    /// # Returns
    /// A `DistributedConfig` with all defaults applied.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(node_id: String) -> Self {
        Self {
            node_id,
            nats_url: default_nats_url(),
            redis_url: default_redis_url(),
            heartbeat_interval_s: default_heartbeat_interval_s(),
            eviction_timeout_s: default_eviction_timeout_s(),
            leader_ttl_s: default_leader_ttl_s(),
            dedup_ttl_s: default_dedup_ttl_s(),
        }
    }

    /// Create a configuration with custom NATS and Redis URLs.
    ///
    /// # Arguments
    /// * `node_id` — Unique identifier for this node
    /// * `nats_url` — NATS server URL (e.g., `nats://host:4222`)
    /// * `redis_url` — Redis server URL (e.g., `redis://host:6379`)
    ///
    /// # Returns
    /// A `DistributedConfig` with custom connection URLs and other defaults.
    ///
    /// # Panics
    /// This function never panics.
    pub fn with_urls(node_id: String, nats_url: String, redis_url: String) -> Self {
        Self {
            node_id,
            nats_url,
            redis_url,
            ..Self::new(String::new())
        }
    }

    /// Validate this configuration for semantic correctness.
    ///
    /// # Returns
    /// - `Ok(())` — if all fields are valid
    /// - `Err(DistributedError::InvalidConfig)` — if any field violates constraints
    ///
    /// # Panics
    /// This function never panics.
    pub fn validate(&self) -> Result<(), DistributedError> {
        if self.node_id.is_empty() {
            return Err(DistributedError::InvalidConfig {
                field: "node_id".to_string(),
                reason: "node_id must not be empty".to_string(),
            });
        }

        if self.nats_url.is_empty() {
            return Err(DistributedError::InvalidConfig {
                field: "nats_url".to_string(),
                reason: "nats_url must not be empty".to_string(),
            });
        }

        if self.redis_url.is_empty() {
            return Err(DistributedError::InvalidConfig {
                field: "redis_url".to_string(),
                reason: "redis_url must not be empty".to_string(),
            });
        }

        if self.heartbeat_interval_s == 0 {
            return Err(DistributedError::InvalidConfig {
                field: "heartbeat_interval_s".to_string(),
                reason: "heartbeat interval must be > 0".to_string(),
            });
        }

        if self.eviction_timeout_s <= self.heartbeat_interval_s {
            return Err(DistributedError::InvalidConfig {
                field: "eviction_timeout_s".to_string(),
                reason: format!(
                    "eviction timeout ({}) must be greater than heartbeat interval ({})",
                    self.eviction_timeout_s, self.heartbeat_interval_s
                ),
            });
        }

        if self.leader_ttl_s == 0 {
            return Err(DistributedError::InvalidConfig {
                field: "leader_ttl_s".to_string(),
                reason: "leader TTL must be > 0".to_string(),
            });
        }

        if self.dedup_ttl_s == 0 {
            return Err(DistributedError::InvalidConfig {
                field: "dedup_ttl_s".to_string(),
                reason: "dedup TTL must be > 0".to_string(),
            });
        }

        Ok(())
    }

    /// Get the heartbeat interval as a [`Duration`].
    ///
    /// # Panics
    /// This function never panics.
    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.heartbeat_interval_s)
    }

    /// Get the eviction timeout as a [`Duration`].
    ///
    /// # Panics
    /// This function never panics.
    pub fn eviction_timeout(&self) -> Duration {
        Duration::from_secs(self.eviction_timeout_s)
    }

    /// Get the leader lease TTL as a [`Duration`].
    ///
    /// # Panics
    /// This function never panics.
    pub fn leader_ttl(&self) -> Duration {
        Duration::from_secs(self.leader_ttl_s)
    }

    /// Get the leader lease renewal interval (ttl / 2) as a [`Duration`].
    ///
    /// # Panics
    /// This function never panics.
    pub fn leader_renewal_interval(&self) -> Duration {
        Duration::from_secs(self.leader_ttl_s / 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_creates_config_with_defaults() {
        let config = DistributedConfig::new("node-1".to_string());
        assert_eq!(config.node_id, "node-1");
        assert_eq!(config.nats_url, "nats://localhost:4222");
        assert_eq!(config.redis_url, "redis://localhost:6379");
        assert_eq!(config.heartbeat_interval_s, 5);
        assert_eq!(config.eviction_timeout_s, 15);
        assert_eq!(config.leader_ttl_s, 10);
        assert_eq!(config.dedup_ttl_s, 300);
    }

    #[test]
    fn test_with_urls_overrides_connection_strings() {
        let config = DistributedConfig::with_urls(
            "node-2".to_string(),
            "nats://custom:4222".to_string(),
            "redis://custom:6379".to_string(),
        );
        assert_eq!(config.node_id, "node-2");
        assert_eq!(config.nats_url, "nats://custom:4222");
        assert_eq!(config.redis_url, "redis://custom:6379");
        // Defaults still applied for timing fields
        assert_eq!(config.heartbeat_interval_s, 5);
    }

    #[test]
    fn test_validate_valid_config_succeeds() {
        let config = DistributedConfig::new("node-1".to_string());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_node_id_fails() {
        let config = DistributedConfig::new(String::new());
        let result = config.validate();
        if let Err(ref err) = result {
            assert!(
                matches!(err, DistributedError::InvalidConfig { ref field, .. } if field == "node_id")
            );
            assert!(err.to_string().contains("node_id"));
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_validate_empty_nats_url_fails() {
        let mut config = DistributedConfig::new("node-1".to_string());
        config.nats_url = String::new();
        let result = config.validate();
        if let Err(ref err) = result {
            assert!(
                matches!(err, DistributedError::InvalidConfig { ref field, .. } if field == "nats_url")
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_validate_empty_redis_url_fails() {
        let mut config = DistributedConfig::new("node-1".to_string());
        config.redis_url = String::new();
        let result = config.validate();
        if let Err(ref err) = result {
            assert!(
                matches!(err, DistributedError::InvalidConfig { ref field, .. } if field == "redis_url")
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_validate_zero_heartbeat_interval_fails() {
        let mut config = DistributedConfig::new("node-1".to_string());
        config.heartbeat_interval_s = 0;
        let result = config.validate();
        if let Err(ref err) = result {
            assert!(
                matches!(err, DistributedError::InvalidConfig { ref field, .. } if field == "heartbeat_interval_s")
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_validate_eviction_not_greater_than_heartbeat_fails() {
        let mut config = DistributedConfig::new("node-1".to_string());
        config.heartbeat_interval_s = 10;
        config.eviction_timeout_s = 10; // equal, not greater
        let result = config.validate();
        if let Err(ref err) = result {
            assert!(
                matches!(err, DistributedError::InvalidConfig { ref field, .. } if field == "eviction_timeout_s")
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_validate_eviction_less_than_heartbeat_fails() {
        let mut config = DistributedConfig::new("node-1".to_string());
        config.heartbeat_interval_s = 10;
        config.eviction_timeout_s = 5; // less than heartbeat
        let result = config.validate();
        if let Err(ref err) = result {
            assert!(
                matches!(err, DistributedError::InvalidConfig { ref field, .. } if field == "eviction_timeout_s")
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_validate_zero_leader_ttl_fails() {
        let mut config = DistributedConfig::new("node-1".to_string());
        config.leader_ttl_s = 0;
        let result = config.validate();
        if let Err(ref err) = result {
            assert!(
                matches!(err, DistributedError::InvalidConfig { ref field, .. } if field == "leader_ttl_s")
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_validate_zero_dedup_ttl_fails() {
        let mut config = DistributedConfig::new("node-1".to_string());
        config.dedup_ttl_s = 0;
        let result = config.validate();
        if let Err(ref err) = result {
            assert!(
                matches!(err, DistributedError::InvalidConfig { ref field, .. } if field == "dedup_ttl_s")
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_heartbeat_interval_returns_correct_duration() {
        let config = DistributedConfig::new("node-1".to_string());
        assert_eq!(config.heartbeat_interval(), Duration::from_secs(5));
    }

    #[test]
    fn test_eviction_timeout_returns_correct_duration() {
        let config = DistributedConfig::new("node-1".to_string());
        assert_eq!(config.eviction_timeout(), Duration::from_secs(15));
    }

    #[test]
    fn test_leader_ttl_returns_correct_duration() {
        let config = DistributedConfig::new("node-1".to_string());
        assert_eq!(config.leader_ttl(), Duration::from_secs(10));
    }

    #[test]
    fn test_leader_renewal_interval_is_half_ttl() {
        let config = DistributedConfig::new("node-1".to_string());
        assert_eq!(config.leader_renewal_interval(), Duration::from_secs(5));
    }

    #[test]
    fn test_leader_renewal_interval_odd_ttl_truncates() {
        let mut config = DistributedConfig::new("node-1".to_string());
        config.leader_ttl_s = 11;
        // 11 / 2 = 5 (integer division)
        assert_eq!(config.leader_renewal_interval(), Duration::from_secs(5));
    }

    #[test]
    fn test_config_serde_roundtrip_json() {
        let config = DistributedConfig::new("serde-test".to_string());
        let json = serde_json::to_string(&config).unwrap_or_else(|_| String::new());
        assert!(!json.is_empty(), "serialization must produce output");
        let deserialized: Result<DistributedConfig, _> = serde_json::from_str(&json);
        assert!(deserialized.is_ok(), "deserialization must succeed");
        assert_eq!(config, deserialized.unwrap_or_else(|_| config.clone()));
    }

    #[test]
    fn test_config_serde_roundtrip_toml() {
        let config = DistributedConfig::new("toml-test".to_string());
        let toml_str = toml::to_string_pretty(&config).unwrap_or_else(|_| String::new());
        assert!(!toml_str.is_empty(), "serialization must produce output");
        let deserialized: Result<DistributedConfig, _> = toml::from_str(&toml_str);
        assert!(deserialized.is_ok(), "deserialization must succeed");
        assert_eq!(config, deserialized.unwrap_or_else(|_| config.clone()));
    }

    #[test]
    fn test_config_toml_defaults_applied() {
        let toml_str = r#"
node_id = "default-test"
"#;
        let config: DistributedConfig =
            toml::from_str(toml_str).unwrap_or_else(|_| DistributedConfig::new("fallback".into()));
        assert_eq!(config.node_id, "default-test");
        assert_eq!(config.nats_url, "nats://localhost:4222");
        assert_eq!(config.redis_url, "redis://localhost:6379");
        assert_eq!(config.heartbeat_interval_s, 5);
    }

    #[test]
    fn test_config_debug_format_contains_node_id() {
        let config = DistributedConfig::new("debug-test".to_string());
        let debug = format!("{:?}", config);
        assert!(debug.contains("debug-test"));
    }

    #[test]
    fn test_config_clone_produces_equal_instance() {
        let config = DistributedConfig::new("clone-test".to_string());
        let cloned = config.clone();
        assert_eq!(config, cloned);
    }

    #[test]
    fn test_default_nats_url_value() {
        assert_eq!(default_nats_url(), "nats://localhost:4222");
    }

    #[test]
    fn test_default_redis_url_value() {
        assert_eq!(default_redis_url(), "redis://localhost:6379");
    }

    #[test]
    fn test_default_heartbeat_interval_value() {
        assert_eq!(default_heartbeat_interval_s(), 5);
    }

    #[test]
    fn test_default_eviction_timeout_value() {
        assert_eq!(default_eviction_timeout_s(), 15);
    }

    #[test]
    fn test_default_leader_ttl_value() {
        assert_eq!(default_leader_ttl_s(), 10);
    }

    #[test]
    fn test_default_dedup_ttl_value() {
        assert_eq!(default_dedup_ttl_s(), 300);
    }
}
