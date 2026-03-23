//! # Multi-Level Priority Queue with Anti-Starvation Aging
//!
//! Provides a five-level priority queue where low-priority items are
//! automatically promoted ("aged") when they have waited longer than the
//! configured threshold. This prevents starvation of [`Priority::Background`]
//! items under sustained [`Priority::Critical`] load.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::priority_queue::{
//!     AgingConfig, MultiLevelQueue, Priority,
//! };
//!
//! let config = AgingConfig::default();
//! let queue: MultiLevelQueue<String> = MultiLevelQueue::new(config);
//!
//! queue.push("critical task".into(), Priority::Critical);
//! queue.push("background task".into(), Priority::Background);
//!
//! // Critical pops first.
//! assert_eq!(queue.pop(), Some("critical task".into()));
//! assert_eq!(queue.pop(), Some("background task".into()));
//! assert_eq!(queue.pop(), None);
//! ```

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

// ---------------------------------------------------------------------------
// Priority
// ---------------------------------------------------------------------------

/// Five discrete priority levels. Lower numeric value = higher urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// Highest urgency — always dequeued first.
    Critical = 0,
    /// High urgency.
    High = 1,
    /// Default urgency.
    Normal = 2,
    /// Low urgency.
    Low = 3,
    /// Lowest urgency — most susceptible to starvation without aging.
    Background = 4,
}

impl Priority {
    /// Total number of priority levels.
    pub const COUNT: usize = 5;

    /// Convert a raw `u8` index (0–4) into a [`Priority`].  Returns `None`
    /// for values outside that range.
    pub fn from_index(idx: u8) -> Option<Self> {
        match idx {
            0 => Some(Self::Critical),
            1 => Some(Self::High),
            2 => Some(Self::Normal),
            3 => Some(Self::Low),
            4 => Some(Self::Background),
            _ => None,
        }
    }

    /// The numeric index of this priority (same as the discriminant).
    #[inline]
    pub fn index(self) -> usize {
        self as usize
    }
}

// ---------------------------------------------------------------------------
// PriorityItem
// ---------------------------------------------------------------------------

/// A wrapped item with priority metadata.
///
/// `effective_priority` starts equal to the assigned priority level and is
/// decremented (promoted) by the aging pass when the item waits too long.
pub struct PriorityItem<T> {
    /// The payload.
    pub item: T,
    /// The priority assigned at enqueue time.
    pub priority: Priority,
    /// Wall-clock instant when the item was enqueued.
    pub enqueued_at: Instant,
    /// Current effective priority, decremented on each aging promotion.
    /// Stored as a raw index (0 = Critical … 4 = Background).
    pub effective_priority: AtomicU8,
}

impl<T> PriorityItem<T> {
    fn new(item: T, priority: Priority) -> Self {
        Self {
            item,
            priority,
            enqueued_at: Instant::now(),
            effective_priority: AtomicU8::new(priority as u8),
        }
    }

    /// Milliseconds elapsed since this item was enqueued.
    pub fn wait_ms(&self) -> u64 {
        self.enqueued_at.elapsed().as_millis() as u64
    }
}

// ---------------------------------------------------------------------------
// AgingConfig
// ---------------------------------------------------------------------------

/// Per-level aging thresholds and promotion caps.
///
/// Index mapping:
/// - `[0]` = threshold for Background → Low
/// - `[1]` = threshold for Low → Normal
/// - `[2]` = threshold for Normal → High
/// - `[3]` = threshold for High → Critical
#[derive(Debug, Clone)]
pub struct AgingConfig {
    /// Milliseconds an item must wait at each level before being promoted.
    /// Four entries (one per "promotable" level: Background, Low, Normal, High).
    pub age_threshold_ms: [u64; 4],
    /// Maximum number of promotion steps an item may receive during its lifetime.
    pub max_age_promotions: u8,
}

impl Default for AgingConfig {
    fn default() -> Self {
        Self {
            // Background waits 5 s, Low 10 s, Normal 30 s, High 60 s.
            age_threshold_ms: [5_000, 10_000, 30_000, 60_000],
            max_age_promotions: 4,
        }
    }
}

// ---------------------------------------------------------------------------
// QueueStats
// ---------------------------------------------------------------------------

/// Aggregate statistics for a [`MultiLevelQueue`].
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    /// Total items ever enqueued (all priorities).
    pub total_enqueued: u64,
    /// Total items ever dequeued.
    pub total_dequeued: u64,
    /// Total aging promotions applied across all items.
    pub total_promotions: u64,
    /// Average wait time in milliseconds per priority level (index = level).
    pub avg_wait_ms: [f64; Priority::COUNT],
}

// ---------------------------------------------------------------------------
// Inner state (held behind Mutex)
// ---------------------------------------------------------------------------

struct Inner<T> {
    /// Five buckets, one per priority level.
    buckets: [VecDeque<Arc<PriorityItem<T>>>; Priority::COUNT],
    config: AgingConfig,
    // Stats accumulators.
    total_enqueued: u64,
    total_dequeued: u64,
    total_promotions: u64,
    /// Sum of wait_ms per level for items that have been dequeued.
    wait_sum_ms: [f64; Priority::COUNT],
    /// Count of dequeued items per level (for average calculation).
    dequeue_count: [u64; Priority::COUNT],
}

impl<T> Inner<T> {
    fn new(config: AgingConfig) -> Self {
        Self {
            buckets: [
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
            ],
            config,
            total_enqueued: 0,
            total_dequeued: 0,
            total_promotions: 0,
            wait_sum_ms: [0.0; Priority::COUNT],
            dequeue_count: [0; Priority::COUNT],
        }
    }

    /// Run the aging pass: scan each "promotable" bucket and move items whose
    /// wait exceeds the threshold into the next-higher-priority bucket.
    ///
    /// Buckets are processed from lowest (Background=4) to highest (High=1).
    /// Critical items can never be promoted further.
    fn age(&mut self) {
        // Levels 4 → 1 are promotable (Background, Low, Normal, High).
        for src_level in (1..=4usize).rev() {
            let threshold_idx = src_level - 1; // maps level 4→[0], 3→[1], 2→[2], 1→[3]
            let threshold_ms = self.config.age_threshold_ms[threshold_idx];
            let dst_level = src_level - 1;

            // We need to drain items from src bucket that qualify.
            // Collect indices to promote (in FIFO order) without holding two mut refs.
            let mut to_promote: Vec<Arc<PriorityItem<T>>> = Vec::new();
            let mut remaining: VecDeque<Arc<PriorityItem<T>>> = VecDeque::new();

            while let Some(arc_item) = self.buckets[src_level].pop_front() {
                let current_eff = arc_item.effective_priority.load(Ordering::Relaxed);
                let promotions_taken = arc_item.priority as u8 - current_eff; // how many times already promoted
                if arc_item.wait_ms() >= threshold_ms
                    && promotions_taken < self.config.max_age_promotions
                {
                    to_promote.push(arc_item);
                } else {
                    remaining.push_back(arc_item);
                }
            }
            self.buckets[src_level] = remaining;

            for arc_item in to_promote {
                arc_item
                    .effective_priority
                    .store(dst_level as u8, Ordering::Relaxed);
                self.buckets[dst_level].push_back(arc_item);
                self.total_promotions += 1;
            }
        }
    }

    /// Dequeue from the highest-priority non-empty bucket.
    fn pop(&mut self) -> Option<(T, usize)> {
        self.age();
        for level in 0..Priority::COUNT {
            if let Some(arc_item) = self.buckets[level].pop_front() {
                let wait_ms = arc_item.wait_ms();
                let original_level = arc_item.priority as usize;
                // Unwrap the Arc — we own the only reference at this point.
                let item = Arc::try_unwrap(arc_item)
                    .ok()
                    .map(|pi| pi.item)
                    .expect("priority_queue: extra Arc reference detected — bug in aging pass");
                self.total_dequeued += 1;
                self.wait_sum_ms[original_level] += wait_ms as f64;
                self.dequeue_count[original_level] += 1;
                return Some((item, original_level));
            }
        }
        None
    }

    fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.len()).sum()
    }

    fn len_by_priority(&self) -> [usize; Priority::COUNT] {
        [
            self.buckets[0].len(),
            self.buckets[1].len(),
            self.buckets[2].len(),
            self.buckets[3].len(),
            self.buckets[4].len(),
        ]
    }

    fn stats(&self) -> QueueStats {
        let mut avg_wait_ms = [0.0f64; Priority::COUNT];
        for i in 0..Priority::COUNT {
            if self.dequeue_count[i] > 0 {
                avg_wait_ms[i] = self.wait_sum_ms[i] / self.dequeue_count[i] as f64;
            }
        }
        QueueStats {
            total_enqueued: self.total_enqueued,
            total_dequeued: self.total_dequeued,
            total_promotions: self.total_promotions,
            avg_wait_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// MultiLevelQueue
// ---------------------------------------------------------------------------

/// Thread-safe multi-level priority queue with anti-starvation aging.
///
/// Items are held in five internal [`VecDeque`] buckets — one per
/// [`Priority`] level. Before every [`pop`](MultiLevelQueue::pop) call, an
/// aging pass scans lower-priority buckets and promotes items whose wait time
/// exceeds the configured threshold.
pub struct MultiLevelQueue<T> {
    inner: Mutex<Inner<T>>,
}

impl<T> MultiLevelQueue<T> {
    /// Create a new queue with the given [`AgingConfig`].
    pub fn new(config: AgingConfig) -> Self {
        Self {
            inner: Mutex::new(Inner::new(config)),
        }
    }

    /// Enqueue `item` at the given `priority`.
    ///
    /// This is a non-async, non-blocking call (only acquires the internal
    /// `Mutex`).
    pub fn push(&self, item: T, priority: Priority) {
        let arc = Arc::new(PriorityItem::new(item, priority));
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.buckets[priority.index()].push_back(arc);
        guard.total_enqueued += 1;
    }

    /// Dequeue the highest-priority available item.
    ///
    /// Runs an aging pass before selecting the item, so a previously
    /// low-priority item that has waited long enough may be returned ahead of
    /// a recently-added same-level item.
    ///
    /// Returns `None` when all buckets are empty.
    pub fn pop(&self) -> Option<T> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.pop().map(|(item, _level)| item)
    }

    /// Total number of items across all buckets.
    pub fn len(&self) -> usize {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.len()
    }

    /// Returns `true` when all buckets are empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Per-priority item counts.  Index `i` corresponds to [`Priority`] level `i`.
    pub fn len_by_priority(&self) -> [usize; Priority::COUNT] {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.len_by_priority()
    }

    /// Snapshot of queue statistics.
    pub fn stats(&self) -> QueueStats {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.stats()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn default_queue() -> MultiLevelQueue<&'static str> {
        MultiLevelQueue::new(AgingConfig::default())
    }

    #[test]
    fn empty_pop_returns_none() {
        let q: MultiLevelQueue<u32> = MultiLevelQueue::new(AgingConfig::default());
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn critical_before_background() {
        let q = default_queue();
        q.push("bg", Priority::Background);
        q.push("crit", Priority::Critical);
        q.push("normal", Priority::Normal);

        // Critical must come first.
        assert_eq!(q.pop(), Some("crit"));
        assert_eq!(q.pop(), Some("normal"));
        assert_eq!(q.pop(), Some("bg"));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn ordering_all_levels() {
        let q = default_queue();
        q.push("bg", Priority::Background);
        q.push("low", Priority::Low);
        q.push("normal", Priority::Normal);
        q.push("high", Priority::High);
        q.push("crit", Priority::Critical);

        assert_eq!(q.pop(), Some("crit"));
        assert_eq!(q.pop(), Some("high"));
        assert_eq!(q.pop(), Some("normal"));
        assert_eq!(q.pop(), Some("low"));
        assert_eq!(q.pop(), Some("bg"));
    }

    #[test]
    fn mixed_priorities_drain_correctly() {
        let q: MultiLevelQueue<u32> = MultiLevelQueue::new(AgingConfig::default());
        for i in 0u32..5 {
            q.push(i, Priority::Normal);
        }
        q.push(99, Priority::Critical);
        q.push(100, Priority::High);

        assert_eq!(q.pop(), Some(99)); // Critical first
        assert_eq!(q.pop(), Some(100)); // High second
        // Then five Normal items in FIFO order.
        for i in 0u32..5 {
            assert_eq!(q.pop(), Some(i));
        }
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn len_and_len_by_priority() {
        let q: MultiLevelQueue<i32> = MultiLevelQueue::new(AgingConfig::default());
        assert_eq!(q.len(), 0);

        q.push(1, Priority::Critical);
        q.push(2, Priority::Normal);
        q.push(3, Priority::Normal);
        q.push(4, Priority::Background);

        assert_eq!(q.len(), 4);
        let by_level = q.len_by_priority();
        assert_eq!(by_level[0], 1); // Critical
        assert_eq!(by_level[1], 0); // High
        assert_eq!(by_level[2], 2); // Normal
        assert_eq!(by_level[3], 0); // Low
        assert_eq!(by_level[4], 1); // Background
    }

    #[test]
    fn aging_promotes_background_to_low() {
        // Use a very short threshold (10 ms) so the test doesn't have to wait.
        let config = AgingConfig {
            age_threshold_ms: [10, 200, 1_000, 5_000],
            max_age_promotions: 4,
        };
        let q: MultiLevelQueue<&str> = MultiLevelQueue::new(config);

        q.push("bg", Priority::Background);
        // Wait 20 ms — long enough to trigger Background → Low promotion.
        thread::sleep(Duration::from_millis(20));

        // Push a Low-priority item *after* sleeping so the Background item
        // has been waiting longer than the threshold.
        q.push("low-new", Priority::Low);

        // pop() will age first; "bg" should have been promoted to Low and be
        // ahead of "low-new" (FIFO within a level, but it was appended to the
        // Low bucket during aging, before "low-new" is enqueued).
        let first = q.pop().expect("should have item");
        let second = q.pop().expect("should have second item");
        // Both items are now at Low priority level; "bg" was promoted first.
        assert_eq!(first, "bg");
        assert_eq!(second, "low-new");
    }

    #[test]
    fn stats_track_enqueue_and_dequeue() {
        let q: MultiLevelQueue<u8> = MultiLevelQueue::new(AgingConfig::default());
        q.push(1, Priority::Critical);
        q.push(2, Priority::Normal);
        q.pop();
        q.pop();

        let stats = q.stats();
        assert_eq!(stats.total_enqueued, 2);
        assert_eq!(stats.total_dequeued, 2);
    }

    #[test]
    fn max_promotions_respected() {
        // Only one promotion allowed.
        let config = AgingConfig {
            age_threshold_ms: [1, 1, 1, 1],
            max_age_promotions: 1,
        };
        let q: MultiLevelQueue<u8> = MultiLevelQueue::new(config);
        q.push(42, Priority::Background);

        // Sleep enough that all thresholds are exceeded.
        thread::sleep(Duration::from_millis(5));
        // pop triggers aging; with max_age_promotions=1 the item can only move
        // one level (Background → Low), not all the way to Critical.
        let _ = q.pop();
        let stats = q.stats();
        // At most 1 promotion should have been recorded.
        assert!(stats.total_promotions <= 1);
    }
}
