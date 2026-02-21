//! Pipeline benchmarks — measures orchestration overhead.
//!
//! Performance contracts from CLAUDE.md:
//! - RAG stage:       P50 <5ms,  P99 <20ms
//! - Assemble stage:  P50 <2ms,  P99 <5ms
//! - Post stage:      P50 <3ms,  P99 <8ms
//! - Stream stage:    P50 <1ms,  P99 <3ms
//! - Full pipeline (excl. inference): P50 <15ms, P99 <40ms
//! - Channel send:    P99 <10μs

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_prompt_orchestrator::{
    send_with_shed, shard_session, EchoWorker, ModelWorker, PromptRequest, SessionId,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_request(id: &str) -> PromptRequest {
    PromptRequest {
        session: SessionId::new(format!("bench-session-{id}")),
        request_id: format!("req-{id}"),
        input: format!("Benchmark prompt number {id}"),
        meta: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Bench: full pipeline end-to-end with EchoWorker (zero-delay)
// ---------------------------------------------------------------------------

fn bench_full_pipeline_no_inference(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("full_pipeline_echo_worker", |b| {
        b.to_async(&rt).iter(|| async {
            let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(0));
            let handles = tokio_prompt_orchestrator::spawn_pipeline(worker);

            let req = make_request("pipe");
            handles.input_tx.send(req).await.expect("send");

            // Give pipeline time to process through all 5 stages.
            // EchoWorker is instant; the only overhead is channel hops + stage logic.
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

            black_box(());
        })
    });
}

// ---------------------------------------------------------------------------
// Bench: pipeline throughput — many requests through EchoWorker
// ---------------------------------------------------------------------------

fn bench_pipeline_throughput(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("pipeline_throughput");
    group.sample_size(20);

    for count in [10u64, 50, 100] {
        group.bench_with_input(BenchmarkId::new("requests", count), &count, |b, &count| {
            b.to_async(&rt).iter(|| async move {
                let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(0));
                let handles = tokio_prompt_orchestrator::spawn_pipeline(worker);

                for i in 0..count {
                    let req = make_request(&i.to_string());
                    if handles.input_tx.send(req).await.is_err() {
                        break;
                    }
                }

                // Allow pipeline to drain
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                black_box(());
            })
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: channel_send — bounded mpsc send latency at various capacities
// Budget: P99 <10μs
// ---------------------------------------------------------------------------

fn bench_channel_send(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("channel_send");

    for capacity in [512usize, 1024, 2048] {
        group.bench_with_input(
            BenchmarkId::new("capacity", capacity),
            &capacity,
            |b, &cap| {
                b.to_async(&rt).iter(|| async move {
                    let (tx, mut rx) = mpsc::channel::<u64>(cap);

                    // Spawn a consumer to prevent the channel from filling up
                    let consumer = tokio::spawn(async move { while rx.recv().await.is_some() {} });

                    for i in 0..100u64 {
                        let _ = tx.send(black_box(i)).await;
                    }

                    drop(tx);
                    let _ = consumer.await;
                })
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: send_with_shed — backpressure-aware send
// ---------------------------------------------------------------------------

fn bench_send_with_shed(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    c.bench_function("send_with_shed_normal", |b| {
        b.to_async(&rt).iter(|| async {
            let (tx, _rx) = mpsc::channel::<u32>(512);
            let result = send_with_shed(&tx, black_box(42), "bench-stage").await;
            black_box(result)
        })
    });
}

// ---------------------------------------------------------------------------
// Bench: shard_session — session-to-shard hash
// ---------------------------------------------------------------------------

fn bench_shard_session(c: &mut Criterion) {
    let session = SessionId::new("benchmark-session-42");

    c.bench_function("shard_session", |b| {
        b.iter(|| {
            black_box(shard_session(black_box(&session), 8));
        })
    });
}

// ---------------------------------------------------------------------------
// Bench: SessionId creation
// ---------------------------------------------------------------------------

fn bench_session_id_creation(c: &mut Criterion) {
    c.bench_function("session_id_creation", |b| {
        b.iter(|| {
            black_box(SessionId::new(black_box("user-session-12345")));
        })
    });
}

criterion_group!(
    pipeline_benches,
    bench_full_pipeline_no_inference,
    bench_pipeline_throughput,
    bench_channel_send,
    bench_send_with_shed,
    bench_shard_session,
    bench_session_id_creation,
);
criterion_main!(pipeline_benches);
