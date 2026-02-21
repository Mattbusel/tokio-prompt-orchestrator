//! Priority Queue
//!
//! Manages requests with different priority levels.
//!
//! ## Usage
//!
//! ```no_run
//! use std::collections::HashMap;
//! use tokio_prompt_orchestrator::{SessionId, PromptRequest};
//! use tokio_prompt_orchestrator::enhanced::{PriorityQueue, Priority};
//! # #[tokio::main]
//! # async fn main() {
//! # let make_req = || PromptRequest {
//! #     session: SessionId::new("s"),
//! #     request_id: String::new(),
//! #     input: String::new(),
//! #     meta: HashMap::new(),
//! # };
//! # let (request1, request2, request3) = (make_req(), make_req(), make_req());
//! let queue = PriorityQueue::new();
//!
//! queue.push(Priority::High, request1).await.ok();
//! queue.push(Priority::Normal, request2).await.ok();
//! queue.push(Priority::Low, request3).await.ok();
//!
//! // Dequeue by priority — highest priority first
//! if let Some((_priority, request)) = queue.pop().await {
//!     println!("{}", request.input);
//! }
//! # }
//! ```

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::debug;

use crate::PromptRequest;

/// Request priority levels
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Lowest priority — background / batch work.
    Low = 0,
    /// Standard priority for most requests.
    #[default]
    Normal = 1,
    /// Elevated priority, processed before `Normal`.
    High = 2,
    /// Highest priority — processed immediately ahead of all others.
    Critical = 3,
}

impl Priority {
    /// Parse a priority level from a name string (`"low"`, `"normal"`, `"high"`, `"critical"`).
    ///
    /// Returns `None` for unrecognised strings.
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "low" => Some(Priority::Low),
            "normal" => Some(Priority::Normal),
            "high" => Some(Priority::High),
            "critical" => Some(Priority::Critical),
            _ => None,
        }
    }
}

/// Prioritized request wrapper
#[derive(Clone)]
struct PrioritizedRequest {
    priority: Priority,
    sequence: u64, // For FIFO within same priority
    request: PromptRequest,
}

impl PartialEq for PrioritizedRequest {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl Eq for PrioritizedRequest {}

impl PartialOrd for PrioritizedRequest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PrioritizedRequest {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first, then lower sequence (FIFO)
        match self.priority.cmp(&other.priority) {
            Ordering::Equal => other.sequence.cmp(&self.sequence), // Reverse for FIFO
            other => other,
        }
    }
}

/// Priority queue for requests
#[derive(Clone)]
pub struct PriorityQueue {
    heap: Arc<Mutex<BinaryHeap<PrioritizedRequest>>>,
    sequence: Arc<Mutex<u64>>,
    max_size: usize,
}

impl PriorityQueue {
    /// Create new priority queue with max size
    pub fn new() -> Self {
        Self::with_capacity(10000)
    }

    /// Create priority queue with specific capacity
    pub fn with_capacity(max_size: usize) -> Self {
        Self {
            heap: Arc::new(Mutex::new(BinaryHeap::new())),
            sequence: Arc::new(Mutex::new(0)),
            max_size,
        }
    }

    /// Push request with priority
    pub async fn push(&self, priority: Priority, request: PromptRequest) -> Result<(), QueueError> {
        let mut heap = self.heap.lock().await;

        if heap.len() >= self.max_size {
            return Err(QueueError::QueueFull);
        }

        let mut seq = self.sequence.lock().await;
        *seq += 1;

        heap.push(PrioritizedRequest {
            priority,
            sequence: *seq,
            request,
        });

        debug!(
            priority = ?priority,
            sequence = *seq,
            queue_size = heap.len(),
            "request enqueued"
        );

        Ok(())
    }

    /// Pop highest priority request
    pub async fn pop(&self) -> Option<(Priority, PromptRequest)> {
        let mut heap = self.heap.lock().await;
        heap.pop().map(|pr| {
            debug!(
                priority = ?pr.priority,
                sequence = pr.sequence,
                queue_size = heap.len(),
                "request dequeued"
            );
            (pr.priority, pr.request)
        })
    }

    /// Get current queue size
    pub async fn len(&self) -> usize {
        self.heap.lock().await.len()
    }

    /// Check if queue is empty
    pub async fn is_empty(&self) -> bool {
        self.heap.lock().await.is_empty()
    }

    /// Get queue statistics by priority
    pub async fn stats(&self) -> QueueStats {
        let heap = self.heap.lock().await;

        let mut stats = QueueStats {
            total: heap.len(),
            by_priority: std::collections::HashMap::new(),
        };

        for item in heap.iter() {
            *stats.by_priority.entry(item.priority).or_insert(0) += 1;
        }

        stats
    }

    /// Clear all requests
    pub async fn clear(&self) {
        let mut heap = self.heap.lock().await;
        heap.clear();
        debug!("priority queue cleared");
    }
}

impl Default for PriorityQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Queue error types
#[derive(Debug)]
pub enum QueueError {
    /// The queue has reached its maximum capacity.
    QueueFull,
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::QueueFull => write!(f, "queue full"),
        }
    }
}

impl std::error::Error for QueueError {}

/// Queue statistics
#[derive(Debug)]
pub struct QueueStats {
    /// Total number of requests currently in the queue.
    pub total: usize,
    /// Breakdown of request counts keyed by priority level.
    pub by_priority: std::collections::HashMap<Priority, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;
    use std::collections::HashMap;

    fn create_request(id: &str) -> PromptRequest {
        PromptRequest {
            session: SessionId::new(id),
            request_id: format!("test-{id}"),
            input: id.to_string(),
            meta: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_priority_ordering() {
        let queue = PriorityQueue::new();

        // Push in random order
        queue
            .push(Priority::Low, create_request("low"))
            .await
            .unwrap();
        queue
            .push(Priority::High, create_request("high"))
            .await
            .unwrap();
        queue
            .push(Priority::Normal, create_request("normal"))
            .await
            .unwrap();
        queue
            .push(Priority::Critical, create_request("critical"))
            .await
            .unwrap();

        // Should pop in priority order
        assert_eq!(queue.pop().await.unwrap().0, Priority::Critical);
        assert_eq!(queue.pop().await.unwrap().0, Priority::High);
        assert_eq!(queue.pop().await.unwrap().0, Priority::Normal);
        assert_eq!(queue.pop().await.unwrap().0, Priority::Low);
    }

    #[tokio::test]
    async fn test_fifo_within_priority() {
        let queue = PriorityQueue::new();

        // Push multiple normal priority requests
        queue
            .push(Priority::Normal, create_request("first"))
            .await
            .unwrap();
        queue
            .push(Priority::Normal, create_request("second"))
            .await
            .unwrap();
        queue
            .push(Priority::Normal, create_request("third"))
            .await
            .unwrap();

        // Should pop in FIFO order
        assert_eq!(queue.pop().await.unwrap().1.input, "first");
        assert_eq!(queue.pop().await.unwrap().1.input, "second");
        assert_eq!(queue.pop().await.unwrap().1.input, "third");
    }

    #[tokio::test]
    async fn test_queue_full() {
        let queue = PriorityQueue::with_capacity(2);

        queue
            .push(Priority::Normal, create_request("1"))
            .await
            .unwrap();
        queue
            .push(Priority::Normal, create_request("2"))
            .await
            .unwrap();

        // 3rd should fail
        assert!(queue
            .push(Priority::Normal, create_request("3"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_queue_stats() {
        let queue = PriorityQueue::new();

        queue
            .push(Priority::High, create_request("h1"))
            .await
            .unwrap();
        queue
            .push(Priority::High, create_request("h2"))
            .await
            .unwrap();
        queue
            .push(Priority::Normal, create_request("n1"))
            .await
            .unwrap();
        queue
            .push(Priority::Low, create_request("l1"))
            .await
            .unwrap();

        let stats = queue.stats().await;
        assert_eq!(stats.total, 4);
        assert_eq!(*stats.by_priority.get(&Priority::High).unwrap(), 2);
        assert_eq!(*stats.by_priority.get(&Priority::Normal).unwrap(), 1);
        assert_eq!(*stats.by_priority.get(&Priority::Low).unwrap(), 1);
    }

    // ── Hardening tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_empty_queue_pop_returns_none() {
        let queue = PriorityQueue::new();
        assert!(queue.pop().await.is_none());
    }

    #[tokio::test]
    async fn test_empty_queue_is_empty_returns_true() {
        let queue = PriorityQueue::new();
        assert!(queue.is_empty().await);
        assert_eq!(queue.len().await, 0);
    }

    #[test]
    fn test_priority_ordering_critical_gt_high_gt_normal_gt_low() {
        assert!(Priority::Critical > Priority::High);
        assert!(Priority::High > Priority::Normal);
        assert!(Priority::Normal > Priority::Low);
    }

    #[test]
    fn test_priority_ordering_is_transitive() {
        let levels = [
            Priority::Low,
            Priority::Normal,
            Priority::High,
            Priority::Critical,
        ];
        for i in 0..levels.len() {
            for j in (i + 1)..levels.len() {
                assert!(
                    levels[i] < levels[j],
                    "{:?} must be < {:?}",
                    levels[i],
                    levels[j]
                );
                for k in (j + 1)..levels.len() {
                    // If a < b and b < c then a < c
                    assert!(
                        levels[i] < levels[k],
                        "transitivity: {:?} < {:?} < {:?}",
                        levels[i],
                        levels[j],
                        levels[k]
                    );
                }
            }
        }
    }

    #[test]
    fn test_priority_equality() {
        assert_eq!(Priority::Normal, Priority::Normal);
        assert_ne!(Priority::Low, Priority::High);
    }

    #[test]
    fn test_priority_default_is_normal() {
        assert_eq!(Priority::default(), Priority::Normal);
    }

    #[test]
    fn test_from_name_valid_values() {
        assert_eq!(Priority::from_name("low"), Some(Priority::Low));
        assert_eq!(Priority::from_name("normal"), Some(Priority::Normal));
        assert_eq!(Priority::from_name("high"), Some(Priority::High));
        assert_eq!(Priority::from_name("critical"), Some(Priority::Critical));
    }

    #[test]
    fn test_from_name_case_insensitive() {
        assert_eq!(Priority::from_name("HIGH"), Some(Priority::High));
        assert_eq!(Priority::from_name("Critical"), Some(Priority::Critical));
        assert_eq!(Priority::from_name("LOW"), Some(Priority::Low));
    }

    #[test]
    fn test_from_name_invalid_returns_none() {
        assert_eq!(Priority::from_name(""), None);
        assert_eq!(Priority::from_name("urgent"), None);
        assert_eq!(Priority::from_name("medium"), None);
    }

    #[tokio::test]
    async fn test_clear_empties_queue() {
        let queue = PriorityQueue::new();
        queue
            .push(Priority::High, create_request("a"))
            .await
            .unwrap();
        queue
            .push(Priority::Low, create_request("b"))
            .await
            .unwrap();

        assert_eq!(queue.len().await, 2);
        queue.clear().await;
        assert!(queue.is_empty().await);
        assert!(queue.pop().await.is_none());
    }

    #[tokio::test]
    async fn test_queue_default_trait() {
        let queue = PriorityQueue::default();
        assert!(queue.is_empty().await);
    }

    #[tokio::test]
    async fn test_with_capacity_zero_rejects_all() {
        let queue = PriorityQueue::with_capacity(0);
        let result = queue.push(Priority::Normal, create_request("x")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_queue_error_display() {
        let err = QueueError::QueueFull;
        assert_eq!(format!("{err}"), "queue full");
    }

    #[tokio::test]
    async fn test_fifo_preserved_across_all_priorities() {
        let queue = PriorityQueue::new();

        // Push 2 of each priority in known order
        queue
            .push(Priority::Normal, create_request("n1"))
            .await
            .unwrap();
        queue
            .push(Priority::Normal, create_request("n2"))
            .await
            .unwrap();
        queue
            .push(Priority::High, create_request("h1"))
            .await
            .unwrap();
        queue
            .push(Priority::High, create_request("h2"))
            .await
            .unwrap();
        queue
            .push(Priority::Low, create_request("l1"))
            .await
            .unwrap();
        queue
            .push(Priority::Low, create_request("l2"))
            .await
            .unwrap();

        // Pop all — verify FIFO within each priority band
        assert_eq!(queue.pop().await.unwrap().1.input, "h1");
        assert_eq!(queue.pop().await.unwrap().1.input, "h2");
        assert_eq!(queue.pop().await.unwrap().1.input, "n1");
        assert_eq!(queue.pop().await.unwrap().1.input, "n2");
        assert_eq!(queue.pop().await.unwrap().1.input, "l1");
        assert_eq!(queue.pop().await.unwrap().1.input, "l2");
    }

    #[tokio::test]
    async fn test_stats_on_empty_queue() {
        let queue = PriorityQueue::new();
        let stats = queue.stats().await;
        assert_eq!(stats.total, 0);
        assert!(stats.by_priority.is_empty());
    }

    #[tokio::test]
    async fn test_push_then_pop_single_item() {
        let queue = PriorityQueue::new();
        queue
            .push(Priority::Critical, create_request("only"))
            .await
            .unwrap();

        let (prio, req) = queue.pop().await.unwrap();
        assert_eq!(prio, Priority::Critical);
        assert_eq!(req.input, "only");
        assert!(queue.is_empty().await);
    }

    #[tokio::test]
    async fn test_queue_len_tracks_correctly() {
        let queue = PriorityQueue::new();
        assert_eq!(queue.len().await, 0);

        queue
            .push(Priority::Normal, create_request("a"))
            .await
            .unwrap();
        assert_eq!(queue.len().await, 1);

        queue
            .push(Priority::Normal, create_request("b"))
            .await
            .unwrap();
        assert_eq!(queue.len().await, 2);

        queue.pop().await;
        assert_eq!(queue.len().await, 1);

        queue.pop().await;
        assert_eq!(queue.len().await, 0);
    }
}
