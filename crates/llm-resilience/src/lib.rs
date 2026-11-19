//! Single-flight deduplication and a circuit breaker for async Rust, with a
//! tower layer and a reqwest middleware. Small enough to drop into an
//! existing client: the core needs only `tokio`, `parking_lot` and `tracing`.
//!
//! - [`Dedup`]: concurrent calls with the same key run once and share the
//!   result (an LLM prompt, a token refresh, a config fetch).
//! - [`CircuitBreaker`]: stop calling a dependency that keeps failing, then
//!   let one probe test whether it recovered.
//! - [`RetrySpend`]: retries capped by dollars, not by count. Each retry
//!   pays its estimated cost from a budget of a floor plus 10% of real
//!   spend per hour, and [`retryable_status`] never retries a 4xx that will
//!   fail the same way again. Retry counts say nothing about money; this
//!   does.
//! - `ResilienceLayer` (feature `tower`): both, around any `tower::Service`.
//! - `ResilienceMiddleware` (feature `reqwest-middleware`): both, for a
//!   `reqwest_middleware::ClientWithMiddleware`, with a breaker per host.
//!   Since 0.2.0 it also replays a completed inference answer to an
//!   identical request for 5 minutes by default: you do not pay for the same
//!   prompt twice unless you ask for it (`Cache-Control: no-cache`).
//!
//! ```
//! use llm_resilience::{CircuitBreaker, Dedup};
//! # #[tokio::main]
//! # async fn main() {
//! let dedup: Dedup<String, Result<String, String>> = Dedup::new();
//! let breaker = CircuitBreaker::new(5, 0.8, std::time::Duration::from_secs(30));
//! let prompt = "What is 2+2?".to_string();
//! let answer = dedup
//!     .run(prompt.clone(), || async {
//!         breaker
//!             .call(|| async { Ok::<_, String>(format!("answer to {prompt}")) })
//!             .await
//!             .map_err(|e| e.to_string())
//!     })
//!     .await;
//! assert_eq!(answer.as_deref(), Ok("answer to What is 2+2?"));
//! # }
//! ```
//!
//! This crate is the resilience core of
//! [`tokio-prompt-orchestrator`](https://docs.rs/tokio-prompt-orchestrator),
//! which re-exports every type here at its old paths. Use this crate when you
//! want only these primitives and not the orchestrator's pipeline, HTTP
//! server or provider clients.
//!
//! # Features
//!
//! | Feature | Adds | Extra dependencies |
//! |---|---|---|
//! | (none) | [`Dedup`], [`CircuitBreaker`], [`RetrySpend`] | `tokio` (sync, time), `parking_lot`, `tracing` |
//! | `tower` | `ResilienceLayer`, `ResilienceService` | `tower-layer`, `tower-service` |
//! | `reqwest-middleware` | `ResilienceMiddleware` | `reqwest` 0.12 (no default features), `reqwest-middleware` 0.4, `http`, `bytes`, `sha2`, `dashmap`, `serde_json`, `async-trait` |

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]

pub mod circuit_breaker;
pub mod retry_spend;
pub mod single_flight;

#[cfg(feature = "tower")]
mod layer;

#[cfg(feature = "reqwest-middleware")]
mod middleware;

pub use circuit_breaker::{
    add_global_event_hook, BreakerEvent, CallPermit, CircuitBreaker, CircuitBreakerError,
    CircuitBreakerStats, CircuitStatus,
};
pub use retry_spend::{
    retryable_status, RetryRefused, RetrySpend, RetrySpendStats, DEFAULT_RETRY_RATIO,
    DEFAULT_RETRY_WINDOW,
};
pub use single_flight::{Claim, Dedup, DedupStats, Leader};

#[cfg(feature = "tower")]
#[cfg_attr(docsrs, doc(cfg(feature = "tower")))]
pub use layer::{NoDedup, ResilienceError, ResilienceLayer, ResilienceService};

#[cfg(feature = "reqwest-middleware")]
#[cfg_attr(docsrs, doc(cfg(feature = "reqwest-middleware")))]
pub use middleware::{
    CircuitOpenError, JoinedRequestFailed, ResilienceMiddleware, SkipDedup, DEFAULT_REPLAY_WINDOW,
    REPLAY_HEADER,
};
