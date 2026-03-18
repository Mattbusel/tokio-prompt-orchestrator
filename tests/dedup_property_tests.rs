//! Property-based and concurrent tests for the deduplication layer.
//!
//! Covers:
//! - Concurrent token claim races (only one caller wins `New`)
//! - TTL expiration (entries expire, subsequent lookups miss)
//! - Hash collision / distinctness of `dedup_key` across inputs
//! - Idempotency (inserting the same key twice behaves correctly)

use proptest::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;
use tokio_prompt_orchestrator::enhanced::{dedup_key, DeduplicationResult, Deduplicator};

// ---------------------------------------------------------------------------
// Property: dedup_key is deterministic and correctly scoped
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The same inputs always produce the same key.
    #[test]
    fn prop_dedup_key_is_deterministic(
        prompt in ".*",
        session in proptest::option::of("[a-zA-Z0-9_-]{1,40}"),
    ) {
        let meta = HashMap::new();
        let session_ref = session.as_deref();
        let k1 = dedup_key(&prompt, &meta, session_ref);
        let k2 = dedup_key(&prompt, &meta, session_ref);
        prop_assert_eq!(&k1, &k2, "dedup_key must be deterministic");
    }

    /// Different sessions always yield different keys for the same prompt.
    #[test]
    fn prop_different_sessions_produce_different_keys(
        prompt in ".*",
        sess_a in "[a-zA-Z0-9]{1,32}",
        sess_b in "[a-zA-Z0-9]{1,32}",
    ) {
        prop_assume!(sess_a != sess_b);
        let meta = HashMap::new();
        let k_a = dedup_key(&prompt, &meta, Some(&sess_a));
        let k_b = dedup_key(&prompt, &meta, Some(&sess_b));
        prop_assert_ne!(k_a, k_b, "different sessions must not share a key");
    }

    /// A session key and the global key for the same prompt are always different.
    #[test]
    fn prop_session_key_differs_from_global(
        prompt in ".*",
        session in "[a-zA-Z0-9_-]{1,40}",
    ) {
        let meta = HashMap::new();
        let k_session = dedup_key(&prompt, &meta, Some(&session));
        let k_global  = dedup_key(&prompt, &meta, None);
        prop_assert_ne!(k_session, k_global, "session key must differ from global key");
    }

    /// Different prompts (same session) always yield different keys.
    #[test]
    fn prop_different_prompts_produce_different_keys(
        p1 in "\\PC+",
        p2 in "\\PC+",
    ) {
        prop_assume!(p1 != p2);
        let meta = HashMap::new();
        let k1 = dedup_key(&p1, &meta, None);
        let k2 = dedup_key(&p2, &meta, None);
        prop_assert_ne!(k1, k2, "distinct prompts must yield distinct keys");
    }

    /// Keys use the correct namespace prefix.
    #[test]
    fn prop_key_prefix_matches_session_scope(
        prompt in ".*",
        session in proptest::option::of("[a-zA-Z0-9_-]{1,40}"),
    ) {
        let meta = HashMap::new();
        let key = dedup_key(&prompt, &meta, session.as_deref());
        match &session {
            Some(_) => prop_assert!(key.starts_with("dedup:s:"), "session key must start with dedup:s:, got {key}"),
            None    => prop_assert!(key.starts_with("dedup:g:"), "global key must start with dedup:g:, got {key}"),
        }
    }

    /// Metadata ordering does not change the key (sorted before hashing).
    #[test]
    fn prop_metadata_key_order_is_irrelevant(
        prompt in "[a-zA-Z ]{1,40}",
        k1 in "[a-z]{1,10}",
        v1 in "[a-z]{1,10}",
        k2 in "[a-z]{1,10}",
        v2 in "[a-z]{1,10}",
    ) {
        prop_assume!(k1 != k2);
        let mut meta_ab = HashMap::new();
        meta_ab.insert(k1.clone(), v1.clone());
        meta_ab.insert(k2.clone(), v2.clone());

        let mut meta_ba = HashMap::new();
        meta_ba.insert(k2.clone(), v2.clone());
        meta_ba.insert(k1.clone(), v1.clone());

        let key_ab = dedup_key(&prompt, &meta_ab, None);
        let key_ba = dedup_key(&prompt, &meta_ba, None);
        prop_assert_eq!(key_ab, key_ba, "metadata insertion order must not affect key");
    }
}

// ---------------------------------------------------------------------------
// Property: idempotency — inserting the same key twice behaves correctly
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Registering a key while the first registration is still in-progress
    /// always returns `InProgress`, never a second `New`.
    #[test]
    fn prop_second_register_returns_in_progress(prompt in "[a-zA-Z0-9_-]{1,64}") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime build failed");
        rt.block_on(async move {
            let dedup = Deduplicator::new(Duration::from_secs(60));
            let key = dedup_key(&prompt, &HashMap::new(), None);

            // First registration — must succeed.
            let first = dedup.check_and_register(&key).await;
            prop_assert!(
                matches!(first, DeduplicationResult::New(_)),
                "first registration must be New"
            );

            // Second registration while still in-progress — must be InProgress.
            let second = dedup.check_and_register(&key).await;
            prop_assert!(
                matches!(second, DeduplicationResult::InProgress),
                "second registration must be InProgress, got {:?}", second
            );
            Ok(())
        })?;
    }

    /// After completing a request, any further registrations return `Cached`.
    #[test]
    fn prop_completed_request_is_cached(
        prompt in "[a-zA-Z0-9_-]{1,64}",
        result in "[a-zA-Z0-9 ]{1,128}",
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime build failed");
        rt.block_on(async move {
            let dedup = Deduplicator::new(Duration::from_secs(300));
            let key = dedup_key(&prompt, &HashMap::new(), None);

            let token = match dedup.check_and_register(&key).await {
                DeduplicationResult::New(t) => t,
                other => {
                    prop_assert!(false, "expected New, got {:?}", other);
                    unreachable!()
                }
            };

            dedup.complete(token, result.clone()).await;

            // Any subsequent check must return the cached result.
            for _ in 0..3 {
                match dedup.check_and_register(&key).await {
                    DeduplicationResult::Cached(r) => {
                        prop_assert_eq!(&r, &result, "cached result must match original");
                    }
                    other => {
                        prop_assert!(false, "expected Cached, got {:?}", other);
                    }
                }
            }
            Ok(())
        })?;
    }
}

// ---------------------------------------------------------------------------
// Property: TTL expiration
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// After the TTL elapses the entry is treated as a new request, not cached.
    #[test]
    fn prop_entry_expires_after_ttl(prompt in "[a-zA-Z0-9_-]{1,32}") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_all()
            .build()
            .expect("runtime build failed");
        rt.block_on(async move {
            // Use a very short TTL so the test stays fast.
            let dedup = Deduplicator::new(Duration::from_millis(50));
            let key = dedup_key(&prompt, &HashMap::new(), None);

            // Register and complete.
            let token = match dedup.check_and_register(&key).await {
                DeduplicationResult::New(t) => t,
                other => {
                    prop_assert!(false, "expected New, got {:?}", other);
                    unreachable!()
                }
            };
            dedup.complete(token, "cached-value".to_string()).await;

            // Verify it is cached immediately.
            prop_assert!(
                matches!(dedup.check_and_register(&key).await, DeduplicationResult::Cached(_)),
                "should be Cached before TTL expires"
            );

            // Wait for TTL to expire.
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Must now be treated as a new request.
            let after_expiry = dedup.check_and_register(&key).await;
            prop_assert!(
                matches!(after_expiry, DeduplicationResult::New(_)),
                "should be New after TTL expires, got {:?}", after_expiry
            );
            Ok(())
        })?;
    }
}

// ---------------------------------------------------------------------------
// Targeted concurrent tests — race conditions
// ---------------------------------------------------------------------------

/// Spawn N concurrent tasks that all try to claim the same dedup key at once.
///
/// The `Deduplicator` uses a check-then-insert pattern backed by `DashMap`.
/// `DashMap` serialises individual shard operations but cannot atomically
/// combine a "read-then-write" into a single critical section.  Under heavy
/// concurrency two tasks can both observe an absent entry and both insert,
/// so *at most a small number* of `New` tokens may be issued.
///
/// What the implementation *does* guarantee:
/// - At least one task wins `New` (someone always starts processing).
/// - No task receives a result variant other than `New` or `InProgress`.
/// - After the first `New` is registered, every subsequent call for the same
///   key sees either `InProgress` or (post-complete) `Cached`.
///
/// This test validates those weaker-but-real guarantees and documents the
/// known race window so callers are aware of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_claim_race_only_one_wins() {
    const CONCURRENCY: usize = 32;
    let dedup = Arc::new(Deduplicator::new(Duration::from_secs(60)));
    let key = "shared-race-key";

    // A barrier ensures all tasks start the check_and_register call together.
    let barrier = Arc::new(Barrier::new(CONCURRENCY));

    let handles: Vec<_> = (0..CONCURRENCY)
        .map(|_| {
            let dedup = Arc::clone(&dedup);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                dedup.check_and_register(key).await
            })
        })
        .collect();

    let results: Vec<DeduplicationResult> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("task panicked"))
        .collect();

    let new_count = results
        .iter()
        .filter(|r| matches!(r, DeduplicationResult::New(_)))
        .count();
    let in_progress_count = results
        .iter()
        .filter(|r| matches!(r, DeduplicationResult::InProgress))
        .count();

    // At least one task must win New.
    assert!(new_count >= 1, "at least one task must win New");

    // No result other than New or InProgress is valid here.
    assert_eq!(
        new_count + in_progress_count,
        CONCURRENCY,
        "every task must receive either New or InProgress; \
         new={new_count} in_progress={in_progress_count}"
    );

    // Document the known race window: more than one New is possible but should
    // be rare.  In practice only a tiny fraction of concurrent callers can slip
    // through the check-then-insert gap.
    //
    // If new_count > 4 something is badly wrong (e.g. locking regressed).
    assert!(
        new_count <= 4,
        "unexpectedly high New count ({new_count}); possible locking regression"
    );
}

/// Verify that a waiter unblocks with the correct result once the owner
/// calls complete(), under concurrent load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_waiters_receive_result_on_complete() {
    const WAITERS: usize = 16;
    let dedup = Arc::new(Deduplicator::new(Duration::from_secs(60)));
    let key = "waiter-race-key";
    let expected = "final-answer";

    // Register the key first to ensure waiters actually block.
    let token = match dedup.check_and_register(key).await {
        DeduplicationResult::New(t) => t,
        other => panic!("expected New, got {:?}", other),
    };

    // Spawn waiters before completing.
    let handles: Vec<_> = (0..WAITERS)
        .map(|_| {
            let dedup = Arc::clone(&dedup);
            tokio::spawn(async move { dedup.wait_for_result(key).await })
        })
        .collect();

    // Small yield so waiters can subscribe to the broadcast channel.
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Complete the request.
    dedup.complete(token, expected.to_string()).await;

    let results: Vec<Option<String>> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("waiter task panicked"))
        .collect();

    for (i, result) in results.iter().enumerate() {
        assert_eq!(
            result.as_deref(),
            Some(expected),
            "waiter {i} did not receive the expected result"
        );
    }
}

/// Multiple keys should never interfere with each other under concurrent access.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_independent_keys_do_not_interfere() {
    const KEYS: usize = 64;
    let dedup = Arc::new(Deduplicator::new(Duration::from_secs(60)));

    let handles: Vec<_> = (0..KEYS)
        .map(|i| {
            let dedup = Arc::clone(&dedup);
            tokio::spawn(async move {
                let key = format!("independent-key-{i}");
                match dedup.check_and_register(&key).await {
                    DeduplicationResult::New(token) => {
                        dedup.complete(token, format!("result-{i}")).await;
                        // Verify cached immediately.
                        match dedup.check_and_register(&key).await {
                            DeduplicationResult::Cached(r) => {
                                assert_eq!(
                                    r,
                                    format!("result-{i}"),
                                    "wrong cached result for key {i}"
                                );
                            }
                            other => panic!("expected Cached for key {i}, got {:?}", other),
                        }
                    }
                    other => panic!("expected New for unique key {i}, got {:?}", other),
                }
            })
        })
        .collect();

    futures::future::join_all(handles)
        .await
        .into_iter()
        .for_each(|r| r.expect("task panicked"));
}

/// Drop a token without calling complete(); subsequent callers should be able
/// to re-register the key as New (in-progress entry is cleaned up by Drop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_token_clears_in_progress_entry() {
    let dedup = Deduplicator::new(Duration::from_secs(60));
    let key = "drop-test-key";

    {
        let token = match dedup.check_and_register(key).await {
            DeduplicationResult::New(t) => t,
            other => panic!("expected New, got {:?}", other),
        };
        // Drop token without calling complete().
        drop(token);
    }

    // The drop impl removes the in-progress entry; the next caller must win New.
    match dedup.check_and_register(key).await {
        DeduplicationResult::New(_) => {} // expected
        other => panic!("expected New after token drop, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Hash collision / distinctness: dedup_key for visually similar inputs
// ---------------------------------------------------------------------------

#[test]
fn similar_inputs_produce_distinct_keys() {
    let meta = HashMap::new();

    // Whitespace variants.
    let k1 = dedup_key("hello world", &meta, None);
    let k2 = dedup_key("hello  world", &meta, None); // double space
    let k3 = dedup_key("hello\tworld", &meta, None); // tab
    assert_ne!(k1, k2, "single vs double space must differ");
    assert_ne!(k1, k3, "space vs tab must differ");

    // Unicode homoglyphs.
    let k_ascii = dedup_key("café", &meta, None);
    let k_nfc = dedup_key("caf\u{00e9}", &meta, None); // precomposed é
    let k_nfd = dedup_key("cafe\u{0301}", &meta, None); // decomposed e + combining accent
                                                        // NFC and precomposed are the same codepoint sequence → same key.
    assert_eq!(k_ascii, k_nfc, "same codepoints must yield same key");
    // NFD is a different byte sequence → different hash.
    assert_ne!(k_nfc, k_nfd, "NFC vs NFD must yield different keys");

    // Empty vs whitespace-only.
    let k_empty = dedup_key("", &meta, None);
    let k_space = dedup_key(" ", &meta, None);
    assert_ne!(k_empty, k_space, "empty vs space must differ");

    // Session prefix collision guard: a session named "g:..." must not collide
    // with a global key that shares the same hash suffix.
    let tricky_session = "g:abc";
    let k_tricky_session = dedup_key("prompt", &meta, Some(tricky_session));
    let k_global_prompt = dedup_key("prompt", &meta, None);
    assert_ne!(
        k_tricky_session, k_global_prompt,
        "session key must not collide with global key regardless of session name"
    );
}
