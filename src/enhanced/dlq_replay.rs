//! Dead-Letter Queue Replay Scheduler
//!
//! [`DlqReplayScheduler`] wraps the pipeline's [`DeadLetterQueue`] and adds
//! controlled re-injection of shed requests back into the pipeline with
//! exponential backoff between each replay, optional per-session filtering,
//! and age-based eviction.
//!
//! ## Design
//!
//! The scheduler holds its own internal queue of [`ReplayEntry`] items.
//! A "dropped" request from the [`DeadLetterQueue`] can be converted into a
//! `ReplayEntry` and pushed here. The scheduler does **not** automatically
//! drain the main `DeadLetterQueue` — callers feed it explicitly, which keeps
//! the concern separation clean and lets callers control replay policy.
//!
//! ## Prometheus Counters
//!
//! | Metric | Description |
//! |--------|-------------|
//! | `dlq_replayed_total` | Requests successfully re-injected into the pipeline |
//! | `dlq_aged_out_total` | Requests evicted by [`DlqReplayScheduler::age_out`] |
//!
//! ## Example
//!
//! ```rust
//! use std::collections::HashMap;
//! use std::time::Duration;
//! use tokio::sync::mpsc;
//! use tokio_prompt_orchestrator::{PromptRequest, SessionId};
//! use tokio_prompt_orchestrator::enhanced::dlq_replay::{DlqReplayScheduler, ReplayEntry};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let (tx, mut rx) = mpsc::channel::<PromptRequest>(16);
//! let scheduler = DlqReplayScheduler::new();
//!
//! let req = PromptRequest {
//!     session: SessionId::new("session-1"),
//!     request_id: "req-42".to_string(),
//!     input: "retry me".to_string(),
//!     meta: HashMap::new(),
//!     deadline: None,
//! };
//!
//! scheduler.push(ReplayEntry::new(req));
//! scheduler.replay_all(&tx).await;
//!
//! let replayed = rx.recv().await.expect("replayed request");
//! assert_eq!(replayed.request_id, "req-42");
//! # }
//! ```

use crate::{PromptRequest, SessionId};
use lazy_static::lazy_static;
use prometheus::{Counter, Opts, Registry};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc::Sender;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Prometheus counters
// ---------------------------------------------------------------------------

lazy_static! {
    static ref DLQ_REGISTRY: Registry = Registry::new();

    /// Total DLQ entries successfully replayed into the pipeline.
    static ref DLQ_REPLAYED: Counter = {
        let c = Counter::with_opts(Opts::new(
            "dlq_replayed_total",
            "Total dead-letter queue entries successfully re-injected into the pipeline",
        ))
        .expect("dlq_replayed_total metric construction");
        DLQ_REGISTRY
            .register(Box::new(c.clone()))
            .expect("dlq_replayed_total registration");
        c
    };

    /// Total DLQ entries removed by age_out().
    static ref DLQ_AGED_OUT: Counter = {
        let c = Counter::with_opts(Opts::new(
            "dlq_aged_out_total",
            "Total dead-letter queue entries evicted because they exceeded max_age",
        ))
        .expect("dlq_aged_out_total metric construction");
        DLQ_REGISTRY
            .register(Box::new(c.clone()))
            .expect("dlq_aged_out_total registration");
        c
    };
}

// ---------------------------------------------------------------------------
// ReplayEntry
// ---------------------------------------------------------------------------

/// A single entry in the [`DlqReplayScheduler`] internal queue.
///
/// Wraps a [`PromptRequest`] together with replay bookkeeping: when the entry
/// was first queued for replay, how many replay attempts have been made, and
/// the next-earliest wall-clock time at which it may be retried (exponential
/// backoff).
#[derive(Debug, Clone)]
pub struct ReplayEntry {
    /// The request to be replayed.
    pub request: PromptRequest,
    /// Wall-clock time at which this entry was added to the replay scheduler.
    pub queued_at: SystemTime,
    /// Number of times this entry has been attempted (started at 0).
    pub attempts: u32,
    /// Earliest instant at which the next attempt is permitted.
    pub next_attempt_at: Instant,
}

impl ReplayEntry {
    /// Create a `ReplayEntry` from a [`PromptRequest`].
    ///
    /// The entry is immediately eligible for replay (`next_attempt_at` = now).
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn new(request: PromptRequest) -> Self {
        Self {
            request,
            queued_at: SystemTime::now(),
            attempts: 0,
            next_attempt_at: Instant::now(),
        }
    }

    /// Returns `true` if the entry's next-attempt window has elapsed.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn is_ready(&self) -> bool {
        Instant::now() >= self.next_attempt_at
    }

    /// Returns `true` if the entry is older than `max_age` (based on [`queued_at`]).
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn is_aged_out(&self, max_age: Duration) -> bool {
        self.queued_at
            .elapsed()
            .map(|e| e >= max_age)
            .unwrap_or(true)
    }

    /// Advance backoff for the next retry attempt using exponential backoff
    /// with a base of 2 and a cap of 60 seconds.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    fn advance_backoff(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
        // 2^attempts seconds, capped at 60 s.
        let secs = (1u64 << self.attempts.min(6)).min(60);
        self.next_attempt_at = Instant::now() + Duration::from_secs(secs);
    }
}

// ---------------------------------------------------------------------------
// DlqReplayScheduler
// ---------------------------------------------------------------------------

/// Dead-letter queue replay scheduler.
///
/// Wraps the pipeline's dead-letter concept and adds:
///
/// - [`replay_all`](Self::replay_all) — re-inject every eligible entry with
///   exponential backoff between sends.
/// - [`replay_by_session`](Self::replay_by_session) — replay only entries
///   belonging to a specific session.
/// - [`age_out`](Self::age_out) — drop entries older than a given
///   [`Duration`].
///
/// `DlqReplayScheduler` is `Clone + Send + Sync`.  All clones share the same
/// internal queue.
///
/// # Prometheus counters
///
/// - `dlq_replayed_total` — incremented on each successful re-injection.
/// - `dlq_aged_out_total` — incremented on each eviction by `age_out`.
#[derive(Clone)]
pub struct DlqReplayScheduler {
    queue: Arc<Mutex<Vec<ReplayEntry>>>,
    /// Total entries replayed across the lifetime of this scheduler.
    pub replayed_total: Arc<AtomicU64>,
    /// Total entries aged out across the lifetime of this scheduler.
    pub aged_out_total: Arc<AtomicU64>,
}

impl DlqReplayScheduler {
    /// Create a new empty `DlqReplayScheduler`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(Vec::new())),
            replayed_total: Arc::new(AtomicU64::new(0)),
            aged_out_total: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Push a new [`ReplayEntry`] into the scheduler queue.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn push(&self, entry: ReplayEntry) {
        let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        debug!(
            request_id = %entry.request.request_id,
            session_id = %entry.request.session,
            "dlq_replay: entry queued"
        );
        q.push(entry);
    }

    /// Return the number of entries currently waiting for replay.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn len(&self) -> usize {
        self.queue.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Return `true` if no entries are queued.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Re-inject **all** eligible entries back into the pipeline.
    ///
    /// An entry is eligible if [`ReplayEntry::is_ready`] returns `true`
    /// (i.e. its exponential-backoff window has elapsed).
    ///
    /// Between consecutive sends the scheduler applies exponential backoff
    /// (from the entry's own `attempts` counter), sleeping the current task
    /// for the computed delay.  Entries that are sent successfully are removed
    /// from the queue.  Entries whose `Sender` is closed are left in the queue
    /// and a `warn!` is emitted.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub async fn replay_all(&self, sender: &Sender<PromptRequest>) {
        self.replay_filtered(sender, |_| true).await;
    }

    /// Re-inject all entries belonging to `session_id` back into the pipeline.
    ///
    /// Behaviour is identical to [`replay_all`](Self::replay_all) but only
    /// entries whose `request.session` matches `session_id` are attempted.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub async fn replay_by_session(&self, session_id: &SessionId, sender: &Sender<PromptRequest>) {
        let sid = session_id.clone();
        self.replay_filtered(sender, move |e| e.request.session == sid)
            .await;
    }

    /// Internal replay implementation with a caller-supplied filter predicate.
    async fn replay_filtered<F>(&self, sender: &Sender<PromptRequest>, filter: F)
    where
        F: Fn(&ReplayEntry) -> bool,
    {
        // Drain the candidates out of the queue under the lock, then process
        // them without holding the lock across await points.
        let candidates: Vec<ReplayEntry> = {
            let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            // Partition: remove entries that match the filter AND are ready.
            let mut to_replay = Vec::new();
            let mut to_keep = Vec::new();
            for entry in q.drain(..) {
                if filter(&entry) && entry.is_ready() {
                    to_replay.push(entry);
                } else {
                    to_keep.push(entry);
                }
            }
            *q = to_keep;
            to_replay
        };

        if candidates.is_empty() {
            debug!("dlq_replay: no eligible entries for replay");
            return;
        }

        info!(count = candidates.len(), "dlq_replay: replaying entries");

        for mut entry in candidates {
            // Apply exponential backoff delay before sending (skip on first attempt).
            if entry.attempts > 0 {
                let delay_secs = (1u64 << entry.attempts.min(6)).min(60);
                let delay = Duration::from_secs(delay_secs);
                debug!(
                    request_id = %entry.request.request_id,
                    delay_secs = delay_secs,
                    "dlq_replay: waiting before retry"
                );
                tokio::time::sleep(delay).await;
            }

            let request_id = entry.request.request_id.clone();
            let session = entry.request.session.clone();

            match sender.try_send(entry.request.clone()) {
                Ok(()) => {
                    let _ = DLQ_REPLAYED.inc();
                    self.replayed_total.fetch_add(1, Ordering::Relaxed);
                    info!(
                        request_id = %request_id,
                        session_id = %session,
                        attempts = entry.attempts + 1,
                        "dlq_replay: request re-injected"
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Channel full — put it back with advanced backoff.
                    entry.advance_backoff();
                    warn!(
                        request_id = %request_id,
                        "dlq_replay: pipeline channel full, rescheduling with backoff"
                    );
                    let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
                    q.push(entry);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    warn!(
                        request_id = %request_id,
                        "dlq_replay: pipeline channel closed, dropping entry"
                    );
                }
            }
        }
    }

    /// Remove and discard all entries older than `max_age`.
    ///
    /// Increments `dlq_aged_out_total` and the `aged_out_total` atomic for
    /// each evicted entry.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn age_out(&self, max_age: Duration) {
        let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let before = q.len();
        q.retain(|e| !e.is_aged_out(max_age));
        let evicted = before - q.len();

        if evicted > 0 {
            let _ = DLQ_AGED_OUT.inc_by(evicted as f64);
            self.aged_out_total
                .fetch_add(evicted as u64, Ordering::Relaxed);
            info!(
                evicted = evicted,
                remaining = q.len(),
                "dlq_replay: aged out entries"
            );
        }
    }
}

impl Default for DlqReplayScheduler {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    fn make_request(id: &str, session: &str) -> PromptRequest {
        PromptRequest {
            session: SessionId::new(session),
            request_id: id.to_string(),
            input: format!("input-{id}"),
            meta: HashMap::new(),
            deadline: None,
        }
    }

    #[tokio::test]
    async fn test_replay_all_sends_entries() {
        let sched = DlqReplayScheduler::new();
        sched.push(ReplayEntry::new(make_request("req-1", "s1")));
        sched.push(ReplayEntry::new(make_request("req-2", "s1")));

        let (tx, mut rx) = mpsc::channel(16);
        sched.replay_all(&tx).await;

        let r1 = rx.try_recv().expect("first replayed");
        let r2 = rx.try_recv().expect("second replayed");

        let mut ids = vec![r1.request_id, r2.request_id];
        ids.sort();
        assert_eq!(ids, vec!["req-1", "req-2"]);

        assert_eq!(sched.replayed_total.load(Ordering::Relaxed), 2);
        assert!(sched.is_empty());
    }

    #[tokio::test]
    async fn test_replay_by_session_filters_correctly() {
        let sched = DlqReplayScheduler::new();
        sched.push(ReplayEntry::new(make_request("r-a", "session-A")));
        sched.push(ReplayEntry::new(make_request("r-b", "session-B")));
        sched.push(ReplayEntry::new(make_request("r-c", "session-A")));

        let (tx, mut rx) = mpsc::channel(16);
        sched
            .replay_by_session(&SessionId::new("session-A"), &tx)
            .await;

        let mut replayed = Vec::new();
        while let Ok(r) = rx.try_recv() {
            replayed.push(r.request_id);
        }
        replayed.sort();
        assert_eq!(replayed, vec!["r-a", "r-c"]);

        // session-B entry should still be in the queue.
        assert_eq!(sched.len(), 1);
    }

    #[test]
    fn test_age_out_removes_old_entries() {
        let sched = DlqReplayScheduler::new();

        // Create an entry and manually backdate its queued_at via a tiny
        // non-zero max_age (0 duration means every entry is "aged out").
        let entry = ReplayEntry {
            request: make_request("old", "s"),
            queued_at: SystemTime::now() - Duration::from_secs(120),
            attempts: 0,
            next_attempt_at: Instant::now(),
        };
        sched.push(entry);

        // Fresh entry — should NOT be evicted.
        sched.push(ReplayEntry::new(make_request("fresh", "s")));

        assert_eq!(sched.len(), 2);
        sched.age_out(Duration::from_secs(60));

        assert_eq!(sched.len(), 1);
        assert_eq!(sched.aged_out_total.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_age_out_keeps_fresh_entries() {
        let sched = DlqReplayScheduler::new();
        sched.push(ReplayEntry::new(make_request("r1", "s")));
        sched.push(ReplayEntry::new(make_request("r2", "s")));

        sched.age_out(Duration::from_secs(3600));

        // Both entries should survive.
        assert_eq!(sched.len(), 2);
        assert_eq!(sched.aged_out_total.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_replay_all_empty_queue_is_noop() {
        let sched = DlqReplayScheduler::new();
        let (tx, mut rx) = mpsc::channel(16);
        sched.replay_all(&tx).await;
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_replay_entry_is_ready_immediately() {
        let entry = ReplayEntry::new(make_request("x", "s"));
        assert!(entry.is_ready());
    }

    #[test]
    fn test_replay_entry_not_ready_after_backoff() {
        let mut entry = ReplayEntry::new(make_request("x", "s"));
        // Simulate 3 prior attempts so backoff is 2^3 = 8 s from now.
        entry.attempts = 3;
        entry.next_attempt_at = Instant::now() + Duration::from_secs(8);
        assert!(!entry.is_ready());
    }

    #[test]
    fn test_is_aged_out_old_entry() {
        let entry = ReplayEntry {
            request: make_request("x", "s"),
            queued_at: SystemTime::now() - Duration::from_secs(200),
            attempts: 0,
            next_attempt_at: Instant::now(),
        };
        assert!(entry.is_aged_out(Duration::from_secs(100)));
        assert!(!entry.is_aged_out(Duration::from_secs(300)));
    }

    #[test]
    fn test_default_trait() {
        let s = DlqReplayScheduler::default();
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn test_replayed_total_counter_increments() {
        let sched = DlqReplayScheduler::new();
        for i in 0..5 {
            sched.push(ReplayEntry::new(make_request(
                &format!("r{i}"),
                "session",
            )));
        }
        let (tx, _rx) = mpsc::channel(16);
        sched.replay_all(&tx).await;
        assert_eq!(sched.replayed_total.load(Ordering::Relaxed), 5);
    }
}
