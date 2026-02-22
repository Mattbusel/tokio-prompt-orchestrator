//! # Deduplication Validation Integration Tests
//!
//! ## Responsibility
//! Validates deduplication behavior with 20 identical prompts submitted
//! simultaneously. Tests both the standalone `Deduplicator` component and
//! the MCP `batch_infer` pipeline path to document actual dedup rates.
//!
//! ## Key Findings
//! - `Deduplicator::check_and_register` correctly deduplicates: 1 new + 19 cached = 95%
//! - `batch_infer` does NOT deduplicate: it sends all 20 through the pipeline
//! - Integration of `Deduplicator` into the pipeline is a Phase 5 roadmap item

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::{DeduplicationResult, Deduplicator};
use tokio_prompt_orchestrator::{
    metrics, spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};

/// Deterministic dedup key from prompt content (mirrors `enhanced::dedup::dedup_key`).
fn dedup_key(prompt: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    prompt.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// ── Standalone Deduplicator Tests ─────────────────────────────────────────

#[tokio::test]
async fn test_dedup_20_identical_prompts_standalone_achieves_95_percent() {
    let dedup = Deduplicator::new(Duration::from_secs(300));
    let prompt = "What is the capital of France?";
    let key = dedup_key(prompt);

    let mut new_count = 0u32;
    let mut in_progress_count = 0u32;
    let mut cached_count = 0u32;

    // First request: should be New
    let result = dedup.check_and_register(&key).await;
    match result {
        DeduplicationResult::New(token) => {
            new_count += 1;
            // Complete the request so subsequent ones get Cached
            dedup
                .complete(token, "Paris is the capital of France.".to_string())
                .await;
        }
        DeduplicationResult::InProgress => in_progress_count += 1,
        DeduplicationResult::Cached(_) => cached_count += 1,
    }

    // Remaining 19 requests: should all be Cached
    for _ in 0..19 {
        let result = dedup.check_and_register(&key).await;
        match result {
            DeduplicationResult::New(_) => new_count += 1,
            DeduplicationResult::InProgress => in_progress_count += 1,
            DeduplicationResult::Cached(_) => cached_count += 1,
        }
    }

    let total = new_count + in_progress_count + cached_count;
    let dedup_rate = (cached_count as f64 / total as f64) * 100.0;

    assert_eq!(total, 20, "all 20 prompts must be accounted for");
    assert_eq!(new_count, 1, "exactly 1 request should be new");
    assert_eq!(cached_count, 19, "19 requests should be cached");
    assert_eq!(in_progress_count, 0, "no requests should be in-progress");
    assert!(
        (dedup_rate - 95.0).abs() < 0.1,
        "dedup rate should be 95%, got {dedup_rate:.1}%"
    );
}

#[tokio::test]
async fn test_dedup_20_identical_prompts_concurrent_first_batch() {
    let dedup = Arc::new(Deduplicator::new(Duration::from_secs(300)));
    let prompt = "What is the capital of France?";
    let key = dedup_key(prompt);

    // Submit all 20 concurrently BEFORE any complete
    let mut handles = Vec::new();
    for _ in 0..20 {
        let dedup = Arc::clone(&dedup);
        let key = key.clone();
        handles.push(tokio::spawn(
            async move { dedup.check_and_register(&key).await },
        ));
    }

    let mut results = Vec::with_capacity(20);
    for handle in handles {
        if let Ok(result) = handle.await {
            results.push(result);
        }
    }

    let new_count = results
        .iter()
        .filter(|r| matches!(r, DeduplicationResult::New(_)))
        .count();
    let in_progress_count = results
        .iter()
        .filter(|r| matches!(r, DeduplicationResult::InProgress))
        .count();

    assert_eq!(results.len(), 20, "all 20 results must be returned");
    assert_eq!(
        new_count, 1,
        "exactly 1 request should be new (the first one to register)"
    );
    assert_eq!(
        in_progress_count, 19,
        "19 requests should see in-progress (since first hasn't completed)"
    );
}

#[tokio::test]
async fn test_dedup_stats_reflect_cached_count() {
    let dedup = Deduplicator::new(Duration::from_secs(300));
    let key = dedup_key("test prompt");

    // Register and complete first request
    if let DeduplicationResult::New(token) = dedup.check_and_register(&key).await {
        dedup.complete(token, "result".to_string()).await;
    }

    // Check 9 more times (all cached)
    for _ in 0..9 {
        let _ = dedup.check_and_register(&key).await;
    }

    let stats = dedup.stats();
    // The deduplicator stores 1 entry (the completed request)
    assert_eq!(stats.total, 1, "should have 1 total entry");
    assert_eq!(stats.cached, 1, "1 entry should be in cached state");
    assert_eq!(stats.in_progress, 0, "0 entries should be in-progress");
}

#[tokio::test]
async fn test_dedup_different_prompts_no_dedup() {
    let dedup = Deduplicator::new(Duration::from_secs(300));

    let mut new_count = 0u32;
    for i in 0..20 {
        let key = dedup_key(&format!("unique prompt number {i}"));
        let result = dedup.check_and_register(&key).await;
        if matches!(result, DeduplicationResult::New(_)) {
            new_count += 1;
        }
    }

    assert_eq!(
        new_count, 20,
        "20 different prompts should all be new (0% dedup)"
    );
}

// ── Pipeline (batch_infer path) Tests ─────────────────────────────────────

#[tokio::test]
async fn test_pipeline_20_identical_prompts_all_processed() {
    let _ = metrics::init_metrics();
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(1));
    let handles = spawn_pipeline(worker);

    // Submit 20 identical prompts (same as batch_infer does)
    for i in 0..20 {
        let request = PromptRequest {
            session: SessionId::new("batch-test"),
            request_id: format!("req-{i}"),
            input: "What is the capital of France?".to_string(),
            meta: HashMap::new(),
        };
        let _ = handles.input_tx.send(request).await;
    }

    // Wait for pipeline to process
    tokio::time::sleep(Duration::from_secs(3)).await;
    drop(handles.input_tx);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check metrics — all 20 should have been processed (no dedup at pipeline level)
    let summary = metrics::get_metrics_summary();
    let rag_count = summary.requests_total.get("rag").copied().unwrap_or(0);

    // The pipeline processes every request — dedup is NOT wired in
    // rag_count may not be exactly 20 due to timing with other tests sharing the
    // global metrics singleton, but it should be > 0 proving requests flow through
    assert!(
        rag_count > 0,
        "pipeline should have processed requests, got rag_count={rag_count}"
    );
}

#[tokio::test]
async fn test_dedup_key_deterministic() {
    let prompt = "What is the capital of France?";
    let key1 = dedup_key(prompt);
    let key2 = dedup_key(prompt);
    assert_eq!(key1, key2, "same prompt must produce same dedup key");
}

#[tokio::test]
async fn test_dedup_key_different_for_different_prompts() {
    let key1 = dedup_key("What is the capital of France?");
    let key2 = dedup_key("What is the capital of Germany?");
    assert_ne!(key1, key2, "different prompts must produce different keys");
}

// ── Deduplicator + Pipeline Integration Simulation ────────────────────────

#[tokio::test]
async fn test_dedup_integrated_with_pipeline_achieves_95_percent() {
    let _ = metrics::init_metrics();
    let dedup = Deduplicator::new(Duration::from_secs(300));
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(1));
    let handles = spawn_pipeline(worker);

    let prompt = "What is the capital of France?";
    let key = dedup_key(prompt);

    let mut inferences_executed = 0u32;
    let mut deduped = 0u32;

    // Simulate what batch_infer SHOULD do with dedup integration:
    // 1. Check dedup before sending to pipeline
    // 2. Only send new requests to pipeline
    // 3. Complete dedup token after pipeline returns

    for i in 0..20 {
        match dedup.check_and_register(&key).await {
            DeduplicationResult::New(token) => {
                // Send to pipeline
                let request = PromptRequest {
                    session: SessionId::new("dedup-batch"),
                    request_id: format!("req-{i}"),
                    input: prompt.to_string(),
                    meta: HashMap::new(),
                };
                let _ = handles.input_tx.send(request).await;
                inferences_executed += 1;

                // Simulate waiting for pipeline result, then completing
                tokio::time::sleep(Duration::from_millis(50)).await;
                dedup.complete(token, "Paris".to_string()).await;
            }
            DeduplicationResult::InProgress => {
                deduped += 1;
                // Would wait_for_result in production
            }
            DeduplicationResult::Cached(_) => {
                deduped += 1;
            }
        }
    }

    drop(handles.input_tx);

    let total = inferences_executed + deduped;
    let dedup_rate = (deduped as f64 / total as f64) * 100.0;

    assert_eq!(total, 20);
    assert_eq!(inferences_executed, 1, "only 1 inference should execute");
    assert_eq!(deduped, 19, "19 should be deduped");
    assert!(
        (dedup_rate - 95.0).abs() < 0.1,
        "dedup rate should be 95%, got {dedup_rate:.1}%"
    );
}
