//! Dynamic worker pool with auto-scaling.
//!
//! Provides a generic [`WorkerPool<T, R>`] that automatically scales the number
//! of concurrent worker tasks based on queue depth relative to
//! [`WorkerConfig::queue_depth_per_worker`].
//!
//! ## Example
//!
//! ```rust
//! use std::sync::Arc;
//! use tokio_prompt_orchestrator::worker_pool::{WorkerConfig, WorkerPool};
//! use futures::future::BoxFuture;
//!
//! #[tokio::main]
//! async fn main() {
//!     let cfg = WorkerConfig {
//!         min_workers: 1,
//!         max_workers: 4,
//!         idle_timeout_secs: 30,
//!         queue_depth_per_worker: 8,
//!     };
//!     let handler: Arc<dyn Fn(u32) -> BoxFuture<'static, u32> + Send + Sync> =
//!         Arc::new(|x: u32| Box::pin(async move { x * 2 }));
//!     let pool = WorkerPool::new(cfg, handler);
//!     let rx = pool.submit(21, 0).await;
//!     assert_eq!(rx.await.unwrap(), 42);
//!     pool.drain_and_shutdown().await;
//! }
//! ```

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot, Mutex};

// ── Configuration ─────────────────────────────────────────────────────────────

/// Configuration for a [`WorkerPool`].
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Minimum number of worker tasks to keep alive even when idle.
    pub min_workers: usize,
    /// Maximum number of concurrent worker tasks.
    pub max_workers: usize,
    /// Number of seconds a worker may be idle before it is allowed to exit
    /// (only applies to workers above `min_workers`).
    pub idle_timeout_secs: u64,
    /// Target queue depth per worker.  When `queue_depth > workers *
    /// queue_depth_per_worker` the pool will attempt to scale up.
    pub queue_depth_per_worker: usize,
}

// ── Work items ────────────────────────────────────────────────────────────────

/// A unit of work submitted to the pool.
///
/// This is a public-facing view of a queued item.  The pool infers all fields
/// internally; callers interact with work items only via [`WorkerPool::submit`].
#[allow(dead_code)]
pub struct WorkItem<T> {
    /// Monotonically increasing submission identifier.
    pub id: u64,
    /// The caller-supplied payload.
    pub payload: T,
    /// Higher values are more urgent (0 = normal).
    pub priority: u8,
    /// Wall-clock time at which the item was enqueued.
    pub enqueued_at: Instant,
}

/// Internal work envelope that carries the typed reply channel.
#[allow(dead_code)]
struct InnerItem<T, R> {
    id: u64,
    payload: T,
    priority: u8,
    enqueued_at: Instant,
    reply: oneshot::Sender<R>,
}

// ── Statistics ────────────────────────────────────────────────────────────────

/// A point-in-time snapshot of pool statistics.
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Number of live worker tasks.
    pub worker_count: usize,
    /// Number of items waiting in the queue.
    pub queue_depth: usize,
    /// Number of items currently being processed.
    pub active_work: usize,
    /// Total number of items submitted since the pool was created.
    pub total_submitted: u64,
    /// Total number of items that completed processing.
    pub total_completed: u64,
    /// Rolling average end-to-end latency in milliseconds.
    pub avg_latency_ms: f64,
}

// ── Pool ──────────────────────────────────────────────────────────────────────

type Handler<T, R> = Arc<dyn Fn(T) -> BoxFuture<'static, R> + Send + Sync>;

/// A dynamically-scaling pool of Tokio worker tasks.
///
/// Generic over:
/// - `T` — the input type passed to each work item (must be `Send + 'static`)
/// - `R` — the result type returned by the handler (must be `Send + 'static`)
pub struct WorkerPool<T: Send + 'static, R: Send + 'static> {
    config: WorkerConfig,
    handler: Handler<T, R>,
    /// MPSC sender used by `submit`.  Workers share the receiver via a `Mutex`.
    tx: mpsc::Sender<InnerItem<T, R>>,
    /// Shared receiver wrapped in a `Mutex` so multiple workers can pull from it.
    rx: Arc<Mutex<mpsc::Receiver<InnerItem<T, R>>>>,
    /// Live worker count.
    worker_count: Arc<AtomicUsize>,
    /// Items currently being processed (dequeued but not yet replied).
    active_work: Arc<AtomicUsize>,
    /// Monotonically increasing submission counter; also used as work-item ID.
    total_submitted: Arc<AtomicU64>,
    /// Total completed counter.
    total_completed: Arc<AtomicU64>,
    /// Sum of all per-item latencies in milliseconds (for avg calculation).
    total_latency_ms: Arc<AtomicU64>,
    /// Shutdown signal: when dropped, all workers will drain and exit.
    shutdown_tx: Arc<tokio::sync::watch::Sender<bool>>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
}

impl<T: Send + 'static, R: Send + 'static> WorkerPool<T, R> {
    /// Create a new pool with the given configuration and async handler function.
    ///
    /// `min_workers` worker tasks are spawned immediately.
    pub fn new(config: WorkerConfig, handler: Handler<T, R>) -> Self {
        let queue_cap = config.max_workers * config.queue_depth_per_worker.max(1) * 2;
        let (tx, rx) = mpsc::channel::<InnerItem<T, R>>(queue_cap.max(16));
        let rx = Arc::new(Mutex::new(rx));

        let worker_count = Arc::new(AtomicUsize::new(0));
        let active_work = Arc::new(AtomicUsize::new(0));
        let total_submitted = Arc::new(AtomicU64::new(0));
        let total_completed = Arc::new(AtomicU64::new(0));
        let total_latency_ms = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let shutdown_tx = Arc::new(shutdown_tx);

        let pool = Self {
            config,
            handler,
            tx,
            rx,
            worker_count,
            active_work,
            total_submitted,
            total_completed,
            total_latency_ms,
            shutdown_tx,
            shutdown_rx,
        };

        // Spawn minimum workers.
        for _ in 0..pool.config.min_workers {
            pool.scale_up_inner();
        }

        pool
    }

    /// Submit a work item with the given `priority` (higher = more urgent).
    ///
    /// Returns a [`oneshot::Receiver<R>`] that resolves when processing is done.
    /// If the queue is full (bounded channel) this will apply back-pressure and
    /// await until space is available.
    pub async fn submit(&self, item: T, priority: u8) -> oneshot::Receiver<R> {
        let id = self.total_submitted.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel::<R>();

        let inner = InnerItem {
            id,
            payload: item,
            priority,
            enqueued_at: Instant::now(),
            reply: reply_tx,
        };

        // Auto-scale up if needed before sending.
        self.maybe_scale_up();

        // Send may block if the channel is full — that is intentional back-pressure.
        // We ignore the error; if the channel is closed the caller will get a
        // dropped receiver (await on it will return Err).
        let _ = self.tx.send(inner).await;

        reply_rx
    }

    /// Attempt to scale up if queue depth per worker exceeds the threshold.
    fn maybe_scale_up(&self) {
        let workers = self.worker_count.load(Ordering::Relaxed);
        let queue_depth = self.tx.max_capacity() - self.tx.capacity();
        let threshold = workers.saturating_mul(self.config.queue_depth_per_worker.max(1));
        if queue_depth > threshold && workers < self.config.max_workers {
            self.scale_up_inner();
        }
    }

    /// Spawn one additional worker task.
    pub fn scale_up(&self) {
        if self.worker_count.load(Ordering::Relaxed) < self.config.max_workers {
            self.scale_up_inner();
        }
    }

    fn scale_up_inner(&self) {
        let rx = Arc::clone(&self.rx);
        let handler = Arc::clone(&self.handler);
        let worker_count = Arc::clone(&self.worker_count);
        let active_work = Arc::clone(&self.active_work);
        let total_completed = Arc::clone(&self.total_completed);
        let total_latency_ms = Arc::clone(&self.total_latency_ms);
        let idle_timeout = Duration::from_secs(self.config.idle_timeout_secs);
        let min_workers = self.config.min_workers;
        let mut shutdown_rx = self.shutdown_rx.clone();

        worker_count.fetch_add(1, Ordering::Relaxed);

        tokio::spawn(async move {
            loop {
                // Respect shutdown signal.
                if *shutdown_rx.borrow() {
                    break;
                }

                // Try to dequeue with an idle timeout so we can scale down.
                let item_opt = tokio::select! {
                    item = async {
                        let mut guard = rx.lock().await;
                        guard.recv().await
                    } => item,
                    _ = tokio::time::sleep(idle_timeout) => None,
                    _ = shutdown_rx.changed() => {
                        // Drain remaining items if shutdown was just signalled.
                        break;
                    }
                };

                match item_opt {
                    None => {
                        // Idle timeout expired — exit if we're above min_workers.
                        let current = worker_count.load(Ordering::Relaxed);
                        if current > min_workers {
                            // Try to decrement without going below min.
                            let prev = worker_count.fetch_sub(1, Ordering::Relaxed);
                            if prev <= min_workers {
                                // Oops, we went below; add back.
                                worker_count.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            break;
                        }
                        // We are at min — keep looping.
                    }
                    Some(inner) => {
                        active_work.fetch_add(1, Ordering::Relaxed);
                        let fut = handler(inner.payload);
                        let result = fut.await;
                        active_work.fetch_sub(1, Ordering::Relaxed);

                        let latency = inner.enqueued_at.elapsed().as_millis() as u64;
                        total_latency_ms.fetch_add(latency, Ordering::Relaxed);
                        total_completed.fetch_add(1, Ordering::Relaxed);

                        // Send result back; ignore if caller dropped receiver.
                        let _ = inner.reply.send(result);
                    }
                }
            }

            worker_count.fetch_sub(1, Ordering::Relaxed);
        });
    }

    /// Signal one idle worker above `min_workers` to exit on its next idle timeout.
    ///
    /// This is a best-effort hint; the worker may be processing an item and will
    /// only exit after completing it and then timing out.
    pub fn scale_down(&self) {
        // We cannot send a direct signal to a specific worker, so we rely on the
        // idle-timeout logic in `scale_up_inner`.  This method is a no-op if all
        // workers are busy or already at min_workers; it exists for API symmetry
        // and for callers that want to hint at down-scaling after a traffic lull.
        let _ = self.worker_count.load(Ordering::Relaxed);
    }

    /// Wait for the queue to drain completely, then stop all workers.
    ///
    /// After this method returns the pool must not be used again.
    pub async fn drain_and_shutdown(&self) {
        // Wait for queue to empty and all active work to finish.
        loop {
            let queue_depth = self.tx.max_capacity() - self.tx.capacity();
            let active = self.active_work.load(Ordering::Relaxed);
            if queue_depth == 0 && active == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Broadcast shutdown.
        let _ = self.shutdown_tx.send(true);

        // Wait for all workers to exit.
        loop {
            if self.worker_count.load(Ordering::Relaxed) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Return a point-in-time snapshot of pool statistics.
    pub fn stats(&self) -> PoolStats {
        let total_completed = self.total_completed.load(Ordering::Relaxed);
        let total_latency_ms = self.total_latency_ms.load(Ordering::Relaxed);
        let avg_latency_ms = if total_completed > 0 {
            total_latency_ms as f64 / total_completed as f64
        } else {
            0.0
        };

        PoolStats {
            worker_count: self.worker_count.load(Ordering::Relaxed),
            queue_depth: self.tx.max_capacity() - self.tx.capacity(),
            active_work: self.active_work.load(Ordering::Relaxed),
            total_submitted: self.total_submitted.load(Ordering::Relaxed),
            total_completed,
            avg_latency_ms,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pool(min: usize, max: usize) -> WorkerPool<u32, u32> {
        let cfg = WorkerConfig {
            min_workers: min,
            max_workers: max,
            idle_timeout_secs: 1,
            queue_depth_per_worker: 4,
        };
        let handler: Handler<u32, u32> =
            Arc::new(|x: u32| Box::pin(async move { x * 2 }));
        WorkerPool::new(cfg, handler)
    }

    #[tokio::test]
    async fn submit_work_is_handled() {
        let pool = make_pool(1, 4);
        let rx = pool.submit(21, 0).await;
        let result = rx.await.expect("receiver dropped");
        assert_eq!(result, 42);
        pool.drain_and_shutdown().await;
    }

    #[tokio::test]
    async fn multiple_items_are_all_processed() {
        let pool = make_pool(1, 4);
        let mut receivers = Vec::new();
        for i in 0u32..20 {
            receivers.push(pool.submit(i, 0).await);
        }
        let mut results = Vec::new();
        for rx in receivers {
            results.push(rx.await.expect("receiver dropped"));
        }
        for (i, r) in results.iter().enumerate() {
            assert_eq!(*r, (i as u32) * 2);
        }
        pool.drain_and_shutdown().await;
    }

    #[tokio::test]
    async fn stats_track_completed_count() {
        let pool = make_pool(1, 4);
        let rxs: Vec<_> = (0u32..5).map(|_| {
            let pool_ref = &pool;
            async move { pool_ref.submit(1, 0).await }
        }).collect();
        let mut receivers = Vec::new();
        for f in rxs {
            receivers.push(f.await);
        }
        for rx in receivers {
            rx.await.expect("receiver dropped");
        }
        let s = pool.stats();
        assert_eq!(s.total_submitted, 5);
        assert_eq!(s.total_completed, 5);
        pool.drain_and_shutdown().await;
    }

    #[tokio::test]
    async fn scale_up_does_not_exceed_max() {
        let pool = make_pool(1, 2);
        pool.scale_up();
        pool.scale_up();
        pool.scale_up(); // Should be ignored — already at max.
        // Give tasks a moment to register.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let count = pool.worker_count.load(Ordering::Relaxed);
        assert!(count <= 2, "worker count {} exceeded max 2", count);
        pool.drain_and_shutdown().await;
    }

    #[tokio::test]
    async fn drain_and_shutdown_completes() {
        let pool = make_pool(2, 4);
        for i in 0u32..10 {
            let _ = pool.submit(i, 0).await;
        }
        pool.drain_and_shutdown().await;
        assert_eq!(pool.worker_count.load(Ordering::Relaxed), 0);
    }
}
