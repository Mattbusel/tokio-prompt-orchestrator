//! # Request Deduplication
//!
//! Coalesces identical in-flight requests so that only one backend call is
//! made when multiple callers submit the same prompt concurrently.
//!
//! ## Design
//!
//! Each call to [`RequestDeduplicator::submit`] computes a SHA-256 key over
//! `model_id + prompt_text`.  If a request with that key is already in-flight,
//! the caller receives a [`DedupDecision::Waiting`] containing a
//! [`tokio::sync::oneshot::Receiver`] that resolves when the original request
//! completes.  The first caller for a key receives
//! [`DedupDecision::Original`] and is responsible for calling
//! [`RequestDeduplicator::complete`] when the result is ready.
//!
//! ## TTL pruning
//!
//! Entries older than [`RequestDeduplicator::TTL_SECS`] (30 s) are pruned on
//! every call to `submit()` to prevent unbounded memory growth if the original
//! caller crashes without completing.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// TTL after which an in-flight dedup entry is considered stale and pruned.
const TTL_SECS: u64 = 30;

/// A unique identifier for an original (non-deduplicated) request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(pub String);

impl RequestId {
    /// Wrap a string as a `RequestId`.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Result of submitting a request to the deduplicator.
pub enum DedupDecision {
    /// This is the first request with this key; the caller should perform
    /// the actual inference and call [`RequestDeduplicator::complete`].
    Original(RequestId),
    /// An identical request is already in-flight.  The receiver will resolve
    /// with the response tokens when the original completes.
    Waiting(oneshot::Receiver<Vec<String>>),
}

/// Snapshot of deduplicator statistics.
#[derive(Debug, Clone)]
pub struct DedupStats {
    /// Total number of requests submitted (including duplicates).
    pub total_submitted: u64,
    /// Number of requests that were deduplicated (returned `Waiting`).
    pub deduplicated: u64,
    /// Number of requests currently in-flight.
    pub active_requests: usize,
    /// Fraction of requests that were deduplicated (`deduplicated / total_submitted`).
    pub dedup_rate: f64,
}

struct InFlightEntry {
    /// Original request ID.
    id: RequestId,
    /// Channels waiting for this request to complete.
    waiters: Vec<oneshot::Sender<Vec<String>>>,
    /// When the entry was created (for TTL pruning).
    created_at: Instant,
}

/// Deduplicates identical in-flight LLM requests by their content hash.
///
/// All methods take `&mut self`, so wrap in a `Mutex` or `RwLock` for
/// concurrent access.
pub struct RequestDeduplicator {
    /// In-flight entries keyed by SHA-256 hash of `model_id + prompt_text`.
    in_flight: HashMap<String, InFlightEntry>,
    total_submitted: u64,
    deduplicated: u64,
}

impl RequestDeduplicator {
    /// TTL for in-flight entries.
    pub const TTL: Duration = Duration::from_secs(TTL_SECS);

    /// Create a new, empty deduplicator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            in_flight: HashMap::new(),
            total_submitted: 0,
            deduplicated: 0,
        }
    }

    /// Compute the SHA-256 deduplication key for a given model ID and prompt.
    pub fn hash_key(model_id: &str, prompt_text: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(model_id.as_bytes());
        hasher.update(b"\x00");
        hasher.update(prompt_text.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Submit a request and decide whether it is the original or a duplicate.
    ///
    /// Prunes stale entries (older than 30 s) before processing the new
    /// request so that a crashed original does not block future requests
    /// with the same key forever.
    pub fn submit(
        &mut self,
        request_id: impl Into<String>,
        model_id: &str,
        prompt_text: &str,
    ) -> DedupDecision {
        // Prune stale entries first.
        self.prune_stale();

        let key = Self::hash_key(model_id, prompt_text);
        self.total_submitted += 1;

        if let Some(entry) = self.in_flight.get_mut(&key) {
            // Duplicate: register a waiter.
            self.deduplicated += 1;
            let (tx, rx) = oneshot::channel();
            entry.waiters.push(tx);
            DedupDecision::Waiting(rx)
        } else {
            // Original: insert the entry.
            let id = RequestId::new(request_id);
            self.in_flight.insert(
                key,
                InFlightEntry {
                    id: id.clone(),
                    waiters: Vec::new(),
                    created_at: Instant::now(),
                },
            );
            DedupDecision::Original(id)
        }
    }

    /// Signal that the original request identified by `model_id` +
    /// `prompt_text` has completed with the given `result`.
    ///
    /// All registered waiters are notified.  Returns the number of waiters
    /// that were fanned out to.
    ///
    /// If no in-flight entry is found (e.g., it was already pruned), returns
    /// `0`.
    pub fn complete(&mut self, model_id: &str, prompt_text: &str, result: Vec<String>) -> usize {
        let key = Self::hash_key(model_id, prompt_text);
        let Some(entry) = self.in_flight.remove(&key) else {
            return 0;
        };
        let waiter_count = entry.waiters.len();
        for tx in entry.waiters {
            // Ignore send errors: receiver may have been dropped.
            let _ = tx.send(result.clone());
        }
        waiter_count
    }

    /// Return current statistics.
    pub fn stats(&self) -> DedupStats {
        let total = self.total_submitted;
        let dedup = self.deduplicated;
        DedupStats {
            total_submitted: total,
            deduplicated: dedup,
            active_requests: self.in_flight.len(),
            dedup_rate: if total == 0 {
                0.0
            } else {
                dedup as f64 / total as f64
            },
        }
    }

    /// Prune in-flight entries older than [`Self::TTL`].
    ///
    /// Called automatically by `submit`; can also be called manually.
    pub fn prune_stale(&mut self) {
        let ttl = Self::TTL;
        self.in_flight
            .retain(|_, entry| entry.created_at.elapsed() < ttl);
    }

    /// Return the number of currently active (in-flight) requests.
    pub fn active_count(&self) -> usize {
        self.in_flight.len()
    }
}

impl Default for RequestDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests (15+)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to build a DedupDecision::Original's RequestId if it matches.
    fn unwrap_original(d: DedupDecision) -> RequestId {
        match d {
            DedupDecision::Original(id) => id,
            DedupDecision::Waiting(_) => panic!("expected Original, got Waiting"),
        }
    }

    fn unwrap_waiting(d: DedupDecision) -> oneshot::Receiver<Vec<String>> {
        match d {
            DedupDecision::Waiting(rx) => rx,
            DedupDecision::Original(_) => panic!("expected Waiting, got Original"),
        }
    }

    #[test]
    fn first_submission_is_original() {
        let mut dedup = RequestDeduplicator::new();
        let decision = dedup.submit("req-1", "gpt-4", "hello");
        let id = unwrap_original(decision);
        assert_eq!(id.as_str(), "req-1");
    }

    #[test]
    fn duplicate_submission_is_waiting() {
        let mut dedup = RequestDeduplicator::new();
        dedup.submit("req-1", "gpt-4", "hello");
        let decision = dedup.submit("req-2", "gpt-4", "hello");
        let _ = unwrap_waiting(decision);
    }

    #[test]
    fn different_prompts_are_both_original() {
        let mut dedup = RequestDeduplicator::new();
        let d1 = dedup.submit("req-1", "gpt-4", "hello");
        let d2 = dedup.submit("req-2", "gpt-4", "world");
        unwrap_original(d1);
        unwrap_original(d2);
    }

    #[test]
    fn different_models_same_prompt_are_both_original() {
        let mut dedup = RequestDeduplicator::new();
        let d1 = dedup.submit("req-1", "gpt-4", "hello");
        let d2 = dedup.submit("req-2", "claude-3", "hello");
        unwrap_original(d1);
        unwrap_original(d2);
    }

    #[tokio::test]
    async fn complete_fans_out_to_waiters() {
        let mut dedup = RequestDeduplicator::new();
        dedup.submit("req-1", "gpt-4", "hello");
        let rx1 = unwrap_waiting(dedup.submit("req-2", "gpt-4", "hello"));
        let rx2 = unwrap_waiting(dedup.submit("req-3", "gpt-4", "hello"));
        let n = dedup.complete("gpt-4", "hello", vec!["token".to_string()]);
        assert_eq!(n, 2);
        let r1 = rx1.await.expect("should receive");
        let r2 = rx2.await.expect("should receive");
        assert_eq!(r1, vec!["token"]);
        assert_eq!(r2, vec!["token"]);
    }

    #[tokio::test]
    async fn complete_removes_entry_so_next_is_original() {
        let mut dedup = RequestDeduplicator::new();
        dedup.submit("req-1", "gpt-4", "hello");
        dedup.complete("gpt-4", "hello", vec![]);
        let d = dedup.submit("req-3", "gpt-4", "hello");
        unwrap_original(d);
    }

    #[test]
    fn stats_initial_zeros() {
        let dedup = RequestDeduplicator::new();
        let s = dedup.stats();
        assert_eq!(s.total_submitted, 0);
        assert_eq!(s.deduplicated, 0);
        assert_eq!(s.active_requests, 0);
        assert!((s.dedup_rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_after_submissions() {
        let mut dedup = RequestDeduplicator::new();
        dedup.submit("r1", "m", "p1");
        dedup.submit("r2", "m", "p1"); // dedup
        dedup.submit("r3", "m", "p2");
        let s = dedup.stats();
        assert_eq!(s.total_submitted, 3);
        assert_eq!(s.deduplicated, 1);
        assert_eq!(s.active_requests, 2);
        let expected_rate = 1.0 / 3.0;
        assert!((s.dedup_rate - expected_rate).abs() < 1e-10);
    }

    #[test]
    fn hash_key_is_deterministic() {
        let k1 = RequestDeduplicator::hash_key("gpt-4", "hello world");
        let k2 = RequestDeduplicator::hash_key("gpt-4", "hello world");
        assert_eq!(k1, k2);
    }

    #[test]
    fn hash_key_differs_for_different_inputs() {
        let k1 = RequestDeduplicator::hash_key("gpt-4", "hello");
        let k2 = RequestDeduplicator::hash_key("gpt-4", "world");
        assert_ne!(k1, k2);
    }

    #[test]
    fn hash_key_differs_for_different_models() {
        let k1 = RequestDeduplicator::hash_key("gpt-4", "hello");
        let k2 = RequestDeduplicator::hash_key("claude-3", "hello");
        assert_ne!(k1, k2);
    }

    #[test]
    fn complete_returns_zero_when_no_entry() {
        let mut dedup = RequestDeduplicator::new();
        let n = dedup.complete("gpt-4", "nonexistent", vec![]);
        assert_eq!(n, 0);
    }

    #[test]
    fn active_count_decreases_after_complete() {
        let mut dedup = RequestDeduplicator::new();
        dedup.submit("r1", "m", "p");
        assert_eq!(dedup.active_count(), 1);
        dedup.complete("m", "p", vec![]);
        assert_eq!(dedup.active_count(), 0);
    }

    #[test]
    fn prune_stale_removes_old_entries() {
        let mut dedup = RequestDeduplicator::new();
        // Manually insert a stale entry.
        let key = RequestDeduplicator::hash_key("m", "p");
        dedup.in_flight.insert(
            key,
            super::InFlightEntry {
                id: RequestId::new("old"),
                waiters: Vec::new(),
                created_at: Instant::now()
                    .checked_sub(Duration::from_secs(60))
                    .unwrap_or_else(Instant::now),
            },
        );
        assert_eq!(dedup.active_count(), 1);
        dedup.prune_stale();
        assert_eq!(dedup.active_count(), 0);
    }

    #[test]
    fn dedup_rate_zero_when_no_submissions() {
        let dedup = RequestDeduplicator::new();
        assert!((dedup.stats().dedup_rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn request_id_display() {
        let id = RequestId::new("my-id");
        assert_eq!(id.to_string(), "my-id");
    }

    #[test]
    fn multiple_waiters_all_notified() {
        let mut dedup = RequestDeduplicator::new();
        dedup.submit("r1", "m", "prompt");
        // Register 5 waiters.
        let mut rxs = Vec::new();
        for i in 2..=6 {
            let rx = unwrap_waiting(dedup.submit(&format!("r{i}"), "m", "prompt"));
            rxs.push(rx);
        }
        let n = dedup.complete("m", "prompt", vec!["out".to_string()]);
        assert_eq!(n, 5);
        // All receivers have a value pending (non-async check via try_recv).
        for rx in rxs {
            assert!(rx.try_recv().is_ok());
        }
    }
}
