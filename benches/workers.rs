//! Worker benchmarks — baseline throughput measurements.
//!
//! EchoWorker is the floor: pure orchestration overhead with zero inference time.
//! Target: >10,000 req/sec proves orchestration is not the bottleneck.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio_prompt_orchestrator::{EchoWorker, ModelWorker, PromptRequest, SessionId};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_request(id: &str) -> PromptRequest {
    PromptRequest {
        session: SessionId::new(format!("bench-{id}")),
        request_id: format!("req-{id}"),
        input: format!("Benchmark inference prompt {id}"),
        meta: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Bench: EchoWorker::infer — single call, zero delay
// ---------------------------------------------------------------------------

fn bench_echo_worker_infer(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");
    let worker = EchoWorker::with_delay(0);

    c.bench_function("echo_worker_infer", |b| {
        b.to_async(&rt).iter(|| async {
            let result = worker.infer(black_box("benchmark prompt")).await;
            let _ = black_box(result);
        })
    });
}

// ---------------------------------------------------------------------------
// Bench: EchoWorker throughput — sequential requests
// ---------------------------------------------------------------------------

fn bench_echo_worker_throughput_sequential(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");
    let worker = EchoWorker::with_delay(0);

    let mut group = c.benchmark_group("echo_worker_throughput_sequential");
    group.sample_size(20);

    for count in [100u64, 500, 1000] {
        group.bench_with_input(BenchmarkId::new("requests", count), &count, |b, &count| {
            b.to_async(&rt).iter(|| async {
                for i in 0..count {
                    let result = worker.infer(&format!("prompt-{i}")).await;
                    let _ = black_box(result);
                }
            })
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: EchoWorker throughput — concurrent requests
// ---------------------------------------------------------------------------

fn bench_echo_worker_throughput_concurrent(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("echo_worker_throughput_concurrent");
    group.sample_size(20);

    for count in [100u64, 500, 1000] {
        group.bench_with_input(BenchmarkId::new("requests", count), &count, |b, &count| {
            b.to_async(&rt).iter(|| async {
                let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::with_delay(0));
                let mut handles = Vec::with_capacity(count as usize);

                for i in 0..count {
                    let w = worker.clone();
                    let prompt = format!("prompt-{i}");
                    handles.push(tokio::spawn(
                        async move { black_box(w.infer(&prompt).await) },
                    ));
                }

                for handle in handles {
                    let _ = handle.await;
                }
            })
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: EchoWorker construction
// ---------------------------------------------------------------------------

fn bench_echo_worker_construction(c: &mut Criterion) {
    c.bench_function("echo_worker_construction", |b| {
        b.iter(|| {
            black_box(EchoWorker::with_delay(black_box(0)));
        })
    });
}

// ---------------------------------------------------------------------------
// Bench: full pipeline with EchoWorker — throughput measurement
// ---------------------------------------------------------------------------

fn bench_pipeline_echo_throughput(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("pipeline_echo_throughput");
    group.sample_size(10);

    for count in [10u64, 50] {
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

                // Wait for pipeline to drain
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                black_box(());
            })
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: dedup savings — identical prompts through pipeline
// Demonstrates cost reduction when dedup is active
// ---------------------------------------------------------------------------

fn bench_dedup_savings_concurrent(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("dedup_savings");
    group.sample_size(10);

    for concurrency in [10u64, 50, 100] {
        group.bench_with_input(
            BenchmarkId::new("identical_prompts", concurrency),
            &concurrency,
            |b, &count| {
                b.to_async(&rt).iter(|| async move {
                    use tokio_prompt_orchestrator::enhanced::Deduplicator;

                    let dedup = Deduplicator::new(std::time::Duration::from_secs(300));
                    let mut tasks = Vec::with_capacity(count as usize);

                    for _ in 0..count {
                        let d = dedup.clone();
                        tasks.push(tokio::spawn(async move {
                            let result = d.check_and_register("identical-prompt-key").await;
                            black_box(result);
                        }));
                    }

                    for task in tasks {
                        let _ = task.await;
                    }

                    let stats = dedup.stats();
                    // Should see in_progress == 1 (only one was new), rest were InProgress/Cached
                    black_box(stats);
                })
            },
        );
    }

    group.finish();
}

criterion_group!(
    worker_benches,
    bench_echo_worker_infer,
    bench_echo_worker_throughput_sequential,
    bench_echo_worker_throughput_concurrent,
    bench_echo_worker_construction,
);

criterion_group!(
    pipeline_echo_benches,
    bench_pipeline_echo_throughput,
    bench_dedup_savings_concurrent,
);

criterion_main!(worker_benches, pipeline_echo_benches);
