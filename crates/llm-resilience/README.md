# llm-resilience

[![crates.io](https://img.shields.io/crates/v/llm-resilience.svg)](https://crates.io/crates/llm-resilience)
[![docs.rs](https://docs.rs/llm-resilience/badge.svg)](https://docs.rs/llm-resilience)
![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-blue)
![license MIT](https://img.shields.io/badge/license-MIT-green)

Single-flight request deduplication and an async circuit breaker for LLM and
HTTP clients, with a tower layer and a reqwest middleware.

- **`Dedup<K, V>`**: concurrent calls with the same key run once and all get
  the result. Ten users sending the same prompt at the same moment cost one
  API call. Cancellation-safe: if the leading call is dropped, a waiter takes
  over.
- **`CircuitBreaker`**: after N consecutive failures, calls fail at once
  instead of each waiting for its own timeout. After the open timeout, one
  probe call (configurable) tests recovery while everyone else is still
  rejected, and each failed probe doubles the wait.
- **`ResilienceLayer`** (feature `tower`): both, around any `tower::Service`.
- **`ResilienceMiddleware`** (feature `reqwest-middleware`): both, for a
  `reqwest_middleware::ClientWithMiddleware`, with one breaker per host.

The core depends on `tokio` (sync, time), `parking_lot` and `tracing`: 12
crates in its dependency tree, 14 with `tower`. It is the resilience core of
[tokio-prompt-orchestrator](https://crates.io/crates/tokio-prompt-orchestrator),
split out so you can use these pieces without the orchestrator's pipeline,
HTTP server and provider clients.

## Use it

```toml
[dependencies]
llm-resilience = { version = "0.1", features = ["tower"] }
```

Around a tower service (an LLM client, an HTTP client, a gRPC channel):

```rust
use llm_resilience::ResilienceLayer;
use tower::ServiceBuilder;

let svc = ServiceBuilder::new()
    .layer(ResilienceLayer::new().dedup_by(|prompt: &String| Some(prompt.clone())))
    .service(my_llm_client);
```

Around a reqwest client (feature `reqwest-middleware`):

```rust
use llm_resilience::ResilienceMiddleware;

let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new())
    .with(ResilienceMiddleware::new())
    .build();
```

Without tower or reqwest:

```rust
use llm_resilience::{CircuitBreaker, Dedup};

let dedup: Dedup<String, Result<String, String>> = Dedup::new();
let breaker = CircuitBreaker::new(5, 0.8, std::time::Duration::from_secs(30));
let answer = dedup
    .run(prompt.clone(), || async {
        breaker.call(|| call_model(&prompt)).await.map_err(|e| e.to_string())
    })
    .await;
```

`cargo run -p llm-resilience --features tower --example tower_layer` runs ten
identical prompts through a fake client and prints how many reached it (one).

## Behaviour worth knowing

- `Dedup` does not cache. Once the leading call finishes the key is free
  again; put a cache in front if you want one.
- Dedup keys should include everything that changes the answer, including the
  API key when one service serves several tenants. `ResilienceMiddleware` keys
  on method, URL, headers (minus tracing and request-id headers) and body, and
  never deduplicates streaming requests.
- Breaker outcomes are tagged with the breaker's generation. A slow call
  admitted before the breaker opened cannot close (or reopen) it while it is
  recovering; only the probes decide.
- `CircuitBreaker::with_event_hook` and `add_global_event_hook` report state
  changes and rejections, for your metrics.

## Versioning and support

Semver. The minimum supported Rust version is 1.88 and is checked in CI; raising
it is a minor version bump. MIT licensed. Maintained by Matthew Busel as part of
tokio-prompt-orchestrator; issues and pull requests go to
<https://github.com/Mattbusel/tokio-prompt-orchestrator>.
