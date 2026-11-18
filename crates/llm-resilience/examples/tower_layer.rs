//! Wrap a (fake) LLM client in ResilienceLayer: ten identical concurrent
//! prompts make one upstream call, and a failing upstream is cut off.
//!
//! cargo run -p llm-resilience --features tower --example tower_layer

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use llm_resilience::{CircuitBreaker, ResilienceLayer};
use tower::{Layer, ServiceExt};

#[tokio::main]
async fn main() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let llm = tower::service_fn(move |prompt: String| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<_, std::io::Error>(format!("answer to {prompt}"))
        }
    });

    let svc = ResilienceLayer::new()
        .dedup_by(|prompt: &String| Some(prompt.clone()))
        .with_circuit_breaker(CircuitBreaker::new(3, 0.8, Duration::from_secs(10)))
        .layer(llm);

    let mut joins = Vec::new();
    for _ in 0..10 {
        joins.push(tokio::spawn(svc.clone().oneshot("What is 2+2?".to_string())));
    }
    for j in joins {
        match j.await {
            Ok(Ok(answer)) => println!("{answer}"),
            Ok(Err(e)) => println!("error: {e}"),
            Err(e) => println!("task failed: {e}"),
        }
    }
    println!(
        "10 identical prompts, {} upstream call(s)",
        calls.load(Ordering::SeqCst)
    );
}
