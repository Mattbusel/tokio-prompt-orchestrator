//! Property-based tests using `proptest`.
//!
//! These tests exercise invariants that must hold for *any* valid input,
//! not just the specific values chosen in unit tests.

use proptest::prelude::*;
use tokio_prompt_orchestrator::{shard_session, DeadLetterQueue, DroppedRequest, SessionId};

// ── shard_session invariants ───────────────────────────────────────────────

proptest! {
    /// For any session string and shard count in 1..=256, the returned shard
    /// index must be strictly less than `shards`.
    #[test]
    fn shard_index_in_bounds(session in "\\PC*", shards in 1usize..=256) {
        let idx = shard_session(&SessionId::new(&session), shards);
        prop_assert!(idx < shards, "shard={idx} >= shards={shards}");
    }

    /// The same session always maps to the same shard (determinism).
    #[test]
    fn shard_is_deterministic(session in "\\PC*", shards in 1usize..=256) {
        let a = shard_session(&SessionId::new(&session), shards);
        let b = shard_session(&SessionId::new(&session), shards);
        prop_assert_eq!(a, b);
    }

    /// shard_session(_, 0) must not panic and must return 0.
    #[test]
    fn shard_zero_shards_returns_zero(session in "\\PC*") {
        let idx = shard_session(&SessionId::new(&session), 0);
        prop_assert_eq!(idx, 0);
    }
}

// ── DeadLetterQueue invariants ─────────────────────────────────────────────

fn make_dropped(id: &str, reason: &str) -> DroppedRequest {
    DroppedRequest {
        request_id: id.to_string(),
        session_id: "s".to_string(),
        reason: reason.to_string(),
        dropped_at: std::time::SystemTime::now(),
    }
}

proptest! {
    /// After pushing N items into a DLQ of capacity C, len() <= C.
    #[test]
    fn dlq_never_exceeds_capacity(
        capacity in 1usize..=64,
        pushes   in 0usize..=128,
    ) {
        let dlq = DeadLetterQueue::new(capacity);
        for i in 0..pushes {
            dlq.push(make_dropped(&i.to_string(), "test"));
        }
        prop_assert!(dlq.len() <= capacity);
    }

    /// drain() returns at most `capacity` items and leaves the queue empty.
    #[test]
    fn dlq_drain_empties_queue(
        capacity in 1usize..=32,
        pushes   in 0usize..=64,
    ) {
        let dlq = DeadLetterQueue::new(capacity);
        for i in 0..pushes {
            dlq.push(make_dropped(&i.to_string(), "drain-test"));
        }
        let drained = dlq.drain();
        prop_assert!(drained.len() <= capacity);
        prop_assert!(dlq.is_empty());
    }
}
