//! Priority Queue
//!
//! Manages requests with different priority levels.
//!
//! ## Usage
//!
//! ```rust
//! use tokio_prompt_orchestrator::enhanced::{PriorityQueue, Priority};
//!
//! let queue = PriorityQueue::new();
//!
//! queue.push(Priority::High, request1).await;
//! queue.push(Priority::Normal, request2).await;
//! queue.push(Priority::Low, request3).await;
//!
//! // Dequeue by priority
//! let request = queue.pop().await.unwrap(); // Gets highest priority first
//! ```

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::debug;

use crate::PromptRequest;

/// Request priority levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
    Critical = 3,
}

impl Default for Priority {
    fn default() -> Self {
        Priority::Normal
    }
}

impl Priority {
    pub fn from_str(s: &str) -> Option<Self> {
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
    pub total: usize,
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
            input: id.to_string(),
            meta: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_priority_ordering() {
        let queue = PriorityQueue::new();

        // Push in random order
        queue.push(Priority::Low, create_request("low")).await.unwrap();
        queue.push(Priority::High, create_request("high")).await.unwrap();
        queue.push(Priority::Normal, create_request("normal")).await.unwrap();
        queue.push(Priority::Critical, create_request("critical")).await.unwrap();

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
        queue.push(Priority::Normal, create_request("first")).await.unwrap();
        queue.push(Priority::Normal, create_request("second")).await.unwrap();
        queue.push(Priority::Normal, create_request("third")).await.unwrap();

        // Should pop in FIFO order
        assert_eq!(queue.pop().await.unwrap().1.input, "first");
        assert_eq!(queue.pop().await.unwrap().1.input, "second");
        assert_eq!(queue.pop().await.unwrap().1.input, "third");
    }

    #[tokio::test]
    async fn test_queue_full() {
        let queue = PriorityQueue::with_capacity(2);

        queue.push(Priority::Normal, create_request("1")).await.unwrap();
        queue.push(Priority::Normal, create_request("2")).await.unwrap();
        
        // 3rd should fail
        assert!(queue.push(Priority::Normal, create_request("3")).await.is_err());
    }

    #[tokio::test]
    async fn test_queue_stats() {
        let queue = PriorityQueue::new();

        queue.push(Priority::High, create_request("h1")).await.unwrap();
        queue.push(Priority::High, create_request("h2")).await.unwrap();
        queue.push(Priority::Normal, create_request("n1")).await.unwrap();
        queue.push(Priority::Low, create_request("l1")).await.unwrap();

        let stats = queue.stats().await;
        assert_eq!(stats.total, 4);
        assert_eq!(*stats.by_priority.get(&Priority::High).unwrap(), 2);
        assert_eq!(*stats.by_priority.get(&Priority::Normal).unwrap(), 1);
        assert_eq!(*stats.by_priority.get(&Priority::Low).unwrap(), 1);
    }
}
