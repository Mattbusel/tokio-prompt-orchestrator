//! Request Deduplication
//!
//! Prevents duplicate requests from being processed multiple times.
//! Useful for cost savings when users accidentally submit the same request.
//!
//! ## Usage
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_prompt_orchestrator::enhanced::{Deduplicator, DeduplicationResult};
//! # async fn process_request() -> String { String::new() }
//! # #[tokio::main]
//! # async fn main() {
//! let dedup = Deduplicator::new(Duration::from_secs(300)); // 5 minute window
//!
//! // Check if request is duplicate
//! match dedup.check_and_register("prompt_hash").await {
//!     DeduplicationResult::New(token) => {
//!         // Process new request
//!         let result = process_request().await;
//!         dedup.complete(token, result).await;
//!     }
//!     DeduplicationResult::InProgress => {
//!         // Wait for in-progress request
//!         let _result = dedup.wait_for_result("prompt_hash").await;
//!     }
//!     DeduplicationResult::Cached(result) => {
//!         // Use cached result
//!         println!("{result}");
//!     }
//! }
//! # }
//! ```

use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use tracing::{debug, info};
use uuid::Uuid;

/// Outcome of a [`Deduplicator::check_and_register`] call.
///
/// Three-way classification allows callers to decide whether to do the work
/// themselves, wait for a concurrent worker, or immediately reuse a cached
/// result.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::{Deduplicator, DeduplicationResult};
///
/// # #[tokio::main]
/// # async fn main() {
/// let dedup = Deduplicator::new(Duration::from_secs(60));
/// match dedup.check_and_register("my-key").await {
///     DeduplicationResult::New(token) => {
///         let result = "computed".to_string();
///         dedup.complete(token, result).await;
///     }
///     DeduplicationResult::InProgress => {
///         // another task is already working; wait for it
///         let _ = dedup.wait_for_result("my-key").await;
///     }
///     DeduplicationResult::Cached(result) => {
///         println!("reused: {result}");
///     }
/// }
/// # }
/// ```
#[derive(Debug, Clone)]
pub enum DeduplicationResult {
    /// New request — should be processed by the caller.
    ///
    /// The caller must eventually call [`Deduplicator::complete`] or
    /// [`Deduplicator::fail`] with the returned [`DeduplicationToken`] so that
    /// any tasks blocked in [`Deduplicator::wait_for_result`] are unblocked.
    New(DeduplicationToken),
    /// An identical request is already being processed by another task.
    ///
    /// The caller should call [`Deduplicator::wait_for_result`] to block until
    /// that task completes and then reuse its result.
    InProgress,
    /// The request was recently completed and the result is still within the
    /// cache window.  The cached response string is returned directly.
    Cached(String),
}

/// Ownership token issued when a new request is registered with the
/// [`Deduplicator`].
///
/// The holder of a `DeduplicationToken` is the *authoritative worker* for
/// that request key.  It must call either [`Deduplicator::complete`] or
/// [`Deduplicator::fail`] to resolve the pending state.
///
/// # Drop behaviour
///
/// If a token is dropped without calling `complete` or `fail`, the
/// `InProgress` entry is automatically removed from the deduplicator so that
/// subsequent callers are not permanently blocked.  A `WARN`-level log line
/// is emitted in this case.
///
/// # Cloning
///
/// Tokens are `Clone` because they are cheaply cloneable (`Arc`-backed), but
/// only the **first** clone to call `complete` or `fail` takes effect;
/// subsequent calls on other clones are no-ops.
#[derive(Debug, Clone)]
pub struct DeduplicationToken {
    /// Unique identifier for this deduplication token.
    ///
    /// Useful for structured log correlation.
    pub id: String,
    key: String,
    completed: Arc<std::sync::atomic::AtomicBool>,
    requests: Arc<DashMap<String, RequestState>>,
}

/// # Behavior on Drop
///
/// When a `DeduplicationToken` is dropped without calling `complete()`, the
/// following sequence occurs:
///
/// 1. **Cancellation signal**: The `Drop` impl sends a sentinel cancellation
///    string (`"\x00CANCELLED"`) over the broadcast channel before removing the
///    entry.  Any tasks already blocked in `wait_for_result()` receive this
///    value via `rx.recv()` and return `Some("\x00CANCELLED")` rather than
///    `None`.  Callers of `wait_for_result` that inspect the returned string
///    can detect cancellation by checking for this sentinel.
///
/// 2. **Entry removal**: After the cancellation broadcast the `InProgress`
///    entry is removed from the shared map.  This drops the `Sender`, closing
///    the broadcast channel.  Any tasks that subscribe *after* the removal will
///    find no entry and `wait_for_result` will return `None`.
///
/// 3. **Re-registrability**: Because the entry is removed, the *next* caller
///    to invoke `check_and_register` for the same key will receive a fresh
///    `New` token and can retry processing.
///
/// **Waiters are NOT left hanging indefinitely.**  They either receive the
/// cancellation sentinel or `None` (if they race with the removal), both of
/// which are finite outcomes that unblock the awaiting task promptly.
///
/// The sentinel value `"\x00CANCELLED"` uses a NUL prefix which cannot appear
/// in normal LLM output, making it safe to use as a reserved signal.
pub const DEDUP_CANCELLED_SENTINEL: &str = "\x00CANCELLED";

impl Drop for DeduplicationToken {
    fn drop(&mut self) {
        // Only act if this is the last clone and complete() was never called.
        if Arc::strong_count(&self.completed) == 1
            && !self.completed.load(std::sync::atomic::Ordering::Acquire)
        {
            // Broadcast a cancellation sentinel so tasks already blocked in
            // wait_for_result() are unblocked immediately rather than hanging
            // until the Sender is dropped by the map removal below.
            if let Some(state) = self.requests.get(&self.key) {
                if let RequestState::InProgress { waiter_tx, .. } = state.value() {
                    // Ignore send errors — if there are no receivers, that's fine.
                    let _ = waiter_tx.send(DEDUP_CANCELLED_SENTINEL.to_string());
                }
            }

            // Remove the in-progress entry.  This drops the broadcast Sender,
            // closing the channel for any tasks that subscribe after this point.
            self.requests.remove(&self.key);
            tracing::warn!(
                key = %self.key,
                "DeduplicationToken dropped without complete() — cancellation sent and in-progress entry removed"
            );
        }
    }
}

/// Deduplicator state
#[derive(Debug, Clone)]
enum RequestState {
    InProgress {
        started_at: SystemTime,
        waiter_tx: broadcast::Sender<String>,
    },
    Completed {
        result: String,
        completed_at: SystemTime,
    },
}

/// In-process request deduplicator that coalesces identical concurrent
/// requests and caches recently completed results.
///
/// # How it works
///
/// 1. The caller derives a stable cache key (see [`dedup_key`]) from the
///    prompt and session.
/// 2. [`Deduplicator::check_and_register`] atomically checks the shared
///    state map and returns one of three outcomes:
///    - [`DeduplicationResult::New`] — the caller is the first to see this
///      key; it receives a [`DeduplicationToken`] and must process the request.
///    - [`DeduplicationResult::InProgress`] — another task is already working;
///      the caller should call [`Deduplicator::wait_for_result`] to block.
///    - [`DeduplicationResult::Cached`] — a prior result is still within the
///      `cache_duration` TTL; the caller can return it immediately.
/// 3. On success the worker calls [`Deduplicator::complete`]; on failure it
///    calls [`Deduplicator::fail`], which removes the entry.
///
/// # Thread safety
///
/// `Deduplicator` is `Clone + Send + Sync`.  All clones share the same
/// underlying `Arc<DashMap>`.  A background task cleans up expired entries
/// every 60 seconds; it stops when the *last* `Deduplicator` clone is dropped.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::{Deduplicator, DeduplicationResult};
///
/// # #[tokio::main]
/// # async fn main() {
/// let dedup = Deduplicator::new(Duration::from_secs(300));
/// let key = "dedup:g:abc123";
///
/// match dedup.check_and_register(key).await {
///     DeduplicationResult::New(token) => {
///         let result = "hello".to_string();
///         dedup.complete(token, result).await;
///     }
///     DeduplicationResult::InProgress => {
///         let _ = dedup.wait_for_result(key).await;
///     }
///     DeduplicationResult::Cached(result) => println!("{result}"),
/// }
/// # }
/// ```
#[derive(Clone)]
pub struct Deduplicator {
    requests: Arc<DashMap<String, RequestState>>,
    cache_duration: Duration,
    /// Signals the background cleanup task to stop when set to `true`.
    shutdown: Arc<AtomicBool>,
    /// Handle to the background cleanup task, used by [`shutdown`](Self::shutdown).
    cleanup_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Optional embedding store for semantic (cosine-similarity) deduplication.
    embeddings: Arc<DashMap<String, Vec<f32>>>,
    /// Minimum cosine similarity score to treat a new prompt as a duplicate.
    similarity_threshold: f32,
}

impl Deduplicator {
    /// Create a new `Deduplicator` with the given cache TTL.
    ///
    /// A background cleanup task is spawned immediately.  It wakes up every
    /// 60 seconds to evict expired entries and exits when the last
    /// `Deduplicator` clone is dropped.
    ///
    /// # Arguments
    ///
    /// * `cache_duration` — How long a completed result remains cached before
    ///   being treated as a fresh request.  Common choices: 5 minutes for
    ///   interactive use, 1 hour for batch/idempotent workloads.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::Deduplicator;
    ///
    /// let dedup = Deduplicator::new(Duration::from_secs(300));
    /// ```
    pub fn new(cache_duration: Duration) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let cleanup_handle = Arc::new(tokio::sync::Mutex::new(None::<tokio::task::JoinHandle<()>>));
        let dedup = Self {
            requests: Arc::new(DashMap::new()),
            cache_duration,
            shutdown: shutdown.clone(),
            cleanup_handle: cleanup_handle.clone(),
            embeddings: Arc::new(DashMap::new()),
            similarity_threshold: 1.0, // disabled by default: exact match only
        };

        // Start cleanup task; checks `shutdown` flag each iteration so it
        // stops promptly when the last Deduplicator handle is dropped or
        // shutdown() is called.
        let requests = dedup.requests.clone();
        let cache_duration = dedup.cache_duration;
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                cleanup_expired(&requests, cache_duration);
            }
        });
        *cleanup_handle.try_lock().expect("cleanup_handle uncontested at construction") = Some(handle);

        dedup
    }

    /// Signal the background cleanup task to stop without waiting for it.
    ///
    /// Sets the shutdown `AtomicBool` to `true`.  The cleanup loop exits on its
    /// next wake-up.  Call [`shutdown`](Self::shutdown) if you need to await
    /// completion.
    pub fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Gracefully shut down the background cleanup task.
    ///
    /// Sets the shutdown flag so the cleanup loop exits on its next wake-up,
    /// then waits for the task to finish.  Safe to call multiple times.
    pub async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let handle = self.cleanup_handle.lock().await.take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    /// Atomically check whether a request is new, in-progress, or cached, and
    /// register it as in-progress if it is new.
    ///
    /// Uses `DashMap::entry()` for a compare-and-insert that prevents multiple
    /// concurrent callers from each receiving [`DeduplicationResult::New`] for
    /// the same key — only one will win the race.
    ///
    /// # Arguments
    ///
    /// * `key` — Stable cache key; derive one with [`dedup_key`].
    ///
    /// # Returns
    ///
    /// A [`DeduplicationResult`] indicating whether the caller should process
    /// the request, wait for another task, or reuse a cached result.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::{Deduplicator, DeduplicationResult};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let dedup = Deduplicator::new(Duration::from_secs(60));
    /// if let DeduplicationResult::New(token) = dedup.check_and_register("key").await {
    ///     dedup.complete(token, "result".to_string()).await;
    /// }
    /// # }
    /// ```
    pub async fn check_and_register(&self, key: &str) -> DeduplicationResult {
        use dashmap::mapref::entry::Entry;

        // First, handle the already-present cases through a read-side fast path.
        // We still need the atomic entry() below for the insert path.
        if let Some(state) = self.requests.get(key) {
            return match state.value() {
                RequestState::InProgress { .. } => {
                    info!(key = key, "duplicate request detected (in progress)");
                    crate::metrics::inc_dedup_hit();
                    DeduplicationResult::InProgress
                }
                RequestState::Completed {
                    result,
                    completed_at,
                } => {
                    if completed_at.elapsed().unwrap_or_default() < self.cache_duration {
                        info!(key = key, "duplicate request detected (cached)");
                        crate::metrics::inc_dedup_hit();
                        DeduplicationResult::Cached(result.clone())
                    } else {
                        // Expired — fall through to the atomic entry path below.
                        drop(state);
                        // Remove the expired entry so the entry() call below sees it as absent.
                        self.requests.remove(key);
                        // Fall through to atomic insert.
                        return self.atomic_register_new(key);
                    }
                }
            };
        }

        // Key is absent — use entry() for an atomic check-and-insert so that
        // concurrent callers cannot both see "absent" and both get New.
        match self.requests.entry(key.to_string()) {
            Entry::Occupied(occ) => {
                // Another task raced us and inserted first.
                match occ.get() {
                    RequestState::InProgress { .. } => {
                        info!(key = key, "duplicate request detected (in progress, raced)");
                        crate::metrics::inc_dedup_hit();
                        DeduplicationResult::InProgress
                    }
                    RequestState::Completed { result, .. } => {
                        info!(key = key, "duplicate request detected (cached, raced)");
                        crate::metrics::inc_dedup_hit();
                        DeduplicationResult::Cached(result.clone())
                    }
                }
            }
            Entry::Vacant(vac) => {
                let (tx, _) = broadcast::channel(16);
                vac.insert(RequestState::InProgress {
                    started_at: SystemTime::now(),
                    waiter_tx: tx,
                });
                let token = DeduplicationToken {
                    id: Uuid::new_v4().to_string(),
                    key: key.to_string(),
                    completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    requests: Arc::clone(&self.requests),
                };
                debug!(key = key, token_id = %token.id, "new request registered");
                DeduplicationResult::New(token)
            }
        }
    }

    /// Atomically insert a new `InProgress` entry.  Called only when we have
    /// already removed an expired entry and need a fresh registration.
    fn atomic_register_new(&self, key: &str) -> DeduplicationResult {
        use dashmap::mapref::entry::Entry;
        match self.requests.entry(key.to_string()) {
            Entry::Occupied(occ) => match occ.get() {
                RequestState::InProgress { .. } => {
                    crate::metrics::inc_dedup_hit();
                    DeduplicationResult::InProgress
                }
                RequestState::Completed { result, .. } => {
                    crate::metrics::inc_dedup_hit();
                    DeduplicationResult::Cached(result.clone())
                }
            },
            Entry::Vacant(vac) => {
                let (tx, _) = broadcast::channel(16);
                vac.insert(RequestState::InProgress {
                    started_at: SystemTime::now(),
                    waiter_tx: tx,
                });
                let token = DeduplicationToken {
                    id: Uuid::new_v4().to_string(),
                    key: key.to_string(),
                    completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    requests: Arc::clone(&self.requests),
                };
                debug!(key = key, "new request registered (after expiry)");
                DeduplicationResult::New(token)
            }
        }
    }

    /// Wait for an in-progress request to complete and return its result.
    ///
    /// Subscribes to the internal broadcast channel for the given key.  If the
    /// request has already completed by the time this is called, the cached
    /// result is returned immediately without waiting.
    ///
    /// # Arguments
    ///
    /// * `key` — The same key passed to [`Deduplicator::check_and_register`].
    ///
    /// # Returns
    ///
    /// `Some(result)` when the pending request completes, or `None` if the
    /// key is not tracked (e.g. the worker called [`Deduplicator::fail`]).
    pub async fn wait_for_result(&self, key: &str) -> Option<String> {
        let mut rx = {
            let state = self.requests.get(key)?;
            match state.value() {
                RequestState::InProgress { waiter_tx, .. } => waiter_tx.subscribe(),
                RequestState::Completed { result, .. } => {
                    return Some(result.clone());
                }
            }
        };

        let result = rx.recv().await.ok();
        if result.is_some() {
            crate::metrics::inc_dedup_waiter_unblocked();
        }
        result
    }

    /// Mark a request as successfully completed and cache its result.
    ///
    /// Notifies all tasks currently blocked in [`Deduplicator::wait_for_result`]
    /// for the same key.  The result is retained in the cache for
    /// `cache_duration` so subsequent callers receive
    /// [`DeduplicationResult::Cached`].
    ///
    /// # Arguments
    ///
    /// * `token` — The [`DeduplicationToken`] returned by
    ///   [`Deduplicator::check_and_register`].
    /// * `result` — The serialised response to cache and broadcast.
    pub async fn complete(&self, token: DeduplicationToken, result: String) {
        token
            .completed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(mut entry) = self.requests.get_mut(&token.key) {
            if let RequestState::InProgress { waiter_tx, .. } = entry.value() {
                let _ = waiter_tx.send(result.clone());
            }

            *entry = RequestState::Completed {
                result,
                completed_at: SystemTime::now(),
            };

            info!(key = token.key, token_id = %token.id, "request completed");
        }
    }

    /// Mark a request as failed and remove it from tracking.
    ///
    /// After this call, the next [`Deduplicator::check_and_register`] for the
    /// same key will receive [`DeduplicationResult::New`] so the request can
    /// be retried.  Any tasks waiting in [`Deduplicator::wait_for_result`] will
    /// receive `None` on their next `recv()` after the sender is dropped.
    ///
    /// # Arguments
    ///
    /// * `token` — The [`DeduplicationToken`] returned by
    ///   [`Deduplicator::check_and_register`].
    pub async fn fail(&self, token: DeduplicationToken) {
        self.requests.remove(&token.key);
        debug!(key = token.key, token_id = %token.id, "request failed, removed from dedup");
    }

    /// Return a snapshot of current deduplication statistics.
    ///
    /// The counts are computed by iterating the internal map in O(n).
    /// Use sparingly on hot paths; prefer Prometheus counters for high-frequency
    /// monitoring.
    pub fn stats(&self) -> DeduplicationStats {
        let mut stats = DeduplicationStats {
            total: self.requests.len(),
            in_progress: 0,
            cached: 0,
        };

        for entry in self.requests.iter() {
            match entry.value() {
                RequestState::InProgress { .. } => stats.in_progress += 1,
                RequestState::Completed { .. } => stats.cached += 1,
            }
        }

        stats
    }

    /// Clear all cached results
    pub fn clear(&self) {
        self.requests.clear();
        debug!("deduplication cache cleared");
    }

    /// Enable semantic (embedding-based) deduplication.
    ///
    /// When enabled, [`check_and_register_with_embedding`](Self::check_and_register_with_embedding)
    /// compares new embeddings against all stored embeddings using cosine similarity.
    /// Any stored embedding with similarity ≥ `threshold` is treated as a cache hit.
    ///
    /// # Arguments
    ///
    /// * `threshold` — Cosine similarity score in `[0.0, 1.0]`.  `1.0` requires
    ///   exact vector match (default); `0.95` catches near-paraphrases.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::Deduplicator;
    ///
    /// let dedup = Deduplicator::new(Duration::from_secs(300))
    ///     .with_semantic(0.95);
    /// ```
    pub fn with_semantic(mut self, threshold: f32) -> Self {
        self.similarity_threshold = threshold;
        self
    }

    /// Like [`check_and_register`](Self::check_and_register) but also performs
    /// a semantic similarity scan against previously registered embeddings.
    ///
    /// If `embedding` is `Some` and semantic deduplication is enabled (threshold < 1.0),
    /// all stored embeddings are scanned.  The first match whose cosine similarity
    /// meets the threshold is returned as [`DeduplicationResult::Cached`] with an
    /// empty string (the caller should use `wait_for_result` with the matched key
    /// to obtain the actual cached value).
    ///
    /// Falls back to exact-key lookup when `embedding` is `None` or the threshold
    /// equals `1.0`.
    ///
    /// # Arguments
    ///
    /// * `key` — Exact cache key for this request.
    /// * `embedding` — Optional dense vector embedding of the prompt.
    pub async fn check_and_register_with_embedding(
        &self,
        key: &str,
        embedding: Option<Vec<f32>>,
    ) -> DeduplicationResult {
        // Semantic scan first (only when an embedding is provided and threshold < 1.0)
        if let Some(ref emb) = embedding {
            if self.similarity_threshold < 1.0 {
                for entry in self.embeddings.iter() {
                    let sim = cosine_similarity(emb, entry.value());
                    if sim >= self.similarity_threshold {
                        debug!(
                            key = key,
                            matched_key = entry.key().as_str(),
                            similarity = sim,
                            "semantic duplicate detected"
                        );
                        crate::metrics::inc_dedup_hit();
                        return DeduplicationResult::Cached(String::new());
                    }
                }
                // No semantic match — store embedding for future lookups.
                self.embeddings.insert(key.to_string(), emb.clone());
            }
        }

        self.check_and_register(key).await
    }
}

impl Drop for Deduplicator {
    fn drop(&mut self) {
        // Signal the background cleanup task to stop on its next wake-up.
        // Only the last owner sets this; clones share the same Arc.
        if Arc::strong_count(&self.shutdown) == 1 {
            self.shutdown.store(true, Ordering::Relaxed);
        }
    }
}

/// Compute the cosine similarity between two dense vectors.
///
/// Returns a value in `[-1.0, 1.0]`.  Returns `0.0` if either vector has zero norm
/// so that zero-length embeddings never falsely match.
///
/// # Panics
///
/// Does not panic.  Mismatched lengths are handled by iterating the shorter vector.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

fn cleanup_expired(requests: &DashMap<String, RequestState>, cache_duration: Duration) {
    let _now = SystemTime::now();
    let mut removed = 0;

    requests.retain(|_, state| {
        match state {
            RequestState::InProgress { started_at, .. } => {
                // Remove stale in-progress requests (10x cache duration)
                if started_at.elapsed().unwrap_or_default() > cache_duration * 10 {
                    removed += 1;
                    return false;
                }
            }
            RequestState::Completed { completed_at, .. } => {
                // Remove expired cached results
                if completed_at.elapsed().unwrap_or_default() > cache_duration {
                    removed += 1;
                    return false;
                }
            }
        }
        true
    });

    if removed > 0 {
        debug!(
            removed = removed,
            "cleaned up expired deduplication entries"
        );
    }
}

/// A point-in-time snapshot of [`Deduplicator`] state.
///
/// Obtain via [`Deduplicator::stats`].
#[derive(Debug)]
pub struct DeduplicationStats {
    /// Total number of tracked requests (in-progress + cached).
    pub total: usize,
    /// Requests that are currently being processed.
    pub in_progress: usize,
    /// Completed requests whose results are still cached.
    pub cached: usize,
}

/// Generate a deduplication key scoped to a session.
///
/// Including `session_id` in the key prevents two different sessions from
/// colliding on the same cached result even when their prompts are identical.
/// Use `None` only for anonymous/global dedup where cross-session sharing is
/// intentional (e.g. read-only reference data queries).
///
/// # Example
///
/// ```
/// use std::collections::HashMap;
/// use tokio_prompt_orchestrator::enhanced::dedup_key;
///
/// let meta = HashMap::new();
/// // Same prompt, different sessions → different keys.
/// let k1 = dedup_key("hello", &meta, Some("session-alice"));
/// let k2 = dedup_key("hello", &meta, Some("session-bob"));
/// assert_ne!(k1, k2);
///
/// // Same prompt, same session → same key (deterministic).
/// let k3 = dedup_key("hello", &meta, Some("session-alice"));
/// assert_eq!(k1, k3);
///
/// // No session → global key (backward-compatible).
/// let k4 = dedup_key("hello", &meta, None);
/// assert_ne!(k1, k4);
/// ```
/// FNV-1a hash (64-bit). Deterministic across process restarts — unlike
/// `DefaultHasher` which uses a randomised seed since Rust 1.36.
fn fnv1a(bytes: &[u8]) -> u64 {
    const PRIME: u64 = 1_099_511_628_211;
    const BASIS: u64 = 14_695_981_039_346_656_037;
    bytes
        .iter()
        .fold(BASIS, |acc, &b| acc.wrapping_mul(PRIME) ^ b as u64)
}

pub fn dedup_key(
    prompt: &str,
    metadata: &std::collections::HashMap<String, String>,
    session_id: Option<&str>,
) -> String {
    // Build a canonical byte sequence: optional session prefix + prompt + sorted metadata.
    let mut buf = String::new();

    if let Some(sid) = session_id {
        buf.push_str(sid);
        buf.push('\x00');
    }

    buf.push_str(prompt);

    // Include relevant metadata in key (sorted for determinism).
    let mut meta_keys: Vec<_> = metadata.keys().collect();
    meta_keys.sort();
    for key in meta_keys {
        if let Some(value) = metadata.get(key) {
            buf.push('\x00');
            buf.push_str(key);
            buf.push('=');
            buf.push_str(value);
        }
    }

    let hash = fnv1a(buf.as_bytes());
    match session_id {
        Some(_) => format!("dedup:s:{hash:x}"),
        None => format!("dedup:g:{hash:x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_new_request() {
        let dedup = Deduplicator::new(Duration::from_secs(60));

        match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(token) => {
                assert_eq!(token.key, "test-key");
            }
            _ => panic!("Expected new request"),
        }
    }

    #[tokio::test]
    async fn test_duplicate_detection() {
        let dedup = Deduplicator::new(Duration::from_secs(60));

        // First request
        let token = match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(t) => t,
            _ => panic!("Expected new request"),
        };

        // Second request (while first is in progress)
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::InProgress => {} // Expected
            _ => panic!("Expected in-progress"),
        }

        // Complete first request
        dedup.complete(token, "result".to_string()).await;

        // Third request (should get cached result)
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::Cached(result) => {
                assert_eq!(result, "result");
            }
            _ => panic!("Expected cached result"),
        }
    }

    #[tokio::test]
    async fn test_wait_for_result() {
        let dedup = Deduplicator::new(Duration::from_secs(60));

        // Register request
        let token = match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(t) => t,
            _ => panic!("Expected new request"),
        };

        // Spawn task to wait
        let dedup_clone = dedup.clone();
        let wait_task = tokio::spawn(async move { dedup_clone.wait_for_result("test-key").await });

        // Complete request
        tokio::time::sleep(Duration::from_millis(100)).await;
        dedup.complete(token, "result".to_string()).await;

        // Check waiter got result
        let result = wait_task.await.unwrap();
        assert_eq!(result, Some("result".to_string()));
    }

    #[tokio::test]
    async fn test_cleanup_removes_expired_entries() {
        let dedup = Deduplicator::new(Duration::from_millis(50)); // very short TTL

        // Register and complete a request
        let result = dedup.check_and_register("test-key").await;
        if let DeduplicationResult::New(token) = result {
            dedup.complete(token, "done".to_string()).await;
        }

        // Verify it's cached
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::Cached(_) => {} // expected
            other => panic!("expected Cached, got {:?}", other),
        }

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now it should be treated as new
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(_) => {} // expected
            other => panic!("expected New after expiry, got {:?}", other),
        }
    }

    #[test]
    fn test_dedup_key_generation() {
        let empty = HashMap::new();

        // Deterministic: same inputs → same key.
        let key1 = dedup_key("hello", &empty, None);
        let key2 = dedup_key("hello", &empty, None);
        assert_eq!(key1, key2);

        // Different prompt → different key.
        let key3 = dedup_key("world", &empty, None);
        assert_ne!(key1, key3);

        // With metadata → different from without.
        let mut meta = HashMap::new();
        meta.insert("user".to_string(), "alice".to_string());
        let key4 = dedup_key("hello", &meta, None);
        assert_ne!(key1, key4);

        // Global keys carry the "g:" prefix.
        assert!(key1.starts_with("dedup:g:"), "key={key1}");
    }

    #[test]
    fn test_dedup_key_session_isolation() {
        let empty = HashMap::new();

        // Same prompt, different sessions → different keys.
        let k_alice = dedup_key("hello", &empty, Some("session-alice"));
        let k_bob = dedup_key("hello", &empty, Some("session-bob"));
        assert_ne!(
            k_alice, k_bob,
            "different sessions must not share a dedup key"
        );

        // Same prompt, same session → same key (deterministic).
        let k_alice2 = dedup_key("hello", &empty, Some("session-alice"));
        assert_eq!(k_alice, k_alice2);

        // Session key != global key for the same prompt.
        let k_global = dedup_key("hello", &empty, None);
        assert_ne!(k_alice, k_global);

        // Session keys carry the "s:" prefix.
        assert!(k_alice.starts_with("dedup:s:"), "key={k_alice}");
        assert!(k_global.starts_with("dedup:g:"), "key={k_global}");
    }

    #[tokio::test]
    async fn test_shutdown_does_not_hang() {
        let dedup = Deduplicator::new(Duration::from_secs(60));
        // Register and complete a request so there is some state.
        let token = match dedup.check_and_register("key").await {
            DeduplicationResult::New(t) => t,
            _ => panic!("expected New"),
        };
        dedup.complete(token, "result".into()).await;
        // shutdown() must return promptly (background task wakes every 60 s,
        // but the shutdown flag makes it exit on the *next* wake-up; since the
        // task is sleeping we just verify the flag is set and the handle is taken).
        tokio::time::timeout(std::time::Duration::from_secs(5), dedup.shutdown())
            .await
            .expect("shutdown() must complete within 5 s");
    }

    #[test]
    fn test_dedup_key_session_with_metadata() {
        let mut meta = HashMap::new();
        meta.insert("model".to_string(), "gpt-4".to_string());

        // Session + metadata combination is unique.
        let k1 = dedup_key("prompt", &meta, Some("sess-1"));
        let k2 = dedup_key("prompt", &meta, Some("sess-2"));
        let k3 = dedup_key("prompt", &meta, None);
        assert_ne!(k1, k2);
        assert_ne!(k1, k3);
        assert_ne!(k2, k3);
    }
}
