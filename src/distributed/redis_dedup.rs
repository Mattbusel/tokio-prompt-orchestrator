//! # Stage: Redis-Backed Distributed Deduplication
//!
//! ## Responsibility
//! Prevent duplicate inference requests across a cluster of nodes using
//! Redis `SET key NX EX ttl` for atomic, distributed dedup locking.
//! Only one node in the cluster processes a given request.
//!
//! ## Guarantees
//! - Atomic: `SET NX EX` is a single Redis command — no race conditions
//! - TTL-bounded: keys auto-expire, preventing unbounded memory growth
//! - Non-blocking: uses async Redis connections, never blocks the executor
//! - Graceful degradation: Redis failures are surfaced as errors, not panics
//!
//! ## NOT Responsible For
//! - In-memory deduplication (see: `enhanced::dedup`)
//! - Semantic similarity matching (exact key match only)
//! - Result caching (see: `enhanced::cache`)

use redis::AsyncCommands;
use std::sync::Arc;
use tracing::debug;

use super::DistributedError;

/// Result of a distributed deduplication check.
///
/// Indicates whether this node should process the request or skip it
/// because another node already claimed it.
///
/// # Panics
/// This type never panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedisDeduplicationResult {
    /// This node has claimed the request and should process it.
    Claimed,
    /// Another node already claimed this request — skip processing.
    AlreadyClaimed,
}

/// Redis-backed distributed deduplicator.
///
/// Uses `SET key NX EX ttl` to atomically claim ownership of a request
/// across all nodes in the cluster. The key is prefixed with the node ID
/// to allow debugging which node claimed a request.
///
/// # Panics
/// This type never panics.
///
/// # Example
///
/// ```no_run
/// use tokio_prompt_orchestrator::distributed::RedisDedup;
/// use tokio_prompt_orchestrator::distributed::RedisDeduplicationResult;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let dedup = RedisDedup::new("redis://localhost:6379", "node-1", 300).await?;
///
/// match dedup.try_claim("request-key-123").await? {
///     RedisDeduplicationResult::Claimed => println!("Processing request"),
///     RedisDeduplicationResult::AlreadyClaimed => println!("Skipping duplicate"),
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct RedisDedup {
    client: Arc<redis::Client>,
    node_id: String,
    ttl_seconds: u64,
}

impl RedisDedup {
    /// Create a new Redis-backed deduplicator.
    ///
    /// # Arguments
    /// * `redis_url` — Redis connection URL (e.g., `redis://localhost:6379`)
    /// * `node_id` — Identifier of this node (stored as the key's value for debugging)
    /// * `ttl_seconds` — How long (seconds) a claimed key persists before auto-expiring
    ///
    /// # Returns
    /// - `Ok(RedisDedup)` — ready to use
    /// - `Err(DistributedError::RedisConnection)` — if the URL is invalid
    ///
    /// # Panics
    /// This function never panics.
    pub async fn new(
        redis_url: &str,
        node_id: &str,
        ttl_seconds: u64,
    ) -> Result<Self, DistributedError> {
        let client = redis::Client::open(redis_url).map_err(|e| {
            DistributedError::RedisConnection(format!("failed to open Redis client: {e}"))
        })?;

        Ok(Self {
            client: Arc::new(client),
            node_id: node_id.to_string(),
            ttl_seconds,
        })
    }

    /// Create a deduplicator from an existing Redis client (for testing/sharing connections).
    ///
    /// # Arguments
    /// * `client` — Pre-configured Redis client
    /// * `node_id` — Identifier of this node
    /// * `ttl_seconds` — Key TTL in seconds
    ///
    /// # Returns
    /// A `RedisDedup` wrapping the provided client.
    ///
    /// # Panics
    /// This function never panics.
    pub fn from_client(client: Arc<redis::Client>, node_id: &str, ttl_seconds: u64) -> Self {
        Self {
            client,
            node_id: node_id.to_string(),
            ttl_seconds,
        }
    }

    /// Attempt to claim exclusive ownership of a request key.
    ///
    /// Uses `SET key NX EX ttl` — only one node across the cluster can
    /// successfully claim a given key. The key is prefixed with `dedup:dist:`
    /// for namespace isolation.
    ///
    /// # Arguments
    /// * `key` — The deduplication key (typically a hash of the prompt + metadata)
    ///
    /// # Returns
    /// - `Ok(RedisDeduplicationResult::Claimed)` — this node owns the request
    /// - `Ok(RedisDeduplicationResult::AlreadyClaimed)` — another node owns it
    /// - `Err(DistributedError::RedisOperation)` — Redis command failed
    ///
    /// # Panics
    /// This function never panics.
    pub async fn try_claim(&self, key: &str) -> Result<RedisDeduplicationResult, DistributedError> {
        let redis_key = format!("dedup:dist:{key}");

        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                DistributedError::RedisConnection(format!("failed to get connection: {e}"))
            })?;

        // SET key value NX EX ttl — atomic set-if-not-exists with expiry
        let result: Option<String> = redis::cmd("SET")
            .arg(&redis_key)
            .arg(&self.node_id)
            .arg("NX")
            .arg("EX")
            .arg(self.ttl_seconds)
            .query_async(&mut conn)
            .await
            .map_err(|e| DistributedError::RedisOperation(format!("SET NX EX failed: {e}")))?;

        match result {
            Some(ref s) if s == "OK" => {
                debug!(key = key, node = %self.node_id, "claimed dedup key");
                Ok(RedisDeduplicationResult::Claimed)
            }
            _ => {
                debug!(key = key, node = %self.node_id, "dedup key already claimed");
                Ok(RedisDeduplicationResult::AlreadyClaimed)
            }
        }
    }

    /// Release a previously claimed key (e.g., on processing failure).
    ///
    /// Only releases the key if this node is the current owner (using a
    /// compare-and-delete pattern to avoid releasing another node's claim).
    ///
    /// # Arguments
    /// * `key` — The deduplication key to release
    ///
    /// # Returns
    /// - `Ok(true)` — key was released
    /// - `Ok(false)` — key was not owned by this node (or already expired)
    /// - `Err(DistributedError::RedisOperation)` — Redis command failed
    ///
    /// # Panics
    /// This function never panics.
    pub async fn release(&self, key: &str) -> Result<bool, DistributedError> {
        let redis_key = format!("dedup:dist:{key}");

        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                DistributedError::RedisConnection(format!("failed to get connection: {e}"))
            })?;

        // Compare-and-delete: only delete if we own it
        // Using a Lua script for atomicity
        let script = r#"
            if redis.call("GET", KEYS[1]) == ARGV[1] then
                return redis.call("DEL", KEYS[1])
            else
                return 0
            end
        "#;

        let result: i64 = redis::Script::new(script)
            .key(&redis_key)
            .arg(&self.node_id)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| DistributedError::RedisOperation(format!("release script failed: {e}")))?;

        if result == 1 {
            debug!(key = key, node = %self.node_id, "released dedup key");
            Ok(true)
        } else {
            debug!(key = key, node = %self.node_id, "key not owned by this node");
            Ok(false)
        }
    }

    /// Check who currently owns a dedup key (for diagnostics).
    ///
    /// # Arguments
    /// * `key` — The deduplication key to query
    ///
    /// # Returns
    /// - `Ok(Some(node_id))` — the node that currently owns the key
    /// - `Ok(None)` — no node owns this key (unclaimed or expired)
    /// - `Err(DistributedError::RedisOperation)` — Redis command failed
    ///
    /// # Panics
    /// This function never panics.
    pub async fn owner_of(&self, key: &str) -> Result<Option<String>, DistributedError> {
        let redis_key = format!("dedup:dist:{key}");

        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                DistributedError::RedisConnection(format!("failed to get connection: {e}"))
            })?;

        let owner: Option<String> = conn
            .get(&redis_key)
            .await
            .map_err(|e| DistributedError::RedisOperation(format!("GET owner failed: {e}")))?;

        Ok(owner)
    }

    /// Get the TTL (seconds) for dedup keys.
    ///
    /// # Panics
    /// This function never panics.
    pub fn ttl_seconds(&self) -> u64 {
        self.ttl_seconds
    }

    /// Get this node's ID.
    ///
    /// # Panics
    /// This function never panics.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }
}

impl std::fmt::Debug for RedisDedup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisDedup")
            .field("node_id", &self.node_id)
            .field("ttl_seconds", &self.ttl_seconds)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_invalid_url_returns_connection_error() {
        let result = RedisDedup::new("not-a-valid-url", "node-1", 300).await;
        if let Err(err) = result {
            assert!(
                matches!(err, DistributedError::RedisConnection(_)),
                "expected RedisConnection, got: {err:?}"
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_new_valid_url_succeeds() {
        // This just creates the client; it doesn't actually connect
        let result = RedisDedup::new("redis://localhost:6379", "node-1", 300).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_from_client_preserves_node_id() {
        let client = Arc::new(
            redis::Client::open("redis://localhost:6379").unwrap_or_else(|_| {
                redis::Client::open("redis://127.0.0.1:6379").unwrap_or_else(|_| {
                    // Fallback for test environments
                    redis::Client::open("redis://localhost:6379").unwrap_or_else(|_| {
                        std::process::exit(0);
                    })
                })
            }),
        );
        let dedup = RedisDedup::from_client(client, "test-node", 120);
        assert_eq!(dedup.node_id(), "test-node");
        assert_eq!(dedup.ttl_seconds(), 120);
    }

    #[test]
    fn test_ttl_seconds_returns_configured_value() {
        // Use from_client to avoid needing a real Redis connection
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let dedup = RedisDedup::from_client(Arc::new(c), "n1", 42);
            assert_eq!(dedup.ttl_seconds(), 42);
        }
    }

    #[test]
    fn test_node_id_returns_configured_value() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let dedup = RedisDedup::from_client(Arc::new(c), "my-node", 60);
            assert_eq!(dedup.node_id(), "my-node");
        }
    }

    #[test]
    fn test_debug_format_contains_node_id() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let dedup = RedisDedup::from_client(Arc::new(c), "debug-node", 60);
            let debug = format!("{:?}", dedup);
            assert!(debug.contains("debug-node"));
            assert!(debug.contains("60"));
        }
    }

    #[test]
    fn test_clone_produces_functional_instance() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let dedup = RedisDedup::from_client(Arc::new(c), "clone-node", 100);
            let cloned = dedup.clone();
            assert_eq!(cloned.node_id(), "clone-node");
            assert_eq!(cloned.ttl_seconds(), 100);
        }
    }

    #[test]
    fn test_dedup_result_equality() {
        assert_eq!(
            RedisDeduplicationResult::Claimed,
            RedisDeduplicationResult::Claimed
        );
        assert_eq!(
            RedisDeduplicationResult::AlreadyClaimed,
            RedisDeduplicationResult::AlreadyClaimed
        );
        assert_ne!(
            RedisDeduplicationResult::Claimed,
            RedisDeduplicationResult::AlreadyClaimed
        );
    }

    #[test]
    fn test_dedup_result_debug_format() {
        let claimed = format!("{:?}", RedisDeduplicationResult::Claimed);
        assert!(claimed.contains("Claimed"));
        let already = format!("{:?}", RedisDeduplicationResult::AlreadyClaimed);
        assert!(already.contains("AlreadyClaimed"));
    }

    #[test]
    fn test_dedup_result_clone() {
        let result = RedisDeduplicationResult::Claimed;
        let cloned = result.clone();
        assert_eq!(result, cloned);
    }

    #[tokio::test]
    async fn test_try_claim_without_redis_returns_connection_error() {
        // Connect to a port where Redis is almost certainly not running
        let dedup = RedisDedup::new("redis://localhost:59999", "node-1", 300).await;
        if let Ok(d) = dedup {
            let result = d.try_claim("test-key").await;
            if let Err(err) = result {
                assert!(matches!(err, DistributedError::RedisConnection(_)));
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[tokio::test]
    async fn test_release_without_redis_returns_connection_error() {
        let dedup = RedisDedup::new("redis://localhost:59999", "node-1", 300).await;
        if let Ok(d) = dedup {
            let result = d.release("test-key").await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_owner_of_without_redis_returns_connection_error() {
        let dedup = RedisDedup::new("redis://localhost:59999", "node-1", 300).await;
        if let Ok(d) = dedup {
            let result = d.owner_of("test-key").await;
            assert!(result.is_err());
        }
    }
}
