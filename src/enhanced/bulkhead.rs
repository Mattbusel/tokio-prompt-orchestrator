//! Bulkhead pattern — limits concurrent executions to prevent resource exhaustion.
//!
//! A bulkhead isolates concurrent execution slots so that one overloaded
//! consumer cannot starve others. Each [`Bulkhead`] wraps a semaphore; callers
//! acquire a permit before starting work and the permit is automatically
//! released when dropped.
//!
//! ## Behaviour
//!
//! - `acquire` is **non-blocking**: it uses `try_acquire` under the hood and
//!   immediately returns `Err` if no permits are available.
//! - The returned [`BulkheadPermit`] releases the semaphore slot on drop.
//! - Multiple bulkheads can share different limits for different subsystems
//!   (e.g. inference vs. RAG vs. post-processing).
//!
//! ## Usage
//!
//! ```rust
//! use tokio_prompt_orchestrator::enhanced::Bulkhead;
//!
//! # #[tokio::main]
//! # async fn main() {
//! let bulkhead = Bulkhead::new("inference", 4);
//!
//! match bulkhead.acquire() {
//!     Ok(permit) => {
//!         // Do guarded work here.
//!         // `permit` releases the slot automatically on drop.
//!         drop(permit);
//!     }
//!     Err(e) => {
//!         eprintln!("Bulkhead full: {e}");
//!     }
//! }
//! # }
//! ```

use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::OrchestratorError;

/// Limits concurrent executions to `max_concurrent` using a semaphore.
///
/// Acquire a permit before starting a unit of work. If no permits are
/// available the call returns `Err` immediately (non-blocking).
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone)]
pub struct Bulkhead {
    semaphore: Arc<Semaphore>,
    name: String,
    max_concurrent: usize,
}

/// An RAII permit that releases one bulkhead slot on drop.
///
/// Returned by [`Bulkhead::acquire`]. Dropping this value returns the slot
/// to the semaphore, allowing the next waiter to proceed.
///
/// # Panics
///
/// This type never panics.
#[must_use = "Dropping a BulkheadPermit immediately releases the slot"]
pub struct BulkheadPermit {
    _inner: OwnedSemaphorePermit,
}

impl std::fmt::Debug for BulkheadPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BulkheadPermit").finish_non_exhaustive()
    }
}

impl Bulkhead {
    /// Creates a new `Bulkhead` that allows at most `max_concurrent` concurrent
    /// permit holders.
    ///
    /// # Arguments
    /// * `name`           — Human-readable name used in error messages.
    /// * `max_concurrent` — Maximum number of simultaneously held permits.
    ///
    /// # Panics
    ///
    /// Panics if `max_concurrent` is 0.
    pub fn new(name: impl Into<String>, max_concurrent: usize) -> Self {
        assert!(max_concurrent > 0, "Bulkhead max_concurrent must be > 0");
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            name: name.into(),
            max_concurrent,
        }
    }

    /// Attempts to acquire a permit without blocking.
    ///
    /// # Returns
    /// - `Ok(BulkheadPermit)` — a slot was available and has been reserved.
    /// - `Err(OrchestratorError::Other(_))` — all slots are currently occupied.
    ///
    /// The error message includes the bulkhead name and the configured limit,
    /// so callers can log it without additional context.
    pub fn acquire(&self) -> Result<BulkheadPermit, OrchestratorError> {
        match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(permit) => Ok(BulkheadPermit { _inner: permit }),
            Err(_) => Err(OrchestratorError::Other(format!(
                "bulkhead '{}' exhausted: max_concurrent={} slots are all occupied",
                self.name, self.max_concurrent
            ))),
        }
    }

    /// Returns the name of this bulkhead.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the maximum number of concurrent permits.
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Returns the number of permits currently available (not held).
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acquire_within_limit_succeeds() {
        let bh = Bulkhead::new("test", 3);
        let p1 = bh.acquire();
        let p2 = bh.acquire();
        let p3 = bh.acquire();

        assert!(p1.is_ok());
        assert!(p2.is_ok());
        assert!(p3.is_ok());
        assert_eq!(bh.available_permits(), 0);
    }

    #[test]
    fn test_acquire_beyond_limit_returns_error() {
        let bh = Bulkhead::new("test", 2);
        let _p1 = bh.acquire().expect("first acquire must succeed");
        let _p2 = bh.acquire().expect("second acquire must succeed");

        let result = bh.acquire();
        assert!(
            result.is_err(),
            "third acquire must fail when limit is 2"
        );

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("test"),
            "error message must include bulkhead name"
        );
    }

    #[test]
    fn test_permit_drop_releases_slot() {
        let bh = Bulkhead::new("test", 1);

        {
            let _permit = bh.acquire().expect("first acquire must succeed");
            assert_eq!(bh.available_permits(), 0);
            // permit is dropped here
        }

        assert_eq!(bh.available_permits(), 1, "slot must be released after drop");
        // Should be acquirable again.
        assert!(bh.acquire().is_ok());
    }

    #[test]
    fn test_name_and_max_concurrent_accessors() {
        let bh = Bulkhead::new("inference", 8);
        assert_eq!(bh.name(), "inference");
        assert_eq!(bh.max_concurrent(), 8);
    }

    #[tokio::test]
    async fn test_concurrent_acquire_and_release() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let bh = Arc::new(Bulkhead::new("concurrent-test", 5));
        let active = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..10 {
            let bh_clone = Arc::clone(&bh);
            let active_clone = Arc::clone(&active);
            handles.push(tokio::spawn(async move {
                match bh_clone.acquire() {
                    Ok(_permit) => {
                        let prev = active_clone.fetch_add(1, Ordering::SeqCst);
                        // Must never exceed max_concurrent.
                        assert!(prev < 5, "active={prev} must be < 5");
                        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                        active_clone.fetch_sub(1, Ordering::SeqCst);
                    }
                    Err(_) => {
                        // Expected when all slots are taken.
                    }
                }
            }));
        }

        for h in handles {
            h.await.expect("task must not panic");
        }
    }

    #[test]
    #[should_panic(expected = "max_concurrent must be > 0")]
    fn test_zero_max_concurrent_panics() {
        let _ = Bulkhead::new("bad", 0);
    }
}
