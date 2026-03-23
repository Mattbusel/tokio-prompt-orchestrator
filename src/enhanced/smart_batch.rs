//! # Smart Adaptive Batcher
//!
//! Groups incoming [`PromptRequest`]s into micro-batches within a configurable
//! time window, dispatching them together to batch-capable inference workers.
//!
//! ## Why batching matters
//!
//! GPU-accelerated inference is most efficient when multiple requests are
//! processed together: a batch of 8 prompts may take only 20% longer than a
//! single prompt while delivering 5–6× higher throughput.  Without batching,
//! each request occupies the full per-request GPU round-trip overhead.
//!
//! ## How it works
//!
//! 1. Requests arrive via [`SmartBatcher::submit`].
//! 2. The batcher collects them in an internal staging buffer.
//! 3. A batch is flushed when **either**:
//!    - The batch reaches [`BatchConfig::max_batch_size`], **or**
//!    - [`BatchConfig::max_wait_ms`] milliseconds elapse since the first
//!      item in the batch arrived.
//! 4. The caller drives flushing by calling [`SmartBatcher::poll_ready`]
//!    in a loop (typically from a dedicated Tokio task).
//!
//! ## Similarity grouping (optional)
//!
//! When [`BatchConfig::group_by_prefix_len`] is `> 0` the batcher places
//! requests with the same prompt prefix (first N bytes) into the same batch.
//! This improves KV-cache utilisation on prefix-caching inference servers
//! (e.g. vLLM, SGLang).
//!
//! ## Example
//!
//! ```no_run
//! use std::collections::HashMap;
//! use tokio_prompt_orchestrator::{SessionId, PromptRequest};
//! use tokio_prompt_orchestrator::enhanced::smart_batch::{SmartBatcher, BatchConfig};
//!
//! #[tokio::main]
//! async fn main() {
//!     let batcher = SmartBatcher::new(BatchConfig {
//!         max_batch_size: 8,
//!         max_wait_ms: 50,
//!         group_by_prefix_len: 64,
//!     });
//!
//!     // Producer: submit requests.
//!     let make = |id: &str| PromptRequest {
//!         session: SessionId::new(id),
//!         request_id: id.to_string(),
//!         input: format!("Summarise document {id}"),
//!         meta: HashMap::new(),
//!         deadline: None,
//!     };
//!     batcher.submit(make("a")).await;
//!     batcher.submit(make("b")).await;
//!
//!     // Consumer: poll until a batch is ready.
//!     if let Some(batch) = batcher.poll_ready().await {
//!         println!("Dispatching batch of {} requests", batch.len());
//!         // Pass `batch` to your batch-capable ModelWorker implementation.
//!     }
//! }
//! ```

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use tokio::sync::Mutex;
use tracing::{debug, trace};

use crate::PromptRequest;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for [`SmartBatcher`].
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Maximum number of requests in a single batch.
    ///
    /// Defaults to `8`.  Must be ≥ 1.
    pub max_batch_size: usize,

    /// Maximum time (milliseconds) to wait for a batch to fill before
    /// flushing with whatever is available.
    ///
    /// Defaults to `50` ms.
    pub max_wait_ms: u64,

    /// Length (in bytes) of the prompt prefix used for similarity grouping.
    ///
    /// When `> 0`, requests are bucketed by their first `group_by_prefix_len`
    /// bytes.  This improves prefix-cache hit rate on compatible servers.
    /// Set to `0` to disable grouping (FIFO ordering).
    ///
    /// Defaults to `0`.
    pub group_by_prefix_len: usize,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 8,
            max_wait_ms: 50,
            group_by_prefix_len: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal bucket
// ---------------------------------------------------------------------------

struct Bucket {
    requests: Vec<PromptRequest>,
    opened_at: Instant,
}

impl Bucket {
    fn new(first: PromptRequest) -> Self {
        Self {
            requests: vec![first],
            opened_at: Instant::now(),
        }
    }

    fn age(&self) -> Duration {
        self.opened_at.elapsed()
    }

    fn is_full(&self, max: usize) -> bool {
        self.requests.len() >= max
    }

    fn is_expired(&self, max_wait: Duration) -> bool {
        self.age() > max_wait
    }
}

// ---------------------------------------------------------------------------
// SmartBatcher
// ---------------------------------------------------------------------------

/// An adaptive batcher that groups [`PromptRequest`]s into micro-batches.
///
/// See the [module documentation][self] for usage details.
#[derive(Clone)]
pub struct SmartBatcher {
    config: BatchConfig,
    /// Prefix key → staging bucket.
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    /// Requests that don't use prefix grouping.
    fifo: Arc<Mutex<Option<Bucket>>>,
    // Metrics
    batches_emitted: Arc<AtomicU64>,
    requests_batched: Arc<AtomicU64>,
    solo_flushes: Arc<AtomicU64>,
}

impl SmartBatcher {
    /// Create a new batcher with the given [`BatchConfig`].
    pub fn new(config: BatchConfig) -> Self {
        Self {
            config,
            buckets: Arc::new(Mutex::new(HashMap::new())),
            fifo: Arc::new(Mutex::new(None)),
            batches_emitted: Arc::new(AtomicU64::new(0)),
            requests_batched: Arc::new(AtomicU64::new(0)),
            solo_flushes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Submit a single [`PromptRequest`] to the staging buffer.
    ///
    /// Returns `true` if submitting this request immediately triggered a
    /// full-batch flush (i.e. [`poll_ready`][SmartBatcher::poll_ready] will
    /// return `Some` on the next call).
    pub async fn submit(&self, request: PromptRequest) -> bool {
        if self.config.group_by_prefix_len > 0 {
            self.submit_grouped(request).await
        } else {
            self.submit_fifo(request).await
        }
    }

    /// Poll for a ready batch.
    ///
    /// Returns `Some(batch)` when either:
    /// - Any bucket has reached [`BatchConfig::max_batch_size`], or
    /// - Any bucket's age exceeds [`BatchConfig::max_wait_ms`].
    ///
    /// Returns `None` when no batch is ready yet.  The caller should call
    /// this in a loop with a short sleep (e.g. 1–5 ms) between iterations.
    pub async fn poll_ready(&self) -> Option<Vec<PromptRequest>> {
        let max_wait = Duration::from_millis(self.config.max_wait_ms);

        if self.config.group_by_prefix_len > 0 {
            let mut map = self.buckets.lock().await;
            // Find the first bucket that is full or expired.
            let ready_key = map
                .iter()
                .find(|(_, b)| b.is_full(self.config.max_batch_size) || b.is_expired(max_wait))
                .map(|(k, _)| k.clone());

            if let Some(key) = ready_key {
                if let Some(bucket) = map.remove(&key) {
                    return self.flush_bucket(bucket);
                }
            }
        } else {
            let mut fifo = self.fifo.lock().await;
            if let Some(bucket) = fifo.as_ref() {
                if bucket.is_full(self.config.max_batch_size) || bucket.is_expired(max_wait) {
                    let bucket = fifo.take().expect("just checked Some");
                    return self.flush_bucket(bucket);
                }
            }
        }

        None
    }

    /// Force-flush all pending requests regardless of batch size or age.
    ///
    /// Returns all requests across all buckets as a single flat batch, or
    /// `None` if nothing is pending.  Useful for graceful shutdown.
    pub async fn flush_all(&self) -> Option<Vec<PromptRequest>> {
        let mut out: Vec<PromptRequest> = Vec::new();

        if self.config.group_by_prefix_len > 0 {
            let mut map = self.buckets.lock().await;
            for (_, bucket) in map.drain() {
                out.extend(bucket.requests);
            }
        } else {
            let mut fifo = self.fifo.lock().await;
            if let Some(bucket) = fifo.take() {
                out.extend(bucket.requests);
            }
        }

        if out.is_empty() {
            None
        } else {
            self.batches_emitted.fetch_add(1, Ordering::Relaxed);
            self.requests_batched
                .fetch_add(out.len() as u64, Ordering::Relaxed);
            debug!(count = out.len(), "force-flushed all pending requests");
            Some(out)
        }
    }

    /// Number of requests currently sitting in staging buckets.
    pub async fn pending_count(&self) -> usize {
        if self.config.group_by_prefix_len > 0 {
            self.buckets
                .lock()
                .await
                .values()
                .map(|b| b.requests.len())
                .sum()
        } else {
            self.fifo
                .lock()
                .await
                .as_ref()
                .map(|b| b.requests.len())
                .unwrap_or(0)
        }
    }

    /// Snapshot of batcher statistics.
    pub fn stats(&self) -> BatcherStats {
        BatcherStats {
            batches_emitted: self.batches_emitted.load(Ordering::Relaxed),
            requests_batched: self.requests_batched.load(Ordering::Relaxed),
            solo_flushes: self.solo_flushes.load(Ordering::Relaxed),
        }
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    async fn submit_grouped(&self, request: PromptRequest) -> bool {
        let prefix = prefix_key(&request.input, self.config.group_by_prefix_len);
        let mut map = self.buckets.lock().await;
        let bucket = map
            .entry(prefix)
            .or_insert_with(|| Bucket::new(request.clone()));
        if bucket.requests.len() > 0
            && !bucket.requests.iter().any(|r| r.request_id == request.request_id)
        {
            bucket.requests.push(request);
        }
        let full = bucket.is_full(self.config.max_batch_size);
        trace!(full, "submitted to grouped bucket");
        full
    }

    async fn submit_fifo(&self, request: PromptRequest) -> bool {
        let mut fifo = self.fifo.lock().await;
        match fifo.as_mut() {
            None => {
                *fifo = Some(Bucket::new(request));
                false
            }
            Some(bucket) => {
                bucket.requests.push(request);
                let full = bucket.is_full(self.config.max_batch_size);
                trace!(full, "submitted to fifo bucket");
                full
            }
        }
    }

    fn flush_bucket(&self, bucket: Bucket) -> Option<Vec<PromptRequest>> {
        let count = bucket.requests.len();
        if count == 0 {
            return None;
        }
        if count == 1 {
            self.solo_flushes.fetch_add(1, Ordering::Relaxed);
        }
        self.batches_emitted.fetch_add(1, Ordering::Relaxed);
        self.requests_batched
            .fetch_add(count as u64, Ordering::Relaxed);
        debug!(
            count,
            age_ms = bucket.age().as_millis(),
            "flushed batch"
        );
        Some(bucket.requests)
    }
}

/// Compute the prefix key for similarity grouping.
fn prefix_key(input: &str, len: usize) -> String {
    let bytes = input.as_bytes();
    let end = len.min(bytes.len());
    // Use the raw bytes to avoid UTF-8 slicing panics.
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics produced by [`SmartBatcher::stats`].
#[derive(Debug, Clone)]
pub struct BatcherStats {
    /// Total number of batches emitted so far.
    pub batches_emitted: u64,
    /// Total number of individual requests batched.
    pub requests_batched: u64,
    /// Number of batches that contained only a single request (timeout flush).
    pub solo_flushes: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::SessionId;

    fn req(id: &str) -> PromptRequest {
        PromptRequest {
            session: SessionId::new(id),
            request_id: id.to_string(),
            input: format!("shared prefix text -- request {id}"),
            meta: HashMap::new(),
            deadline: None,
        }
    }

    fn req_with_input(id: &str, input: &str) -> PromptRequest {
        PromptRequest {
            session: SessionId::new(id),
            request_id: id.to_string(),
            input: input.to_string(),
            meta: HashMap::new(),
            deadline: None,
        }
    }

    #[tokio::test]
    async fn batch_fills_on_max_size() {
        let batcher = SmartBatcher::new(BatchConfig {
            max_batch_size: 3,
            max_wait_ms: 10_000,
            group_by_prefix_len: 0,
        });

        batcher.submit(req("a")).await;
        batcher.submit(req("b")).await;
        batcher.submit(req("c")).await;

        let batch = batcher.poll_ready().await;
        assert!(batch.is_some());
        assert_eq!(batch.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn batch_flushes_on_timeout() {
        let batcher = SmartBatcher::new(BatchConfig {
            max_batch_size: 100,
            max_wait_ms: 10,
            group_by_prefix_len: 0,
        });

        batcher.submit(req("x")).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let batch = batcher.poll_ready().await;
        assert!(batch.is_some());
        assert_eq!(batch.unwrap().len(), 1);

        let stats = batcher.stats();
        assert_eq!(stats.solo_flushes, 1);
    }

    #[tokio::test]
    async fn no_batch_when_empty_and_not_expired() {
        let batcher = SmartBatcher::new(BatchConfig::default());
        assert!(batcher.poll_ready().await.is_none());
    }

    #[tokio::test]
    async fn flush_all_drains_everything() {
        let batcher = SmartBatcher::new(BatchConfig {
            max_batch_size: 100,
            max_wait_ms: 10_000,
            group_by_prefix_len: 0,
        });

        batcher.submit(req("p")).await;
        batcher.submit(req("q")).await;

        let batch = batcher.flush_all().await;
        assert!(batch.is_some());
        assert_eq!(batch.unwrap().len(), 2);
        assert_eq!(batcher.pending_count().await, 0);
    }

    #[tokio::test]
    async fn prefix_grouping_routes_by_prefix() {
        let batcher = SmartBatcher::new(BatchConfig {
            max_batch_size: 10,
            max_wait_ms: 10_000,
            group_by_prefix_len: 20,
        });

        // Two requests with the same prefix.
        batcher
            .submit(req_with_input("1", "summarise this document carefully"))
            .await;
        batcher
            .submit(req_with_input("2", "summarise this document quickly"))
            .await;

        // One request with a different prefix.
        batcher
            .submit(req_with_input("3", "translate the following text"))
            .await;

        // Should have 2 buckets: 1 with 2 items, 1 with 1 item.
        assert_eq!(batcher.pending_count().await, 3);
    }

    #[tokio::test]
    async fn stats_track_emitted_batches() {
        let batcher = SmartBatcher::new(BatchConfig {
            max_batch_size: 2,
            max_wait_ms: 10_000,
            group_by_prefix_len: 0,
        });

        batcher.submit(req("m")).await;
        batcher.submit(req("n")).await;
        batcher.poll_ready().await;

        let s = batcher.stats();
        assert_eq!(s.batches_emitted, 1);
        assert_eq!(s.requests_batched, 2);
    }
}
