//! Single-flight dedup and a circuit breaker for any tower service, without
//! the pipeline or the proxy.
//!
//! Run with: `cargo run --example tower_resilience --features tower`
//!
//! The "LLM" here is a `service_fn` that sleeps and counts its calls, so the
//! example runs offline. Swap it for your own tower-based client.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_prompt_orchestrator::enhanced::CircuitBreaker;
use tokio_prompt_orchestrator::integrations::ResilienceLayer;
use tower::{ServiceBuilder, ServiceExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let calls = Arc::new(AtomicUsize::new(0));
    let healthy = Arc::new(std::sync::atomic::AtomicBool::new(true));

    let llm = {
        let calls = calls.clone();
        let healthy = healthy.clone();
        tower::service_fn(move |prompt: String| {
            let calls = calls.clone();
            let healthy = healthy.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
                if healthy.load(Ordering::SeqCst) {
                    Ok(format!("answer to {prompt:?}"))
                } else {
                    Err(std::io::Error::other("provider returned 503"))
                }
            }
        })
    };

    let breaker = CircuitBreaker::new(3, 0.8, Duration::from_secs(30));
    let svc = ServiceBuilder::new()
        .layer(
            ResilienceLayer::new()
                // Same prompt in flight = one upstream call.
                .dedup_by(|prompt: &String| Some(prompt.clone()))
                .with_circuit_breaker(breaker.clone()),
        )
        .service(llm);

    // 1. Twenty concurrent identical requests.
    let mut tasks = Vec::new();
    for _ in 0..20 {
        tasks.push(tokio::spawn(
            svc.clone()
                .oneshot("What is the capital of France?".to_string()),
        ));
    }
    for t in tasks {
        t.await??;
    }
    println!(
        "20 identical concurrent requests -> {} upstream call(s)",
        calls.load(Ordering::SeqCst)
    );
    if let Some(stats) = svc.dedup_stats() {
        println!(
            "dedup: {} leader(s), {} coalesced",
            stats.leaders, stats.coalesced
        );
    }

    // 2. The provider goes down: three failures open the breaker, and the
    //    next calls fail fast without touching the provider.
    healthy.store(false, Ordering::SeqCst);
    let before = calls.load(Ordering::SeqCst);
    for i in 0..6 {
        match svc.clone().oneshot(format!("prompt {i}")).await {
            Ok(_) => println!("call {i}: ok"),
            Err(e) if e.is_circuit_open() => println!("call {i}: failed fast ({e})"),
            Err(e) => println!("call {i}: upstream error ({e})"),
        }
    }
    println!(
        "6 calls during the outage -> {} upstream call(s); breaker open: {}",
        calls.load(Ordering::SeqCst) - before,
        breaker.is_open_sync()
    );
    Ok(())
}
