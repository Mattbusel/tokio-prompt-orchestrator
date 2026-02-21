//! # Stage: Leader Election
//!
//! ## Responsibility
//! Implement distributed leader election using Redis `SETNX` with TTL-based
//! lease renewal. Exactly one node in the cluster is the leader at any time.
//! The leader renews its lease every `ttl/2`. If the leader dies, the lease
//! expires and another node takes over.
//!
//! ## Guarantees
//! - At-most-one leader: Redis `SET NX` ensures only one node holds the lock
//! - Self-healing: lease expiry allows automatic failover on leader death
//! - Renewable: active leaders renew before expiry to prevent flapping
//! - Observable: callers can query current leader at any time
//!
//! ## NOT Responsible For
//! - Deciding what the leader does (that's the caller's responsibility)
//! - Cluster membership (see: `cluster`)
//! - NATS messaging (see: `nats`)

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::DistributedError;

/// Describes the role of this node in the cluster.
///
/// # Panics
/// This type never panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaderRole {
    /// This node is the current leader.
    Leader,
    /// This node is a follower. Contains the current leader's node ID if known.
    Follower(Option<String>),
}

/// Redis-based leader election using `SET key NX EX ttl`.
///
/// Provides `try_elect`, `renew`, and `step_down` operations. A background
/// renewal loop can be spawned via `spawn_renewal_loop`.
///
/// # Panics
/// This type never panics.
///
/// # Example
///
/// ```no_run
/// use tokio_prompt_orchestrator::distributed::LeaderElection;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let election = LeaderElection::new("redis://localhost:6379", "node-1", 10).await?;
///
/// if election.try_elect().await? {
///     println!("I am the leader!");
/// }
/// # Ok(())
/// # }
/// ```
pub struct LeaderElection {
    client: Arc<redis::Client>,
    node_id: String,
    ttl_seconds: u64,
    leader_key: String,
    role_tx: watch::Sender<LeaderRole>,
    role_rx: watch::Receiver<LeaderRole>,
}

impl LeaderElection {
    /// Create a new leader election instance.
    ///
    /// # Arguments
    /// * `redis_url` — Redis connection URL
    /// * `node_id` — This node's unique identifier
    /// * `ttl_seconds` — Leader lease TTL in seconds
    ///
    /// # Returns
    /// - `Ok(LeaderElection)` — ready to participate in elections
    /// - `Err(DistributedError::RedisConnection)` — invalid Redis URL
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

        let (role_tx, role_rx) = watch::channel(LeaderRole::Follower(None));

        Ok(Self {
            client: Arc::new(client),
            node_id: node_id.to_string(),
            ttl_seconds,
            leader_key: "orchestrator:leader".to_string(),
            role_tx,
            role_rx,
        })
    }

    /// Create a leader election from an existing Redis client.
    ///
    /// # Arguments
    /// * `client` — Pre-configured Redis client
    /// * `node_id` — This node's unique identifier
    /// * `ttl_seconds` — Leader lease TTL in seconds
    ///
    /// # Returns
    /// A `LeaderElection` wrapping the provided client.
    ///
    /// # Panics
    /// This function never panics.
    pub fn from_client(client: Arc<redis::Client>, node_id: &str, ttl_seconds: u64) -> Self {
        let (role_tx, role_rx) = watch::channel(LeaderRole::Follower(None));
        Self {
            client,
            node_id: node_id.to_string(),
            ttl_seconds,
            leader_key: "orchestrator:leader".to_string(),
            role_tx,
            role_rx,
        }
    }

    /// Attempt to become the leader.
    ///
    /// Uses `SET key NX EX ttl` — succeeds only if no leader currently exists.
    ///
    /// # Returns
    /// - `Ok(true)` — this node is now the leader
    /// - `Ok(false)` — another node is already leader
    /// - `Err(DistributedError::RedisOperation)` — Redis command failed
    ///
    /// # Panics
    /// This function never panics.
    pub async fn try_elect(&self) -> Result<bool, DistributedError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                DistributedError::RedisConnection(format!("failed to get connection: {e}"))
            })?;

        let result: Option<String> = redis::cmd("SET")
            .arg(&self.leader_key)
            .arg(&self.node_id)
            .arg("NX")
            .arg("EX")
            .arg(self.ttl_seconds)
            .query_async(&mut conn)
            .await
            .map_err(|e| {
                DistributedError::RedisOperation(format!("leader election SET NX failed: {e}"))
            })?;

        match result {
            Some(ref s) if s == "OK" => {
                info!(node = %self.node_id, ttl = self.ttl_seconds, "elected as leader");
                let _ = self.role_tx.send(LeaderRole::Leader);
                Ok(true)
            }
            _ => {
                // Check who the current leader is
                let current: Option<String> = redis::cmd("GET")
                    .arg(&self.leader_key)
                    .query_async(&mut conn)
                    .await
                    .unwrap_or(None);
                debug!(node = %self.node_id, current_leader = ?current, "election lost");
                let _ = self.role_tx.send(LeaderRole::Follower(current));
                Ok(false)
            }
        }
    }

    /// Renew the leader lease (only succeeds if this node is the current leader).
    ///
    /// Uses a Lua script for atomic compare-and-extend.
    ///
    /// # Returns
    /// - `Ok(true)` — lease renewed
    /// - `Ok(false)` — this node is not the leader (lease stolen or expired)
    /// - `Err(DistributedError::RedisOperation)` — Redis command failed
    ///
    /// # Panics
    /// This function never panics.
    pub async fn renew(&self) -> Result<bool, DistributedError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                DistributedError::RedisConnection(format!("failed to get connection: {e}"))
            })?;

        // Atomic compare-and-extend: only renew if we still hold the lock
        let script = r#"
            if redis.call("GET", KEYS[1]) == ARGV[1] then
                return redis.call("EXPIRE", KEYS[1], ARGV[2])
            else
                return 0
            end
        "#;

        let result: i64 = redis::Script::new(script)
            .key(&self.leader_key)
            .arg(&self.node_id)
            .arg(self.ttl_seconds)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| {
                DistributedError::RedisOperation(format!("leader renewal script failed: {e}"))
            })?;

        if result == 1 {
            debug!(node = %self.node_id, ttl = self.ttl_seconds, "leader lease renewed");
            Ok(true)
        } else {
            warn!(node = %self.node_id, "lease renewal failed — no longer leader");
            let _ = self.role_tx.send(LeaderRole::Follower(None));
            Ok(false)
        }
    }

    /// Voluntarily step down as leader.
    ///
    /// Only deletes the key if this node is the current holder (atomic).
    ///
    /// # Returns
    /// - `Ok(true)` — successfully stepped down
    /// - `Ok(false)` — was not the leader
    /// - `Err(DistributedError::RedisOperation)` — Redis command failed
    ///
    /// # Panics
    /// This function never panics.
    pub async fn step_down(&self) -> Result<bool, DistributedError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                DistributedError::RedisConnection(format!("failed to get connection: {e}"))
            })?;

        let script = r#"
            if redis.call("GET", KEYS[1]) == ARGV[1] then
                return redis.call("DEL", KEYS[1])
            else
                return 0
            end
        "#;

        let result: i64 = redis::Script::new(script)
            .key(&self.leader_key)
            .arg(&self.node_id)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| {
                DistributedError::RedisOperation(format!("step down script failed: {e}"))
            })?;

        if result == 1 {
            info!(node = %self.node_id, "stepped down from leadership");
            let _ = self.role_tx.send(LeaderRole::Follower(None));
            Ok(true)
        } else {
            debug!(node = %self.node_id, "step down — was not leader");
            Ok(false)
        }
    }

    /// Query who the current leader is.
    ///
    /// # Returns
    /// - `Ok(Some(node_id))` — the current leader's node ID
    /// - `Ok(None)` — no leader (election needed)
    /// - `Err(DistributedError::RedisOperation)` — Redis command failed
    ///
    /// # Panics
    /// This function never panics.
    pub async fn current_leader(&self) -> Result<Option<String>, DistributedError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                DistributedError::RedisConnection(format!("failed to get connection: {e}"))
            })?;

        let leader: Option<String> = redis::cmd("GET")
            .arg(&self.leader_key)
            .query_async(&mut conn)
            .await
            .map_err(|e| DistributedError::RedisOperation(format!("GET leader failed: {e}")))?;

        Ok(leader)
    }

    /// Check if this node believes it is the leader (from local state).
    ///
    /// This is a local check based on the last election/renewal result,
    /// not a Redis query. For authoritative status, use [`current_leader`].
    ///
    /// # Panics
    /// This function never panics.
    pub fn is_leader(&self) -> bool {
        matches!(*self.role_rx.borrow(), LeaderRole::Leader)
    }

    /// Get a receiver for leadership role changes.
    ///
    /// Can be used to react to leadership transitions.
    ///
    /// # Panics
    /// This function never panics.
    pub fn role_watcher(&self) -> watch::Receiver<LeaderRole> {
        self.role_rx.clone()
    }

    /// Get this node's ID.
    ///
    /// # Panics
    /// This function never panics.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Get the configured lease TTL in seconds.
    ///
    /// # Panics
    /// This function never panics.
    pub fn ttl_seconds(&self) -> u64 {
        self.ttl_seconds
    }

    /// Get the renewal interval (ttl / 2).
    ///
    /// # Panics
    /// This function never panics.
    pub fn renewal_interval(&self) -> Duration {
        Duration::from_secs(self.ttl_seconds / 2)
    }

    /// Spawn a background loop that periodically attempts election and renewal.
    ///
    /// - If this node is not the leader, attempts election every `ttl` seconds.
    /// - If this node is the leader, renews every `ttl/2` seconds.
    /// - Runs until the returned `tokio::task::JoinHandle` is aborted.
    ///
    /// # Returns
    /// A `JoinHandle` for the background election loop.
    ///
    /// # Panics
    /// This function never panics.
    pub fn spawn_election_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let renewal = Duration::from_secs(this.ttl_seconds / 2);
            let election_retry = Duration::from_secs(this.ttl_seconds);

            loop {
                if this.is_leader() {
                    match this.renew().await {
                        Ok(true) => {
                            debug!(node = %this.node_id, "leader lease renewed in loop");
                        }
                        Ok(false) => {
                            warn!(node = %this.node_id, "lost leadership in renewal loop");
                        }
                        Err(e) => {
                            warn!(node = %this.node_id, error = ?e, "renewal failed");
                        }
                    }
                    tokio::time::sleep(renewal).await;
                } else {
                    match this.try_elect().await {
                        Ok(true) => {
                            info!(node = %this.node_id, "won election in loop");
                        }
                        Ok(false) => {
                            debug!(node = %this.node_id, "election attempt — not elected");
                        }
                        Err(e) => {
                            warn!(node = %this.node_id, error = ?e, "election attempt failed");
                        }
                    }
                    tokio::time::sleep(election_retry).await;
                }
            }
        })
    }
}

impl Clone for LeaderElection {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            node_id: self.node_id.clone(),
            ttl_seconds: self.ttl_seconds,
            leader_key: self.leader_key.clone(),
            role_tx: self.role_tx.clone(),
            role_rx: self.role_rx.clone(),
        }
    }
}

impl std::fmt::Debug for LeaderElection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaderElection")
            .field("node_id", &self.node_id)
            .field("ttl_seconds", &self.ttl_seconds)
            .field("leader_key", &self.leader_key)
            .field("is_leader", &self.is_leader())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_invalid_url_returns_connection_error() {
        let result = LeaderElection::new("not-a-url", "node-1", 10).await;
        if let Err(e) = result {
            assert!(matches!(e, DistributedError::RedisConnection(_)));
        } else {
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_new_valid_url_succeeds() {
        let result = LeaderElection::new("redis://localhost:6379", "node-1", 10).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_from_client_sets_fields() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let election = LeaderElection::from_client(Arc::new(c), "test-node", 30);
            assert_eq!(election.node_id(), "test-node");
            assert_eq!(election.ttl_seconds(), 30);
            assert!(!election.is_leader());
        }
    }

    #[test]
    fn test_initial_role_is_follower() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let election = LeaderElection::from_client(Arc::new(c), "n1", 10);
            assert!(!election.is_leader());
            assert!(matches!(
                *election.role_rx.borrow(),
                LeaderRole::Follower(None)
            ));
        }
    }

    #[test]
    fn test_renewal_interval_is_half_ttl() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let election = LeaderElection::from_client(Arc::new(c), "n1", 20);
            assert_eq!(election.renewal_interval(), Duration::from_secs(10));
        }
    }

    #[test]
    fn test_renewal_interval_odd_ttl() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let election = LeaderElection::from_client(Arc::new(c), "n1", 15);
            // 15 / 2 = 7 (integer division)
            assert_eq!(election.renewal_interval(), Duration::from_secs(7));
        }
    }

    #[test]
    fn test_role_watcher_returns_receiver() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let election = LeaderElection::from_client(Arc::new(c), "n1", 10);
            let rx = election.role_watcher();
            assert!(matches!(*rx.borrow(), LeaderRole::Follower(None)));
        }
    }

    #[test]
    fn test_leader_role_equality() {
        assert_eq!(LeaderRole::Leader, LeaderRole::Leader);
        assert_eq!(
            LeaderRole::Follower(Some("n1".to_string())),
            LeaderRole::Follower(Some("n1".to_string()))
        );
        assert_ne!(LeaderRole::Leader, LeaderRole::Follower(None));
        assert_ne!(
            LeaderRole::Follower(Some("n1".to_string())),
            LeaderRole::Follower(Some("n2".to_string()))
        );
    }

    #[test]
    fn test_leader_role_debug_format() {
        let leader = format!("{:?}", LeaderRole::Leader);
        assert!(leader.contains("Leader"));
        let follower = format!("{:?}", LeaderRole::Follower(Some("n1".to_string())));
        assert!(follower.contains("Follower"));
        assert!(follower.contains("n1"));
    }

    #[test]
    fn test_leader_role_clone() {
        let role = LeaderRole::Follower(Some("n1".to_string()));
        let cloned = role.clone();
        assert_eq!(role, cloned);
    }

    #[test]
    fn test_debug_format_contains_node_id() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let election = LeaderElection::from_client(Arc::new(c), "debug-node", 10);
            let debug = format!("{:?}", election);
            assert!(debug.contains("debug-node"));
            assert!(debug.contains("10"));
        }
    }

    #[test]
    fn test_clone_produces_independent_instance() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let election = LeaderElection::from_client(Arc::new(c), "clone-node", 10);
            let cloned = election.clone();
            assert_eq!(cloned.node_id(), "clone-node");
            assert_eq!(cloned.ttl_seconds(), 10);
        }
    }

    #[tokio::test]
    async fn test_try_elect_without_redis_returns_error() {
        let result = LeaderElection::new("redis://localhost:59999", "node-1", 10).await;
        if let Ok(election) = result {
            let elect_result = election.try_elect().await;
            assert!(elect_result.is_err());
        }
    }

    #[tokio::test]
    async fn test_renew_without_redis_returns_error() {
        let result = LeaderElection::new("redis://localhost:59999", "node-1", 10).await;
        if let Ok(election) = result {
            let renew_result = election.renew().await;
            assert!(renew_result.is_err());
        }
    }

    #[tokio::test]
    async fn test_step_down_without_redis_returns_error() {
        let result = LeaderElection::new("redis://localhost:59999", "node-1", 10).await;
        if let Ok(election) = result {
            let step_result = election.step_down().await;
            assert!(step_result.is_err());
        }
    }

    #[tokio::test]
    async fn test_current_leader_without_redis_returns_error() {
        let result = LeaderElection::new("redis://localhost:59999", "node-1", 10).await;
        if let Ok(election) = result {
            let leader_result = election.current_leader().await;
            assert!(leader_result.is_err());
        }
    }

    #[test]
    fn test_role_tx_updates_role_rx() {
        let client = redis::Client::open("redis://localhost:6379");
        if let Ok(c) = client {
            let election = LeaderElection::from_client(Arc::new(c), "n1", 10);
            assert!(!election.is_leader());

            // Simulate becoming leader
            let _ = election.role_tx.send(LeaderRole::Leader);
            assert!(election.is_leader());

            // Simulate losing leadership
            let _ = election
                .role_tx
                .send(LeaderRole::Follower(Some("n2".to_string())));
            assert!(!election.is_leader());
        }
    }
}
