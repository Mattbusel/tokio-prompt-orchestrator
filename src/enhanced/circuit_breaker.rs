//! Circuit Breaker
//!
//! Prevents cascading failures by stopping requests to failing services.
//!
//! ## States
//! - **Closed**: Normal operation, requests flow through
//! - **Open**: Service failing, requests rejected immediately
//! - **Half-Open**: A bounded number of probe requests (one by default) test
//!   whether the service recovered; everyone else is still rejected
//!
//! This module re-exports [`llm_resilience::circuit_breaker`], the small crate
//! that holds the orchestrator's resilience core. Depend on `llm-resilience`
//! directly if you want the breaker without the rest of the orchestrator.
//! Once [`init_metrics`](crate::metrics::init_metrics) has run, every breaker
//! in the process feeds the `orchestrator_circuit_breaker_*` Prometheus
//! counters.
//!
//! ## Usage
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_prompt_orchestrator::enhanced::CircuitBreaker;
//! use tokio_prompt_orchestrator::enhanced::circuit_breaker::CircuitBreakerError;
//! # #[tokio::main]
//! # async fn main() {
//! let breaker = CircuitBreaker::new(5, 0.5, Duration::from_secs(60));
//!
//! match breaker.call(|| async {
//!     // Your operation: replace with a real async call
//!     Ok::<&str, &str>("inference result")
//! }).await {
//!     Ok(result) => println!("{result}"), // Success
//!     Err(CircuitBreakerError::Open) => {
//!         // Circuit open, fail fast
//!     }
//!     Err(CircuitBreakerError::Failed(e)) => {
//!         eprintln!("Operation failed: {e}");
//!     }
//! }
//! # }
//! ```

pub use llm_resilience::circuit_breaker::*;
