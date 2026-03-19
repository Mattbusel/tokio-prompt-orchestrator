//! # Example: Request Deduplication Demo
//!
//! Demonstrates: How the Deduplicator coalesces identical concurrent requests
//! so only one actually runs through inference while the rest share the result.
//!
//! Run with: `cargo run --example dedup_demo`
//! Features needed: none

use std::collections::HashMap;
use std::time::Duration;
use tokio_prompt_orchestrator::enhanced::{
    dedup::{self, DeduplicationResult},
    Deduplicator,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

/// Simulated inference cost per request (in fictional USD micro-cents).
const COST_PER_INFERENCE: f64 = 0.004;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Request Deduplication Demo");
    info!("==========================");
    info!("");
    info!("Scenario: 10 concurrent clients all ask the same question at the same time.");
    info!("Only 1 should hit the (expensive) model; the other 9 reuse the result.");
    info!("");

    // Create a deduplicator with a 5-minute cache window.
    let dedup = Deduplicator::new(Duration::from_secs(300));

    // The shared prompt all 10 clients send.
    let prompt = "What is the meaning of life?";
    let meta: HashMap<String, String> = HashMap::new();

    // Generate the deterministic dedup key (based on prompt + meta + no model override).
    let key = dedup::dedup_key(prompt, &meta, None);
    info!("Dedup key for prompt: {key}");
    info!("");

    // ── Launch 10 concurrent tasks, all sending the same request ──────────
    info!("Launching 10 concurrent clients...");
    let mut tasks = Vec::new();

    for client_id in 1..=10usize {
        let dedup_clone = dedup.clone();
        let key_clone = key.clone();

        let task = tokio::spawn(async move {
            // Small jitter so tasks start at slightly different moments
            tokio::time::sleep(Duration::from_millis(client_id as u64 * 5)).await;

            match dedup_clone.check_and_register(&key_clone).await {
                DeduplicationResult::New(token) => {
                    info!(
                        "  Client {:02}: NEW — this client wins the race, running inference...",
                        client_id
                    );

                    // Simulate the (slow) model call.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let answer = "42 — and everything that implies.".to_string();

                    dedup_clone.complete(token, answer.clone()).await;
                    info!("  Client {:02}: inference done, result cached for waiters.", client_id);
                    ("new", answer)
                }

                DeduplicationResult::InProgress => {
                    info!(
                        "  Client {:02}: IN-PROGRESS — request already running, waiting for result...",
                        client_id
                    );
                    let result = dedup_clone
                        .wait_for_result(&key_clone)
                        .await
                        .unwrap_or_else(|| "(no result)".to_string());
                    info!(
                        "  Client {:02}: DEDUPLICATED — received result: \"{}\"",
                        client_id, result
                    );
                    ("deduplicated", result)
                }

                DeduplicationResult::Cached(result) => {
                    info!(
                        "  Client {:02}: CACHED — returned immediately: \"{}\"",
                        client_id, result
                    );
                    ("cached", result)
                }
            }
        });

        tasks.push(task);
    }

    // Collect all results.
    let mut new_count = 0usize;
    let mut dedup_count = 0usize;
    let mut cached_count = 0usize;

    for task in tasks {
        match task.await {
            Ok((kind, _)) => match kind {
                "new" => new_count += 1,
                "deduplicated" => dedup_count += 1,
                "cached" => cached_count += 1,
                _ => {}
            },
            Err(e) => tracing::warn!("task panicked: {e}"),
        }
    }

    // ── Round 2: demonstrate the cached path ──────────────────────────────
    info!("");
    info!("Round 2: Sending the same request again (should hit the cache instantly)");

    match dedup.check_and_register(&key).await {
        DeduplicationResult::Cached(result) => {
            info!("  Post-round cache hit: \"{}\"", result);
            cached_count += 1;
        }
        other => {
            info!("  Unexpected result: {other:?}");
        }
    }

    // ── Summary ───────────────────────────────────────────────────────────
    info!("");
    info!("Summary");
    info!("=======");

    let stats = dedup.stats();
    info!("  New (ran inference):  {new_count}");
    info!("  Deduplicated:         {dedup_count}");
    info!("  Cached:               {cached_count}");
    info!("");
    info!("  Dedup stats — total: {}, in_progress: {}, cached: {}",
          stats.total, stats.in_progress, stats.cached);
    info!("");

    let total_requests = (new_count + dedup_count + cached_count) as f64;
    let actual_inferences = new_count as f64;
    let cost_without_dedup = total_requests * COST_PER_INFERENCE;
    let cost_with_dedup = actual_inferences * COST_PER_INFERENCE;
    let savings = cost_without_dedup - cost_with_dedup;
    let savings_pct = (savings / cost_without_dedup) * 100.0;

    info!("Cost estimate (fictional):");
    info!("  Without dedup: ${cost_without_dedup:.4} ({total_requests} inferences @ ${COST_PER_INFERENCE})");
    info!("  With dedup:    ${cost_with_dedup:.4} ({actual_inferences} inference)");
    info!("  Savings:       ${savings:.4} ({savings_pct:.0}% cost reduction)");

    Ok(())
}
