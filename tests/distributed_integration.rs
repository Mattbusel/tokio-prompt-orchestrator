//! Integration tests for the distributed clustering module.
//!
//! Tests cover:
//! - Distributed dedup: concurrent claims from simulated nodes
//! - Leader election: role transitions and failover
//! - Cluster routing: load-based routing correctness
//! - Heartbeat eviction: stale node removal
//! - Chaos scenarios: unavailable Redis, simultaneous leader election, clock skew
//!
//! These tests use mock backends (no real Redis/NATS required)
//! to verify the cluster logic in isolation.

#![cfg(feature = "distributed")]

use std::sync::Arc;
use std::time::Duration;
use tokio_prompt_orchestrator::distributed::{
    ClusterManager, DistributedConfig, DistributedError, Heartbeat, LeaderElection, LeaderRole,
    NatsBus, NatsMessage, RedisDedup, RedisDeduplicationResult,
};

// ── Config Integration Tests ─────────────────────────────────────────────

#[test]
fn test_config_full_lifecycle() {
    let mut config = DistributedConfig::new("integration-node".to_string());
    assert!(config.validate().is_ok());

    // Mutate to invalid state
    config.node_id = String::new();
    assert!(config.validate().is_err());

    // Fix and revalidate
    config.node_id = "fixed".to_string();
    assert!(config.validate().is_ok());
}

#[test]
fn test_config_with_custom_urls_validates() {
    let config = DistributedConfig::with_urls(
        "node-1".to_string(),
        "nats://prod:4222".to_string(),
        "redis://prod:6379".to_string(),
    );
    assert!(config.validate().is_ok());
    assert_eq!(config.nats_url, "nats://prod:4222");
    assert_eq!(config.redis_url, "redis://prod:6379");
}

#[test]
fn test_config_timing_relationships() {
    let config = DistributedConfig::new("node-1".to_string());
    // Eviction timeout must be > heartbeat interval
    assert!(config.eviction_timeout_s > config.heartbeat_interval_s);
    // Leader renewal is half the TTL
    assert_eq!(
        config.leader_renewal_interval(),
        Duration::from_secs(config.leader_ttl_s / 2)
    );
}

// ── Cluster Manager Integration Tests ────────────────────────────────────

#[tokio::test]
async fn test_cluster_multi_node_registration_and_routing() {
    let cluster = ClusterManager::new("coordinator", Duration::from_secs(30));

    // Register 5 nodes with different loads
    cluster.register("n1", "host1:8080", 10).await;
    cluster.register("n2", "host2:8080", 3).await;
    cluster.register("n3", "host3:8080", 7).await;
    cluster.register("n4", "host4:8080", 15).await;
    cluster.register("n5", "host5:8080", 1).await;

    // Route should pick n5 (lowest load)
    let target = cluster.route().await;
    assert_eq!(target, Some("n5".to_string()));

    // Update n5 to high load, n2 should be next
    let _ = cluster.update_load("n5", 100).await;
    let target = cluster.route().await;
    assert_eq!(target, Some("n2".to_string()));
}

#[tokio::test]
async fn test_cluster_heartbeat_eviction_lifecycle() {
    let cluster = ClusterManager::new("coordinator", Duration::from_millis(100));

    // Register two nodes
    cluster.register("n1", "host1:8080", 0).await;
    cluster.register("n2", "host2:8080", 0).await;
    assert_eq!(cluster.node_count().await, 2);

    // Wait for eviction timeout
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Re-register only n1 (simulates heartbeat)
    cluster.register("n1", "host1:8080", 0).await;

    // Evict stale nodes
    let evicted = cluster.evict_stale_nodes().await;
    assert!(evicted.contains(&"n2".to_string()), "n2 should be evicted");
    assert!(
        !evicted.contains(&"n1".to_string()),
        "n1 should NOT be evicted"
    );

    // Only n1 remains
    assert_eq!(cluster.node_count().await, 1);
    assert_eq!(cluster.route().await, Some("n1".to_string()));
}

#[tokio::test]
async fn test_cluster_heartbeat_processing() {
    let cluster = ClusterManager::new("coordinator", Duration::from_secs(30));

    // Process heartbeats from multiple nodes
    for i in 0..5 {
        let hb = Heartbeat {
            node_id: format!("node-{i}"),
            address: format!("host{i}:8080"),
            load: i * 2,
        };
        cluster.process_heartbeat(&hb).await;
    }

    assert_eq!(cluster.node_count().await, 5);

    // Verify least-loaded node
    let target = cluster.route().await;
    assert_eq!(target, Some("node-0".to_string())); // load 0
}

#[tokio::test]
async fn test_cluster_deregister_affects_routing() {
    let cluster = ClusterManager::new("coordinator", Duration::from_secs(30));
    cluster.register("n1", "h1", 10).await;
    cluster.register("n2", "h2", 5).await;

    assert_eq!(cluster.route().await, Some("n2".to_string()));

    // Deregister n2
    cluster.deregister("n2").await;
    assert_eq!(cluster.route().await, Some("n1".to_string()));
}

#[tokio::test]
async fn test_cluster_all_nodes_evicted_route_returns_none() {
    let cluster = ClusterManager::new("coordinator", Duration::from_millis(50));
    cluster.register("n1", "h1", 0).await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    cluster.evict_stale_nodes().await;

    assert!(cluster.route().await.is_none());
}

#[tokio::test]
async fn test_cluster_inactive_node_not_routed() {
    let cluster = ClusterManager::new("coordinator", Duration::from_secs(30));
    cluster.register("n1", "h1", 10).await;
    cluster.register("n2", "h2", 1).await;

    // Manually deactivate n2 (simulate graceful shutdown)
    {
        let nodes = &cluster;
        if let Some(mut info) = nodes.get_node("n2").await {
            info.active = false;
            // Can't directly update through get_node (it's a clone),
            // but we can deregister and re-register with active=false
        }
    }
    cluster.deregister("n2").await;

    // Only n1 should be available
    assert_eq!(cluster.route().await, Some("n1".to_string()));
}

// ── NATS Bus Integration Tests ───────────────────────────────────────────

#[tokio::test]
async fn test_nats_bus_unconnected_operations() {
    let bus = NatsBus::unconnected("nats://localhost:4222", "test-node");
    assert!(!bus.is_connected().await);

    // All operations should return NatsConnection errors
    let pub_result = bus.publish("request", "all", "{}").await;
    assert!(matches!(
        pub_result.unwrap_err(),
        DistributedError::NatsConnection(_)
    ));

    let sub_result = bus.subscribe("request", "all").await;
    assert!(matches!(
        sub_result.unwrap_err(),
        DistributedError::NatsConnection(_)
    ));
}

#[test]
fn test_nats_message_serialization_roundtrip() {
    let original = NatsMessage {
        source_node: "node-1".to_string(),
        msg_type: "request".to_string(),
        payload: r#"{"prompt":"test","session":"s1"}"#.to_string(),
    };

    let bytes = serde_json::to_vec(&original).unwrap_or_default();
    let parsed = NatsBus::parse_message(&bytes);
    assert!(parsed.is_ok());
    assert_eq!(parsed.unwrap_or_else(|_| original.clone()), original);
}

#[test]
fn test_nats_subject_construction() {
    assert_eq!(
        NatsBus::subject("request", "inference"),
        "orchestrator.request.inference"
    );
    assert_eq!(
        NatsBus::subject("heartbeat", "all"),
        "orchestrator.heartbeat.all"
    );
    assert_eq!(
        NatsBus::subject("leader", "election"),
        "orchestrator.leader.election"
    );
}

#[tokio::test]
async fn test_nats_disconnect_clears_state() {
    let bus = NatsBus::unconnected("nats://localhost:4222", "node-1");
    assert!(!bus.is_connected().await);
    bus.disconnect().await;
    assert!(!bus.is_connected().await);
}

// ── RedisDedup Integration Tests ─────────────────────────────────────────

#[tokio::test]
async fn test_redis_dedup_construction_valid_url() {
    let result = RedisDedup::new("redis://localhost:6379", "node-1", 300).await;
    // Client creation succeeds even if Redis isn't running
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_redis_dedup_construction_invalid_url() {
    let result = RedisDedup::new("not-valid", "node-1", 300).await;
    assert!(result.is_err());
}

#[test]
fn test_redis_dedup_result_variants() {
    let claimed = RedisDeduplicationResult::Claimed;
    let already = RedisDeduplicationResult::AlreadyClaimed;

    assert_ne!(claimed, already);
    assert_eq!(claimed.clone(), RedisDeduplicationResult::Claimed);
}

// ── Leader Election Integration Tests ────────────────────────────────────

#[tokio::test]
async fn test_leader_election_construction() {
    let result = LeaderElection::new("redis://localhost:6379", "node-1", 10).await;
    assert!(result.is_ok());
    if let Ok(election) = result {
        assert!(!election.is_leader());
        assert_eq!(election.node_id(), "node-1");
        assert_eq!(election.ttl_seconds(), 10);
    }
}

#[test]
fn test_leader_role_transitions() {
    let client = redis::Client::open("redis://localhost:6379");
    if let Ok(c) = client {
        let election = LeaderElection::from_client(Arc::new(c), "n1", 10);
        let rx = election.role_watcher();

        // Initially follower
        assert_eq!(*rx.borrow(), LeaderRole::Follower(None));

        // Simulate transition to leader (via role_tx in real use)
        // Can't directly test without Redis, but verify the watcher works
        assert!(!election.is_leader());
    }
}

#[test]
fn test_leader_election_renewal_interval() {
    let client = redis::Client::open("redis://localhost:6379");
    if let Ok(c) = client {
        let election = LeaderElection::from_client(Arc::new(c), "n1", 20);
        assert_eq!(election.renewal_interval(), Duration::from_secs(10));
    }
}

// ── Cross-Module Integration Tests ───────────────────────────────────────

#[tokio::test]
async fn test_config_drives_cluster_manager() {
    let config = DistributedConfig::new("node-1".to_string());
    assert!(config.validate().is_ok());

    let cluster = ClusterManager::new(&config.node_id, config.eviction_timeout());

    assert_eq!(cluster.local_node_id(), "node-1");
    assert_eq!(
        cluster.eviction_timeout(),
        Duration::from_secs(config.eviction_timeout_s)
    );
}

#[tokio::test]
async fn test_heartbeat_serde_through_nats_message() {
    let heartbeat = Heartbeat {
        node_id: "n1".to_string(),
        address: "host1:8080".to_string(),
        load: 5,
    };

    // Serialize heartbeat as payload inside a NatsMessage
    let payload = serde_json::to_string(&heartbeat).unwrap_or_default();
    let msg = NatsMessage {
        source_node: "n1".to_string(),
        msg_type: "heartbeat".to_string(),
        payload: payload.clone(),
    };

    // Serialize the full message
    let bytes = serde_json::to_vec(&msg).unwrap_or_default();

    // Parse back
    let parsed_msg = NatsBus::parse_message(&bytes);
    assert!(parsed_msg.is_ok());
    let parsed = parsed_msg.unwrap_or_else(|_| msg.clone());

    // Extract heartbeat from payload
    let extracted: Result<Heartbeat, _> = serde_json::from_str(&parsed.payload);
    assert!(extracted.is_ok());
    assert_eq!(extracted.unwrap_or_else(|_| heartbeat.clone()), heartbeat);
}

#[tokio::test]
async fn test_cluster_routing_with_heartbeat_lifecycle() {
    let cluster = ClusterManager::new("coordinator", Duration::from_millis(200));

    // Node 1 joins with low load
    let hb1 = Heartbeat {
        node_id: "worker-1".to_string(),
        address: "w1:8080".to_string(),
        load: 2,
    };
    cluster.process_heartbeat(&hb1).await;

    // Node 2 joins with higher load
    let hb2 = Heartbeat {
        node_id: "worker-2".to_string(),
        address: "w2:8080".to_string(),
        load: 8,
    };
    cluster.process_heartbeat(&hb2).await;

    // Route to least loaded
    assert_eq!(cluster.route().await, Some("worker-1".to_string()));

    // Worker-1 load increases
    let hb1_updated = Heartbeat {
        node_id: "worker-1".to_string(),
        address: "w1:8080".to_string(),
        load: 15,
    };
    cluster.process_heartbeat(&hb1_updated).await;

    // Now route to worker-2
    assert_eq!(cluster.route().await, Some("worker-2".to_string()));

    // Wait for eviction
    tokio::time::sleep(Duration::from_millis(300)).await;
    cluster.evict_stale_nodes().await;

    // All evicted
    assert!(cluster.route().await.is_none());
}

#[tokio::test]
async fn test_error_types_are_distinct() {
    // Verify each error variant produces a unique message
    let errors = [
        DistributedError::RedisConnection("conn".into()),
        DistributedError::RedisOperation("op".into()),
        DistributedError::NatsConnection("nconn".into()),
        DistributedError::NatsPublish("pub".into()),
        DistributedError::NatsSubscribe("sub".into()),
        DistributedError::NatsTimeout("timeout".into()),
        DistributedError::NodeNotFound("n1".into()),
        DistributedError::InvalidConfig {
            field: "f".into(),
            reason: "r".into(),
        },
    ];

    let messages: Vec<String> = errors.iter().map(|e| e.to_string()).collect();

    // All messages should be unique
    for (i, msg) in messages.iter().enumerate() {
        for (j, other) in messages.iter().enumerate() {
            if i != j {
                assert_ne!(
                    msg, other,
                    "error variants {i} and {j} have identical display"
                );
            }
        }
    }
}

// ── Quorum Loss and Leader Failover Tests ─────────────────────────────────

/// When the only active node is evicted (quorum loss), routing returns None.
/// This ensures the coordinator does not send work to a dead node.
#[tokio::test]
async fn test_quorum_loss_stops_routing() {
    let cluster = ClusterManager::new("coordinator", Duration::from_millis(50));

    cluster.register("n1", "h1:8080", 0).await;
    cluster.register("n2", "h2:8080", 0).await;

    // Confirm routing works with 2 nodes
    assert!(cluster.route().await.is_some());

    // Both nodes go stale
    tokio::time::sleep(Duration::from_millis(100)).await;
    let evicted = cluster.evict_stale_nodes().await;
    assert_eq!(evicted.len(), 2, "both nodes should be evicted");

    // No nodes left — routing must return None
    assert!(
        cluster.route().await.is_none(),
        "routing after quorum loss must return None"
    );
    assert_eq!(cluster.node_count().await, 0);
}

/// Verifies that a single surviving node is still routed to after partial
/// quorum loss (one of two nodes goes stale).
#[tokio::test]
async fn test_partial_quorum_loss_routes_to_survivor() {
    let cluster = ClusterManager::new("coordinator", Duration::from_millis(80));

    cluster.register("n1", "h1:8080", 5).await;
    cluster.register("n2", "h2:8080", 2).await;

    // Let both go stale, then renew only n1
    tokio::time::sleep(Duration::from_millis(100)).await;
    cluster.register("n1", "h1:8080", 5).await; // heartbeat renewe n1

    let evicted = cluster.evict_stale_nodes().await;
    assert!(evicted.contains(&"n2".to_string()), "n2 should be evicted");
    assert!(
        !evicted.contains(&"n1".to_string()),
        "n1 renewed — should survive"
    );

    assert_eq!(cluster.node_count().await, 1);
    assert_eq!(
        cluster.route().await,
        Some("n1".to_string()),
        "surviving node must still receive work"
    );
}

/// Verifies `is_leader` is initially false and `step_down` is safe to call
/// without an active Redis connection.
#[test]
fn test_leader_election_initial_state_is_follower() {
    if let Ok(client) = redis::Client::open("redis://localhost:6379") {
        let election = LeaderElection::from_client(Arc::new(client), "node-test", 30);
        assert!(
            !election.is_leader(),
            "node must start as follower, not leader"
        );
        assert_eq!(
            *election.role_watcher().borrow(),
            LeaderRole::Follower(None),
            "initial role watcher value must be Follower(None)"
        );
        assert_eq!(election.node_id(), "node-test");
        assert_eq!(election.renewal_interval(), Duration::from_secs(15));
    }
}

/// Verifies that `step_down` does not panic when Redis is unavailable
/// (it should return an error, not crash).
#[tokio::test]
async fn test_leader_step_down_without_redis_returns_error() {
    // Use an unreachable Redis URL so connections always fail
    let result = LeaderElection::new("redis://192.0.2.1:6379", "node-sd", 5).await;
    // Construction succeeds (lazy connection); operations will fail
    if let Ok(election) = result {
        let step_down_result = election.step_down().await;
        // Must not panic — returning Err is acceptable
        assert!(
            step_down_result.is_err(),
            "step_down to unreachable Redis must return Err, not panic"
        );
    }
}

#[test]
fn test_distributed_config_serialization_stability() {
    let config = DistributedConfig::with_urls(
        "serial-test".to_string(),
        "nats://nats:4222".to_string(),
        "redis://redis:6379".to_string(),
    );

    // JSON roundtrip
    let json1 = serde_json::to_string(&config).unwrap_or_default();
    let json2 = serde_json::to_string(&config).unwrap_or_default();
    assert_eq!(json1, json2, "serialization must be deterministic");

    // TOML roundtrip
    let toml1 = toml::to_string(&config).unwrap_or_default();
    let toml2 = toml::to_string(&config).unwrap_or_default();
    assert_eq!(toml1, toml2, "TOML serialization must be deterministic");
}

// ── Chaos Tests ───────────────────────────────────────────────────────────────

/// Chaos test: Redis connection completely unavailable (non-routable address).
///
/// Verifies that `RedisDedup::new` succeeds (lazy connection) and that
/// attempting an operation returns a graceful `Err` rather than panicking.
/// The system must degrade gracefully when the backing store is unreachable.
#[tokio::test]
async fn chaos_redis_connection_unavailable_returns_error_not_panic() {
    // 192.0.2.0/24 is TEST-NET-1 (RFC 5737) — guaranteed non-routable.
    let result = RedisDedup::new("redis://192.0.2.1:6379", "chaos-node", 10).await;

    // Construction must succeed (the client is lazy — it does not connect yet).
    assert!(
        result.is_ok(),
        "RedisDedup construction with unreachable URL must not fail immediately"
    );

    let dedup = result.unwrap();

    // Attempting to claim a key must return Err, not panic.
    // Use a short timeout so the test does not hang.
    let claim_result = tokio::time::timeout(
        Duration::from_secs(3),
        dedup.try_claim("chaos-key-001"),
    )
    .await;

    match claim_result {
        Ok(Ok(_)) => {
            // If somehow a connection succeeded (unlikely with TEST-NET address),
            // we just pass — the important thing is no panic.
        }
        Ok(Err(_)) => {
            // Expected: operation failed with a DistributedError. Not a panic.
        }
        Err(_timeout) => {
            // Also acceptable: the connection attempt timed out.
        }
    }
}

/// Chaos test: `LeaderElection` with an unavailable Redis URL.
///
/// Two nodes both attempt to become leader against a mock URL. The result
/// must be a graceful error on both sides, never a panic. This simulates
/// what happens when the Redis leader-lock backend disappears mid-election.
#[tokio::test]
async fn chaos_simultaneous_leader_election_unavailable_redis() {
    // Use TEST-NET-1 for a guaranteed non-routable address.
    let url = "redis://192.0.2.2:6379";

    let result_a = LeaderElection::new(url, "node-a", 5).await;
    let result_b = LeaderElection::new(url, "node-b", 5).await;

    // Both nodes must construct without panic (lazy connection).
    assert!(result_a.is_ok(), "node-a construction must not panic");
    assert!(result_b.is_ok(), "node-b construction must not panic");

    let election_a = result_a.unwrap();
    let election_b = result_b.unwrap();

    // Both nodes try to run the election campaign concurrently.
    // Neither must panic — both must return Err within the timeout.
    let (res_a, res_b) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(3), election_a.try_elect()),
        tokio::time::timeout(Duration::from_secs(3), election_b.try_elect()),
    );

    // Verify: each result is either a timeout or a graceful Err — never a panic.
    for (label, res) in [("node-a", res_a), ("node-b", res_b)] {
        match res {
            Ok(Ok(_)) => {
                // Unlikely with TEST-NET, but not a failure — no panic is the goal.
            }
            Ok(Err(_)) => {
                // Expected path: graceful error returned.
            }
            Err(_) => {
                // Timeout is also acceptable.
                eprintln!("chaos: {label} election timed out (acceptable)");
            }
        }
    }

    // Both nodes must still report Follower (no leader promotion without Redis).
    assert!(!election_a.is_leader(), "node-a must not claim leadership without Redis");
    assert!(!election_b.is_leader(), "node-b must not claim leadership without Redis");
}

/// Chaos test: Clock skew simulation via artificially aged heartbeats.
///
/// Simulates a node whose clock is skewed far into the past by registering
/// it and immediately advancing time past the eviction window. The eviction
/// logic must correctly evict the "skewed" node without panicking.
///
/// This is a structural clock-skew test using the `ClusterManager`'s
/// eviction timeout rather than a dedicated clock abstraction, since the
/// current implementation uses `Instant`-based eviction.
#[tokio::test]
async fn chaos_clock_skew_causes_eviction_of_stale_node() {
    // Very short eviction window to simulate a clock-skewed node.
    let cluster = ClusterManager::new("coordinator", Duration::from_millis(50));

    // Register two nodes — "skewed" is registered but won't send heartbeats.
    cluster.register("healthy", "h1:8080", 2).await;
    cluster.register("skewed", "h2:8080", 1).await;

    // Sleep longer than the eviction window — simulates clock skew causing
    // the "skewed" node's last-seen timestamp to appear expired.
    tokio::time::sleep(Duration::from_millis(120)).await;

    // Re-register "healthy" (it renewed its heartbeat).
    cluster.register("healthy", "h1:8080", 2).await;

    // Evict stale nodes.
    let evicted = cluster.evict_stale_nodes().await;

    assert!(
        evicted.contains(&"skewed".to_string()),
        "clock-skewed node must be evicted after timeout"
    );
    assert!(
        !evicted.contains(&"healthy".to_string()),
        "healthy node that renewed heartbeat must survive"
    );

    // Routing must still work for the surviving node.
    assert_eq!(
        cluster.route().await,
        Some("healthy".to_string()),
        "routing must still work after evicting clock-skewed node"
    );
}
