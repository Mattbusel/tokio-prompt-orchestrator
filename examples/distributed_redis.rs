//! # Example: Distributed Redis Deduplication
//!
//! Demonstrates: Cross-node request deduplication using Redis.
//!
//! Run with: `cargo run --example distributed_redis --features distributed`
//! Features needed: distributed
//!
//! Prerequisites:
//!   - A Redis server reachable at `redis://127.0.0.1:6379` (or override via
//!     `REDIS_URL` env var).
//!   - Start Redis: `docker run -p 6379:6379 redis:7-alpine`
//!
//! What you will see:
//!   - Two simulated pipeline nodes share the same Redis instance.
//!   - Both nodes receive the same request key at the same time.
//!   - Only one node successfully "claims" the request; the other sees
//!     `AlreadyClaimed` and skips processing.
//!   - This prevents duplicate expensive model calls across a cluster.

// ── Guard: if the `distributed` feature is disabled, print a helpful message ─

#[cfg(not(feature = "distributed"))]
fn main() {
    eprintln!();
    eprintln!("This example requires the `distributed` feature flag.");
    eprintln!();
    eprintln!("Run with:");
    eprintln!("  cargo run --example distributed_redis --features distributed");
    eprintln!();
    eprintln!("Prerequisites:");
    eprintln!("  docker run -p 6379:6379 redis:7-alpine");
    eprintln!();
}

// ── Full implementation when `distributed` feature is enabled ─────────────────

#[cfg(feature = "distributed")]
mod inner {
    use std::time::Duration;
    use tokio_prompt_orchestrator::distributed::{RedisDedup, RedisDeduplicationResult};

    /// Simulates one pipeline node claiming and optionally processing a request.
    async fn node_process(
        node_id: &str,
        dedup: &RedisDedup,
        request_key: &str,
    ) -> RedisDeduplicationResult {
        match dedup.try_claim(request_key).await {
            Ok(result) => {
                match &result {
                    RedisDeduplicationResult::Claimed => {
                        println!(
                            "  [{}] CLAIMED key='{}' — running inference...",
                            node_id, request_key
                        );
                        // Simulate model inference latency.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        println!("  [{}] inference complete.", node_id);
                    }
                    RedisDeduplicationResult::AlreadyClaimed => {
                        println!(
                            "  [{}] ALREADY_CLAIMED key='{}' — skipping (another node is handling it).",
                            node_id, request_key
                        );
                    }
                }
                result
            }
            Err(e) => {
                eprintln!("  [{}] Redis error: {e}", node_id);
                // Treat Redis failure as "not claimed" so the caller can fall back.
                RedisDeduplicationResult::AlreadyClaimed
            }
        }
    }

    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        println!();
        println!("Distributed Redis Deduplication Demo");
        println!("=====================================");
        println!();

        let redis_url = std::env::var("REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());

        println!("Connecting to Redis: {redis_url}");
        println!();

        // Create two RedisDedup instances — one per simulated node.
        // In production each would run in a separate process on a different host,
        // but they all point at the same Redis instance.
        let node_a = match RedisDedup::new(&redis_url, "node-A", 60).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Could not connect to Redis: {e}");
                eprintln!();
                eprintln!("Is Redis running?  Try: docker run -p 6379:6379 redis:7-alpine");
                return Err(e.into());
            }
        };
        let node_b = RedisDedup::new(&redis_url, "node-B", 60).await?;

        println!("Connected.  Two nodes created: node-A and node-B.");
        println!();

        // ── Scenario 1: Same key, sequential claims ───────────────────────
        println!("Scenario 1: Sequential claims for the same request key");
        println!("-------------------------------------------------------");

        let key = "dedup:scenario-1:prompt-hash-abc123";

        let result_a = node_process("node-A", &node_a, key).await;
        let result_b = node_process("node-B", &node_b, key).await;

        assert_eq!(
            result_a,
            RedisDeduplicationResult::Claimed,
            "node-A should claim first"
        );
        assert_eq!(
            result_b,
            RedisDeduplicationResult::AlreadyClaimed,
            "node-B should see it already claimed"
        );
        println!();
        println!("  Result: node-A processed, node-B skipped.");
        println!("  Cross-node dedup prevented duplicate inference.");

        // ── Scenario 2: Concurrent claims (race condition) ────────────────
        println!();
        println!("Scenario 2: Concurrent claims — simulating a race condition");
        println!("-------------------------------------------------------------");
        println!("  Both nodes attempt to claim the same key at the same time.");

        let key2 = "dedup:scenario-2:prompt-hash-xyz789";
        let node_a2 = node_a.clone();
        let node_b2 = node_b.clone();
        let key2_a = key2.to_string();
        let key2_b = key2.to_string();

        let (result_a2, result_b2) = tokio::join!(
            tokio::spawn(async move { node_process("node-A", &node_a2, &key2_a).await }),
            tokio::spawn(async move { node_process("node-B", &node_b2, &key2_b).await }),
        );

        let ra = result_a2.unwrap_or(RedisDeduplicationResult::AlreadyClaimed);
        let rb = result_b2.unwrap_or(RedisDeduplicationResult::AlreadyClaimed);

        // Exactly one node must have claimed it.
        let claimed_count = [&ra, &rb]
            .iter()
            .filter(|r| ***r == RedisDeduplicationResult::Claimed)
            .count();

        println!();
        println!("  node-A result: {:?}", ra);
        println!("  node-B result: {:?}", rb);
        println!("  Nodes that ran inference: {claimed_count} / 2");
        assert_eq!(claimed_count, 1, "exactly one node should claim the key");
        println!("  Redis SET NX guaranteed exactly-once execution.");

        // ── Scenario 3: Different keys are independent ────────────────────
        println!();
        println!("Scenario 3: Different request keys are claimed independently");
        println!("--------------------------------------------------------------");

        let key3a = "dedup:scenario-3:prompt-hash-111";
        let key3b = "dedup:scenario-3:prompt-hash-222";

        let r3a = node_process("node-A", &node_a, key3a).await;
        let r3b = node_process("node-B", &node_b, key3b).await;

        assert_eq!(r3a, RedisDeduplicationResult::Claimed);
        assert_eq!(r3b, RedisDeduplicationResult::Claimed);
        println!();
        println!("  Both nodes claimed different keys — no interference.");

        println!();
        println!("Summary");
        println!("=======");
        println!("  Redis SET NX EX provides atomic cross-node dedup.");
        println!("  Only one node processes each unique request, even under races.");
        println!("  Keys auto-expire (TTL=60s) so the lock is never held forever.");
        println!("  Use `RedisDedup::new(redis_url, node_id, ttl_seconds)` to set up.");

        Ok(())
    }
}

#[cfg(feature = "distributed")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    inner::run().await
}
