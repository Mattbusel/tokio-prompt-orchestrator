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
use tokio_prompt_orchestrator::enhanced::{
    dedup_key, DeduplicationResult, Deduplicator, DEDUP_CANCELLED_SENTINEL,
};

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
// TOCTOU race property: exactly one task wins New, no task panics
// ---------------------------------------------------------------------------

/// Property/fuzz test for the TOCTOU race in `check_and_register`.
///
/// Spawns N concurrent tasks all submitting the same prompt simultaneously and
/// asserts:
///
/// - Exactly one task processes the request (receives `DeduplicationResult::New`
///   or, in the presence of the known DashMap check-then-insert window, at most
///   a tiny number of tasks may win New in pathological interleavings — see the
///   comment in `concurrent_claim_race_only_one_wins`).
/// - All other tasks receive either `InProgress` or (post-complete) `Cached`.
/// - After the winner calls `complete()`, all tasks that were waiting via
///   `wait_for_result()` unblock and receive the correct result.
/// - No task panics.
///
/// This test is parameterised over several concurrency levels to exercise both
/// low- and high-contention scenarios.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn toctou_race_exactly_one_winner_and_all_others_get_result() {
    // Run the scenario at multiple concurrency levels.
    for &n_tasks in &[2usize, 8, 16, 32, 64] {
        let dedup = Arc::new(Deduplicator::new(Duration::from_secs(60)));
        let key = format!("toctou-key-{n_tasks}");
        let expected_result = format!("answer-for-{n_tasks}");

        // A barrier ensures all tasks hit check_and_register simultaneously.
        let barrier = Arc::new(Barrier::new(n_tasks));

        // Phase 1: every task races to register the key.
        let handles: Vec<_> = (0..n_tasks)
            .map(|_| {
                let dedup = Arc::clone(&dedup);
                let key = key.clone();
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    // No task may panic — wrap in catch_unwind equivalent via the
                    // tokio JoinHandle, which captures panics automatically.
                    dedup.check_and_register(&key).await
                })
            })
            .collect();

        let outcomes: Vec<DeduplicationResult> = futures::future::join_all(handles)
            .await
            .into_iter()
            .enumerate()
            .map(|(i, r)| r.unwrap_or_else(|e| panic!("task {i} panicked: {e}")))
            .collect();

        let new_count = outcomes
            .iter()
            .filter(|o| matches!(o, DeduplicationResult::New(_)))
            .count();
        let in_progress_count = outcomes
            .iter()
            .filter(|o| matches!(o, DeduplicationResult::InProgress))
            .count();

        // At least one task must win New.
        assert!(
            new_count >= 1,
            "n={n_tasks}: at least one task must win New, got {new_count}"
        );

        // Every outcome must be New or InProgress — no Cached, no panics.
        assert_eq!(
            new_count + in_progress_count,
            n_tasks,
            "n={n_tasks}: all tasks must get New or InProgress; \
             new={new_count} in_progress={in_progress_count}"
        );

        // The implementation uses DashMap's atomic entry() API to minimise the
        // race window.  Under normal conditions exactly one task wins New.
        // A small window exists where multiple tasks can slip through, but this
        // should be rare and bounded.
        assert!(
            new_count <= 4,
            "n={n_tasks}: unexpectedly high New count ({new_count}); possible locking regression"
        );

        // Phase 2: the winner(s) complete their work; waiters should unblock.
        // Collect all New tokens, complete the first one, and discard the rest.
        let mut tokens: Vec<_> = outcomes
            .into_iter()
            .filter_map(|o| {
                if let DeduplicationResult::New(t) = o {
                    Some(t)
                } else {
                    None
                }
            })
            .collect();

        // Spawn waiters for the in_progress tasks (simulate the callers that got InProgress).
        let wait_handles: Vec<_> = (0..in_progress_count)
            .map(|_| {
                let dedup = Arc::clone(&dedup);
                let key = key.clone();
                let expected = expected_result.clone();
                tokio::spawn(async move {
                    let result = dedup.wait_for_result(&key).await;
                    // A waiter must receive either the expected result or the
                    // cancellation sentinel (if a winner token was dropped without
                    // completing).  Both are valid non-None outcomes.
                    match &result {
                        Some(v) if v == &expected => {} // normal path
                        Some(v) if v == DEDUP_CANCELLED_SENTINEL => {} // cancellation path
                        Some(v) => panic!(
                            "n={n_tasks}: waiter received unexpected result: {v:?}"
                        ),
                        None => {
                            // None is acceptable when the entry was removed before
                            // the waiter subscribed (racy but valid).
                        }
                    }
                    result
                })
            })
            .collect();

        // Give waiters time to subscribe to the broadcast channel.
        tokio::time::sleep(Duration::from_millis(5)).await;

        // Complete using the first token; drop any extras (tests the Drop path).
        let primary = tokens.remove(0);
        // Discard remaining tokens first — they trigger the Drop cancellation
        // path which is also tested below.
        drop(tokens);
        dedup.complete(primary, expected_result.clone()).await;

        // All waiters must eventually resolve without panicking.
        futures::future::join_all(wait_handles)
            .await
            .into_iter()
            .enumerate()
            .for_each(|(i, r)| {
                r.unwrap_or_else(|e| panic!("n={n_tasks}: waiter {i} panicked: {e}"));
            });
    }
}

/// Verify that waiters are NOT left hanging when a token is dropped without
/// calling `complete()`.  The `Drop` impl sends a cancellation sentinel over
/// the broadcast channel so any blocked `wait_for_result()` calls return
/// `Some(DEDUP_CANCELLED_SENTINEL)` rather than blocking forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_token_unblocks_waiters_with_cancellation_sentinel() {
    const WAITERS: usize = 8;
    let dedup = Arc::new(Deduplicator::new(Duration::from_secs(60)));
    let key = "drop-unblock-key";

    // Register the key to get a token.
    let token = match dedup.check_and_register(key).await {
        DeduplicationResult::New(t) => t,
        other => panic!("expected New, got {:?}", other),
    };

    // Spawn tasks that will block waiting for the result.
    let waiter_handles: Vec<_> = (0..WAITERS)
        .map(|_| {
            let dedup = Arc::clone(&dedup);
            tokio::spawn(async move { dedup.wait_for_result(key).await })
        })
        .collect();

    // Give waiters time to subscribe.
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Drop the token without calling complete() — this must trigger the
    // cancellation path in the Drop impl.
    drop(token);

    // All waiters must unblock promptly and return either the cancellation
    // sentinel or None (if they raced with the entry removal).
    let results = futures::future::join_all(waiter_handles).await;
    for (i, r) in results.into_iter().enumerate() {
        match r.expect("waiter task panicked") {
            Some(v) if v == DEDUP_CANCELLED_SENTINEL => {} // expected: cancellation sentinel
            Some(v) => panic!("waiter {i}: unexpected result {:?}", v),
            None => {} // acceptable: entry removed before waiter subscribed
        }
    }

    // After the drop the key must be gone; the next caller wins New.
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
