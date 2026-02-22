//! # Stage: Distributed Coordination
//!
//! ## Responsibility
//! Provide optional Redis-backed coordination for multi-instance HelixRouter
//! deployments. Handles cross-instance deduplication, leader election, and
//! cluster-wide statistics aggregation.
//!
//! ## Guarantees
//! - Thread-safe: all operations are safe under concurrent access.
//! - Non-panicking: no production code path can panic.
//! - Bounded: dedup cache is bounded by the number of unique job hashes.
//! - Serde-compatible: `DistributedConfig` round-trips through JSON.
//!
//! ## NOT Responsible For
//! - Actual Redis I/O (gated behind `redis` optional dependency).
//! - Routing decisions (see `router` module).
//! - Job execution (see `strategies` module).

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::helix::types::JobKind;

// ── Error ────────────────────────────────────────────────────────────────

/// Errors that can occur during distributed coordination operations.
///
/// Each variant describes a specific failure mode in the coordination layer,
/// enabling callers to handle failures with precision.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, thiserror::Error)]
pub enum DistributedError {
    /// The coordinator failed to establish a connection to Redis.
    #[error("Redis connection failed: {0}")]
    ConnectionFailed(String),

    /// A Redis command returned an error during execution.
    #[error("Redis operation failed: {0}")]
    OperationFailed(String),

    /// Serialization or deserialization of coordinator data failed.
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Another instance won the leader election before this one.
    #[error("Leader election lost to instance {0}")]
    LeaderElectionLost(String),

    /// The coordinator has not yet established a connection.
    #[error("Coordinator not connected")]
    NotConnected,
}

// ── Config ───────────────────────────────────────────────────────────────

/// Configuration for the distributed coordination layer.
///
/// Controls Redis connectivity, leader election timing, statistics
/// publishing cadence, and deduplication cache lifetime.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributedConfig {
    /// Redis connection URL (e.g. `redis://127.0.0.1:6379`).
    pub redis_url: String,
    /// Unique identifier for this HelixRouter instance.
    pub instance_id: String,
    /// Time-to-live in seconds for the leader lock key.
    pub leader_ttl_secs: u64,
    /// How often (in seconds) this instance publishes its stats to Redis.
    pub stats_publish_interval_secs: u64,
    /// Time-to-live in seconds for deduplication entries.
    pub dedup_ttl_secs: u64,
}

impl Default for DistributedConfig {
    /// Returns a default configuration suitable for local development.
    ///
    /// # Returns
    ///
    /// A `DistributedConfig` with sensible defaults:
    /// - Redis on localhost port 6379
    /// - Instance ID `helix-0`
    /// - 30-second leader TTL
    /// - 5-second stats publish interval
    /// - 300-second dedup TTL
    ///
    /// # Panics
    ///
    /// This function never panics.
    fn default() -> Self {
        Self {
            redis_url: "redis://127.0.0.1:6379".to_string(),
            instance_id: "helix-0".to_string(),
            leader_ttl_secs: 30,
            stats_publish_interval_secs: 5,
            dedup_ttl_secs: 300,
        }
    }
}

// ── Coordinator ──────────────────────────────────────────────────────────

/// Mock/no-op distributed coordinator for multi-instance HelixRouter deployments.
///
/// This implementation uses in-memory data structures (`HashSet`, `Vec`) to
/// simulate the coordination interface. The real Redis-backed implementation
/// is gated behind the `redis` optional dependency and the
/// `helix-distributed` feature flag.
///
/// # Panics
///
/// This type never panics.
pub struct DistributedCoordinator {
    /// Configuration for this coordinator instance.
    config: DistributedConfig,
    /// Whether this coordinator has been "connected" (simulated).
    connected: bool,
    /// Whether this instance currently holds the leader lock.
    is_leader_flag: bool,
    /// In-memory deduplication cache keyed by job hash.
    dedup_cache: HashSet<u64>,
    /// Collected cluster statistics from all instances.
    cluster_stats: Vec<String>,
}

impl DistributedCoordinator {
    /// Creates a new coordinator with the given configuration.
    ///
    /// The coordinator starts in a disconnected state. Call [`connect`](Self::connect)
    /// before performing any coordination operations.
    ///
    /// # Arguments
    ///
    /// * `config` - The distributed configuration specifying Redis URL,
    ///   instance identity, and timing parameters.
    ///
    /// # Returns
    ///
    /// A new `DistributedCoordinator` in disconnected state.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(config: DistributedConfig) -> Self {
        Self {
            config,
            connected: false,
            is_leader_flag: false,
            dedup_cache: HashSet::new(),
            cluster_stats: Vec::new(),
        }
    }

    /// Establishes a connection to the coordination backend.
    ///
    /// In this mock implementation, this simply sets the internal connected
    /// flag to `true`. The real implementation would open a Redis connection
    /// using the URL from [`DistributedConfig::redis_url`].
    ///
    /// # Returns
    ///
    /// - `Ok(())` on successful connection.
    /// - `Err(DistributedError::ConnectionFailed)` if the connection cannot
    ///   be established (not triggered in the mock).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn connect(&mut self) -> Result<(), DistributedError> {
        self.connected = true;
        Ok(())
    }

    /// Checks whether a job with the given hash has already been processed.
    ///
    /// Used for cross-instance deduplication: before executing a job, the
    /// router checks whether another instance has already completed it.
    ///
    /// # Arguments
    ///
    /// * `job_hash` - A deterministic hash identifying the job, produced by
    ///   [`job_hash`].
    ///
    /// # Returns
    ///
    /// - `Ok(true)` if the job hash is present in the dedup cache.
    /// - `Ok(false)` if the job hash is not known.
    /// - `Err(DistributedError::NotConnected)` if the coordinator is not connected.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn check_dedup(&self, job_hash: u64) -> Result<bool, DistributedError> {
        if !self.connected {
            return Err(DistributedError::NotConnected);
        }
        Ok(self.dedup_cache.contains(&job_hash))
    }

    /// Registers a job hash as processed in the dedup cache.
    ///
    /// After a job completes, calling this method ensures that other instances
    /// (and future runs on this instance) will see it as already processed.
    ///
    /// # Arguments
    ///
    /// * `job_hash` - A deterministic hash identifying the job, produced by
    ///   [`job_hash`].
    ///
    /// # Returns
    ///
    /// - `Ok(())` on successful registration.
    /// - `Err(DistributedError::NotConnected)` if the coordinator is not connected.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn register_dedup(&mut self, job_hash: u64) -> Result<(), DistributedError> {
        if !self.connected {
            return Err(DistributedError::NotConnected);
        }
        self.dedup_cache.insert(job_hash);
        Ok(())
    }

    /// Attempts to acquire the leader lock for this instance.
    ///
    /// In a real deployment this would use Redis `SETNX` with a TTL to
    /// atomically claim leadership. In this mock, the first call always
    /// succeeds.
    ///
    /// # Returns
    ///
    /// - `Ok(true)` if this instance acquired leadership.
    /// - `Ok(false)` if another instance already holds the lock.
    /// - `Err(DistributedError::NotConnected)` if the coordinator is not connected.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn try_become_leader(&mut self) -> Result<bool, DistributedError> {
        if !self.connected {
            return Err(DistributedError::NotConnected);
        }
        if self.is_leader_flag {
            // Already the leader.
            return Ok(true);
        }
        // Mock: first caller always wins.
        self.is_leader_flag = true;
        Ok(true)
    }

    /// Returns whether this instance currently holds the leader lock.
    ///
    /// # Returns
    ///
    /// `true` if this instance is the leader, `false` otherwise.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn is_leader(&self) -> bool {
        self.is_leader_flag
    }

    /// Publishes this instance's statistics to the cluster stats channel.
    ///
    /// In a real deployment this would `PUBLISH` to a Redis pub/sub channel
    /// or write to a shared key. In this mock, the stats are appended to an
    /// in-memory vector.
    ///
    /// # Arguments
    ///
    /// * `stats_json` - A JSON-encoded string containing the instance's
    ///   current statistics snapshot.
    ///
    /// # Returns
    ///
    /// - `Ok(())` on successful publication.
    /// - `Err(DistributedError::NotConnected)` if the coordinator is not connected.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn publish_stats(&mut self, stats_json: &str) -> Result<(), DistributedError> {
        if !self.connected {
            return Err(DistributedError::NotConnected);
        }
        self.cluster_stats.push(stats_json.to_string());
        Ok(())
    }

    /// Reads all cluster statistics that have been published.
    ///
    /// In a real deployment this would subscribe to a Redis pub/sub channel
    /// or scan a shared keyspace. In this mock, it returns a clone of the
    /// in-memory stats vector.
    ///
    /// # Returns
    ///
    /// - `Ok(Vec<String>)` containing all published stats entries.
    /// - `Err(DistributedError::NotConnected)` if the coordinator is not connected.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub async fn read_cluster_stats(&self) -> Result<Vec<String>, DistributedError> {
        if !self.connected {
            return Err(DistributedError::NotConnected);
        }
        Ok(self.cluster_stats.clone())
    }

    /// Returns whether the coordinator is currently connected.
    ///
    /// # Returns
    ///
    /// `true` if [`connect`](Self::connect) has been called successfully,
    /// `false` otherwise.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Returns a reference to the coordinator's configuration.
    ///
    /// # Returns
    ///
    /// A reference to the [`DistributedConfig`] used to create this coordinator.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn config(&self) -> &DistributedConfig {
        &self.config
    }
}

// ── Job Hash ─────────────────────────────────────────────────────────────

/// Computes a deterministic hash of a job's kind and cost for deduplication.
///
/// The hash is stable within a single process run and is suitable for
/// checking whether two jobs represent the same logical work unit.
///
/// # Arguments
///
/// * `kind` - The [`JobKind`] variant of the job.
/// * `cost` - The estimated compute cost of the job.
///
/// # Returns
///
/// A `u64` hash value. Equal inputs always produce equal outputs within
/// the same process.
///
/// # Panics
///
/// This function never panics.
pub fn job_hash(kind: &JobKind, cost: u64) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut hasher);
    cost.hash(&mut hasher);
    hasher.finish()
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- DistributedConfig ---------------------------------------------------

    #[test]
    fn test_distributed_config_default() {
        let cfg = DistributedConfig::default();
        assert_eq!(cfg.redis_url, "redis://127.0.0.1:6379");
        assert_eq!(cfg.instance_id, "helix-0");
        assert_eq!(cfg.leader_ttl_secs, 30);
        assert_eq!(cfg.stats_publish_interval_secs, 5);
        assert_eq!(cfg.dedup_ttl_secs, 300);
    }

    #[test]
    fn test_distributed_config_serde_roundtrip() {
        let cfg = DistributedConfig {
            redis_url: "redis://10.0.0.1:6380".to_string(),
            instance_id: "helix-42".to_string(),
            leader_ttl_secs: 60,
            stats_publish_interval_secs: 10,
            dedup_ttl_secs: 600,
        };
        let json = serde_json::to_string(&cfg)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test ser: {e}")));
        let back: DistributedConfig = serde_json::from_str(&json)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test deser: {e}")));
        assert_eq!(back.redis_url, cfg.redis_url);
        assert_eq!(back.instance_id, cfg.instance_id);
        assert_eq!(back.leader_ttl_secs, cfg.leader_ttl_secs);
        assert_eq!(
            back.stats_publish_interval_secs,
            cfg.stats_publish_interval_secs
        );
        assert_eq!(back.dedup_ttl_secs, cfg.dedup_ttl_secs);
    }

    // -- Coordinator lifecycle -----------------------------------------------

    #[test]
    fn test_coordinator_new_not_connected() {
        let coord = DistributedCoordinator::new(DistributedConfig::default());
        assert!(!coord.is_connected());
    }

    #[tokio::test]
    async fn test_coordinator_connect_succeeds() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        let result = coord.connect().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_coordinator_connect_sets_connected() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        assert!(!coord.is_connected());
        coord
            .connect()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test connect: {e}")));
        assert!(coord.is_connected());
    }

    #[tokio::test]
    async fn test_double_connect_succeeds() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        coord
            .connect()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test connect 1: {e}")));
        let result = coord.connect().await;
        assert!(result.is_ok());
        assert!(coord.is_connected());
    }

    // -- Deduplication -------------------------------------------------------

    #[tokio::test]
    async fn test_dedup_check_unknown_returns_false() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        coord
            .connect()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test connect: {e}")));
        let found = coord
            .check_dedup(12345)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test check: {e}")));
        assert!(!found);
    }

    #[tokio::test]
    async fn test_dedup_register_then_check_returns_true() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        coord
            .connect()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test connect: {e}")));
        coord
            .register_dedup(42)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test register: {e}")));
        let found = coord
            .check_dedup(42)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test check: {e}")));
        assert!(found);
    }

    #[tokio::test]
    async fn test_dedup_check_not_connected_returns_error() {
        let coord = DistributedCoordinator::new(DistributedConfig::default());
        let result = coord.check_dedup(1).await;
        assert!(result.is_err());
        let err = result
            .err()
            .unwrap_or_else(|| std::panic::panic_any("expected error"));
        assert!(
            format!("{err}").contains("not connected"),
            "error message should mention 'not connected', got: {err}"
        );
    }

    // -- Leader election -----------------------------------------------------

    #[tokio::test]
    async fn test_coordinator_is_not_leader_by_default() {
        let coord = DistributedCoordinator::new(DistributedConfig::default());
        assert!(!coord.is_leader().await);
    }

    #[tokio::test]
    async fn test_leader_election_first_wins() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        coord
            .connect()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test connect: {e}")));
        let won = coord
            .try_become_leader()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test election: {e}")));
        assert!(won);
        assert!(coord.is_leader().await);
    }

    #[tokio::test]
    async fn test_leader_not_connected_returns_error() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        let result = coord.try_become_leader().await;
        assert!(result.is_err());
        let err = result
            .err()
            .unwrap_or_else(|| std::panic::panic_any("expected error"));
        assert!(
            format!("{err}").contains("not connected"),
            "error message should mention 'not connected', got: {err}"
        );
    }

    // -- Stats ---------------------------------------------------------------

    #[tokio::test]
    async fn test_publish_stats_stores_data() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        coord
            .connect()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test connect: {e}")));
        coord
            .publish_stats(r#"{"jobs": 10}"#)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test publish: {e}")));
        let stats = coord
            .read_cluster_stats()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test read: {e}")));
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0], r#"{"jobs": 10}"#);
    }

    #[tokio::test]
    async fn test_read_cluster_stats_returns_published() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        coord
            .connect()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test connect: {e}")));
        coord
            .publish_stats(r#"{"a": 1}"#)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test publish 1: {e}")));
        coord
            .publish_stats(r#"{"b": 2}"#)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test publish 2: {e}")));
        coord
            .publish_stats(r#"{"c": 3}"#)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test publish 3: {e}")));
        let stats = coord
            .read_cluster_stats()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test read: {e}")));
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0], r#"{"a": 1}"#);
        assert_eq!(stats[1], r#"{"b": 2}"#);
        assert_eq!(stats[2], r#"{"c": 3}"#);
    }

    #[tokio::test]
    async fn test_publish_stats_not_connected_returns_error() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        let result = coord.publish_stats("{}").await;
        assert!(result.is_err());
        let err = result
            .err()
            .unwrap_or_else(|| std::panic::panic_any("expected error"));
        assert!(
            format!("{err}").contains("not connected"),
            "error message should mention 'not connected', got: {err}"
        );
    }

    // -- job_hash ------------------------------------------------------------

    #[test]
    fn test_job_hash_deterministic() {
        let h1 = job_hash(&JobKind::Hash, 100);
        let h2 = job_hash(&JobKind::Hash, 100);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_job_hash_different_inputs_differ() {
        let h1 = job_hash(&JobKind::Hash, 100);
        let h2 = job_hash(&JobKind::Prime, 100);
        let h3 = job_hash(&JobKind::Hash, 200);
        assert_ne!(h1, h2, "different kinds should produce different hashes");
        assert_ne!(h1, h3, "different costs should produce different hashes");
    }

    // -- Additional coverage -------------------------------------------------

    #[tokio::test]
    async fn test_register_dedup_not_connected_returns_error() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        let result = coord.register_dedup(99).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_cluster_stats_not_connected_returns_error() {
        let coord = DistributedCoordinator::new(DistributedConfig::default());
        let result = coord.read_cluster_stats().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_leader_re_election_returns_true() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        coord
            .connect()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test connect: {e}")));
        let first = coord
            .try_become_leader()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test election 1: {e}")));
        assert!(first);
        // Calling again while already leader should still return true.
        let second = coord
            .try_become_leader()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test election 2: {e}")));
        assert!(second);
    }

    #[test]
    fn test_coordinator_config_accessor() {
        let cfg = DistributedConfig {
            redis_url: "redis://custom:1234".to_string(),
            instance_id: "test-node".to_string(),
            leader_ttl_secs: 15,
            stats_publish_interval_secs: 2,
            dedup_ttl_secs: 60,
        };
        let coord = DistributedCoordinator::new(cfg.clone());
        assert_eq!(coord.config().redis_url, "redis://custom:1234");
        assert_eq!(coord.config().instance_id, "test-node");
    }

    #[test]
    fn test_distributed_error_display_messages() {
        let e1 = DistributedError::ConnectionFailed("timeout".to_string());
        assert!(format!("{e1}").contains("timeout"));

        let e2 = DistributedError::OperationFailed("SET failed".to_string());
        assert!(format!("{e2}").contains("SET failed"));

        let e3 = DistributedError::SerializationError("invalid json".to_string());
        assert!(format!("{e3}").contains("invalid json"));

        let e4 = DistributedError::LeaderElectionLost("helix-3".to_string());
        assert!(format!("{e4}").contains("helix-3"));

        let e5 = DistributedError::NotConnected;
        assert!(format!("{e5}").contains("not connected"));
    }

    #[tokio::test]
    async fn test_dedup_multiple_entries_independent() {
        let mut coord = DistributedCoordinator::new(DistributedConfig::default());
        coord
            .connect()
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test connect: {e}")));
        coord
            .register_dedup(1)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test register 1: {e}")));
        coord
            .register_dedup(2)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test register 2: {e}")));

        assert!(coord
            .check_dedup(1)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test check 1: {e}"))));
        assert!(coord
            .check_dedup(2)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test check 2: {e}"))));
        assert!(!coord
            .check_dedup(3)
            .await
            .unwrap_or_else(|e| std::panic::panic_any(format!("test check 3: {e}"))));
    }

    #[test]
    fn test_job_hash_all_kinds() {
        let kinds = [JobKind::Hash, JobKind::Prime, JobKind::MonteCarlo];
        let hashes: Vec<u64> = kinds.iter().map(|k| job_hash(k, 50)).collect();
        // All three should be distinct.
        assert_ne!(hashes[0], hashes[1]);
        assert_ne!(hashes[1], hashes[2]);
        assert_ne!(hashes[0], hashes[2]);
    }
}
