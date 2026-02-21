//! # Stage: Distributed Clustering
//!
//! ## Responsibility
//! Enable the orchestrator to scale across multiple machines via:
//! - **NATS**: pub/sub messaging for inter-node request routing and heartbeats
//! - **Redis Dedup**: cross-node deduplication using `SET NX EX` so only one
//!   node processes each unique request
//! - **Leader Election**: Redis-based SETNX with TTL lease renewal
//! - **Cluster Manager**: node registry, heartbeat tracking, stale-node eviction,
//!   and load-based routing to the least-loaded node
//!
//! ## Guarantees
//! - At-most-one processing: Redis NX ensures cross-node dedup atomicity
//! - At-most-one leader: SETNX ensures single leader at any time
//! - Self-healing: heartbeat timeout evicts dead nodes, lease expiry triggers re-election
//! - Non-blocking: all operations are async and never block the Tokio executor
//! - Feature-gated: entire module is behind `#[cfg(feature = "distributed")]`
//!
//! ## NOT Responsible For
//! - In-memory deduplication (see: `enhanced::dedup`)
//! - In-memory caching (see: `enhanced::cache`)
//! - Pipeline stage logic (see: `stages`)
//! - Worker management (see: `worker`)

pub mod cluster;
pub mod config;
pub mod leader;
pub mod nats;
pub mod redis_dedup;

// Re-exports for convenience
pub use cluster::{ClusterManager, Heartbeat, NodeInfo};
pub use config::DistributedConfig;
pub use leader::{LeaderElection, LeaderRole};
pub use nats::{NatsBus, NatsMessage};
pub use redis_dedup::{RedisDedup, RedisDeduplicationResult};

use thiserror::Error;

/// Errors specific to the distributed clustering module.
///
/// Every error variant maps to a specific failure mode in distributed
/// operations. All variants implement `std::error::Error` via [`thiserror`].
///
/// # Panics
/// This type never panics.
#[derive(Error, Debug)]
pub enum DistributedError {
    /// Redis connection or URL parsing failed.
    #[error("Redis connection error: {0}")]
    RedisConnection(String),

    /// A Redis command failed during execution.
    #[error("Redis operation error: {0}")]
    RedisOperation(String),

    /// NATS connection failed.
    #[error("NATS connection error: {0}")]
    NatsConnection(String),

    /// NATS publish failed.
    #[error("NATS publish error: {0}")]
    NatsPublish(String),

    /// NATS subscribe failed.
    #[error("NATS subscribe error: {0}")]
    NatsSubscribe(String),

    /// NATS request timed out.
    #[error("NATS request timeout: {0}")]
    NatsTimeout(String),

    /// A node was not found in the cluster topology.
    #[error("Node not found: {0}")]
    NodeNotFound(String),

    /// Configuration validation failed.
    #[error("Invalid config field '{field}': {reason}")]
    InvalidConfig {
        /// The field that failed validation.
        field: String,
        /// Why the field is invalid.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distributed_error_redis_connection_display() {
        let err = DistributedError::RedisConnection("timeout".to_string());
        assert_eq!(err.to_string(), "Redis connection error: timeout");
    }

    #[test]
    fn test_distributed_error_redis_operation_display() {
        let err = DistributedError::RedisOperation("SET failed".to_string());
        assert_eq!(err.to_string(), "Redis operation error: SET failed");
    }

    #[test]
    fn test_distributed_error_nats_connection_display() {
        let err = DistributedError::NatsConnection("refused".to_string());
        assert_eq!(err.to_string(), "NATS connection error: refused");
    }

    #[test]
    fn test_distributed_error_nats_publish_display() {
        let err = DistributedError::NatsPublish("timeout".to_string());
        assert_eq!(err.to_string(), "NATS publish error: timeout");
    }

    #[test]
    fn test_distributed_error_nats_subscribe_display() {
        let err = DistributedError::NatsSubscribe("bad subject".to_string());
        assert_eq!(err.to_string(), "NATS subscribe error: bad subject");
    }

    #[test]
    fn test_distributed_error_nats_timeout_display() {
        let err = DistributedError::NatsTimeout("5s".to_string());
        assert_eq!(err.to_string(), "NATS request timeout: 5s");
    }

    #[test]
    fn test_distributed_error_node_not_found_display() {
        let err = DistributedError::NodeNotFound("n1".to_string());
        assert_eq!(err.to_string(), "Node not found: n1");
    }

    #[test]
    fn test_distributed_error_invalid_config_display() {
        let err = DistributedError::InvalidConfig {
            field: "node_id".to_string(),
            reason: "must not be empty".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Invalid config field 'node_id': must not be empty"
        );
    }

    #[test]
    fn test_distributed_error_debug_format() {
        let err = DistributedError::RedisConnection("test".to_string());
        let debug = format!("{:?}", err);
        assert!(debug.contains("RedisConnection"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn test_all_error_variants_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // DistributedError must be Send + Sync for use across async tasks
        assert_send_sync::<DistributedError>();
    }

    #[test]
    fn test_all_error_variants_implement_std_error() {
        fn assert_std_error<T: std::error::Error>() {}
        assert_std_error::<DistributedError>();
    }

    // Re-export availability tests
    #[test]
    fn test_reexports_are_accessible() {
        // These compile-time checks verify that re-exports work
        let _config = DistributedConfig::new("test".to_string());
        let _result = RedisDeduplicationResult::Claimed;
        let _role = LeaderRole::Leader;
        let _hb = Heartbeat {
            node_id: "n1".to_string(),
            address: "addr".to_string(),
            load: 0,
        };
        let _msg = NatsMessage {
            source_node: "n1".to_string(),
            msg_type: "test".to_string(),
            payload: "{}".to_string(),
        };
    }
}
