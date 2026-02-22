//! HelixRouter benchmarks — measures routing and execution overhead.
//!
//! Performance contracts:
//! - `choose_strategy`:   < 1μs  (pure function, no async)
//! - Inline execution:    < 10μs (baseline overhead)
//! - Spawn overhead:      < 50μs (tokio::spawn + join)
//! - CpuPool overhead:    < 100μs (channel + spawn_blocking)
//! - Throughput (1K jobs): < 500ms wall time

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;
use tokio_prompt_orchestrator::helix::config::PressureConfig;
use tokio_prompt_orchestrator::helix::strategies;
use tokio_prompt_orchestrator::helix::{HelixConfig, HelixRouter, Job, JobId, JobKind};

// ---------------------------------------------------------------------------
// choose_strategy — pure function, target <1μs
// ---------------------------------------------------------------------------

fn bench_choose_strategy(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let router = rt.block_on(async { HelixRouter::new(HelixConfig::default()) });
    let pressure = PressureConfig::default();

    c.bench_function("helix_choose_strategy", |b| {
        b.iter(|| {
            black_box(router.choose_strategy(black_box(150), black_box(2), black_box(4), &pressure))
        });
    });
}

// ---------------------------------------------------------------------------
// execute_job — per-kind compute cost baseline
// ---------------------------------------------------------------------------

fn bench_execute_job_hash(c: &mut Criterion) {
    let job = Job {
        id: JobId(0),
        kind: JobKind::Hash,
        cost: 50,
    };
    c.bench_function("helix_execute_hash_50", |b| {
        b.iter(|| black_box(strategies::execute_job(black_box(&job))));
    });
}

fn bench_execute_job_prime(c: &mut Criterion) {
    let job = Job {
        id: JobId(0),
        kind: JobKind::Prime,
        cost: 100,
    };
    c.bench_function("helix_execute_prime_100", |b| {
        b.iter(|| black_box(strategies::execute_job(black_box(&job))));
    });
}

fn bench_execute_job_montecarlo(c: &mut Criterion) {
    let job = Job {
        id: JobId(0),
        kind: JobKind::MonteCarlo,
        cost: 50,
    };
    c.bench_function("helix_execute_montecarlo_50", |b| {
        b.iter(|| black_box(strategies::execute_job(black_box(&job))));
    });
}

// ---------------------------------------------------------------------------
// submit — strategy overhead (inline, spawn, cpu_pool)
// ---------------------------------------------------------------------------

fn bench_submit_inline(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let router = rt.block_on(async { HelixRouter::new(HelixConfig::default()) });

    c.bench_function("helix_submit_inline", |b| {
        b.to_async(&rt).iter(|| {
            let r = router.clone();
            async move {
                let job = Job {
                    id: JobId(0),
                    kind: JobKind::Hash,
                    cost: 10,
                };
                black_box(r.submit(job).await)
            }
        });
    });
}

fn bench_submit_spawn(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let router = rt.block_on(async { HelixRouter::new(HelixConfig::default()) });

    c.bench_function("helix_submit_spawn", |b| {
        b.to_async(&rt).iter(|| {
            let r = router.clone();
            async move {
                let job = Job {
                    id: JobId(0),
                    kind: JobKind::Hash,
                    cost: 100,
                };
                black_box(r.submit(job).await)
            }
        });
    });
}

fn bench_submit_cpu_pool(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let router = rt.block_on(async { HelixRouter::new(HelixConfig::default()) });

    c.bench_function("helix_submit_cpu_pool", |b| {
        b.to_async(&rt).iter(|| {
            let r = router.clone();
            async move {
                let job = Job {
                    id: JobId(0),
                    kind: JobKind::Hash,
                    cost: 300,
                };
                black_box(r.submit(job).await)
            }
        });
    });
}

// ---------------------------------------------------------------------------
// throughput — 1K jobs through the router
// ---------------------------------------------------------------------------

fn bench_throughput_1k(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let router = rt.block_on(async { HelixRouter::new(HelixConfig::default()) });

    c.bench_function("helix_throughput_1k_jobs", |b| {
        b.to_async(&rt).iter(|| {
            let r = router.clone();
            async move {
                for i in 0u64..1000 {
                    let cost = (i * 7 + 13) % 200; // Mix of inline + spawn
                    let job = Job {
                        id: JobId(i),
                        kind: JobKind::Hash,
                        cost,
                    };
                    let _ = black_box(r.submit(job).await);
                }
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Group & main
// ---------------------------------------------------------------------------

criterion_group!(
    helix_benches,
    bench_choose_strategy,
    bench_execute_job_hash,
    bench_execute_job_prime,
    bench_execute_job_montecarlo,
    bench_submit_inline,
    bench_submit_spawn,
    bench_submit_cpu_pool,
    bench_throughput_1k,
);

criterion_main!(helix_benches);
