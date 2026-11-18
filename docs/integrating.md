# Integrating into an existing codebase

Three ways in, from least to most code. Pick the smallest one that does the job.

| You want | Use | Adds to your build |
|---|---|---|
| Identical in-flight requests share one call, and a failing provider fails fast, inside the client you already have | `llm-resilience` crate | 12 crates (14 with `tower`) |
| The same, without touching application code | `orchestrator` binary as an OpenAI-compatible proxy | nothing (a separate process or container) |
| A full request pipeline: queue, dedup, breaker, retries, deadlines, dead-letter queue, metrics | `tokio-prompt-orchestrator` library | about 200 crates with default features |

## 1. The library primitives: `llm-resilience`

```toml
llm-resilience = { version = "0.1", features = ["tower"] }
```

A tower stack (an LLM client, HTTP client or gRPC channel that is a `tower::Service`):

```rust
use llm_resilience::ResilienceLayer;

let svc = tower::ServiceBuilder::new()
    .layer(ResilienceLayer::new().dedup_by(|prompt: &String| Some(prompt.clone())))
    .service(llm_client);
```

A reqwest client (`features = ["reqwest-middleware"]`, reqwest 0.12, reqwest-middleware 0.4):

```rust
use llm_resilience::ResilienceMiddleware;

let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new())
    .with(ResilienceMiddleware::new()) // dedup + one circuit breaker per host
    .build();
```

Any async function:

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

What to know before merging it:

- **Dependencies.** The core is `tokio` (only the `sync` and `time` features),
  `parking_lot` and `tracing`. `tower` adds `tower-layer` and `tower-service`.
  `reqwest-middleware` uses reqwest without default features, so TLS comes
  from the `reqwest::Client` you already build; it never pulls in OpenSSL or a
  second TLS stack.
- **Keys.** Dedup keys must include everything that changes the answer,
  credentials too when one client serves several tenants. Return `None` from
  `dedup_by` for calls that must always run (non-idempotent or streaming).
  `ResilienceMiddleware` already keys on method, URL, headers and body and
  skips streaming requests.
- **No cache.** `Dedup` only joins calls that overlap in time; nothing is
  stored after the leading call returns.
- **Half-open.** One probe at a time tests recovery; everyone else gets
  `CircuitBreakerError::Open` (or `ResilienceError::CircuitOpen` with a
  `retry_after`) until it succeeds.
- **Metrics.** `CircuitBreaker::with_event_hook` (one breaker) or
  `llm_resilience::add_global_event_hook` (all breakers) reports opens,
  half-opens, closes and rejections to whatever metrics system you use.

## 2. The proxy: no code change

```sh
docker run --rm -p 8080:8080 -e OPENAI_API_KEY=sk-... \
  ghcr.io/mattbusel/tokio-prompt-orchestrator --provider openai --model gpt-4o-mini
export OPENAI_BASE_URL=http://127.0.0.1:8080/v1
```

Every OpenAI SDK reads `OPENAI_BASE_URL`, so existing services go through the
proxy without a code change. A Helm chart is in `charts/`.

## 3. The full pipeline: `tokio-prompt-orchestrator`

```rust
use std::sync::Arc;
use tokio_prompt_orchestrator::{spawn_pipeline, ModelWorker, OpenAiWorker};

let worker: Arc<dyn ModelWorker> = Arc::new(OpenAiWorker::new("gpt-4o-mini")?);
let handles = spawn_pipeline(worker);
```

See the crate docs for the request and output types, and `examples/` for
complete programs. No features are on by default; `web-api`, `tui`, `mcp`,
`distributed` and the integrations (`async-openai`, `genai`, `rig`, `tower`,
`reqwest-middleware`) are opt-in.

## Stability, MSRV, license

- Both crates follow semver. The types re-exported from `llm-resilience`
  (`Dedup`, `enhanced::CircuitBreaker`, `integrations::ResilienceLayer`,
  `integrations::ResilienceMiddleware`) keep their old paths in
  `tokio-prompt-orchestrator`.
- Minimum supported Rust version: 1.88 for both, checked in CI. Some optional
  orchestrator features need newer Rust (`rig`: 1.95, `rig-transport`: 1.97.1,
  `tantivy`: 1.90), noted in `Cargo.toml`.
- MIT licensed. `cargo audit` runs in CI with no ignored advisories.
- Maintained by Matthew Busel. Issues and pull requests:
  <https://github.com/Mattbusel/tokio-prompt-orchestrator>.
