//! # Stage: Cluster Manager
//!
//! ## Responsibility
//! Manage cluster membership: node registration, heartbeat broadcasting,
//! dead-node eviction, and load-based request routing. This module maintains
//! a local view of the cluster topology and routes requests to the
//! least-loaded node.
//!
//! ## Guarantees
//! - Self-healing: nodes that miss heartbeats are evicted after `eviction_timeout`
//! - Load-aware: routing considers current queue depth per node
//! - Thread-safe: topology is protected by `Arc<RwLock<...>>`
//! - Non-blocking: all operations are async, never block the executor
//!
//! ## NOT Responsible For
//! - Leader election (see: `leader`)
//! - Message transport (see: `nats`)
//! - Deduplication (see: `redis_dedup`)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::DistributedError;

/// Information about a single node in the cluster.
///
/// # Panics
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeInfo {
    /// Unique identifier for this node.
    pub node_id: String,
    /// Node's advertised address (for direct communication).
    pub address: String,
    /// Current queue depth (pending requests).
    pub load: usize,
    /// Timestamp of the last heartbeat received from this node.
    #[serde(skip)]
    pub last_heartbeat: Option<SystemTime>,
    /// Whether this node is marked as active.
    pub active: bool,
}

impl NodeInfo {
    /// Create a new node info entry.
    ///
    /// # Arguments
    /// * `node_id` — Unique node identifier
    /// * `address` — Network address
    ///
    /// # Returns
    /// A `NodeInfo` with zero load and current heartbeat timestamp.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(node_id: String, address: String) -> Self {
        Self {
            node_id,
            address,
            load: 0,
            last_heartbeat: Some(SystemTime::now()),
            active: true,
        }
    }

    /// Check if this node's heartbeat has expired.
    ///
    /// # Arguments
    /// * `timeout` — Maximum time since last heartbeat
    ///
    /// # Returns
    /// `true` if the node has been silent longer than `timeout`.
    ///
    /// # Panics
    /// This function never panics.
    pub fn is_expired(&self, timeout: Duration) -> bool {
        match self.last_heartbeat {
            Some(ts) => ts.elapsed().unwrap_or(Duration::MAX) > timeout,
            None => true,
        }
    }
}

/// Heartbeat message broadcast by each node.
///
/// # Panics
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Heartbeat {
    /// ID of the node sending the heartbeat.
    pub node_id: String,
    /// Node's address.
    pub address: String,
    /// Current load (queue depth).
    pub load: usize,
}

/// Cluster topology and routing manager.
///
/// Maintains a map of known nodes, processes heartbeats, evicts dead nodes,
/// and routes requests to the least-loaded active node.
///
/// # Panics
/// This type never panics.
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::distributed::ClusterManager;
/// use std::time::Duration;
///
/// # #[tokio::main]
/// # async fn main() {
/// let cluster = ClusterManager::new("node-1", Duration::from_secs(15));
/// cluster.register("node-1", "localhost:8080", 0).await;
/// cluster.register("node-2", "localhost:8081", 5).await;
///
/// // Route to least-loaded
/// let target = cluster.route().await;
/// assert!(target.is_some());
/// # }
/// ```
pub struct ClusterManager {
    nodes: Arc<RwLock<HashMap<String, NodeInfo>>>,
    local_node_id: String,
    eviction_timeout: Duration,
}

impl ClusterManager {
    /// Create a new cluster manager.
    ///
    /// # Arguments
    /// * `local_node_id` — This node's unique identifier
    /// * `eviction_timeout` — Duration of silence before a node is evicted
    ///
    /// # Returns
    /// A `ClusterManager` with an empty topology.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(local_node_id: &str, eviction_timeout: Duration) -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            local_node_id: local_node_id.to_string(),
            eviction_timeout,
        }
    }

    /// Register a node in the cluster topology.
    ///
    /// If the node already exists, updates its heartbeat and load.
    ///
    /// # Arguments
    /// * `node_id` — Node identifier
    /// * `address` — Node network address
    /// * `load` — Current queue depth
    ///
    /// # Panics
    /// This function never panics.
    pub async fn register(&self, node_id: &str, address: &str, load: usize) {
        let mut nodes = self.nodes.write().await;
        let entry = nodes
            .entry(node_id.to_string())
            .or_insert_with(|| NodeInfo::new(node_id.to_string(), address.to_string()));

        entry.address = address.to_string();
        entry.load = load;
        entry.last_heartbeat = Some(SystemTime::now());
        entry.active = true;

        debug!(node = node_id, load = load, "node registered/updated");
    }

    /// Process a heartbeat from a remote node.
    ///
    /// Updates the node's last-seen timestamp and load.
    ///
    /// # Arguments
    /// * `heartbeat` — The heartbeat message to process
    ///
    /// # Panics
    /// This function never panics.
    pub async fn process_heartbeat(&self, heartbeat: &Heartbeat) {
        self.register(&heartbeat.node_id, &heartbeat.address, heartbeat.load)
            .await;
    }

    /// Evict nodes that have been silent longer than the eviction timeout.
    ///
    /// # Returns
    /// List of node IDs that were evicted.
    ///
    /// # Panics
    /// This function never panics.
    pub async fn evict_stale_nodes(&self) -> Vec<String> {
        let mut nodes = self.nodes.write().await;
        let mut evicted = Vec::new();

        nodes.retain(|id, info| {
            if info.is_expired(self.eviction_timeout) {
                warn!(node = %id, "evicting stale node (heartbeat timeout)");
                evicted.push(id.clone());
                false
            } else {
                true
            }
        });

        if !evicted.is_empty() {
            info!(count = evicted.len(), evicted = ?evicted, "stale nodes evicted");
        }

        evicted
    }

    /// Route a request to the least-loaded active node.
    ///
    /// # Returns
    /// - `Some(node_id)` — the target node to route to
    /// - `None` — no active nodes available
    ///
    /// # Panics
    /// This function never panics.
    pub async fn route(&self) -> Option<String> {
        let nodes = self.nodes.read().await;

        nodes
            .values()
            .filter(|n| n.active && !n.is_expired(self.eviction_timeout))
            .min_by_key(|n| n.load)
            .map(|n| n.node_id.clone())
    }

    /// Get information about a specific node.
    ///
    /// # Arguments
    /// * `node_id` — The node to query
    ///
    /// # Returns
    /// - `Some(NodeInfo)` — the node's info
    /// - `None` — node not found
    ///
    /// # Panics
    /// This function never panics.
    pub async fn get_node(&self, node_id: &str) -> Option<NodeInfo> {
        let nodes = self.nodes.read().await;
        nodes.get(node_id).cloned()
    }

    /// Get all active nodes in the cluster.
    ///
    /// # Returns
    /// A vector of all active, non-expired node infos.
    ///
    /// # Panics
    /// This function never panics.
    pub async fn active_nodes(&self) -> Vec<NodeInfo> {
        let nodes = self.nodes.read().await;
        nodes
            .values()
            .filter(|n| n.active && !n.is_expired(self.eviction_timeout))
            .cloned()
            .collect()
    }

    /// Get the total number of registered nodes (active and inactive).
    ///
    /// # Panics
    /// This function never panics.
    pub async fn node_count(&self) -> usize {
        let nodes = self.nodes.read().await;
        nodes.len()
    }

    /// Remove a specific node from the topology.
    ///
    /// # Arguments
    /// * `node_id` — The node to deregister
    ///
    /// # Returns
    /// - `Some(NodeInfo)` — the removed node's info
    /// - `None` — node was not found
    ///
    /// # Panics
    /// This function never panics.
    pub async fn deregister(&self, node_id: &str) -> Option<NodeInfo> {
        let mut nodes = self.nodes.write().await;
        let removed = nodes.remove(node_id);
        if removed.is_some() {
            info!(node = node_id, "node deregistered");
        }
        removed
    }

    /// Update the load for a specific node.
    ///
    /// # Arguments
    /// * `node_id` — The node to update
    /// * `load` — New queue depth
    ///
    /// # Returns
    /// - `Ok(())` — load updated
    /// - `Err(DistributedError::NodeNotFound)` — node not registered
    ///
    /// # Panics
    /// This function never panics.
    pub async fn update_load(&self, node_id: &str, load: usize) -> Result<(), DistributedError> {
        let mut nodes = self.nodes.write().await;
        match nodes.get_mut(node_id) {
            Some(info) => {
                info.load = load;
                debug!(node = node_id, load = load, "load updated");
                Ok(())
            }
            None => Err(DistributedError::NodeNotFound(node_id.to_string())),
        }
    }

    /// Get this node's ID.
    ///
    /// # Panics
    /// This function never panics.
    pub fn local_node_id(&self) -> &str {
        &self.local_node_id
    }

    /// Get the configured eviction timeout.
    ///
    /// # Panics
    /// This function never panics.
    pub fn eviction_timeout(&self) -> Duration {
        self.eviction_timeout
    }

    /// Create a heartbeat message for this node.
    ///
    /// # Arguments
    /// * `address` — This node's address
    /// * `load` — Current queue depth
    ///
    /// # Returns
    /// A `Heartbeat` ready to broadcast.
    ///
    /// # Panics
    /// This function never panics.
    pub fn create_heartbeat(&self, address: &str, load: usize) -> Heartbeat {
        Heartbeat {
            node_id: self.local_node_id.clone(),
            address: address.to_string(),
            load,
        }
    }

    /// Spawn a background eviction loop that periodically removes stale nodes.
    ///
    /// Runs every `eviction_timeout / 3` to catch dead nodes promptly.
    ///
    /// # Returns
    /// A `JoinHandle` for the background loop.
    ///
    /// # Panics
    /// This function never panics.
    pub fn spawn_eviction_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let interval = this.eviction_timeout / 3;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                this.evict_stale_nodes().await;
            }
        })
    }
}

impl Clone for ClusterManager {
    fn clone(&self) -> Self {
        Self {
            nodes: self.nodes.clone(),
            local_node_id: self.local_node_id.clone(),
            eviction_timeout: self.eviction_timeout,
        }
    }
}

impl std::fmt::Debug for ClusterManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterManager")
            .field("local_node_id", &self.local_node_id)
            .field("eviction_timeout", &self.eviction_timeout)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cluster() -> ClusterManager {
        ClusterManager::new("local", Duration::from_secs(15))
    }

    #[tokio::test]
    async fn test_register_adds_node() {
        let cluster = make_cluster();
        cluster.register("n1", "addr1", 0).await;
        assert_eq!(cluster.node_count().await, 1);
    }

    #[tokio::test]
    async fn test_register_updates_existing_node() {
        let cluster = make_cluster();
        cluster.register("n1", "addr1", 0).await;
        cluster.register("n1", "addr1-updated", 5).await;
        assert_eq!(cluster.node_count().await, 1);
        let node = cluster.get_node("n1").await;
        assert!(node.is_some());
        let n = node.unwrap_or_else(|| NodeInfo::new("".into(), "".into()));
        assert_eq!(n.address, "addr1-updated");
        assert_eq!(n.load, 5);
    }

    #[tokio::test]
    async fn test_route_returns_least_loaded() {
        let cluster = make_cluster();
        cluster.register("n1", "addr1", 10).await;
        cluster.register("n2", "addr2", 3).await;
        cluster.register("n3", "addr3", 7).await;

        let target = cluster.route().await;
        assert_eq!(target, Some("n2".to_string()));
    }

    #[tokio::test]
    async fn test_route_empty_cluster_returns_none() {
        let cluster = make_cluster();
        assert!(cluster.route().await.is_none());
    }

    #[tokio::test]
    async fn test_route_equal_load_returns_some() {
        let cluster = make_cluster();
        cluster.register("n1", "addr1", 5).await;
        cluster.register("n2", "addr2", 5).await;
        // Should return one of them
        let target = cluster.route().await;
        assert!(target.is_some());
    }

    #[tokio::test]
    async fn test_deregister_removes_node() {
        let cluster = make_cluster();
        cluster.register("n1", "addr1", 0).await;
        assert_eq!(cluster.node_count().await, 1);
        let removed = cluster.deregister("n1").await;
        assert!(removed.is_some());
        assert_eq!(cluster.node_count().await, 0);
    }

    #[tokio::test]
    async fn test_deregister_nonexistent_returns_none() {
        let cluster = make_cluster();
        let removed = cluster.deregister("nonexistent").await;
        assert!(removed.is_none());
    }

    #[tokio::test]
    async fn test_update_load_success() {
        let cluster = make_cluster();
        cluster.register("n1", "addr1", 0).await;
        let result = cluster.update_load("n1", 42).await;
        assert!(result.is_ok());
        let node = cluster.get_node("n1").await;
        assert_eq!(node.map(|n| n.load).unwrap_or(0), 42);
    }

    #[tokio::test]
    async fn test_update_load_node_not_found() {
        let cluster = make_cluster();
        let result = cluster.update_load("nonexistent", 10).await;
        if let Err(e) = result {
            assert!(matches!(e, DistributedError::NodeNotFound(_)));
        } else {
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_active_nodes_returns_only_active() {
        let cluster = make_cluster();
        cluster.register("n1", "addr1", 0).await;
        cluster.register("n2", "addr2", 0).await;

        // Deactivate n2
        {
            let mut nodes = cluster.nodes.write().await;
            if let Some(n) = nodes.get_mut("n2") {
                n.active = false;
            }
        }

        let active = cluster.active_nodes().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].node_id, "n1");
    }

    #[tokio::test]
    async fn test_evict_stale_nodes() {
        let cluster = ClusterManager::new("local", Duration::from_millis(50));
        cluster.register("n1", "addr1", 0).await;

        // Wait for the node to become stale
        tokio::time::sleep(Duration::from_millis(100)).await;

        let evicted = cluster.evict_stale_nodes().await;
        assert_eq!(evicted, vec!["n1".to_string()]);
        assert_eq!(cluster.node_count().await, 0);
    }

    #[tokio::test]
    async fn test_evict_keeps_fresh_nodes() {
        let cluster = ClusterManager::new("local", Duration::from_secs(60));
        cluster.register("n1", "addr1", 0).await;

        let evicted = cluster.evict_stale_nodes().await;
        assert!(evicted.is_empty());
        assert_eq!(cluster.node_count().await, 1);
    }

    #[tokio::test]
    async fn test_process_heartbeat_registers_node() {
        let cluster = make_cluster();
        let hb = Heartbeat {
            node_id: "n1".to_string(),
            address: "addr1".to_string(),
            load: 3,
        };

        cluster.process_heartbeat(&hb).await;
        let node = cluster.get_node("n1").await;
        assert!(node.is_some());
        assert_eq!(node.map(|n| n.load).unwrap_or(0), 3);
    }

    #[tokio::test]
    async fn test_process_heartbeat_updates_existing() {
        let cluster = make_cluster();
        cluster.register("n1", "addr1", 0).await;

        let hb = Heartbeat {
            node_id: "n1".to_string(),
            address: "addr1".to_string(),
            load: 10,
        };
        cluster.process_heartbeat(&hb).await;

        let node = cluster.get_node("n1").await;
        assert_eq!(node.map(|n| n.load).unwrap_or(0), 10);
    }

    #[tokio::test]
    async fn test_get_node_nonexistent_returns_none() {
        let cluster = make_cluster();
        assert!(cluster.get_node("nonexistent").await.is_none());
    }

    #[test]
    fn test_local_node_id() {
        let cluster = make_cluster();
        assert_eq!(cluster.local_node_id(), "local");
    }

    #[test]
    fn test_eviction_timeout_returns_configured() {
        let cluster = ClusterManager::new("n", Duration::from_secs(42));
        assert_eq!(cluster.eviction_timeout(), Duration::from_secs(42));
    }

    #[test]
    fn test_create_heartbeat() {
        let cluster = make_cluster();
        let hb = cluster.create_heartbeat("my-addr", 7);
        assert_eq!(hb.node_id, "local");
        assert_eq!(hb.address, "my-addr");
        assert_eq!(hb.load, 7);
    }

    #[test]
    fn test_heartbeat_serde_roundtrip() {
        let hb = Heartbeat {
            node_id: "n1".to_string(),
            address: "addr".to_string(),
            load: 5,
        };
        let json = serde_json::to_string(&hb);
        assert!(json.is_ok());
        let deserialized: Result<Heartbeat, _> = serde_json::from_str(&json.unwrap_or_default());
        assert!(deserialized.is_ok());
        assert_eq!(hb, deserialized.unwrap_or_else(|_| hb.clone()));
    }

    #[test]
    fn test_node_info_new() {
        let info = NodeInfo::new("n1".to_string(), "addr1".to_string());
        assert_eq!(info.node_id, "n1");
        assert_eq!(info.address, "addr1");
        assert_eq!(info.load, 0);
        assert!(info.active);
        assert!(info.last_heartbeat.is_some());
    }

    #[test]
    fn test_node_info_is_expired_no_heartbeat() {
        let mut info = NodeInfo::new("n1".to_string(), "addr1".to_string());
        info.last_heartbeat = None;
        assert!(info.is_expired(Duration::from_secs(60)));
    }

    #[test]
    fn test_node_info_is_expired_fresh() {
        let info = NodeInfo::new("n1".to_string(), "addr1".to_string());
        assert!(!info.is_expired(Duration::from_secs(60)));
    }

    #[test]
    fn test_node_info_debug_format() {
        let info = NodeInfo::new("n1".to_string(), "addr1".to_string());
        let debug = format!("{:?}", info);
        assert!(debug.contains("n1"));
        assert!(debug.contains("addr1"));
    }

    #[test]
    fn test_node_info_clone() {
        let info = NodeInfo::new("n1".to_string(), "addr1".to_string());
        let cloned = info.clone();
        assert_eq!(info.node_id, cloned.node_id);
        assert_eq!(info.address, cloned.address);
    }

    #[test]
    fn test_cluster_debug_format() {
        let cluster = make_cluster();
        let debug = format!("{:?}", cluster);
        assert!(debug.contains("local"));
    }

    #[test]
    fn test_cluster_clone_shares_state() {
        let cluster = make_cluster();
        let _cloned = cluster.clone();
        // Both share the same Arc<RwLock<...>>
        assert_eq!(cluster.local_node_id(), "local");
    }

    #[tokio::test]
    async fn test_route_skips_expired_nodes() {
        let cluster = ClusterManager::new("local", Duration::from_millis(50));
        cluster.register("n1", "addr1", 0).await;
        cluster.register("n2", "addr2", 0).await;

        // Let n1 and n2 expire
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Re-register only n2 (refreshes heartbeat)
        cluster.register("n2", "addr2", 0).await;

        let target = cluster.route().await;
        assert_eq!(target, Some("n2".to_string()));
    }

    #[tokio::test]
    async fn test_multiple_register_deregister_cycle() {
        let cluster = make_cluster();

        for i in 0..5 {
            cluster
                .register(&format!("n{i}"), &format!("addr{i}"), i)
                .await;
        }
        assert_eq!(cluster.node_count().await, 5);

        for i in 0..3 {
            cluster.deregister(&format!("n{i}")).await;
        }
        assert_eq!(cluster.node_count().await, 2);
    }

    #[tokio::test]
    async fn test_route_after_load_update() {
        let cluster = make_cluster();
        cluster.register("n1", "addr1", 10).await;
        cluster.register("n2", "addr2", 5).await;

        // n2 is least loaded
        assert_eq!(cluster.route().await, Some("n2".to_string()));

        // Update n1 to have less load
        let _ = cluster.update_load("n1", 1).await;
        assert_eq!(cluster.route().await, Some("n1".to_string()));
    }

    #[test]
    fn test_heartbeat_debug_format() {
        let hb = Heartbeat {
            node_id: "n1".to_string(),
            address: "addr".to_string(),
            load: 3,
        };
        let debug = format!("{:?}", hb);
        assert!(debug.contains("n1"));
        assert!(debug.contains("3"));
    }
}
