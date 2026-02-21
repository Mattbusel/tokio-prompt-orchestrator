//! # Stage: Model Routing Intelligence
//!
//! ## Responsibility
//! Automatically route prompts to the optimal worker backend based on
//! complexity scoring.  Simple prompts go to the free local model (llama.cpp /
//! Mistral); complex prompts go directly to the paid cloud model (Claude API).
//! Prompts in the middle zone try local first with cloud fallback.
//!
//! ## Guarantees
//! - Deterministic: the same prompt text always produces the same complexity
//!   score and initial routing decision.
//! - Thread-safe: all state (`CostTracker`, adaptive threshold) uses atomics
//!   or interior locking — safe under concurrent pipeline access.
//! - Non-blocking: `route()` is a pure O(n) scan over the prompt with no
//!   I/O, allocation-free hot path.
//! - Bounded: adaptive threshold adjustment is clamped between
//!   `local_threshold` and `1.0`, so it can never oscillate unboundedly.
//!
//! ## NOT Responsible For
//! - Actually calling the workers (that belongs to `stages` / `worker`)
//! - Cross-node routing (future distributed module)
//! - Semantic understanding of prompt quality (heuristic-only)

pub mod config;
pub mod cost_tracker;
pub mod router;
pub mod scorer;

// Re-exports for convenience
pub use config::RoutingConfig;
pub use cost_tracker::{CostSnapshot, CostTracker};
pub use router::{ModelRouter, RoutingDecision};
pub use scorer::{ComplexityScorer, ScoreBreakdown};
