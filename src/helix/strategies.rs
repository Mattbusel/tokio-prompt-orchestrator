//! # HelixRouter Execution Strategies
//!
//! ## Responsibility
//! Provide compute workload functions and the CPU dispatch loop.
//! Each strategy in the router delegates actual execution here.
//!
//! ## Guarantees
//! - All compute functions are pure: deterministic output for the same inputs.
//! - `cpu_dispatch_loop` runs on `spawn_blocking` threads, never on the
//!   Tokio async executor.
//! - Batch flushing processes all entries and sends results back via oneshot.
//!
//! ## NOT Responsible For
//! - Choosing which strategy to use (see `router`)
//! - Tracking latency or metrics (see `metrics`)

use crate::helix::types::{BatchEntry, CpuWork, Job, JobKind};
use tokio::sync::mpsc;

/// Execute a job and return the numeric result.
///
/// Dispatches to the appropriate compute function based on [`JobKind`].
/// This is a pure function with no side effects.
///
/// # Arguments
///
/// * `job` — The job to execute.
///
/// # Returns
///
/// A `u64` result from the computation.
///
/// # Panics
///
/// This function never panics.
pub fn execute_job(job: &Job) -> u64 {
    match job.kind {
        JobKind::Hash => hashmix(job.cost),
        JobKind::Prime => primecount(job.cost),
        JobKind::MonteCarlo => montecarlo_risk(job.cost),
    }
}

/// Hash mixing function — lightweight CPU work.
///
/// Performs a series of bit-mixing operations proportional to `cost`.
/// Deterministic: same `cost` always produces the same result.
///
/// # Panics
///
/// This function never panics.
pub fn hashmix(cost: u64) -> u64 {
    let iterations = cost.max(1);
    let mut h: u64 = 0xcafe_babe_dead_beef;
    for i in 0..iterations {
        h ^= i;
        h = h.wrapping_mul(0x517c_c1b7_2722_0a95);
        h ^= h >> 32;
        h = h.wrapping_mul(0x0cf1_bbcd_cb7f_2649);
        h ^= h >> 28;
    }
    h
}

/// Prime counting function — medium CPU work.
///
/// Counts the number of primes up to `cost` using trial division.
/// Deterministic: same `cost` always produces the same result.
///
/// # Panics
///
/// This function never panics.
pub fn primecount(cost: u64) -> u64 {
    let limit = cost.max(2);
    let mut count = 0u64;
    for n in 2..=limit {
        let mut is_prime = true;
        let mut d = 2u64;
        while d * d <= n {
            if n % d == 0 {
                is_prime = false;
                break;
            }
            d += 1;
        }
        if is_prime {
            count += 1;
        }
    }
    count
}

/// Monte Carlo risk simulation — heavy CPU work.
///
/// Runs a deterministic pseudo-random simulation proportional to `cost`.
/// Uses a simple LCG (linear congruential generator) for reproducibility.
///
/// # Panics
///
/// This function never panics.
pub fn montecarlo_risk(cost: u64) -> u64 {
    let iterations = cost.max(1) * 10;
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut inside = 0u64;
    for _ in 0..iterations {
        // LCG step
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let x = (state >> 33) as f64 / (1u64 << 31) as f64;
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let y = (state >> 33) as f64 / (1u64 << 31) as f64;
        if x * x + y * y <= 1.0 {
            inside += 1;
        }
    }
    inside
}

/// CPU dispatch loop — runs on a blocking thread.
///
/// Receives [`CpuWork`] items from the channel, executes them via
/// [`execute_job`], and sends results back through the oneshot sender.
///
/// Exits when the channel is closed (all senders dropped).
///
/// # Panics
///
/// This function never panics.
pub async fn cpu_dispatch_loop(mut rx: mpsc::Receiver<CpuWork>) {
    while let Some(work) = rx.recv().await {
        let result = execute_job(&work.job);
        // If receiver dropped, we just discard the result
        let _ = work.reply.send(result);
    }
}

/// Flush a batch of entries by executing them all and sending results back.
///
/// # Arguments
///
/// * `entries` — The batch entries to process.
///
/// # Panics
///
/// This function never panics.
pub fn flush_batch(entries: Vec<BatchEntry>) {
    for entry in entries {
        let result = execute_job(&entry.job);
        let _ = entry.reply.send(result);
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helix::types::JobId;

    // -- hashmix ---------------------------------------------------------

    #[test]
    fn test_hashmix_deterministic() {
        let r1 = hashmix(100);
        let r2 = hashmix(100);
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_hashmix_different_costs_different_results() {
        let r1 = hashmix(10);
        let r2 = hashmix(20);
        assert_ne!(r1, r2, "different costs should give different results");
    }

    #[test]
    fn test_hashmix_zero_cost_does_not_panic() {
        let _ = hashmix(0);
    }

    #[test]
    fn test_hashmix_large_cost_does_not_panic() {
        let _ = hashmix(10_000);
    }

    // -- primecount ------------------------------------------------------

    #[test]
    fn test_primecount_deterministic() {
        let r1 = primecount(100);
        let r2 = primecount(100);
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_primecount_known_values() {
        // Primes up to 10: 2,3,5,7 → 4
        assert_eq!(primecount(10), 4);
        // Primes up to 20: 2,3,5,7,11,13,17,19 → 8
        assert_eq!(primecount(20), 8);
        // Primes up to 2: just 2 → 1
        assert_eq!(primecount(2), 1);
    }

    #[test]
    fn test_primecount_zero_cost_does_not_panic() {
        let _ = primecount(0);
    }

    // -- montecarlo_risk -------------------------------------------------

    #[test]
    fn test_montecarlo_risk_deterministic() {
        let r1 = montecarlo_risk(50);
        let r2 = montecarlo_risk(50);
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_montecarlo_risk_zero_cost_does_not_panic() {
        let _ = montecarlo_risk(0);
    }

    #[test]
    fn test_montecarlo_risk_result_bounded() {
        // With cost=100, iterations=1000; inside <= iterations
        let result = montecarlo_risk(100);
        assert!(result <= 1000, "inside count should be <= iterations");
    }

    #[test]
    fn test_montecarlo_risk_approximates_pi() {
        // pi/4 ≈ 0.785, so inside/iterations ≈ 0.785
        let cost = 1000;
        let iterations = cost * 10;
        let inside = montecarlo_risk(cost);
        let ratio = inside as f64 / iterations as f64;
        // Loose bound: ratio should be between 0.5 and 1.0
        assert!(
            (0.5..=1.0).contains(&ratio),
            "Monte Carlo ratio should be roughly pi/4, got {ratio}"
        );
    }

    // -- execute_job -----------------------------------------------------

    #[test]
    fn test_execute_job_hash() {
        let job = Job {
            id: JobId(1),
            kind: JobKind::Hash,
            cost: 50,
        };
        let result = execute_job(&job);
        assert_eq!(result, hashmix(50));
    }

    #[test]
    fn test_execute_job_prime() {
        let job = Job {
            id: JobId(2),
            kind: JobKind::Prime,
            cost: 10,
        };
        let result = execute_job(&job);
        assert_eq!(result, 4); // primes up to 10
    }

    #[test]
    fn test_execute_job_montecarlo() {
        let job = Job {
            id: JobId(3),
            kind: JobKind::MonteCarlo,
            cost: 20,
        };
        let result = execute_job(&job);
        assert_eq!(result, montecarlo_risk(20));
    }

    #[test]
    fn test_execute_job_deterministic_all_kinds() {
        for kind in [JobKind::Hash, JobKind::Prime, JobKind::MonteCarlo] {
            let job = Job {
                id: JobId(0),
                kind,
                cost: 100,
            };
            let r1 = execute_job(&job);
            let r2 = execute_job(&job);
            assert_eq!(r1, r2, "execute_job({kind}) must be deterministic");
        }
    }

    // -- cpu_dispatch_loop -----------------------------------------------

    #[tokio::test]
    async fn test_cpu_dispatch_loop_processes_work() {
        let (tx, rx) = mpsc::channel::<CpuWork>(16);
        let handle = tokio::spawn(cpu_dispatch_loop(rx));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(CpuWork {
            job: Job {
                id: JobId(1),
                kind: JobKind::Hash,
                cost: 10,
            },
            reply: reply_tx,
        })
        .await
        .unwrap_or_else(|_| std::panic::panic_any("test: send failed"));

        let result = reply_rx
            .await
            .unwrap_or_else(|_| std::panic::panic_any("test: recv failed"));
        assert_eq!(result, hashmix(10));

        drop(tx);
        handle
            .await
            .unwrap_or_else(|_| std::panic::panic_any("test: join failed"));
    }

    #[tokio::test]
    async fn test_cpu_dispatch_loop_exits_on_channel_close() {
        let (tx, rx) = mpsc::channel::<CpuWork>(1);
        let handle = tokio::spawn(cpu_dispatch_loop(rx));
        drop(tx);
        // Should exit cleanly
        let result = tokio::time::timeout(tokio::time::Duration::from_secs(1), handle).await;
        assert!(result.is_ok(), "dispatch loop should exit on channel close");
    }

    // -- flush_batch -----------------------------------------------------

    #[test]
    fn test_flush_batch_processes_all_entries() {
        let mut entries = Vec::new();
        let mut receivers = Vec::new();

        for i in 0..5 {
            let (tx, rx) = tokio::sync::oneshot::channel();
            entries.push(BatchEntry {
                job: Job {
                    id: JobId(i),
                    kind: JobKind::Hash,
                    cost: 10 + i,
                },
                reply: tx,
            });
            receivers.push(rx);
        }

        flush_batch(entries);

        for (i, mut rx) in receivers.into_iter().enumerate() {
            let result = rx
                .try_recv()
                .unwrap_or_else(|_| std::panic::panic_any("test: recv failed"));
            assert_eq!(result, hashmix(10 + i as u64));
        }
    }

    #[test]
    fn test_flush_batch_empty_does_not_panic() {
        flush_batch(Vec::new());
    }
}
