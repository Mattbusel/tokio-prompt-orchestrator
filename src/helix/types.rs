//! # HelixRouter Core Types
//!
//! ## Responsibility
//! Define the fundamental data types for the adaptive compute router:
//! jobs, strategies, outputs, and supporting enumerations.
//!
//! ## Guarantees
//! - All types are `Send + Sync` for safe cross-task sharing.
//! - Serde (de)serialisable for config files and API responses.
//! - `Display` implemented for human-readable logging.
//!
//! ## NOT Responsible For
//! - Routing decisions (see `router`)
//! - Execution logic (see `strategies`)

use serde::{Deserialize, Serialize};
use std::fmt;

/// Unique identifier for a job submitted to the router.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "job-{}", self.0)
    }
}

/// The kind of compute work a job represents.
///
/// Each variant maps to a different synthetic workload in the strategies
/// module, with varying CPU intensity and latency profiles.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobKind {
    /// Hash mixing — lightweight CPU work.
    Hash,
    /// Prime counting — medium CPU work.
    Prime,
    /// Monte Carlo risk simulation — heavy CPU work.
    MonteCarlo,
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hash => write!(f, "hash"),
            Self::Prime => write!(f, "prime"),
            Self::MonteCarlo => write!(f, "montecarlo"),
        }
    }
}

/// The routing strategy selected for a job.
///
/// Strategies are ordered by cost (resource usage): `Inline` is cheapest,
/// `Drop` means the job is shed entirely.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Strategy {
    /// Execute synchronously in the current task. Lowest overhead.
    Inline,
    /// Spawn a new Tokio task. Moderate overhead.
    Spawn,
    /// Send to a CPU-bound thread pool via `spawn_blocking`. For heavy work.
    CpuPool,
    /// Accumulate into a batch queue and flush when the batch is full.
    Batch,
    /// Shed the job entirely under extreme pressure. Zero execution cost.
    Drop,
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline => write!(f, "inline"),
            Self::Spawn => write!(f, "spawn"),
            Self::CpuPool => write!(f, "cpu_pool"),
            Self::Batch => write!(f, "batch"),
            Self::Drop => write!(f, "drop"),
        }
    }
}

/// A job submitted to the HelixRouter for execution.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Unique identifier for this job.
    pub id: JobId,
    /// The kind of compute work to perform.
    pub kind: JobKind,
    /// Estimated compute cost in arbitrary units (0–1000+).
    /// Used by `choose_strategy()` to select the routing strategy.
    pub cost: u64,
}

impl fmt::Display for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({}, cost={})", self.id, self.kind, self.cost)
    }
}

/// The result of executing a job through the router.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    /// ID of the job that produced this output.
    pub job_id: JobId,
    /// Numeric result of the computation.
    pub result: u64,
    /// Strategy that was used to execute the job.
    pub strategy: Strategy,
    /// Wall-clock execution time in milliseconds.
    pub latency_ms: f64,
}

impl fmt::Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}→{} via {} ({:.2}ms)",
            self.job_id, self.result, self.strategy, self.latency_ms
        )
    }
}

/// A unit of CPU-bound work dispatched to the blocking thread pool.
///
/// Sent through a channel from the router to `cpu_dispatch_loop()`.
///
/// # Panics
///
/// This type never panics.
pub struct CpuWork {
    /// The job to execute.
    pub job: Job,
    /// Channel to send the result back to the caller.
    pub reply: tokio::sync::oneshot::Sender<u64>,
}

/// An entry in a batch queue, waiting to be flushed.
///
/// # Panics
///
/// This type never panics.
pub struct BatchEntry {
    /// The job to execute.
    pub job: Job,
    /// Channel to send the result back to the caller.
    pub reply: tokio::sync::oneshot::Sender<u64>,
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- JobId ---------------------------------------------------------------

    #[test]
    fn test_job_id_display_format() {
        let id = JobId(42);
        assert_eq!(format!("{id}"), "job-42");
    }

    #[test]
    fn test_job_id_equality() {
        assert_eq!(JobId(1), JobId(1));
        assert_ne!(JobId(1), JobId(2));
    }

    #[test]
    fn test_job_id_hash_deterministic() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        JobId(42).hash(&mut h1);
        JobId(42).hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn test_job_id_serde_roundtrip() {
        let id = JobId(99);
        let json = serde_json::to_string(&id)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test ser: {e}")));
        let back: JobId = serde_json::from_str(&json)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test deser: {e}")));
        assert_eq!(id, back);
    }

    // -- JobKind -------------------------------------------------------------

    #[test]
    fn test_job_kind_display_hash() {
        assert_eq!(format!("{}", JobKind::Hash), "hash");
    }

    #[test]
    fn test_job_kind_display_prime() {
        assert_eq!(format!("{}", JobKind::Prime), "prime");
    }

    #[test]
    fn test_job_kind_display_montecarlo() {
        assert_eq!(format!("{}", JobKind::MonteCarlo), "montecarlo");
    }

    #[test]
    fn test_job_kind_serde_roundtrip() {
        for kind in [JobKind::Hash, JobKind::Prime, JobKind::MonteCarlo] {
            let json = serde_json::to_string(&kind)
                .unwrap_or_else(|e| std::panic::panic_any(format!("test ser: {e}")));
            let back: JobKind = serde_json::from_str(&json)
                .unwrap_or_else(|e| std::panic::panic_any(format!("test deser: {e}")));
            assert_eq!(kind, back);
        }
    }

    // -- Strategy ------------------------------------------------------------

    #[test]
    fn test_strategy_display_all_variants() {
        assert_eq!(format!("{}", Strategy::Inline), "inline");
        assert_eq!(format!("{}", Strategy::Spawn), "spawn");
        assert_eq!(format!("{}", Strategy::CpuPool), "cpu_pool");
        assert_eq!(format!("{}", Strategy::Batch), "batch");
        assert_eq!(format!("{}", Strategy::Drop), "drop");
    }

    #[test]
    fn test_strategy_equality() {
        assert_eq!(Strategy::Inline, Strategy::Inline);
        assert_ne!(Strategy::Inline, Strategy::Spawn);
    }

    #[test]
    fn test_strategy_serde_roundtrip() {
        for s in [
            Strategy::Inline,
            Strategy::Spawn,
            Strategy::CpuPool,
            Strategy::Batch,
            Strategy::Drop,
        ] {
            let json = serde_json::to_string(&s)
                .unwrap_or_else(|e| std::panic::panic_any(format!("test ser: {e}")));
            let back: Strategy = serde_json::from_str(&json)
                .unwrap_or_else(|e| std::panic::panic_any(format!("test deser: {e}")));
            assert_eq!(s, back);
        }
    }

    // -- Job -----------------------------------------------------------------

    #[test]
    fn test_job_display_format() {
        let job = Job {
            id: JobId(7),
            kind: JobKind::Prime,
            cost: 150,
        };
        assert_eq!(format!("{job}"), "job-7(prime, cost=150)");
    }

    #[test]
    fn test_job_serde_roundtrip() {
        let job = Job {
            id: JobId(1),
            kind: JobKind::Hash,
            cost: 50,
        };
        let json = serde_json::to_string(&job)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test ser: {e}")));
        let back: Job = serde_json::from_str(&json)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test deser: {e}")));
        assert_eq!(back.id, JobId(1));
        assert_eq!(back.kind, JobKind::Hash);
        assert_eq!(back.cost, 50);
    }

    // -- Output --------------------------------------------------------------

    #[test]
    fn test_output_display_format() {
        let out = Output {
            job_id: JobId(3),
            result: 42,
            strategy: Strategy::CpuPool,
            latency_ms: 1.23,
        };
        assert_eq!(format!("{out}"), "job-3→42 via cpu_pool (1.23ms)");
    }

    #[test]
    fn test_output_serde_roundtrip() {
        let out = Output {
            job_id: JobId(1),
            result: 100,
            strategy: Strategy::Inline,
            latency_ms: 0.5,
        };
        let json = serde_json::to_string(&out)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test ser: {e}")));
        let back: Output = serde_json::from_str(&json)
            .unwrap_or_else(|e| std::panic::panic_any(format!("test deser: {e}")));
        assert_eq!(back.job_id, JobId(1));
        assert_eq!(back.result, 100);
        assert_eq!(back.strategy, Strategy::Inline);
    }

    // -- Send + Sync checks --------------------------------------------------

    #[test]
    fn test_types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Job>();
        assert_send_sync::<Output>();
        assert_send_sync::<JobId>();
        assert_send_sync::<JobKind>();
        assert_send_sync::<Strategy>();
    }
}
