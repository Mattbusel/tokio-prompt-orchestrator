//! Semantic dedup with a real local embedding model (fastembed, BGE-small-en-v1.5).
//!
//! Ignored by default because the first run downloads the model (128 MB)
//! and ONNX Runtime. Run with:
//!
//! ```text
//! cargo test --features fastembed --test semantic_dedup_tests -- --ignored --nocapture
//! ```
//!
//! The pairs below are what decide a threshold: questions that mean the same
//! thing must score above it, and questions that are related but need a
//! different answer must score below it. The printed scores are recorded in
//! the docs of `Deduplicator::with_embedder`.

#![cfg(feature = "fastembed")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use tokio_prompt_orchestrator::embedding::{cosine_similarity, Embedder};
use tokio_prompt_orchestrator::enhanced::{DeduplicationResult, Deduplicator};
use tokio_prompt_orchestrator::integrations::FastEmbedder;

/// Same question, different words: these should share an answer.
const SAME: &[(&str, &str)] = &[
    (
        "What is the capital of France?",
        "Which city is the capital of France?",
    ),
    (
        "How do I reverse a list in Python?",
        "What's the way to reverse a Python list?",
    ),
    (
        "Summarize this ticket: login fails after password reset",
        "Give me a summary of this ticket: login fails after password reset",
    ),
    (
        "What time zone is Tokyo in?",
        "Which time zone does Tokyo use?",
    ),
    ("How many ounces are in a pound?", "How many oz in one lb?"),
];

/// Related wording, different answer: these must NOT share an answer.
const DIFFERENT: &[(&str, &str)] = &[
    (
        "What is the capital of France?",
        "What is the capital of Germany?",
    ),
    (
        "How do I reverse a list in Python?",
        "How do I sort a list in Python?",
    ),
    (
        "Summarize this ticket: login fails after password reset",
        "Summarize this ticket: checkout fails after coupon applied",
    ),
    (
        "Convert 10 miles to kilometers",
        "Convert 10 kilometers to miles",
    ),
    ("Is 17 a prime number?", "Is 21 a prime number?"),
];

async fn score(embedder: &FastEmbedder, a: &str, b: &str) -> f32 {
    let va = embedder.embed(a).await.expect("embed a");
    let vb = embedder.embed(b).await.expect("embed b");
    cosine_similarity(&va, &vb)
}

#[tokio::test]
#[ignore = "downloads a 128 MB embedding model on first run"]
async fn bge_small_with_the_guard_never_reuses_a_different_answer() {
    use tokio_prompt_orchestrator::enhanced::same_specifics;
    let embedder = FastEmbedder::try_default().expect("load BGE-small");
    let threshold = 0.93;

    let mut reused = 0;
    for (a, b) in SAME {
        let s = score(&embedder, a, b).await;
        let ok = s >= threshold && same_specifics(a, b);
        reused += usize::from(ok);
        println!(
            "same      {s:.4} guard={:<5} reuse={ok:<5} {a:?} | {b:?}",
            same_specifics(a, b)
        );
    }
    for (a, b) in DIFFERENT {
        let s = score(&embedder, a, b).await;
        let ok = s >= threshold && same_specifics(a, b);
        println!(
            "different {s:.4} guard={:<5} reuse={ok:<5} {a:?} | {b:?}",
            same_specifics(a, b)
        );
        assert!(
            !ok,
            "would reuse the answer to a different question: {a:?} | {b:?}"
        );
    }
    println!(
        "same-meaning pairs reused at {threshold}: {reused} of {}",
        SAME.len()
    );
    // Embedding alone is NOT enough: "Convert 10 miles to kilometers" and
    // "Convert 10 kilometers to miles" score 0.99, above every true
    // paraphrase here. The guard is what keeps that pair apart.
}

#[tokio::test]
#[ignore = "downloads a 128 MB embedding model on first run"]
async fn paraphrase_reuses_the_answer_end_to_end() {
    let embedder: Arc<dyn Embedder> = Arc::new(FastEmbedder::try_default().expect("load"));
    let dedup = Deduplicator::new(Duration::from_secs(60)).with_embedder(embedder, 0.9);

    let first = "What is the capital of France?";
    match dedup.check_and_register_semantic("k1", first).await.0 {
        DeduplicationResult::New(token) => dedup.complete(token, "Paris.".into()).await,
        other => panic!("expected new, got {other:?}"),
    }

    let paraphrase = "Which city is the capital of France?";
    let (result, matched) = dedup.check_and_register_semantic("k2", paraphrase).await;
    assert!(
        matches!(result, DeduplicationResult::Cached(ref t) if t == "Paris."),
        "{result:?}"
    );
    println!(
        "paraphrase similarity {:.4}",
        matched.expect("semantic").similarity
    );

    let different = "What is the capital of Germany?";
    let (result, _) = dedup.check_and_register_semantic("k3", different).await;
    assert!(
        matches!(result, DeduplicationResult::New(_)),
        "Germany must not get France's answer"
    );
}
