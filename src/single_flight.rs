//! A generic single-flight primitive: concurrent calls with the same key run
//! the work once and all get the result.
//!
//! This module re-exports [`llm_resilience::single_flight`], the small crate
//! that holds the orchestrator's resilience core. Depend on `llm-resilience`
//! directly if you want [`Dedup`] without the rest of the orchestrator.
//!
//! ```
//! use tokio_prompt_orchestrator::Dedup;
//! # #[tokio::main]
//! # async fn main() {
//! let dedup: Dedup<&'static str, u64> = Dedup::new();
//! let v = dedup.run("user:7", || async { 42 }).await;
//! assert_eq!(v, 42);
//! # }
//! ```

pub use llm_resilience::single_flight::*;
