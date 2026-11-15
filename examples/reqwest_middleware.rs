//! Dedup and a per-host circuit breaker for raw reqwest LLM calls, as a
//! reqwest-middleware middleware.
//!
//! Run with: `cargo run --example reqwest_middleware --features reqwest-middleware`
//!
//! The example starts a local mock of an OpenAI-style endpoint, so it needs no
//! API key and no network. Point the URL at a real provider in your app.

use std::sync::Arc;
use std::time::Duration;

use reqwest_middleware::{ClientBuilder, Error};
use tokio_prompt_orchestrator::integrations::{CircuitOpenError, ResilienceMiddleware};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"choices":[{"message":{"content":"Paris."}}]}"#)
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&provider)
        .await;
    Mock::given(method("POST"))
        .and(path("/down/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&provider)
        .await;

    let resilience = Arc::new(ResilienceMiddleware::new().with_circuit_breaker_per_host(
        3,
        0.8,
        Duration::from_secs(30),
    ));
    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(resilience.clone())
        .build();

    // 1. Twenty identical chat requests in flight at once.
    let url = format!("{}/v1/chat/completions", provider.uri());
    let body = serde_json::json!({
        "model": "gpt-4o-mini",
        "messages": [{"role": "user", "content": "Capital of France?"}]
    });
    let sends = (0..20).map(|_| client.post(&url).bearer_auth("sk-demo").json(&body).send());
    for reply in futures::future::join_all(sends).await {
        let text = reply?.text().await?;
        debug_assert!(text.contains("Paris"));
    }
    println!(
        "20 identical requests -> {} upstream call(s)",
        provider.received_requests().await.map_or(0, |r| r.len())
    );
    if let Some(stats) = resilience.dedup_stats() {
        println!(
            "dedup: {} leader(s), {} coalesced",
            stats.leaders, stats.coalesced
        );
    }

    // 2. The provider starts answering 503: three of them open the breaker
    //    and the rest fail at once without a network round trip.
    let down = format!("{}/down/v1/chat/completions", provider.uri());
    for i in 0..6 {
        let body = serde_json::json!({ "model": "gpt-4o-mini", "messages": [], "n": i });
        match client.post(&down).json(&body).send().await {
            Ok(resp) => println!("call {i}: HTTP {}", resp.status()),
            Err(Error::Middleware(e)) if e.downcast_ref::<CircuitOpenError>().is_some() => {
                println!("call {i}: failed fast ({e})")
            }
            Err(e) => println!("call {i}: error ({e})"),
        }
    }
    Ok(())
}
