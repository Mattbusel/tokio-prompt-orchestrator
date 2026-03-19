//! # Example: Dead-Letter Queue (DLQ) Inspection
//!
//! Demonstrates: How shed (dropped) requests land in the DLQ and how to inspect
//! and replay them.
//!
//! The pipeline is deliberately given tiny channel capacities so that flooding
//! it with requests triggers backpressure shedding.  The DLQ captures every
//! dropped request for later inspection and replay.
//!
//! Run with: `cargo run --example dlq_inspection`
//! Features needed: none

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_prompt_orchestrator::{
    spawn_pipeline, DeadLetterQueue, DroppedRequest, EchoWorker, ModelWorker, PromptRequest,
    SessionId,
};
use tracing::{warn, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::WARN) // quiet the pipeline noise for clarity
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Switch to INFO for our own messages.
    let subscriber2 = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    // (We already set the global above; use eprintln for clarity in this demo.)
    drop(subscriber2);

    println!();
    println!("Dead-Letter Queue (DLQ) Inspection Demo");
    println!("=========================================");
    println!();
    println!("A pipeline is started with a slow worker to maximise backpressure.");
    println!("We then flood it with 50 requests.  Shed requests accumulate in the DLQ.");
    println!();

    // Use a slow worker (200 ms delay) so the inference channel backs up quickly.
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(200));
    let handles = spawn_pipeline(worker);

    // The DLQ is shared between the pipeline and us via Arc.
    let dlq: Arc<DeadLetterQueue> = handles.dlq.clone();

    // ── Phase 1: Flood the pipeline ───────────────────────────────────────
    println!("Phase 1: Flooding with 50 rapid-fire requests...");

    let mut sent = 0usize;
    let mut rejected = 0usize;

    for i in 0..50usize {
        let req = PromptRequest {
            session: SessionId::new(format!("flood-session-{}", i % 5)),
            request_id: format!("flood-req-{i:03}"),
            input: format!("Flood request #{i}: What is {i} + {i}?"),
            meta: {
                let mut m = HashMap::new();
                m.insert("batch".to_string(), "flood".to_string());
                m.insert("index".to_string(), i.to_string());
                m
            },
            deadline: None,
        };

        // try_send gives us instant backpressure without blocking.
        match handles.input_tx.try_send(req) {
            Ok(_) => sent += 1,
            Err(_) => rejected += 1,
        }
    }

    println!("  Sent (accepted):   {sent}");
    println!("  Rejected (channel full): {rejected}");
    println!();

    // Give the pipeline a moment to process and accumulate DLQ entries.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Phase 2: Inspect the DLQ ─────────────────────────────────────────
    println!("Phase 2: Inspecting the Dead-Letter Queue...");

    let dlq_len = dlq.len();
    println!("  DLQ currently holds {dlq_len} entries");

    // NOTE: drain() empties the DLQ and returns all entries.
    // Use peek-style inspection before draining if you want to keep entries.
    if dlq_len == 0 {
        println!("  (DLQ is empty — the pipeline processed everything without shedding.)");
        println!("  Tip: try increasing the flood count or using a slower worker delay.");
    } else {
        println!("  Draining DLQ for inspection...");
        println!();

        // We drain into a local Vec so we can inspect and then replay.
        let dropped: Vec<DroppedRequest> = dlq.drain();

        println!("  === DLQ Contents ({} entries) ===", dropped.len());
        for (i, entry) in dropped.iter().enumerate().take(10) {
            let age = entry
                .dropped_at
                .elapsed()
                .unwrap_or(Duration::from_secs(0));
            println!(
                "  [{i:02}] request_id={} session={} reason=\"{}\" age={:.1}s",
                entry.request_id,
                entry.session_id,
                entry.reason,
                age.as_secs_f64()
            );
        }
        if dropped.len() > 10 {
            println!("  ... and {} more entries", dropped.len() - 10);
        }

        println!();
        println!("  DLQ after drain: {} entries", dlq.len());

        // ── Phase 3: Replay DLQ entries ──────────────────────────────────
        println!();
        println!("Phase 3: Replaying DLQ entries into a fresh pipeline...");

        let replay_worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(10));
        let replay_handles = spawn_pipeline(replay_worker);

        let mut replayed = 0usize;
        let mut replay_rejected = 0usize;

        for entry in &dropped {
            // Reconstruct a minimal PromptRequest from the DLQ metadata.
            // In production you would persist the full request alongside the DLQ entry.
            let replay_req = PromptRequest {
                session: SessionId::new(entry.session_id.clone()),
                request_id: format!("replay-{}", entry.request_id),
                input: format!("[REPLAY] original request_id={}", entry.request_id),
                meta: {
                    let mut m = HashMap::new();
                    m.insert("replayed".to_string(), "true".to_string());
                    m.insert("original_request_id".to_string(), entry.request_id.clone());
                    m
                },
                deadline: None,
            };

            match replay_handles.input_tx.try_send(replay_req) {
                Ok(_) => replayed += 1,
                Err(_) => {
                    warn!("replay channel full, dropping replay entry");
                    replay_rejected += 1;
                }
            }
        }

        println!("  Replayed: {replayed} / {} DLQ entries", dropped.len());
        if replay_rejected > 0 {
            println!("  Replay rejected (still overloaded): {replay_rejected}");
        }

        // Wait for replay pipeline to drain.
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(replay_handles.input_tx);
    }

    // Shutdown original pipeline.
    drop(handles.input_tx);

    println!();
    println!("Summary");
    println!("=======");
    println!("  The DLQ acts as a safety net for shed requests.");
    println!("  You can inspect entries to understand what was dropped and why,");
    println!("  then replay them to a less-loaded pipeline or at a later time.");
    println!("  Capacity: DLQ holds the most-recent 1 024 dropped requests by default.");

    Ok(())
}
