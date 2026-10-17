//! Learned intelligence subsystem --- neural routing, prompt optimization,
//! predictive autoscaling, quality estimation, lexical dedup, feedback ingestion.
//! All items require the `intelligence` feature flag.

pub mod autoscale;
pub mod bridge;
pub mod feedback;
pub mod lexical_dedup;
pub mod prompt_opt;
pub mod quality;
pub mod router;
/// Legacy module re-exported for backward compatibility.
/// Use [`lexical_dedup`] instead.
pub mod semantic_dedup;
