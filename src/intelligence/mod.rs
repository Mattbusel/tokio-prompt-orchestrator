//! Learned intelligence subsystem --- neural routing, prompt optimization,
//! predictive autoscaling, quality estimation, semantic dedup, feedback ingestion.
//! All items require the `intelligence` feature flag.

pub mod autoscale;
pub mod feedback;
pub mod prompt_opt;
pub mod quality;
pub mod router;
pub mod semantic_dedup;
